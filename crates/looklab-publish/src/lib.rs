// SPDX-License-Identifier: AGPL-3.0-or-later
//! Published static-site data: manifests, artifact hashes, page data and a sink-based
//! writer; no filesystem access.

mod audit;
mod body;
mod conformance;
mod hash;
mod input_transform;
mod interface;
mod manifest;
mod manual;
mod match_bundle;
#[cfg(feature = "photo-scenes")]
pub mod photo;
mod preview;
mod qualification;
mod scene;
#[cfg(feature = "scene-sources")]
mod scene_sources;
mod site;
mod source_lut;

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
pub use input_transform::{
    InputTransformEntry, InputTransformVersion, PublishedInputTransform, input_transform_path,
};
pub use interface::{
    CctSummary, INTERFACE_DEFINITION_ARTIFACT, INTERFACE_MANIFEST_FORMAT,
    INTERFACE_REPORT_ARTIFACT, InterfaceManifestFormat, InterfaceProfileEntry,
    InterfaceProfileManifest, InterfaceProvenance, MAX_REFERENCE_LUT_NODES,
    PublishedInterfaceProfile, RoundTripReport, interface_dir, interface_manifest_path,
    reference_round_trip,
};
pub use looklab_schema::{MatchBundleRevision, MatchBundleRevisionId, SelectionScope};
pub use manifest::{
    ArtifactBlob, ArtifactEntry, BodyUse, CARD_ARTIFACT, CameraModelProvenance, DOCUMENT_ARTIFACT,
    GateCheck, GateSummary, HashMismatch, LookArtifact, LookStatus, PREVIEW_LUT_ARTIFACT,
    ProvenanceSummary, RELEASE_ARTIFACT, SYNTHETIC_FIXTURE_GATE, SYNTHETIC_SAMPLE_GATE,
    VersionManifest, verify_artifact,
};
pub use manual::{ManualArtifact, ManualCapabilityPage, ManualPage, ManualSummary};
pub use match_bundle::{
    ExcludedSim, MATCH_BUNDLE_FORMAT, MatchBundle, MatchBundleFormat, MatchSim, SimArtifacts,
    SimExclusion, body_artifact, bundle_artifact, sim_artifacts, transform_artifact,
};
pub use preview::{
    LUT_BYTES, LUT_SIZE, PREVIEW_HIGH_EV, PREVIEW_LOW_EV, PREVIEW_PIVOT, PreparedScene, bake_lut,
    bake_lut_preview, bake_preview_lut, preview_shaper, preview_unshaper, render_preview,
};
pub use qualification::{
    BodyStateFormat, BundleEntry, BundleSelection, CurrentEntry, CurrentFormat, EligibilityEntry,
    EligibilityFormat, FamilyHeadPage, PublishedBodyState, PublishedBundleRevisionPage,
    PublishedCurrentProjection, PublishedReportSummary, RealizationCard, ReportFormat,
    ReportSummaryEntry, SelectedTargetSummary, SiteReport, SubjectCurrentPage,
    TargetEligibilitySnapshot, body_state_path, bundle_artifacts_path, bundle_dir,
    bundle_manifest_path, current_path, eligibility_path, report_path,
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
    LookVersionPage, LookVisibility, MANIFEST_NAME, PublishedSupport, ReferencePreview,
    SCENE_INDEX_PATH, SITE_FORMAT, SiteData, SiteIndex, SiteSink, VersionNotes, VersionSummary,
    WeakSpot, audit_log_path, body_path, compare_versions, index_path, look_path, manifest_path,
    manual_path, scene_index_path, scene_path, scene_thumbnail_path, version_dir, write_site,
};
pub use source_lut::{
    PreviewBaseline, SOURCE_CUBE_ARTIFACT, SOURCE_LUT_ARTIFACT, SOURCE_LUT_FORMAT, SourceLutFormat,
    SourceLutPreview, SourceLutPreviewContext, VersionLookShape, look_shape, look_shape_names,
};

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("invalid path `{0}`")]
    Path(String),
    #[error("match bundle: {0}")]
    MatchBundle(String),
    #[error("invalid artifact hash `{0}`")]
    Hash(String),
    #[error("{0}")]
    Interface(String),
    #[error("{0}")]
    InputTransform(String),
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
    #[error("report `{0}` appears twice")]
    DuplicateReport(String),
    #[error("eligibility snapshot `{0}` appears twice")]
    DuplicateEligibility(String),
    #[error("current subject `{0}` appears twice")]
    DuplicateCurrent(String),
    #[error("bundle revision `{0}` appears twice")]
    DuplicateBundle(String),
    #[error("interface profile `{0}` has two current revisions")]
    DuplicateInterface(String),
    #[error("{0}")]
    UnknownReference(String),
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
    #[error("a version carries both a layered Look and a source-only LUT preview")]
    ContradictoryLookArtifacts,
    #[error("a version carries neither a layered Look nor a source-only LUT preview")]
    MissingLookArtifact,
    #[error("source-only LUT preview: {0}")]
    SourceLut(String),
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
