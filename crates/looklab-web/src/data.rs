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

use crate::proposal_form::FixtureFile;
use crate::request_digest::{RequestDomain, challenge, request_digest};
use crate::webauthn::{self, PowParameters, SignedFields};
use crate::worker;

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
    let bytes = get("/data/index.json").await?;
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
    serde_json::from_slice(&bytes).map_err(|error| format!("/data/index.json: {error}"))
}

pub async fn body(make: &str, model: &str) -> Result<BodyPage, String> {
    get_json(&format!("/data/bodies/{make}/{model}.json")).await
}

pub async fn look(slug: &str) -> Result<LookPage, String> {
    get_json(&format!("/data/looks/{slug}.json")).await
}

pub async fn proposal(id: &ProposalRef) -> Result<ProposalView, String> {
    get_json(&format!("/api/proposals/{}", id.as_str())).await
}

pub async fn audit_log() -> Result<AuditLog, String> {
    get_json("/data/audit-log.json").await
}

pub async fn scenes() -> Result<SceneIndex, String> {
    get_json("/data/scenes/index.json").await
}

pub async fn scene(name: &str) -> Result<PreparedScene, String> {
    let index = index().await?;
    let summary = index
        .scenes
        .iter()
        .find(|scene| scene.name.as_str() == name)
        .ok_or_else(|| format!("unknown scene `{name}`"))?;
    let bytes = get(&format!("/{}", summary.path.as_str())).await?;
    let digest = ArtifactHash::of(&bytes);
    if digest != summary.sha256 || bytes.len() as u64 != summary.bytes {
        return Err(format!(
            "scene `{name}` does not match the site index: expected {}, got {digest}",
            summary.sha256
        ));
    }
    let scene = Scene::from_f16_bytes(
        SceneName::new(name).map_err(|error| error.to_string())?,
        summary.width,
        summary.height,
        &bytes,
    )
    .map_err(|error| error.to_string())?;
    Ok(PreparedScene::new(scene))
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
    let bytes = get(&format!("/data/looks/{slug}/{version}/manifest.json")).await?;
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
    let bytes = get(&format!(
        "/data/looks/{slug}/{version}/{}",
        entry.name.as_str()
    ))
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

pub async fn version(slug: &str, version: &str) -> Result<LoadedVersion, String> {
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
        let bytes = get(&format!(
            "/data/looks/{slug}/{version}/{}",
            entry.name.as_str()
        ))
        .await?;
        verify_artifact(&manifest, &entry.name, &bytes)
            .map_err(|error| format!("{slug} {version}: {error}"))?;
        match LookArtifact::of(&entry.name) {
            LookArtifact::Document => look_bytes = Some(bytes.clone()),
            LookArtifact::PreviewLut => lut = Some(bytes.clone()),
            LookArtifact::Other => {}
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

#[derive(Deserialize)]
struct Queue {
    proposals: Vec<QueueProposal>,
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
}

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

static MODERATION_SESSION: Mutex<Option<ModeratorToken>> = Mutex::new(None);

pub fn moderation_session_open() -> bool {
    MODERATION_SESSION
        .lock()
        .map(|token| token.is_some())
        .unwrap_or(false)
}

pub fn close_moderation_session() {
    if let Ok(mut slot) = MODERATION_SESSION.lock() {
        *slot = None;
    }
}

/// # Errors
/// Reports a WebAuthn failure or a missing moderator role.
pub async fn open_moderation_session() -> Result<(), String> {
    let credential = webauthn::ensure_credential().await?;
    let signed = webauthn::sign_request(RequestDomain::ModSession, &[], Some(&credential)).await?;
    let request = Request::post("/api/mod/session")
        .header("Content-Type", "application/json")
        .body(Some(json_body(&signed.fields)?))
        .map_err(|error| error.to_string())?;
    let reply: SessionReply = api_send("/api/mod/session", request).await?;
    if let Ok(mut slot) = MODERATION_SESSION.lock() {
        *slot = Some(ModeratorToken(reply.session_token));
    }
    Ok(())
}

/// # Errors
/// Reports a closed session or a server failure.
pub async fn moderation_queue() -> Result<Vec<QueueProposal>, String> {
    let token = moderation_token()?;
    let request = Request::get("/api/mod/queue")
        .header("Authorization", &format!("Bearer {}", token.0))
        .build()
        .map_err(|error| error.to_string())?;
    let queue: Queue = api_send("/api/mod/queue", request).await?;
    Ok(queue.proposals)
}

/// # Errors
/// Reports a closed session or a server failure.
pub async fn moderator_proposal(id: &ProposalRef) -> Result<ModeratorProposal, String> {
    let token = moderation_token()?;
    let path = format!("/api/mod/proposal/{}", id.as_str());
    let request = Request::get(&path)
        .header("Authorization", &format!("Bearer {}", token.0))
        .build()
        .map_err(|error| error.to_string())?;
    api_send(&path, request).await
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
    let request = Request::post("/api/mod/decision")
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {}", token.0))
        .body(Some(json_body(&body)?))
        .map_err(|error| error.to_string())?;
    api_send("/api/mod/decision", request).await
}

/// # Errors
/// Reports a closed session or a server failure.
pub async fn moderator_message(id: &ProposalRef, body: &str) -> Result<(), String> {
    let token = moderation_token()?;
    let request = Request::post("/api/mod/proposal-message")
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {}", token.0))
        .body(Some(json_body(&ModerationMessageRequest {
            proposal_id: id.as_str(),
            body,
        })?))
        .map_err(|error| error.to_string())?;
    let _: serde_json::Value = api_send("/api/mod/proposal-message", request).await?;
    Ok(())
}

/// # Errors
/// Reports a WebAuthn failure, an invalid tag list or a server rejection.
pub async fn confirm_proposal(
    id: &ProposalRef,
    tags: &[LabeledTag],
) -> Result<ProposalView, String> {
    let credential = webauthn::ensure_credential().await?;
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
    let request = Request::post("/api/proposals/confirm")
        .header("Content-Type", "application/json")
        .body(Some(json_body(&ConfirmRequest {
            proposal_id: id.as_str(),
            tags,
            signed: &signed.fields,
        })?))
        .map_err(|error| error.to_string())?;
    api_send("/api/proposals/confirm", request).await
}

/// # Errors
/// Reports a WebAuthn failure, a closed proposal or a server rejection.
pub async fn proposer_thread(id: &ProposalRef) -> Result<Vec<ProposalMessage>, String> {
    let credential = webauthn::ensure_credential().await?;
    let bytes = serde_json::to_vec(&ThreadSigningBody {
        proposal_id: id.as_str(),
    })
    .map_err(|error| error.to_string())?;
    let signed =
        webauthn::sign_request(RequestDomain::ProposalThread, &bytes, Some(&credential)).await?;
    let request = Request::post("/api/proposals/thread")
        .header("Content-Type", "application/json")
        .body(Some(json_body(&ThreadRequest {
            proposal_id: id.as_str(),
            signed: &signed.fields,
        })?))
        .map_err(|error| error.to_string())?;
    let reply: ThreadReply = api_send("/api/proposals/thread", request).await?;
    Ok(reply.messages)
}

/// # Errors
/// Reports a WebAuthn failure, a closed proposal or a server rejection.
pub async fn proposer_message(id: &ProposalRef, body: &str) -> Result<(), String> {
    let credential = webauthn::ensure_credential().await?;
    let bytes = serde_json::to_vec(&ProposerMessageSigningBody {
        proposal_id: id.as_str(),
        body,
    })
    .map_err(|error| error.to_string())?;
    let signed =
        webauthn::sign_request(RequestDomain::ProposalThread, &bytes, Some(&credential)).await?;
    let request = Request::post("/api/proposals/message")
        .header("Content-Type", "application/json")
        .body(Some(json_body(&ProposerMessageRequest {
            proposal_id: id.as_str(),
            body,
            signed: &signed.fields,
        })?))
        .map_err(|error| error.to_string())?;
    let _: serde_json::Value = api_send("/api/proposals/message", request).await?;
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
pub async fn upload_fixture(file: &FixtureFile) -> Result<String, String> {
    let credential = webauthn::ensure_credential().await?;
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
    let request = Request::post("/api/fixtures/donate")
        .header("Content-Type", "application/octet-stream")
        .body(Some(JsValue::from(js_sys::Uint8Array::from(
            framed.as_slice(),
        ))))
        .map_err(|error| error.to_string())?;
    let reply: FixtureUploadReply = api_send("/api/fixtures/donate", request).await?;
    Ok(reply.source)
}

/// # Errors
/// Reports a WebAuthn failure, proof-of-work failure or a server rejection.
pub async fn submit_proposal(
    kind: ProposalKind,
    payload: &str,
    fixtures: &[FixtureDonation],
) -> Result<Submitted, String> {
    let credential = webauthn::ensure_credential().await?;
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
    let request = Request::post("/api/proposals")
        .header("Content-Type", "application/json")
        .body(Some(json_body(&SubmitRequest {
            kind: kind.as_str(),
            payload,
            fixtures,
            pow,
            signed: &signed.fields,
        })?))
        .map_err(|error| error.to_string())?;
    api_send("/api/proposals", request).await
}

fn moderation_token() -> Result<ModeratorToken, String> {
    MODERATION_SESSION
        .lock()
        .ok()
        .and_then(|token| token.clone())
        .ok_or_else(|| "open a moderator session first".to_owned())
}

fn default_parameters() -> PowParameters {
    let defaults = default_pow();
    PowParameters {
        memory_kib: defaults.memory_kib,
        iterations: defaults.iterations,
        difficulty_bits_per_image: defaults.difficulty_bits_per_image,
    }
}

async fn solve_pow(challenge: [u8; 32], params: PowParameters) -> Result<Vec<u64>, String> {
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
