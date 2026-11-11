// SPDX-License-Identifier: AGPL-3.0-or-later
//! Fetching site data and checking every artifact against its version manifest.

use std::sync::{Arc, Mutex};

use gloo_net::http::Request;
use looklab_model::LookDocument;
use looklab_publish::{
    ArtifactEntry, ArtifactHash, AuditLog, BodyPage, BundleEntry, BundleSelection, CurrentEntry,
    EligibilityEntry, LUT_BYTES, LookArtifact, LookPage, PreparedScene, PreviewBaseline,
    PublishedReportSummary, ReportSummaryEntry, RoundTripReport, SOURCE_LUT_ARTIFACT, Scene,
    SceneIndex, SceneName, SelectionScope, SiteIndex, SitePath, SiteReport, SourceLutPreview,
    SubjectCurrentPage, TargetEligibilitySnapshot, VersionLookShape, VersionManifest,
    VersionSummary, bundle_dir, look_shape, verify_artifact,
};
use looklab_schema::{
    Applicability, BodyRealizationId, ControlEpochId, EligibilitySnapshotId, FirmwareId,
    FixtureDonation, FixtureUpload, LabeledTag, LookId, MatchBundleRevision, MatchBundleRevisionId,
    MatchParams, ModelReportId, ProposalKind, ProposalRef, ProposalView, PublishedTargetId, SimKey,
    SubjectKey, TargetEligibilityStatus, TargetVersion, WbModeKey,
};
use looklab_wasm::prep::{ModuleFunction, default_pow};
use looklab_wasm::{PowParamsSpec, decode_pow_reply, module_fn_id, solve_pow_request};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;

use crate::base::{api_url, publication_url};
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
    static SITE_INDEX: std::cell::RefCell<Option<SiteIndex>> = const { std::cell::RefCell::new(None) };
    static REFERENCE_PREVIEW: std::cell::OnceCell<Arc<Vec<u8>>> = const { std::cell::OnceCell::new() };
    static PREPARED_SCENES: std::cell::RefCell<std::collections::VecDeque<(SceneName, bool, PreparedScene)>> = const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
}

pub async fn published_raw(path: &SitePath) -> Result<Vec<u8>, String> {
    get(&publication_url(&format!("/{}", path.as_str()))).await
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

pub async fn index() -> Result<SiteIndex, String> {
    if let Some(index) = SITE_INDEX.with(|cache| cache.borrow().clone()) {
        return Ok(index);
    }
    fetch_index().await
}

/// Verifies the index against the deployment's advertised root and replaces the cached copy.
pub async fn fetch_index() -> Result<SiteIndex, String> {
    let bytes = get(&publication_url("/data/index.json")).await?;
    let expected = expected_site_root().await?;
    let actual = ArtifactHash::of(&bytes);
    if actual.as_str() != expected.as_str() {
        return Err(format!(
            "site index does not match the advertised root: expected {expected}, got {actual}"
        ));
    }
    let index: SiteIndex =
        serde_json::from_slice(&bytes).map_err(|error| format!("/data/index.json: {error}"))?;
    SITE_INDEX.with(|cache| *cache.borrow_mut() = Some(index.clone()));
    Ok(index)
}

#[derive(Deserialize)]
struct SiteRootReply {
    root: String,
}

async fn expected_site_root() -> Result<String, String> {
    if crate::base::server() == crate::base::Server::Preview {
        let root = crate::site_root();
        if root.is_empty() {
            return Err("this build has no embedded site root digest".to_owned());
        }
        return Ok(root.to_owned());
    }
    get_json::<SiteRootReply>(&crate::base::site_root_url())
        .await
        .map(|reply| reply.root)
}

pub async fn body(make: &str, model: &str) -> Result<BodyPage, String> {
    get_json(&publication_url(&format!(
        "/data/bodies/{make}/{model}.json"
    )))
    .await
}

pub async fn body_optional(make: &str, model: &str) -> Result<Option<BodyPage>, String> {
    let id = looklab_schema::BodyId::new(&format!("{make}/{model}"))
        .map_err(|error| error.to_string())?;
    if !index().await?.bodies.iter().any(|body| body.id == id) {
        return Ok(None);
    }
    body(make, model).await.map(Some)
}

#[derive(Deserialize)]
struct RegisteredBodies {
    bodies: Vec<looklab_schema::Body>,
}

pub async fn bodies_without_schema() -> Result<Vec<looklab_schema::Body>, String> {
    let reply: RegisteredBodies = get_json(&api_url("/api/body-context")).await?;
    Ok(reply.bodies)
}

enum ManualFetchError {
    /// The page bytes differ from the index entry.
    IndexMismatch(String),
    Other(String),
}

pub async fn manual(
    make: &str,
    model: &str,
) -> Result<Option<looklab_publish::ManualPage>, String> {
    let id = looklab_schema::BodyId::new(&format!("{make}/{model}"))
        .map_err(|error| error.to_string())?;
    match manual_from(&index().await?, &id, make, model).await {
        Ok(page) => Ok(page),
        // A publication since the index was cached replaces the page.
        Err(ManualFetchError::IndexMismatch(_)) => {
            manual_from(&fetch_index().await?, &id, make, model)
                .await
                .map_err(|error| match error {
                    ManualFetchError::IndexMismatch(message) | ManualFetchError::Other(message) => {
                        message
                    }
                })
        }
        Err(ManualFetchError::Other(message)) => Err(message),
    }
}

async fn manual_from(
    index: &SiteIndex,
    id: &looklab_schema::BodyId,
    make: &str,
    model: &str,
) -> Result<Option<looklab_publish::ManualPage>, ManualFetchError> {
    let Some(summary) = index.manuals.iter().find(|manual| manual.id == *id) else {
        return Ok(None);
    };
    let path = summary.artifact.path.as_str();
    let bytes = get(&publication_url(&format!("/{path}")))
        .await
        .map_err(ManualFetchError::Other)?;
    let digest = ArtifactHash::of(&bytes);
    if digest != summary.artifact.sha256 || bytes.len() as u64 != summary.artifact.bytes {
        return Err(ManualFetchError::IndexMismatch(format!(
            "{make}/{model}: manual page does not match the site index: expected {}, got {digest}",
            summary.artifact.sha256
        )));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| ManualFetchError::Other(format!("/{path}: {error}")))
}

pub async fn look(slug: &str) -> Result<LookPage, String> {
    get_json(&publication_url(&format!("/data/looks/{slug}.json"))).await
}

pub async fn proposal(id: &ProposalRef) -> Result<looklab_schema::ProposalDetailView, String> {
    get_json(&api_url(&format!("/api/proposals/{}/detail", id.as_str()))).await
}

pub async fn audit_log() -> Result<AuditLog, String> {
    get_json(&publication_url("/data/audit-log.json")).await
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

#[derive(Clone, Debug, serde::Deserialize)]
pub struct WantedFile {
    pub bounties: Vec<looklab_trust::Bounty>,
    #[serde(default)]
    pub evidence_requests: Vec<looklab_schema::EvidenceRequestWire>,
}

pub async fn wanted() -> Result<WantedFile, String> {
    get_json(&api_url("/bounties.json")).await
}

/// The wanted-shot requests, when the API is available.
pub async fn wanted_optional() -> Option<WantedFile> {
    if !crate::base::server_available() {
        return None;
    }
    wanted().await.ok()
}

/// The Look an evidence request is about, when the publication names one.
///
/// A per-realization request resolves through its body page, whose native-sim
/// cards name the Look each `(epoch, sim)` realization belongs to.
pub fn evidence_look<'a>(
    index: &'a SiteIndex,
    bodies: &'a [BodyPage],
    request: &looklab_schema::EvidenceRequestWire,
) -> Option<&'a looklab_model::file::Slug> {
    match &request.subject {
        looklab_schema::ModelSubject::SharedFamily { family } => index
            .looks
            .iter()
            .find(|look| look.slug.as_str() == family.as_str())
            .map(|look| &look.slug),
        looklab_schema::ModelSubject::BodyRealization { realization } => bodies
            .iter()
            .find(|body| body.id == realization.body)
            .and_then(|body| {
                body.native_looks.iter().find(|card| {
                    card.epoch == realization.epoch.get() && card.sim_key == realization.native_sim
                })
            })
            .map(|card| &card.look),
    }
}

/// Whether the Look an evidence request names has a published version.
pub fn evidence_look_published(
    index: &SiteIndex,
    bodies: &[BodyPage],
    request: &looklab_schema::EvidenceRequestWire,
) -> bool {
    evidence_look(index, bodies, request).is_some_and(|slug| {
        index
            .looks
            .iter()
            .any(|look| &look.slug == slug && !look.versions.is_empty())
    })
}

/// The active request for the independent captures a Look still needs.
pub fn missing_captures(wanted: &WantedFile, index: &SiteIndex, look: &str) -> Option<String> {
    wanted
        .evidence_requests
        .iter()
        .find(|request| {
            request.kind == looklab_schema::EvidenceRequestKind::MissingIndependentCaptures
                && evidence_look(index, &[], request).is_some_and(|slug| slug.as_str() == look)
        })
        .map(|request| request.detail.clone())
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
    let bytes = get(&publication_url(&format!("/{}", path.as_str()))).await?;
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

/// What a published version contains.
#[derive(Clone, Debug)]
pub enum VersionContent {
    /// Layered coefficients and the measurements they produce.
    Layered(Box<LayeredVersion>),
    /// A source LUT and its composition preview, without fitted layers.
    SourcePreview(Box<SourceLutPreview>),
}

#[derive(Clone, Debug)]
pub struct LayeredVersion {
    pub document: LookDocument,
    pub diagnostics: looklab_report::LookDiagnostics,
}

#[derive(Clone, Debug)]
pub struct LoadedVersion {
    pub manifest: VersionManifest,
    pub content: VersionContent,
    pub lut: Arc<Vec<u8>>,
    pub artifacts: Vec<(String, Vec<u8>)>,
}

/// The label a source-only preview carries instead of a measured character.
pub fn source_preview_label(preview: &SourceLutPreview) -> String {
    let mut label = match &preview.preview.baseline {
        PreviewBaseline::NeutralFallback => format!(
            "Preview only. The published preview LUT applies the source LUT to the neutral baseline {}@{} because no identified rendering of the declared base is available.",
            looklab_model::file::NEUTRAL_BASELINE_SLUG,
            looklab_model::file::NEUTRAL_BASELINE_VERSION,
        ),
        PreviewBaseline::Approximated { base } => format!(
            "Preview only. The published preview LUT applies the source LUT to {}, an identified approximation of the declared base.",
            base_label(base)
        ),
    };
    match preview.encoding.base() {
        Some(base) => label.push_str(&format!(
            " The declared display base {} is not measured, so this version has no fitted layers and no measured character.",
            base_label(base)
        )),
        None => label.push_str(
            " This version has no fitted layers, so it has no measured character.",
        ),
    }
    label
}

/// The composition the source-only preview applies, in pipeline order.
pub fn source_preview_composition(preview: &SourceLutPreview) -> String {
    let base = match &preview.preview.baseline {
        PreviewBaseline::NeutralFallback => format!(
            "neutral baseline {}@{}",
            looklab_model::file::NEUTRAL_BASELINE_SLUG,
            looklab_model::file::NEUTRAL_BASELINE_VERSION
        ),
        PreviewBaseline::Approximated { base } => {
            format!("{} (approximation)", base_label(base))
        }
    };
    let input = match preview.encoding.base() {
        Some(_) => base,
        None => "declared scene-referred encoding".to_owned(),
    };
    format!("scene XYZ -> {input} -> original LUT -> sRGB")
}

pub(crate) fn base_label(base: &looklab_model::file::BaseReference) -> String {
    match base {
        looklab_model::file::BaseReference::Look { slug, version } => {
            format!("{}@{}", slug.as_str(), version.as_str())
        }
        looklab_model::file::BaseReference::BodyRealization { body, .. } => {
            body.as_str().to_owned()
        }
        looklab_model::file::BaseReference::FamilyRendering {
            maker, rendering, ..
        } => format!("{maker} {rendering}"),
    }
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
    let bytes = get(&publication_url(&format!(
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
    let manifest: VersionManifest =
        serde_json::from_slice(&bytes).map_err(|error| format!("{slug} {version}: {error}"))?;
    if manifest.look.as_str() != slug || manifest.version.as_str() != version {
        return Err(format!(
            "{slug} {version}: the manifest identifies {} {}",
            manifest.look, manifest.version
        ));
    }
    Ok(manifest)
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
    let bytes = get(&publication_url(&format!(
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

pub async fn reference_preview() -> Result<Option<Arc<Vec<u8>>>, String> {
    if let Some(preview) = REFERENCE_PREVIEW.with(|cache| cache.get().cloned()) {
        return Ok(Some(preview));
    }
    let index = index().await?;
    let Some(preview) = index.reference_preview else {
        return Ok(None);
    };
    let bytes = get(&publication_url(&format!("/{}", preview.path.as_str()))).await?;
    if bytes.len() != LUT_BYTES
        || bytes.len() as u64 != preview.bytes
        || ArtifactHash::of(&bytes) != preview.sha256
    {
        return Err("The neutral preview could not be verified. Reload to try again.".to_owned());
    }
    let bytes = Arc::new(bytes);
    REFERENCE_PREVIEW.with(|cache| {
        let _ = cache.set(bytes.clone());
    });
    Ok(Some(bytes))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CardUse {
    Download,
    VerifyRenderTag,
}

pub async fn card(slug: &str, version: &str, purpose: CardUse) -> Result<Vec<u8>, String> {
    let index = match purpose {
        CardUse::Download => fetch_index().await?,
        CardUse::VerifyRenderTag => index().await?,
    };
    let summary = version_summary(&index, slug, version)?;
    if purpose == CardUse::Download && (!summary.card_eligible || !summary.permits_new_outputs()) {
        return Err(format!(
            "{slug} {version}: this target has no eligible firmware-card download"
        ));
    }
    let manifest = verified_manifest(slug, version, summary).await?;
    if look_shape(&manifest).map_err(|error| format!("{slug} {version}: {error}"))?
        == VersionLookShape::SourcePreview
    {
        return Err(format!(
            "{slug} {version}: a source-only LUT preview has no card file"
        ));
    }
    let entry = manifest
        .artifacts
        .iter()
        .find(|entry| matches!(LookArtifact::of(&entry.name), LookArtifact::Card))
        .ok_or_else(|| format!("{slug} {version}: no card file in the manifest"))?;
    let bytes = get(&publication_url(&format!(
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

pub struct ExportTarget {
    pub manifest_sha256: ArtifactHash,
    pub bytes: Vec<u8>,
    pub scope: SelectionScope,
    pub blockers: Vec<String>,
}

pub async fn export_target(
    slug: &looklab_model::file::Slug,
    version: &looklab_model::file::LookVersion,
    expected_manifest: Option<&ArtifactHash>,
) -> Result<ExportTarget, String> {
    let index = fetch_index().await?;
    let summary = version_summary(&index, slug.as_str(), version.as_str())?;
    if !summary.matchable {
        return Err("This target version is not eligible for a software export.".to_owned());
    }
    if !summary.permits_new_outputs() {
        return Err(
            "This target version is no longer applicable. This export is unavailable.".to_owned(),
        );
    }
    if expected_manifest.is_some_and(|expected| expected != &summary.manifest_sha256) {
        return Err("The target manifest differs from the pinned export.".to_owned());
    }
    let manifest = verified_manifest(slug.as_str(), version.as_str(), summary).await?;
    if look_shape(&manifest).map_err(|error| error.to_string())? == VersionLookShape::SourcePreview
    {
        return Err(
            "A source-only LUT preview has no fitted layers, so no software export exists."
                .to_owned(),
        );
    }
    let artifact = manifest
        .artifacts
        .iter()
        .find(|entry| matches!(LookArtifact::of(&entry.name), LookArtifact::Document))
        .ok_or("The target manifest has no Look document.")?;
    let path = looklab_publish::version_dir(slug, version)
        .map_err(|error| error.to_string())?
        .join(artifact.name.as_str());
    let bytes = published_bytes(&path, artifact.sha256.clone(), artifact.bytes).await?;
    let document = LookDocument::decode(&bytes).map_err(|error| error.to_string())?;
    if document.file.look.slug != *slug || document.file.look.version != *version {
        return Err("The Look document differs from the pinned target identity.".to_owned());
    }
    if !document.is_matchable() {
        return Err("This Look document is not eligible for export.".to_owned());
    }
    let look = index
        .looks
        .iter()
        .find(|look| look.slug == *slug)
        .ok_or("The target is not in the published catalog.")?;
    let (scope, blockers) = crate::selection::catalog_version_scope(look, version);
    Ok(ExportTarget {
        manifest_sha256: summary.manifest_sha256.clone(),
        bytes,
        scope,
        blockers,
    })
}

pub struct LoadedInterface {
    pub pin: looklab_publish::InterfaceProfileEntry,
    pub manifest: looklab_publish::InterfaceProfileManifest,
    pub definition: String,
    pub profile: looklab_schema::InterfaceProfile,
    pub report: looklab_publish::RoundTripReport,
    pub report_bytes: Vec<u8>,
}

pub async fn interface_profile(
    pin: &looklab_publish::InterfaceProfileEntry,
) -> Result<LoadedInterface, String> {
    let expected_path = looklab_publish::interface_manifest_path(&pin.manifest_sha256)
        .map_err(|error| error.to_string())?;
    if pin.manifest != expected_path {
        return Err("The editing-app manifest path differs from its revision.".to_owned());
    }
    let manifest_bytes = published_bytes(
        &pin.manifest,
        pin.manifest_sha256.clone(),
        pin.manifest_bytes,
    )
    .await?;
    let manifest: looklab_publish::InterfaceProfileManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|error| error.to_string())?;
    if manifest.id != pin.id || manifest.name != pin.name {
        return Err("The editing-app manifest differs from its catalog identity.".to_owned());
    }
    let directory =
        looklab_publish::interface_dir(&pin.manifest_sha256).map_err(|error| error.to_string())?;
    let definition = published_raw(&directory.join(manifest.definition.name.as_str())).await?;
    let profile = manifest
        .verify_definition(&definition)
        .map_err(|error| error.to_string())?;
    let report_bytes = published_raw(&directory.join(manifest.report.name.as_str())).await?;
    let report = manifest
        .verify_report(&report_bytes)
        .map_err(|error| error.to_string())?;
    let definition = String::from_utf8(definition).map_err(|error| error.to_string())?;
    Ok(LoadedInterface {
        pin: pin.clone(),
        manifest,
        definition,
        profile,
        report,
        report_bytes,
    })
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
    let index = match purpose {
        VersionPurpose::Browse => index().await?,
        VersionPurpose::Match => fetch_index().await?,
    };
    let summary = version_summary(&index, slug, version)?;
    if matches!(purpose, VersionPurpose::Match) && !summary.matchable {
        return Err(format!(
            "{slug} {version}: this version is a preview only and cannot be matched."
        ));
    }
    if matches!(purpose, VersionPurpose::Match) && !summary.permits_new_outputs() {
        return Err(format!(
            "{slug} {version}: this target is no longer eligible for matching."
        ));
    }
    let manifest = verified_manifest(slug, version, summary).await?;
    let shape = look_shape(&manifest).map_err(|error| format!("{slug} {version}: {error}"))?;
    if matches!(purpose, VersionPurpose::Match) && shape != VersionLookShape::Layered {
        return Err(format!(
            "{slug} {version}: this version is a source-only LUT preview and cannot be matched."
        ));
    }
    let mut look_bytes = None;
    let mut descriptor = None;
    let mut lut = None;
    let mut artifacts = Vec::with_capacity(manifest.artifacts.len());
    for entry in &manifest.artifacts {
        let is_descriptor = entry.name.as_str() == SOURCE_LUT_ARTIFACT;
        if matches!(purpose, VersionPurpose::Browse)
            && !is_descriptor
            && !matches!(
                LookArtifact::of(&entry.name),
                LookArtifact::Document | LookArtifact::PreviewLut
            )
        {
            continue;
        }
        let bytes = get(&publication_url(&format!(
            "/data/looks/{slug}/{version}/{}",
            entry.name.as_str()
        )))
        .await?;
        verify_artifact(&manifest, &entry.name, &bytes)
            .map_err(|error| format!("{slug} {version}: {error}"))?;
        if is_descriptor {
            descriptor = Some(bytes.clone());
        }
        match LookArtifact::of(&entry.name) {
            LookArtifact::Document => look_bytes = Some(bytes.clone()),
            LookArtifact::PreviewLut => lut = Some(bytes.clone()),
            LookArtifact::Card | LookArtifact::Other => {}
        }
        artifacts.push((entry.name.as_str().to_owned(), bytes));
    }
    let lut = lut.ok_or_else(|| format!("{slug} {version}: no preview LUT"))?;
    if lut.len() != LUT_BYTES {
        return Err(format!(
            "{slug} {version}: preview LUT has {} bytes",
            lut.len()
        ));
    }
    let content = match shape {
        VersionLookShape::Layered => {
            let look_bytes =
                look_bytes.ok_or_else(|| format!("{slug} {version}: no look artifact"))?;
            let document = LookDocument::decode(&look_bytes)
                .map_err(|error| format!("{slug} {version}: {error}"))?;
            let experimental_only = document.file.look.kind
                == looklab_model::LookKind::NativeFamily
                && document.match_refusal() == Some(looklab_model::file::MatchRefusal::Unsupported);
            if matches!(purpose, VersionPurpose::Match)
                && !document.is_matchable()
                && !experimental_only
            {
                return Err(format!(
                    "{slug} {version}: this version is a preview only and cannot be matched."
                ));
            }
            let probes = looklab_model::probes::v1();
            let adapter = crate::diagnostics::DocumentLook::new(&document, &probes);
            let diagnostics = looklab_report::look_diagnostics(&adapter, &probes);
            VersionContent::Layered(Box::new(LayeredVersion {
                document,
                diagnostics,
            }))
        }
        VersionLookShape::SourcePreview => {
            let descriptor =
                descriptor.ok_or_else(|| format!("{slug} {version}: no source LUT descriptor"))?;
            let preview = SourceLutPreview::decode(&descriptor)
                .map_err(|error| format!("{slug} {version}: {error}"))?;
            preview
                .verify_artifacts(&manifest)
                .map_err(|error| format!("{slug} {version}: {error}"))?;
            VersionContent::SourcePreview(Box::new(preview))
        }
    };
    Ok(LoadedVersion {
        manifest,
        content,
        lut: Arc::new(lut),
        artifacts,
    })
}

pub async fn published_bytes(
    path: &SitePath,
    sha256: ArtifactHash,
    bytes: u64,
) -> Result<Vec<u8>, String> {
    let url = publication_url(&format!("/{}", path.as_str()));
    let data = get(&url).await?;
    if data.len() as u64 != bytes || ArtifactHash::of(&data) != sha256 {
        return Err(format!(
            "{} does not match the site index: expected {sha256} ({bytes} bytes), got {} ({} bytes)",
            path.as_str(),
            ArtifactHash::of(&data),
            data.len()
        ));
    }
    Ok(data)
}

pub fn report_entry<'a>(
    index: &'a SiteIndex,
    id: &ModelReportId,
) -> Option<&'a ReportSummaryEntry> {
    index.reports.iter().find(|entry| entry.report == *id)
}

pub async fn report(index: &SiteIndex, id: &ModelReportId) -> Result<SiteReport, String> {
    let entry = report_entry(index, id)
        .ok_or_else(|| format!("report {id} is not listed in the site index"))?;
    let bytes = published_bytes(&entry.path, entry.sha256.clone(), entry.bytes).await?;
    let report: SiteReport = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{}: {error}", entry.path.as_str()))?;
    if report.summary.report != *id {
        return Err(format!("{} names another report", entry.path.as_str()));
    }
    Ok(report)
}

pub fn eligibility_entry<'a>(
    index: &'a SiteIndex,
    id: &EligibilitySnapshotId,
) -> Option<&'a EligibilityEntry> {
    index.eligibility.iter().find(|entry| entry.id == *id)
}

pub async fn eligibility(
    index: &SiteIndex,
    id: &EligibilitySnapshotId,
) -> Result<TargetEligibilitySnapshot, String> {
    let entry = eligibility_entry(index, id)
        .ok_or_else(|| format!("eligibility snapshot {id} is not listed in the site index"))?;
    let bytes = published_bytes(&entry.path, entry.sha256.clone(), entry.bytes).await?;
    let snapshot: TargetEligibilitySnapshot = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{}: {error}", entry.path.as_str()))?;
    if snapshot.id != *id {
        return Err(format!("{} names another snapshot", entry.path.as_str()));
    }
    Ok(snapshot)
}

pub fn current_entry<'a>(index: &'a SiteIndex, subject: &SubjectKey) -> Option<&'a CurrentEntry> {
    index
        .current
        .iter()
        .find(|entry| &entry.subject.key() == subject)
}

pub async fn current(
    index: &SiteIndex,
    subject: &SubjectKey,
) -> Result<SubjectCurrentPage, String> {
    let entry = current_entry(index, subject).ok_or_else(|| {
        format!("current selection for {subject} is not listed in the site index")
    })?;
    let bytes = published_bytes(&entry.path, entry.sha256.clone(), entry.bytes).await?;
    let page: SubjectCurrentPage = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{}: {error}", entry.path.as_str()))?;
    if &page.subject.key() != subject {
        return Err(format!("{} names another subject", entry.path.as_str()));
    }
    Ok(page)
}

pub fn bundle_entry<'a>(
    index: &'a SiteIndex,
    body: &looklab_schema::BodyId,
    epoch: u32,
) -> Option<&'a BundleEntry> {
    index
        .bundles
        .iter()
        .find(|entry| &entry.body == body && entry.epoch == epoch)
}

pub async fn bundle_artifacts(
    _index: &SiteIndex,
    entry: &BundleEntry,
) -> Result<Vec<ArtifactEntry>, String> {
    let bytes = published_bytes(
        &entry.artifacts_path,
        entry.artifacts_sha256.clone(),
        entry.artifacts_bytes,
    )
    .await?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("{}: {error}", entry.artifacts_path.as_str()))
}

pub async fn bundle_revision(
    _index: &SiteIndex,
    entry: &BundleEntry,
) -> Result<MatchBundleRevision, String> {
    let bytes = published_bytes(&entry.path, entry.sha256.clone(), entry.bytes).await?;
    let text =
        std::str::from_utf8(&bytes).map_err(|_| format!("{} is not UTF-8", entry.path.as_str()))?;
    let manifest = MatchBundleRevision::parse_manifest(text, 0)
        .map_err(|error| format!("{}: {error}", entry.path.as_str()))?;
    if manifest.id.to_hex() != entry.bundle.to_hex() {
        return Err(format!(
            "{} names another bundle revision",
            entry.path.as_str()
        ));
    }
    Ok(manifest)
}

/// The bundle selection a body page publishes for an epoch.
pub fn selected_bundle(page: &BodyPage, epoch: u32) -> Option<&BundleSelection> {
    page.bundles
        .iter()
        .find(|selection| selection.epoch == epoch && selection.applicability.is_current())
        .or_else(|| {
            page.bundles
                .iter()
                .find(|selection| selection.epoch == epoch)
        })
}

pub fn bundle_sim_entry<'a>(
    index: &'a SiteIndex,
    body: &looklab_schema::BodyId,
    epoch: u32,
    bundle: &str,
) -> Option<&'a BundleEntry> {
    index.bundles.iter().find(|entry| {
        &entry.body == body && entry.epoch == epoch && entry.bundle.to_hex() == bundle
    })
}

/// Whether a report's current selection still applies to its subject.
pub fn report_current(index: &SiteIndex, id: &ModelReportId) -> bool {
    report_entry(index, id).is_some_and(|entry| entry.applicability.is_current())
}

pub fn eligibility_current(index: &SiteIndex, id: &EligibilitySnapshotId) -> bool {
    eligibility_entry(index, id)
        .is_some_and(|entry| entry.status == TargetEligibilityStatus::Eligible)
}

/// The published target locator for a `look@version` pair.
pub fn published_target(
    look: &looklab_model::file::Slug,
    version: &looklab_model::file::LookVersion,
) -> Option<PublishedTargetId> {
    let look = LookId::new(look.as_str()).ok()?;
    let version = version
        .as_str()
        .split('.')
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .and_then(TargetVersion::new)?;
    Some(PublishedTargetId { look, version })
}

/// The current-projection subject of one camera realization.
pub fn realization_subject(body: &looklab_schema::BodyId, epoch: u32, sim: &SimKey) -> SubjectKey {
    SubjectKey::for_realization(&BodyRealizationId {
        body: body.clone(),
        epoch: ControlEpochId::new(epoch),
        native_sim: sim.clone(),
    })
}

pub async fn bundle_modes(
    index: &SiteIndex,
    page: &BodyPage,
    epoch: u32,
) -> Result<(Vec<WbModeKey>, Vec<FirmwareId>), String> {
    let Some(selection) = selected_bundle(page, epoch) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let entry = bundle_sim_entry(index, &page.id, epoch, &selection.bundle.to_hex())
        .ok_or_else(|| "The selected camera bundle is not listed in the site index.".to_owned())?;
    let manifest = bundle_revision(index, entry).await?;
    let Some(hash) = manifest.sims.iter().find_map(|sim| sim.body_params) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let dir = bundle_dir(&manifest.body, epoch, &manifest.id).map_err(|error| error.to_string())?;
    let path = dir.join("body_params.json");
    let bytes = published_raw(&path).await?;
    if ArtifactHash::of(&bytes).as_str() != hash.to_hex() {
        return Err("The body parameters do not match the bundle revision.".to_owned());
    }
    let params: MatchParams =
        serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", path.as_str()))?;
    params
        .validate(&manifest.body)
        .map_err(|error| error.to_owned())?;
    let mut firmware = Vec::new();
    for sim in &manifest.sims {
        let Some(hash) = sim.realization_params else {
            continue;
        };
        let path = dir.join(&hash.to_hex());
        let bytes = published_raw(&path).await?;
        if ArtifactHash::of(&bytes).as_str() != hash.to_hex() {
            return Err(
                "A realization parameter blob does not match the bundle revision.".to_owned(),
            );
        }
        let realization: looklab_schema::MatchRealizationParams = serde_json::from_slice(&bytes)
            .map_err(|error| format!("{}: {error}", path.as_str()))?;
        realization.validate().map_err(|error| error.to_owned())?;
        for entry in realization.firmware {
            if !firmware.contains(&entry.firmware) {
                firmware.push(entry.firmware);
            }
        }
    }
    Ok((
        params.wb.into_iter().map(|entry| entry.mode).collect(),
        firmware,
    ))
}

pub async fn bundle_qualification(
    index: &SiteIndex,
    body: &looklab_schema::BodyId,
    epoch: u32,
    sim: &SimKey,
    bundle: &MatchBundleRevisionId,
    mode: SelectionScope,
) -> Result<(SelectionScope, Option<PublishedReportSummary>), String> {
    let subject = realization_subject(body, epoch, sim);
    let Ok(page) = current(index, &subject).await else {
        return Ok((SelectionScope::Unassessed, None));
    };
    let Some(selection) = page
        .bundles
        .iter()
        .find(|selection| selection.bundle.to_hex() == bundle.to_hex())
    else {
        return Ok((SelectionScope::Unassessed, None));
    };
    if !selection.applicability.is_current() || selection.scope == SelectionScope::Unassessed {
        return Ok((SelectionScope::Unassessed, None));
    }
    let wanted = match mode {
        SelectionScope::Qualified => looklab_schema::ProjectionUse::QualifiedMatch,
        _ => looklab_schema::ProjectionUse::ExperimentalMatch,
    };
    let Some(report_id) = page
        .projections
        .iter()
        .find(|projection| {
            projection.capability == looklab_schema::QualificationCapability::ControlsMatching
                && projection.use_ == wanted
                && projection.applicability.is_current()
        })
        .and_then(|projection| projection.selected_report)
    else {
        return Ok((selection.scope, None));
    };
    let report = report(index, &report_id).await?;
    if report_is_passing_controls(&report.summary) {
        Ok((selection.scope, Some(report.summary)))
    } else {
        Ok((selection.scope, None))
    }
}

pub fn selection_scope_label(scope: SelectionScope) -> &'static str {
    match scope {
        SelectionScope::Qualified => "measured for these settings",
        SelectionScope::Experimental => "experimental",
        SelectionScope::Unassessed => "unassessed",
    }
}

pub fn applicability_label(applicability: &Applicability) -> String {
    match applicability {
        Applicability::Current => "current".to_owned(),
        Applicability::Invalidated { reason } => format!("invalidated: {}", reason.as_str()),
        Applicability::Unassessed => "unassessed".to_owned(),
    }
}

pub fn report_is_passing_controls(report: &PublishedReportSummary) -> bool {
    report.capability == looklab_schema::QualificationCapability::ControlsMatching
        && report.outcome == looklab_schema::AssessmentOutcome::Pass
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
    #[serde(default)]
    pub manual_checks: Vec<looklab_schema::ManualCheckReview>,
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

#[derive(Serialize)]
struct LookListingRequest<'a> {
    look: &'a str,
    reason: &'a str,
}

#[derive(Serialize)]
struct RevertLookRequest<'a> {
    look: &'a str,
    target_version: u32,
    rejected_contributions: &'a [String],
    reason: &'a str,
}

#[derive(Serialize)]
struct IntakeBlockRequest<'a> {
    credential: &'a str,
    reason: &'a str,
}

#[derive(Serialize)]
struct ReAnchorLookRequest<'a> {
    look: &'a str,
    version: u32,
    reason: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestorationHold {
    DependencyInvalidated,
    DependencyHistoryUnavailable,
    CurrentTargetUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Restoration {
    Current,
    HistoricalNeedsRebuild { reason: RestorationHold },
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct IntakeBlockReceipt {
    pub credential: String,
    pub blocked_at: i64,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ReAnchorReceipt {
    pub look: looklab_upload::LookSlug,
    pub version: u32,
    pub reason: String,
    pub anchored_at: i64,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct RevertReceipt {
    pub look: looklab_upload::LookSlug,
    pub target_version: u32,
    pub new_version: u32,
    pub rejected: Vec<String>,
    pub restoration: Restoration,
    pub lut_dependents: Vec<RevertVersionRef>,
    pub refit_queued: bool,
    pub reverted_at: i64,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct RevertVersionRef {
    pub look: looklab_upload::LookSlug,
    pub version: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LookListingAction {
    Hide,
    Remove,
    Relist,
}

impl LookListingAction {
    fn path(self) -> String {
        format!("/api/mod/look/{self}")
    }
}

impl core::fmt::Display for LookListingAction {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Hide => "hide",
            Self::Remove => "remove",
            Self::Relist => "relist",
        })
    }
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
pub async fn set_look_listing(
    action: LookListingAction,
    look: &str,
    reason: &str,
) -> Result<(), String> {
    let token = moderation_token()?;
    let path = api_url(&action.path());
    let request = Request::post(&path)
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {}", token.0))
        .body(Some(json_body(&LookListingRequest { look, reason })?))
        .map_err(|error| error.to_string())?;
    let _: EmptyReply = moderation_send(&path, request).await?;
    Ok(())
}

/// # Errors
/// Reports a closed session, an unpublished target, or a server rejection.
pub async fn revert_look(
    look: &str,
    target_version: u32,
    rejected_contributions: &[String],
    reason: &str,
) -> Result<RevertReceipt, String> {
    let token = moderation_token()?;
    let path = api_url("/api/mod/look/revert");
    let request = Request::post(&path)
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {}", token.0))
        .body(Some(json_body(&RevertLookRequest {
            look,
            target_version,
            rejected_contributions,
            reason,
        })?))
        .map_err(|error| error.to_string())?;
    moderation_send(&path, request).await
}

/// # Errors
/// Reports a closed session or a server rejection.
pub async fn block_intake(credential: &str, reason: &str) -> Result<IntakeBlockReceipt, String> {
    let token = moderation_token()?;
    let path = api_url("/api/mod/credential/block");
    let request = Request::post(&path)
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {}", token.0))
        .body(Some(json_body(&IntakeBlockRequest { credential, reason })?))
        .map_err(|error| error.to_string())?;
    moderation_send(&path, request).await
}

/// # Errors
/// Reports a closed session, an unreleased version, or a server rejection.
pub async fn reanchor_look(
    look: &str,
    version: u32,
    reason: &str,
) -> Result<ReAnchorReceipt, String> {
    let token = moderation_token()?;
    let path = api_url("/api/mod/look/reanchor");
    let request = Request::post(&path)
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {}", token.0))
        .body(Some(json_body(&ReAnchorLookRequest {
            look,
            version,
            reason,
        })?))
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

/// Posts the canonical confirmation request bytes from `looklab-wasm`.
///
/// # Errors
/// Reports a server rejection or a malformed request.
pub async fn submit_manual_confirmation(
    body: &[u8],
) -> Result<looklab_schema::ManualConfirmationSubmission, String> {
    let path = api_url("/api/manual-confirmations");
    let request = Request::post(&path)
        .header("Content-Type", "application/json")
        .body(Some(JsValue::from(js_sys::Uint8Array::from(body))))
        .map_err(|error| error.to_string())?;
    api_send(&path, request).await
}

/// Posts the canonical withdraw request bytes from `looklab-wasm`.
///
/// # Errors
/// Reports a server rejection or a malformed request.
pub async fn withdraw_manual_confirmation(
    camera: &looklab_schema::BodyId,
    signed: &SignedFields,
) -> Result<(), String> {
    let body = looklab_wasm::manual_withdraw_request(
        camera,
        &signed.nonce,
        &signed.credential_id,
        &signed.authenticator_data,
        &signed.client_data_json,
        &signed.signature,
    )
    .map_err(|error| error.to_string())?;
    let path = api_url("/api/manual-confirmations/withdraw");
    let request = Request::post(&path)
        .header("Content-Type", "application/json")
        .body(Some(JsValue::from(js_sys::Uint8Array::from(
            body.as_slice(),
        ))))
        .map_err(|error| error.to_string())?;
    api_send_empty(&path, request).await
}

/// # Errors
/// Reports a server rejection or a malformed response.
pub async fn manual_summary(
    make: &str,
    model: &str,
) -> Result<looklab_schema::ManualConfirmationSummary, String> {
    get_json(&api_url(&format!(
        "/api/manual-confirmations/{make}/{model}"
    )))
    .await
}

/// # Errors
/// Reports a closed moderation session, a server rejection or a malformed response.
pub async fn moderate_manual_confirmation(
    camera: &looklab_schema::BodyId,
    key: looklab_schema::CapabilityKey,
    body: &[u8],
) -> Result<looklab_schema::ManualConfirmationSummary, String> {
    let token = moderation_token()?;
    let path = api_url(&format!(
        "/api/manual-confirmations/{}/{}/moderate",
        camera.as_str(),
        key.as_str()
    ));
    let request = Request::post(&path)
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {}", token.0))
        .body(Some(JsValue::from(js_sys::Uint8Array::from(body))))
        .map_err(|error| error.to_string())?;
    let response = request
        .send()
        .await
        .map_err(|error| format!("{path}: {error}"))?;
    if response.status() == 401 {
        close_moderation_session();
        return Err("Your review session ended. Confirm it's you to continue.".to_owned());
    }
    if response.status() == 409 {
        let text = response.text().await.unwrap_or_default();
        let conflict = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .or_else(|| value.get("code"))
                    .and_then(|code| code.as_str())
                    .map(|code| code == "CONFLICT")
            })
            .unwrap_or(false);
        if conflict {
            return Err(
                "This control definition changed. Reload the camera page before recording a decision."
                    .to_owned(),
            );
        }
        return Err(format!("{path}: HTTP 409"));
    }
    if !response.ok() {
        return Err(format!("{path}: HTTP {}", response.status()));
    }
    response
        .json()
        .await
        .map_err(|error| format!("{path}: {error}"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ApiErrorCode {
    Conflict,
    CredentialBlocked,
    RateLimited,
    Forbidden,
    SchemaInvalid,
    SimulationTimeout,
    UnsupportedMediaType,
    Other(String),
}

impl ApiErrorCode {
    fn parse(value: &str) -> Self {
        match value {
            "CONFLICT" => Self::Conflict,
            "CREDENTIAL_BLOCKED" => Self::CredentialBlocked,
            "RATE_LIMITED" => Self::RateLimited,
            "FORBIDDEN" => Self::Forbidden,
            "SCHEMA_INVALID" => Self::SchemaInvalid,
            "SIMULATION_TIMEOUT" => Self::SimulationTimeout,
            "UNSUPPORTED_MEDIA_TYPE" => Self::UnsupportedMediaType,
            _ => Self::Other(value.to_owned()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ApiRejection {
    status: u16,
    code: Option<ApiErrorCode>,
    action: Option<String>,
}

impl ApiRejection {
    fn parse(status: u16, body: &str) -> Self {
        #[derive(Deserialize)]
        struct ErrorBody {
            error: Option<String>,
            action: Option<String>,
        }
        let body = serde_json::from_str::<ErrorBody>(body).ok();
        Self {
            status,
            code: body
                .as_ref()
                .and_then(|body| body.error.as_deref())
                .map(ApiErrorCode::parse),
            action: body.and_then(|body| body.action),
        }
    }

    fn message(&self, path: &str) -> String {
        match &self.code {
            Some(ApiErrorCode::Conflict) => format!(
                "{path}: the server rejected this as a conflict. Reload and check the existing entry before trying again."
            ),
            Some(ApiErrorCode::CredentialBlocked) => format!(
                "{path}: this profile cannot submit new contributions. Existing uploads can still be withdrawn from the status link."
            ),
            Some(ApiErrorCode::RateLimited) => {
                format!("{path}: too many requests. Wait a moment and try again.")
            }
            Some(ApiErrorCode::Forbidden) => {
                format!("{path}: this profile cannot do that.")
            }
            Some(ApiErrorCode::SchemaInvalid) => {
                format!("{path}: the server rejected the payload schema.")
            }
            Some(ApiErrorCode::SimulationTimeout) => format!(
                "{path}: the server could not check this profile in time. Try a smaller LUT grid."
            ),
            Some(ApiErrorCode::UnsupportedMediaType) => {
                format!("{path}: the server did not accept the request format.")
            }
            Some(ApiErrorCode::Other(code)) => match &self.action {
                Some(action) => format!("{path}: HTTP {} ({code}, {action})", self.status),
                None => format!("{path}: HTTP {} ({code})", self.status),
            },
            None => format!("{path}: HTTP {}", self.status),
        }
    }
}

fn api_rejection(path: &str, status: u16, body: &str) -> String {
    ApiRejection::parse(status, body).message(path)
}

async fn api_send_empty(path: &str, request: Request) -> Result<(), String> {
    let response = request
        .send()
        .await
        .map_err(|error| format!("{path}: {error}"))?;
    if !response.ok() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(api_rejection(path, status, &body));
    }
    Ok(())
}

/// # Errors
/// Reports a WebAuthn failure, an invalid tag list or a server rejection.
pub async fn confirm_proposal(
    session: crate::session::SessionContext,
    id: &ProposalRef,
    tags: &[LabeledTag],
) -> Result<looklab_schema::ProposalDetailView, String> {
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
    let proposal: ProposalView = api_send(&path, request).await?;
    Ok(looklab_schema::ProposalDetailView {
        proposal,
        confirmation: looklab_schema::ProposalConfirmation::CameraMenu,
    })
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
pub async fn submit_lut_proposal(
    session: crate::session::SessionContext,
    kind: ProposalKind,
    payload: &str,
    fixtures: &[FixtureDonation],
    file: &crate::proposal_form::LutFileDraft,
) -> Result<Submitted, String> {
    let submission = looklab_wasm::ProposalSubmission {
        kind,
        payload,
        fixtures,
    };
    let credential = webauthn::profile_credential(session).await?;
    let bytes =
        looklab_wasm::proposal_signing_bytes(&submission).map_err(|error| error.to_string())?;
    let signed = webauthn::sign_request(RequestDomain::Proposal, &bytes, Some(&credential)).await?;
    let digest = request_digest(RequestDomain::Proposal, &bytes);
    let challenge = challenge(&signed.nonce, &digest);
    let pow = solve_pow(challenge, signed.pow.unwrap_or_else(default_parameters)).await?;
    let framed =
        looklab_wasm::lut_proposal_request_bytes(&submission, &pow, &signed.fields, &file.bytes)
            .map_err(|error| error.to_string())?;
    let path = api_url("/api/proposals/lut");
    let request = Request::post(&path)
        .header("Content-Type", "application/octet-stream")
        .body(Some(JsValue::from(js_sys::Uint8Array::from(
            framed.as_slice(),
        ))))
        .map_err(|error| error.to_string())?;
    api_send(&path, request).await
}

/// # Errors
/// Reports a WebAuthn failure, proof-of-work failure or a server rejection.
pub async fn submit_proposal(
    session: crate::session::SessionContext,
    kind: ProposalKind,
    payload: &str,
    fixtures: &[FixtureDonation],
) -> Result<Submitted, String> {
    let submission = looklab_wasm::ProposalSubmission {
        kind,
        payload,
        fixtures,
    };
    let credential = webauthn::profile_credential(session).await?;
    let bytes =
        looklab_wasm::proposal_signing_bytes(&submission).map_err(|error| error.to_string())?;
    let signed = webauthn::sign_request(RequestDomain::Proposal, &bytes, Some(&credential)).await?;
    let digest = request_digest(RequestDomain::Proposal, &bytes);
    let challenge = challenge(&signed.nonce, &digest);
    let pow = solve_pow(challenge, signed.pow.unwrap_or_else(default_parameters)).await?;
    let body = looklab_wasm::proposal_request_bytes(&submission, &pow, &signed.fields)
        .map_err(|error| error.to_string())?;
    let text = std::str::from_utf8(&body).map_err(|error| error.to_string())?;
    let path = api_url("/api/proposals");
    let request = Request::post(&path)
        .header("Content-Type", "application/json")
        .body(Some(JsValue::from_str(text)))
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
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(api_rejection(path, status, &body));
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
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(api_rejection(path, status, &body));
    }
    response
        .json()
        .await
        .map_err(|error| format!("{path}: {error}"))
}
