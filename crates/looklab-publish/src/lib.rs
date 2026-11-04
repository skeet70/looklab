// SPDX-License-Identifier: AGPL-3.0-or-later
//! Published static-site data: manifests, artifact hashes, page data and a sink-based
//! writer; no filesystem access.

mod audit;
mod body;
mod hash;
mod manifest;
mod preview;
mod scene;
mod site;

pub use audit::{
    AuditEntry, AuditEvent, AuditLog, ContributionRef, CredentialRef, Decision, LookRef,
    ProposalRef, Role, SplitKind, Timestamp, VersionRef,
};
pub use body::{
    Affect, BodyExtras, BodyKind, BodyPage, BodyStatus, CapabilitiesPage, ControlConstraint,
    ControlDefaultPage, ControlPage, ControlRangePage, CredentialTier, EpochPage,
    InputTransformPage, MappingStatus, NativeLookCard, NativeSimPage, RealizationNote,
    SolvePlanStepPage, ToleratedPage, TransformSource,
};
pub use hash::{ArtifactHash, ArtifactName, SitePath};
pub use manifest::{
    ArtifactBlob, ArtifactEntry, BodyUse, DOCUMENT_ARTIFACT, GateCheck, GateSummary, HashMismatch,
    LookArtifact, LookStatus, PREVIEW_LUT_ARTIFACT, ProvenanceSummary, VersionManifest,
    verify_artifact,
};
pub use preview::{LUT_BYTES, LUT_SIZE, PreparedScene, bake_preview_lut, render_preview};
pub use scene::{SCENE_FORMAT, Scene, SceneIndex, SceneName, SceneSummary, f16_to_f32, f32_to_f16};
pub use site::{
    AUDIT_LOG_PATH, BodySummary, INDEX_PATH, LicensePage, LookPage, LookSummary, LookVersionPage,
    MANIFEST_NAME, SCENE_INDEX_PATH, SITE_FORMAT, SiteData, SiteIndex, SiteSink, VersionNotes,
    VersionSummary, WeakSpot, audit_log_path, body_path, index_path, look_path, manifest_path,
    scene_index_path, scene_path, version_dir, write_site,
};

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("invalid path `{0}`")]
    Path(String),
    #[error("invalid artifact hash `{0}`")]
    Hash(String),
    #[error("look `{look}` version `{version}` has no artifacts")]
    NoArtifacts { look: String, version: String },
    #[error("artifact `{0}` appears twice")]
    DuplicateArtifact(String),
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
    #[error("scene contains a value that is not finite")]
    SceneValue,
    #[error("preview LUT has {0} bytes")]
    PreviewLut(usize),
    #[error(transparent)]
    Manifest(#[from] HashMismatch),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Model(#[from] looklab_model::ModelError),
}
