// SPDX-License-Identifier: AGPL-3.0-or-later
//! Published static-site data: manifests, artifact hashes, page data and a sink-based
//! writer; no filesystem access.

mod audit;
mod body;
mod conformance;
mod hash;
mod manifest;
mod manual;
mod match_bundle;
#[cfg(feature = "photo-scenes")]
pub mod photo;
mod preview;
mod scene;
#[cfg(feature = "scene-sources")]
mod scene_sources;
mod site;

pub use audit::{
    AuditEntry, AuditEvent, AuditLog, ContributionRef, CredentialRef, Decision, LookRef,
    ProposalRef, Role, SplitKind, Timestamp, VersionRef,
};
pub use body::{
    Affect, BodyExtras, BodyKind, BodyPage, BodyStatus, CapabilitiesPage, ControlConstraint,
    ControlDefaultPage, ControlPage, ControlRangePage, CredentialTier, EpochPage,
    InputTransformPage, MappingStatus, NativeLookCard, NativeSimPage, PreparationBlocker,
    PreparationPage, PreparationReadiness, RealizationNote, SolvePlanStepPage, ToleratedPage,
    TransformSource,
};
pub use conformance::{
    CONFORMANCE_ARCHIVE_PATH, CONFORMANCE_METADATA_PATH, ConformanceArchive, ConformanceEntry,
    build_conformance_archive, write_conformance_pack,
};
pub use hash::{ArtifactHash, ArtifactName, SitePath};
pub use manifest::{
    ArtifactBlob, ArtifactEntry, BodyUse, CARD_ARTIFACT, DOCUMENT_ARTIFACT, GateCheck, GateSummary,
    HashMismatch, LookArtifact, LookStatus, PREVIEW_LUT_ARTIFACT, ProvenanceSummary,
    VersionManifest, verify_artifact,
};
pub use manual::{ManualArtifact, ManualCapabilityPage, ManualPage, ManualSummary};
pub use match_bundle::{
    ExcludedSim, MATCH_BUNDLE_FORMAT, MatchBundle, MatchBundleFormat, MatchSim, SimArtifacts,
    SimExclusion, body_artifact, bundle_artifact, sim_artifacts, transform_artifact,
};
pub use preview::{
    LUT_BYTES, LUT_SIZE, PREVIEW_HIGH_EV, PREVIEW_LOW_EV, PREVIEW_PIVOT, PreparedScene, bake_lut,
    bake_preview_lut, preview_shaper, preview_unshaper, render_preview,
};
pub use scene::{
    SCENE_FORMAT, Scene, SceneArtifact, SceneCredit, SceneIndex, SceneKind, SceneName,
    SceneSummary, test_chart_scenes,
};
#[cfg(feature = "scene-sources")]
pub use scene_sources::{
    SCENE_SOURCE_FORMAT, SCENE_SOURCE_INDEX, SceneSource, SceneSourceIndex, read_scene_sources,
    write_scene_sources,
};
pub use site::{
    AUDIT_LOG_PATH, BodySummary, INDEX_PATH, LicensePage, LookListing, LookPage, LookSummary,
    LookVersionPage, LookVisibility, MANIFEST_NAME, ReferencePreview, SCENE_INDEX_PATH,
    SITE_FORMAT, SiteData, SiteIndex, SiteSink, VersionNotes, VersionSummary, WeakSpot,
    audit_log_path, body_path, compare_versions, index_path, look_path, manifest_path,
    manual_path, scene_index_path, scene_path, scene_thumbnail_path, version_dir, write_site,
};

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("invalid path `{0}`")]
    Path(String),
    #[error("match bundle: {0}")]
    MatchBundle(String),
    #[error("invalid artifact hash `{0}`")]
    Hash(String),
    #[error("look `{look}` version `{version}` has no artifacts")]
    NoArtifacts { look: String, version: String },
    #[error("artifact `{0}` appears twice")]
    DuplicateArtifact(String),
    #[error("conformance archive: {0}")]
    Conformance(String),
    #[error("body `{body}` does not match schema body `{schema}`")]
    BodyMismatch { body: String, schema: String },
    #[error("body `{0}` appears twice")]
    DuplicateBody(String),
    #[error("look `{0}` appears twice")]
    DuplicateLook(String),
    #[error("look `{look}` version `{version}` appears twice")]
    DuplicateVersion { look: String, version: String },
    #[error("look `{expected}` has a manifest for look `{found}`")]
    ManifestLook { expected: String, found: String },
    #[error("scene `{0}` appears twice")]
    DuplicateScene(String),
    #[error("scene dimensions do not match the sample count")]
    SceneDimensions,
    #[error("chart scene size {0} is not in 3..=1600")]
    ChartSize(u32),
    #[error("scene sources: {0}")]
    SceneSources(String),
    #[error("scene contains a value that is not finite")]
    SceneValue,
    #[error("invalid prepared scene bytes")]
    PreparedScene,
    #[error("preview LUT has {0} bytes")]
    PreviewLut(usize),
    #[error("scene PNG: {0}")]
    ScenePng(String),
    #[error(transparent)]
    Manifest(#[from] HashMismatch),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Model(#[from] looklab_model::ModelError),
}
