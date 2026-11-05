// SPDX-License-Identifier: AGPL-3.0-or-later
//! Fetching site data and checking every artifact against its version manifest.

use std::sync::{Arc, Mutex};

use gloo_net::http::Request;
use looklab_model::LookDocument;
use looklab_publish::{
    ArtifactHash, AuditLog, BodyPage, LUT_BYTES, LookArtifact, LookPage, PreparedScene, Scene,
    SceneIndex, SceneName, SiteIndex, VersionManifest, VersionSummary, verify_artifact,
};
use looklab_schema::{
    FixtureDonation, FixtureUpload, LabeledTag, ProposalKind, ProposalRef, ProposalView,
};
use looklab_wasm::prep::{ModuleFunction, default_pow};
use looklab_wasm::{PowParamsSpec, decode_pow_reply, module_fn_id, solve_pow_request};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;

use crate::base::{api_url, site_url};
use crate::proposal_form::FixtureFile;
use crate::request_digest::{RequestDomain, challenge, request_digest};
use crate::webauthn::{self, PowParameters, SignedFields};
use crate::worker;

#[wasm_bindgen::prelude::wasm_bindgen(inline_js = r#"
export function looklabNow() { return performance.now(); }
export function looklabMeasure(name, start) { performance.measure(name, {start, end: performance.now()}); }
"#)]
extern "C" {
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = looklabNow)]
    fn performance_now() -> f64;
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = looklabMeasure)]
    fn performance_measure(name: &str, start: f64);
}

pub(crate) enum PreviewStage {
    SceneDecode,
    ScenePrepare,
    SceneReceive,
    LutApply,
    CanvasPaint,
}

pub(crate) fn timed<T>(stage: PreviewStage, operation: impl FnOnce() -> T) -> T {
    let start = performance_now();
    let result = operation();
    let name = match stage {
        PreviewStage::SceneDecode => "looklab.scene.decode",
        PreviewStage::ScenePrepare => "looklab.scene.prepare",
        PreviewStage::SceneReceive => "looklab.scene.receive",
        PreviewStage::LutApply => "looklab.lut.apply",
        PreviewStage::CanvasPaint => "looklab.canvas.paint",
    };
    performance_measure(name, start);
    result
}

#[derive(Clone, Debug, PartialEq)]
pub enum Load<T> {
    Loading,
    Failed(String),
    Ready(T),
}

impl<T> Load<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Self::Ready(value) => Some(value),
            _ => None,
        }
    }
}

thread_local! {
    static SITE_INDEX: std::cell::OnceCell<SiteIndex> = const { std::cell::OnceCell::new() };
    static PREPARED_SCENES: std::cell::RefCell<std::collections::VecDeque<(SceneName, bool, PreparedScene)>> = const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
}

async fn get(path: &str) -> Result<Vec<u8>, String> {
    let response = Request::get(path)
        .send()
        .await
        .map_err(|error| format!("{path}: {error}"))?;
    if !response.ok() {
        return Err(format!("{path}: HTTP {}", response.status()));
    }
    response
        .binary()
        .await
        .map_err(|error| format!("{path}: {error}"))
}

async fn get_json<T: DeserializeOwned>(path: &str) -> Result<T, String> {
    let bytes = get(path).await?;
    serde_json::from_slice(&bytes).map_err(|error| format!("{path}: {error}"))
}

/// The site index, verified against the digest embedded in this build.
pub async fn index() -> Result<SiteIndex, String> {
    if let Some(index) = SITE_INDEX.with(|cache| cache.get().cloned()) {
        return Ok(index);
    }
    let bytes = get(&site_url("/data/index.json")).await?;
    let expected = crate::site_root();
    if expected.is_empty() {
        return Err("this build has no embedded site root digest".to_owned());
    }
    let actual = ArtifactHash::of(&bytes);
    if actual.as_str() != expected {
        return Err(format!(
            "site index does not match the embedded root: expected {expected}, got {actual}"
        ));
    }
    let index: SiteIndex =
        serde_json::from_slice(&bytes).map_err(|error| format!("/data/index.json: {error}"))?;
    SITE_INDEX.with(|cache| {
        let _ = cache.set(index.clone());
    });
    Ok(index)
}

pub async fn body(make: &str, model: &str) -> Result<BodyPage, String> {
    get_json(&site_url(&format!("/data/bodies/{make}/{model}.json"))).await
}

pub async fn look(slug: &str) -> Result<LookPage, String> {
    get_json(&site_url(&format!("/data/looks/{slug}.json"))).await
}

pub async fn proposal(id: &ProposalRef) -> Result<ProposalView, String> {
    get_json(&api_url(&format!("/api/proposals/{}", id.as_str()))).await
}

pub async fn audit_log() -> Result<AuditLog, String> {
    get_json(&site_url("/data/audit-log.json")).await
}

pub async fn scenes() -> Result<SceneIndex, String> {
    Ok(SceneIndex {
        format: looklab_publish::SCENE_FORMAT.to_owned(),
        scenes: index().await?.scenes,
    })
}

pub async fn recipes() -> Result<Vec<looklab_trust::RecipeLeaderboard>, String> {
    get_json(&api_url("/api/recipes")).await
}

#[derive(serde::Deserialize)]
struct BountyFile {
    bounties: Vec<looklab_trust::Bounty>,
}

pub async fn bounties() -> Result<Vec<looklab_trust::Bounty>, String> {
    let file: BountyFile = get_json(&api_url("/bounties.json")).await?;
    Ok(file.bounties)
}

pub async fn scene(name: &str) -> Result<PreparedScene, String> {
    load_scene(name, false).await
}

pub async fn scene_thumbnail(name: &str) -> Result<PreparedScene, String> {
    load_scene(name, true).await
}

fn reference_scene(png: &[u8]) -> Result<PreparedScene, JsValue> {
    let name =
        SceneName::new("scene-reference").map_err(|error| JsValue::from_str(&error.to_string()))?;
    Scene::from_png(name, png)
        .map(PreparedScene::new)
        .map_err(|error| JsValue::from_str(&error.to_string()))
}

#[wasm_bindgen::prelude::wasm_bindgen]
pub fn looklab_prepare_scene_reference(png: &[u8]) -> Result<Vec<u8>, JsValue> {
    reference_scene(png).map(|scene| scene.to_bytes())
}

#[wasm_bindgen::prelude::wasm_bindgen]
pub fn looklab_render_scene_reference(png: &[u8], lut: &[u8]) -> Result<Vec<u8>, JsValue> {
    looklab_publish::render_preview(lut, &reference_scene(png)?)
        .map_err(|error| JsValue::from_str(&error.to_string()))
}

async fn load_scene(name: &str, thumbnail: bool) -> Result<PreparedScene, String> {
    if let Some(scene) = PREPARED_SCENES.with(|cache| {
        cache
            .borrow()
            .iter()
            .find(|(key, small, _)| key.as_str() == name && *small == thumbnail)
            .map(|(_, _, scene)| scene.clone())
    }) {
        return Ok(scene);
    }
    let index = index().await?;
    let summary = index
        .scenes
        .iter()
        .find(|scene| scene.name.as_str() == name)
        .ok_or_else(|| format!("unknown scene `{name}`"))?;
    let (path, hash, size, width, height) = if thumbnail {
        let t = &summary.thumbnail;
        (&t.path, &t.sha256, t.bytes, t.width, t.height)
    } else {
        (
            &summary.path,
            &summary.sha256,
            summary.bytes,
            summary.width,
            summary.height,
        )
    };
    let bytes = get(&site_url(&format!("/{}", path.as_str()))).await?;
    let digest = ArtifactHash::of(&bytes);
    if &digest != hash || bytes.len() as u64 != size {
        return Err(format!(
            "scene `{name}` does not match the site index: expected {}, got {digest}",
            hash
        ));
    }
    let scene_name = SceneName::new(name).map_err(|error| error.to_string())?;
    let mut prepared = if thumbnail {
        let scene = timed(PreviewStage::SceneDecode, || {
            Scene::from_png(scene_name, &bytes)
        })
        .map_err(|error| error.to_string())?;
        timed(PreviewStage::ScenePrepare, || PreparedScene::new(scene))
    } else {
        worker::prepare_scene(scene_name, &bytes).await?
    };
    if prepared.width() != width || prepared.height() != height {
        return Err(format!(
            "scene `{name}` dimensions do not match the site index"
        ));
    }
    prepared.scene.kind = summary.kind;
    prepared.scene.title = summary.title.clone();
    prepared.scene.credit = summary.credit.clone();
    PREPARED_SCENES.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.push_back((prepared.scene.name.clone(), thumbnail, prepared.clone()));
        while cache
            .iter()
            .map(|(_, _, scene)| scene.pixels())
            .sum::<usize>()
            > 2_000_000
        {
            cache.pop_front();
        }
    });
    Ok(prepared)
}

pub fn artifact<'a>(version: &'a LoadedVersion, name: &str) -> Option<&'a [u8]> {
    version
        .artifacts
        .iter()
        .find(|(entry, _)| entry == name)
        .map(|(_, bytes)| bytes.as_slice())
}
#[derive(Clone, Debug)]
pub struct LoadedVersion {
    pub manifest: VersionManifest,
    pub document: LookDocument,
    pub lut: Arc<Vec<u8>>,
    pub diagnostics: looklab_report::LookDiagnostics,
    pub artifacts: Vec<(String, Vec<u8>)>,
}

fn version_summary<'a>(
    index: &'a SiteIndex,
    slug: &str,
    version: &str,
) -> Result<&'a VersionSummary, String> {
    index
        .looks
        .iter()
        .find(|look| look.slug.as_str() == slug)
        .and_then(|look| {
            look.versions
                .iter()
                .find(|entry| entry.version.as_str() == version)
        })
        .ok_or_else(|| format!("{slug} {version} is not listed in the site index"))
}

async fn verified_manifest(
    slug: &str,
    version: &str,
    summary: &VersionSummary,
) -> Result<VersionManifest, String> {
    let bytes = get(&site_url(&format!(
        "/data/looks/{slug}/{version}/manifest.json"
    )))
    .await?;
    let digest = ArtifactHash::of(&bytes);
    if digest != summary.manifest_sha256 || bytes.len() as u64 != summary.manifest_bytes {
        return Err(format!(
            "{slug} {version}: manifest does not match the site index: expected {}, got {digest}",
            summary.manifest_sha256
        ));
    }
    serde_json::from_slice(&bytes).map_err(|error| format!("{slug} {version}: {error}"))
}

/// Downloads and verifies only the preview LUT.
///
/// # Errors
/// Reports a missing manifest entry, a hash mismatch or a wrong LUT length.
pub async fn preview(slug: &str, version: &str) -> Result<Arc<Vec<u8>>, String> {
    let index = index().await?;
    let summary = version_summary(&index, slug, version)?;
    let manifest = verified_manifest(slug, version, summary).await?;
    let entry = manifest
        .artifacts
        .iter()
        .find(|entry| matches!(LookArtifact::of(&entry.name), LookArtifact::PreviewLut))
        .ok_or_else(|| format!("{slug} {version}: no preview LUT in the manifest"))?;
    let bytes = get(&site_url(&format!(
        "/data/looks/{slug}/{version}/{}",
        entry.name.as_str()
    )))
    .await?;
    verify_artifact(&manifest, &entry.name, &bytes)
        .map_err(|error| format!("{slug} {version}: {error}"))?;
    if bytes.len() != LUT_BYTES {
        return Err(format!(
            "{slug} {version}: preview LUT has {} bytes",
            bytes.len()
        ));
    }
    Ok(Arc::new(bytes))
}

pub async fn card(slug: &str, version: &str) -> Result<Vec<u8>, String> {
    let index = index().await?;
    let summary = version_summary(&index, slug, version)?;
    let manifest = verified_manifest(slug, version, summary).await?;
    let entry = manifest
        .artifacts
        .iter()
        .find(|entry| matches!(LookArtifact::of(&entry.name), LookArtifact::Card))
        .ok_or_else(|| format!("{slug} {version}: no card file in the manifest"))?;
    let bytes = get(&site_url(&format!(
        "/data/looks/{slug}/{version}/{}",
        entry.name.as_str()
    )))
    .await?;
    verify_artifact(&manifest, &entry.name, &bytes)
        .map_err(|error| format!("{slug} {version}: {error}"))?;
    Ok(bytes)
}

pub async fn version(slug: &str, version: &str) -> Result<LoadedVersion, String> {
    load_version(slug, version, VersionPurpose::Browse).await
}

pub async fn match_version(slug: &str, version: &str) -> Result<LoadedVersion, String> {
    load_version(slug, version, VersionPurpose::Match).await
}

#[derive(Clone, Copy)]
enum VersionPurpose {
    Browse,
    Match,
}

async fn load_version(
    slug: &str,
    version: &str,
    purpose: VersionPurpose,
) -> Result<LoadedVersion, String> {
    let index = index().await?;
    let summary = version_summary(&index, slug, version)?;
    let manifest = verified_manifest(slug, version, summary).await?;
    if manifest.look.as_str() != slug || manifest.version.as_str() != version {
        return Err(format!(
            "manifest names {} {}, page requested {slug} {version}",
            manifest.look, manifest.version
        ));
    }
    let mut look_bytes = None;
    let mut lut = None;
    let mut artifacts = Vec::with_capacity(manifest.artifacts.len());
    for entry in &manifest.artifacts {
        if matches!(purpose, VersionPurpose::Browse)
            && !matches!(
                LookArtifact::of(&entry.name),
                LookArtifact::Document | LookArtifact::PreviewLut
            )
        {
            continue;
        }
        let bytes = get(&site_url(&format!(
            "/data/looks/{slug}/{version}/{}",
            entry.name.as_str()
        )))
        .await?;
        verify_artifact(&manifest, &entry.name, &bytes)
            .map_err(|error| format!("{slug} {version}: {error}"))?;
        match LookArtifact::of(&entry.name) {
            LookArtifact::Document => look_bytes = Some(bytes.clone()),
            LookArtifact::PreviewLut => lut = Some(bytes.clone()),
            LookArtifact::Card | LookArtifact::Other => {}
        }
        artifacts.push((entry.name.as_str().to_owned(), bytes));
    }
    let look_bytes = look_bytes.ok_or_else(|| format!("{slug} {version}: no look artifact"))?;
    let lut = lut.ok_or_else(|| format!("{slug} {version}: no preview LUT"))?;
    if lut.len() != LUT_BYTES {
        return Err(format!(
            "{slug} {version}: preview LUT has {} bytes",
            lut.len()
        ));
    }
    let document =
        LookDocument::decode(&look_bytes).map_err(|error| format!("{slug} {version}: {error}"))?;
    let probes = looklab_model::probes::v1();
    let adapter = crate::diagnostics::DocumentLook::new(&document, &probes);
    let diagnostics = looklab_report::look_diagnostics(&adapter, &probes);
    Ok(LoadedVersion {
        manifest,
        document,
        lut: Arc::new(lut),
        diagnostics,
        artifacts,
    })
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ProposalMessage {
    pub author: MessageAuthor,
    #[serde(default)]
    pub handle: Option<String>,
    pub body: String,
    pub created_at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageAuthor {
    Proposer,
    Moderator,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ModeratorProposal {
    pub proposal: ProposalView,
    pub payload: String,
    #[serde(default)]
    pub thread: Vec<ProposalMessage>,
    #[serde(default)]
    pub round_trip: Option<RoundTripReport>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct RoundTripReport {
    pub probes: usize,
    pub target_clipped: usize,
    pub flagged: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct QueueProposal {
    pub id: ProposalRef,
    pub kind: ProposalKind,
    pub summary: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ReviewQueue {
    pub proposals: Vec<QueueProposal>,
    #[serde(default)]
    pub failed_gates: Vec<FailedGate>,
    #[serde(default)]
    pub fit_holds: Vec<FitHold>,
    #[serde(default)]
    pub serial_reuse: Vec<SerialReuseFlag>,
    #[serde(default)]
    pub nominations: Vec<EndorsementNomination>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct FitHold {
    pub look: looklab_upload::LookSlug,
    pub consecutive: u32,
    pub first_attempt: i64,
    pub last_attempt: i64,
    pub diagnostics: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct FailedGate {
    pub look: looklab_upload::LookSlug,
    pub version: looklab_trust::VersionId,
    pub report: FailedGateReport,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct FailedGateReport {
    pub passed: Option<bool>,
    #[serde(default)]
    pub violations: Vec<looklab_trust::Violation>,
    pub root_previous: Option<looklab_trust::RootGateReport>,
    pub root_anchor: Option<looklab_trust::RootGateReport>,
    pub drift_previous: Option<looklab_trust::DriftGateReport>,
    pub drift_anchor: Option<looklab_trust::DriftGateReport>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct SerialReuseFlag {
    pub contributions: Vec<looklab_publish::ContributionRef>,
    pub credentials: Vec<looklab_publish::CredentialRef>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct EndorsementNomination {
    pub credential: looklab_publish::CredentialRef,
    pub display_handle: Option<crate::webauthn::DisplayHandle>,
    pub accepted_contributions: AcceptedContributions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct AcceptedContributions(u64);

impl AcceptedContributions {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approve,
    Reject,
    RequestInfo,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct SnapshotRecord {
    pub version: u64,
    pub content_hash: String,
    pub files: usize,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct DecisionReceipt {
    #[serde(default)]
    pub snapshot: Option<SnapshotRecord>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Submitted {
    pub view: ProposalView,
}

#[derive(Deserialize)]
struct SessionReply {
    session_token: String,
    expires_in_seconds: ModeratorLifetimeSeconds,
}

#[derive(Deserialize)]
#[serde(transparent)]
struct ModeratorLifetimeSeconds(u64);

#[derive(Deserialize)]
struct ThreadReply {
    #[serde(default)]
    messages: Vec<ProposalMessage>,
}

#[derive(Serialize)]
struct SubmitSigningBody<'a> {
    kind: &'a str,
    payload: &'a str,
    fixtures: &'a [FixtureDonation],
}

#[derive(Serialize)]
struct SubmitRequest<'a> {
    kind: &'a str,
    payload: &'a str,
    fixtures: &'a [FixtureDonation],
    pow: Vec<u64>,
    #[serde(flatten)]
    signed: &'a SignedFields,
}

#[derive(Serialize)]
struct ConfirmationSigningBody<'a> {
    proposal_id: &'a str,
    tags: &'a [LabeledTag],
}

#[derive(Serialize)]
struct ConfirmRequest<'a> {
    proposal_id: &'a str,
    tags: &'a [LabeledTag],
    #[serde(flatten)]
    signed: &'a SignedFields,
}

#[derive(Serialize)]
struct ThreadSigningBody<'a> {
    proposal_id: &'a str,
}

#[derive(Serialize)]
struct ThreadRequest<'a> {
    proposal_id: &'a str,
    #[serde(flatten)]
    signed: &'a SignedFields,
}

#[derive(Serialize)]
struct ProposerMessageSigningBody<'a> {
    proposal_id: &'a str,
    body: &'a str,
}

#[derive(Serialize)]
struct ProposerMessageRequest<'a> {
    proposal_id: &'a str,
    body: &'a str,
    #[serde(flatten)]
    signed: &'a SignedFields,
}

#[derive(Serialize)]
struct ModerationDecisionRequest<'a> {
    proposal_id: &'a str,
    decision: Decision,
    reason: &'a str,
}

#[derive(Serialize)]
struct ModerationMessageRequest<'a> {
    proposal_id: &'a str,
    body: &'a str,
}

#[derive(Clone)]
struct ModeratorToken(String);

impl std::fmt::Debug for ModeratorToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ModeratorToken(..)")
    }
}

struct ModeratorSession {
    token: ModeratorToken,
    expires_at: ModeratorExpiry,
}

struct ModeratorExpiry(f64);

static MODERATION_SESSION: Mutex<Option<ModeratorSession>> = Mutex::new(None);

pub fn moderation_session_remaining() -> Option<std::time::Duration> {
    let mut slot = MODERATION_SESSION.lock().ok()?;
    let remaining = slot.as_ref()?.expires_at.0 - js_sys::Date::now();
    if remaining <= 0.0 {
        *slot = None;
        return None;
    }
    Some(std::time::Duration::from_secs_f64(remaining / 1000.0))
}

pub fn moderation_session_open() -> bool {
    moderation_session_remaining().is_some()
}

pub fn close_moderation_session() {
    if let Ok(mut slot) = MODERATION_SESSION.lock() {
        *slot = None;
    }
}

/// # Errors
/// Reports a WebAuthn failure or a missing moderator role.
pub async fn open_moderation_session(
    session: crate::session::SessionContext,
) -> Result<std::time::Duration, String> {
    let credential = webauthn::profile_credential(session).await?;
    let signed = webauthn::sign_request(RequestDomain::ModSession, &[], Some(&credential)).await?;
    let path = api_url("/api/mod/session");
    let request = Request::post(&path)
        .header("Content-Type", "application/json")
        .body(Some(json_body(&signed.fields)?))
        .map_err(|error| error.to_string())?;
    let reply: SessionReply = api_send(&path, request).await?;
    let lifetime = std::time::Duration::from_secs(reply.expires_in_seconds.0);
    if let Ok(mut slot) = MODERATION_SESSION.lock() {
        *slot = Some(ModeratorSession {
            token: ModeratorToken(reply.session_token),
            expires_at: ModeratorExpiry(js_sys::Date::now() + lifetime.as_secs_f64() * 1000.0),
        });
    }
    Ok(lifetime)
}

/// # Errors
/// Reports a closed session or a server failure.
pub async fn moderation_queue() -> Result<ReviewQueue, String> {
    let token = moderation_token()?;
    let path = api_url("/api/mod/queue");
    let request = Request::get(&path)
        .header("Authorization", &format!("Bearer {}", token.0))
        .build()
        .map_err(|error| error.to_string())?;
    moderation_send(&path, request).await
}

/// # Errors
/// Reports a closed session or a server failure.
pub async fn moderator_proposal(id: &ProposalRef) -> Result<ModeratorProposal, String> {
    let token = moderation_token()?;
    let path = api_url(&format!("/api/mod/proposal/{}", id.as_str()));
    let request = Request::get(&path)
        .header("Authorization", &format!("Bearer {}", token.0))
        .build()
        .map_err(|error| error.to_string())?;
    moderation_send(&path, request).await
}

/// # Errors
/// Reports a closed session, a missing reason or a server failure.
pub async fn decide(
    id: &ProposalRef,
    decision: Decision,
    reason: &str,
) -> Result<DecisionReceipt, String> {
    let token = moderation_token()?;
    let body = ModerationDecisionRequest {
        proposal_id: id.as_str(),
        decision,
        reason,
    };
    let path = api_url("/api/mod/decision");
    let request = Request::post(&path)
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {}", token.0))
        .body(Some(json_body(&body)?))
        .map_err(|error| error.to_string())?;
    moderation_send(&path, request).await
}

/// # Errors
/// Reports a closed session or a server failure.
pub async fn moderator_message(id: &ProposalRef, body: &str) -> Result<(), String> {
    let token = moderation_token()?;
    let path = api_url("/api/mod/proposal-message");
    let request = Request::post(&path)
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {}", token.0))
        .body(Some(json_body(&ModerationMessageRequest {
            proposal_id: id.as_str(),
            body,
        })?))
        .map_err(|error| error.to_string())?;
    let _: EmptyReply = moderation_send(&path, request).await?;
    Ok(())
}

/// # Errors
/// Reports a WebAuthn failure, an invalid tag list or a server rejection.
pub async fn confirm_proposal(
    session: crate::session::SessionContext,
    id: &ProposalRef,
    tags: &[LabeledTag],
) -> Result<ProposalView, String> {
    let credential = webauthn::profile_credential(session).await?;
    let bytes = serde_json::to_vec(&ConfirmationSigningBody {
        proposal_id: id.as_str(),
        tags,
    })
    .map_err(|error| error.to_string())?;
    let signed = webauthn::sign_request(
        RequestDomain::ProposalConfirmation,
        &bytes,
        Some(&credential),
    )
    .await?;
    let path = api_url("/api/proposals/confirm");
    let request = Request::post(&path)
        .header("Content-Type", "application/json")
        .body(Some(json_body(&ConfirmRequest {
            proposal_id: id.as_str(),
            tags,
            signed: &signed.fields,
        })?))
        .map_err(|error| error.to_string())?;
    api_send(&path, request).await
}

/// # Errors
/// Reports a WebAuthn failure, a closed proposal or a server rejection.
pub async fn proposer_thread(
    session: crate::session::SessionContext,
    id: &ProposalRef,
) -> Result<Vec<ProposalMessage>, String> {
    let credential = webauthn::profile_credential(session).await?;
    let bytes = serde_json::to_vec(&ThreadSigningBody {
        proposal_id: id.as_str(),
    })
    .map_err(|error| error.to_string())?;
    let signed =
        webauthn::sign_request(RequestDomain::ProposalThread, &bytes, Some(&credential)).await?;
    let path = api_url("/api/proposals/thread");
    let request = Request::post(&path)
        .header("Content-Type", "application/json")
        .body(Some(json_body(&ThreadRequest {
            proposal_id: id.as_str(),
            signed: &signed.fields,
        })?))
        .map_err(|error| error.to_string())?;
    let reply: ThreadReply = api_send(&path, request).await?;
    Ok(reply.messages)
}

/// # Errors
/// Reports a WebAuthn failure, a closed proposal or a server rejection.
pub async fn proposer_message(
    session: crate::session::SessionContext,
    id: &ProposalRef,
    body: &str,
) -> Result<(), String> {
    let credential = webauthn::profile_credential(session).await?;
    let bytes = serde_json::to_vec(&ProposerMessageSigningBody {
        proposal_id: id.as_str(),
        body,
    })
    .map_err(|error| error.to_string())?;
    let signed =
        webauthn::sign_request(RequestDomain::ProposalThread, &bytes, Some(&credential)).await?;
    let path = api_url("/api/proposals/message");
    let request = Request::post(&path)
        .header("Content-Type", "application/json")
        .body(Some(json_body(&ProposerMessageRequest {
            proposal_id: id.as_str(),
            body,
            signed: &signed.fields,
        })?))
        .map_err(|error| error.to_string())?;
    let _: serde_json::Value = api_send(&path, request).await?;
    Ok(())
}

#[derive(Serialize)]
struct FixtureUploadHeader<'a> {
    file: &'a str,
    sha256: looklab_schema::Sha256Hex,
    size: u64,
    pow: Vec<u64>,
    #[serde(flatten)]
    signed: &'a SignedFields,
}

#[derive(Deserialize)]
struct FixtureUploadReply {
    source: String,
}

/// Returns the source string the uploaded file's donation cites.
///
/// # Errors
/// Reports a WebAuthn, proof-of-work, size or server failure.
pub async fn upload_fixture(
    session: crate::session::SessionContext,
    file: &FixtureFile,
) -> Result<String, String> {
    let credential = webauthn::profile_credential(session).await?;
    let size = u64::try_from(file.bytes.len()).map_err(|_| "file is too large".to_owned())?;
    let upload = FixtureUpload {
        file: file.name.clone(),
        sha256: file.sha256,
        size,
    };
    upload.validate().map_err(|errors| {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    let signing_bytes = serde_json::to_vec(&upload).map_err(|error| error.to_string())?;
    let signed = webauthn::sign_request(
        RequestDomain::FixtureUpload,
        &signing_bytes,
        Some(&credential),
    )
    .await?;
    let digest = request_digest(RequestDomain::FixtureUpload, &signing_bytes);
    let challenge = challenge(&signed.nonce, &digest);
    let pow = solve_pow(challenge, signed.pow.unwrap_or_else(default_parameters)).await?;
    let header = serde_json::to_vec(&FixtureUploadHeader {
        file: &upload.file,
        sha256: upload.sha256,
        size: upload.size,
        pow,
        signed: &signed.fields,
    })
    .map_err(|error| error.to_string())?;
    let framed = looklab_schema::frame_fixture_upload(&header, &file.bytes)
        .map_err(|error| error.to_string())?;
    let path = api_url("/api/fixtures/donate");
    let request = Request::post(&path)
        .header("Content-Type", "application/octet-stream")
        .body(Some(JsValue::from(js_sys::Uint8Array::from(
            framed.as_slice(),
        ))))
        .map_err(|error| error.to_string())?;
    let reply: FixtureUploadReply = api_send(&path, request).await?;
    Ok(reply.source)
}

/// # Errors
/// Reports a WebAuthn failure, proof-of-work failure or a server rejection.
pub async fn submit_proposal(
    session: crate::session::SessionContext,
    kind: ProposalKind,
    payload: &str,
    fixtures: &[FixtureDonation],
) -> Result<Submitted, String> {
    let credential = webauthn::profile_credential(session).await?;
    let bytes = serde_json::to_vec(&SubmitSigningBody {
        kind: kind.as_str(),
        payload,
        fixtures,
    })
    .map_err(|error| error.to_string())?;
    let signed = webauthn::sign_request(RequestDomain::Proposal, &bytes, Some(&credential)).await?;
    let digest = request_digest(RequestDomain::Proposal, &bytes);
    let challenge = challenge(&signed.nonce, &digest);
    let pow = solve_pow(challenge, signed.pow.unwrap_or_else(default_parameters)).await?;
    let path = api_url("/api/proposals");
    let request = Request::post(&path)
        .header("Content-Type", "application/json")
        .body(Some(json_body(&SubmitRequest {
            kind: kind.as_str(),
            payload,
            fixtures,
            pow,
            signed: &signed.fields,
        })?))
        .map_err(|error| error.to_string())?;
    api_send(&path, request).await
}

fn moderation_token() -> Result<ModeratorToken, String> {
    moderation_session_remaining()
        .ok_or_else(|| "Confirm it's you to continue reviewing.".to_owned())?;
    MODERATION_SESSION
        .lock()
        .ok()
        .and_then(|session| session.as_ref().map(|session| session.token.clone()))
        .ok_or_else(|| "Confirm it's you to continue reviewing.".to_owned())
}

#[derive(Deserialize)]
struct EmptyReply {}

async fn moderation_send<T: DeserializeOwned>(path: &str, request: Request) -> Result<T, String> {
    let response = request
        .send()
        .await
        .map_err(|error| format!("{path}: {error}"))?;
    if response.status() == 401 {
        close_moderation_session();
        return Err("Your review session ended. Confirm it's you to continue.".to_owned());
    }
    if !response.ok() {
        return Err(format!("{path}: HTTP {}", response.status()));
    }
    response
        .json()
        .await
        .map_err(|error| format!("{path}: {error}"))
}

pub(crate) fn default_parameters() -> PowParameters {
    let defaults = default_pow();
    PowParameters {
        memory_kib: defaults.memory_kib,
        iterations: defaults.iterations,
        difficulty_bits_per_image: defaults.difficulty_bits_per_image,
    }
}

pub(crate) async fn solve_pow(
    challenge: [u8; 32],
    params: PowParameters,
) -> Result<Vec<u64>, String> {
    let request = solve_pow_request(
        challenge,
        1,
        PowParamsSpec {
            memory_kib: params.memory_kib,
            iterations: params.iterations,
            difficulty_bits_per_image: params.difficulty_bits_per_image,
        },
        0,
    )
    .map_err(|error| error.to_string())?;
    let mut worker = worker::PrepWorker::spawn()?;
    let reply = worker
        .call(module_fn_id(ModuleFunction::SolvePow), request)
        .await;
    worker.terminate();
    let reply = reply.map_err(|error| error.message())?;
    Ok(decode_pow_reply(&reply.bytes)
        .map_err(|error| error.to_string())?
        .solutions
        .to_vec())
}

fn json_body<T: Serialize>(value: &T) -> Result<JsValue, String> {
    serde_json::to_string(value)
        .map(|text| JsValue::from_str(&text))
        .map_err(|error| error.to_string())
}

async fn api_send<T: DeserializeOwned>(path: &str, request: Request) -> Result<T, String> {
    let response = request
        .send()
        .await
        .map_err(|error| format!("{path}: {error}"))?;
    if !response.ok() {
        return Err(format!("{path}: HTTP {}", response.status()));
    }
    response
        .json()
        .await
        .map_err(|error| format!("{path}: {error}"))
}
