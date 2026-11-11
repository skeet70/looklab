// SPDX-License-Identifier: AGPL-3.0-or-later
//! Scoped model qualification persistence (docs/03 §1; docs/05 §8).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use looklab_schema::{
    Applicability, ArtifactBinding, ArtifactRole, ArtifactVisibility, AssessmentOutcome, BodyId,
    BodyRealizationId, BodyStatePins, BodyStateRecord, BodyStateStage, BodyStateVersion,
    CandidateSafety, CellId, CohortAdmissionRequest, CohortBasis, CohortMember, CohortRevision,
    CohortRole, ContributionRef, ControlEpochId, ControlSchema, DependencyGeneration,
    DependencyKind, DependencyRef, DependencyTarget, EligibilitySnapshotId, EnginePrerequisite,
    EnginePrerequisiteId, EvidenceRequest, EvidenceRequestKind, FamilyHead, FamilyTargetId,
    ImageRef, InvalidationReason, LegacyScope, LegacyScopeKind, LegacyScopeReason, LegacyTarget,
    LookId, MatchBundleRevision, MatchBundleRevisionId, ModelReport, ModelReportId, ModelSubject,
    ModelVersionId, ModelVersionKind, PendingGeneration, ProbeSetId, ProjectionUse,
    PublishedTargetId, QualificationCapability, QualificationValueError, RealizationRevisionId,
    RealizationStatus, RealizationVersion, ReportScope, ReportValidationError, SafetyCheck,
    ScopeKey, Sha256Hex, SimKey, SourceRef, SubjectKey, SupportedMask, TargetBasis,
    TargetEligibility, TargetEligibilityReason, TargetEligibilityStatus, TargetVersion,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Row, Sqlite, SqliteConnection, SqlitePool, Transaction};

use crate::ids::JobId;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("qualification value: {0}")]
    Value(#[from] QualificationValueError),
    #[error("report: {0}")]
    Report(#[from] ReportValidationError),
    #[error("unknown family target {0:?}")]
    UnknownFamilyTarget(FamilyTargetId),
    #[error("unknown published target {0:?}")]
    UnknownPublishedTarget(PublishedTargetId),
    #[error("unknown body state {body}@{version:?}")]
    UnknownBodyState {
        body: BodyId,
        version: BodyStateVersion,
    },
    #[error("schema body {schema} does not match {body}")]
    SchemaBodyMismatch { schema: BodyId, body: BodyId },
    #[error("{body} has no control epoch {epoch}")]
    UnknownEpoch { body: BodyId, epoch: u32 },
    #[error("{body} epoch {epoch} has no native sim {sim}")]
    UnknownNativeSim {
        body: BodyId,
        epoch: u32,
        sim: SimKey,
    },
    #[error("dependency {dependency:?} has no generation row")]
    UnregisteredDependency { dependency: DependencyRef },
    #[error("dependency {dependency:?} is revoked")]
    RevokedDependency { dependency: DependencyRef },
    #[error("dependency {dependency:?} changed after the pre-fit snapshot")]
    StaleDependency { dependency: DependencyRef },
    #[error("unknown model subject {0}")]
    UnknownSubject(SubjectKey),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("numerical sources are not the active implementation")]
    SupersededSources,
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

pub async fn begin_immediate(pool: &SqlitePool) -> Result<Transaction<'_, Sqlite>> {
    Ok(pool.begin_with("BEGIN IMMEDIATE").await?)
}

fn look_id(value: &str) -> Result<LookId> {
    LookId::new(value).map_err(|error| StoreError::InvalidInput(error.to_string()))
}

fn body_id(value: &str) -> Result<BodyId> {
    BodyId::new(value).map_err(|error| StoreError::InvalidInput(error.to_string()))
}

fn report_id(value: &str) -> Result<ModelReportId> {
    ModelReportId::parse(value).map_err(|error| StoreError::InvalidInput(error.to_string()))
}

fn parse_subject(value: &str) -> Result<ModelSubject> {
    Ok(SubjectKey::new(value)?
        .parse()
        .map_err(QualificationValueError::from)?)
}

fn stored_subject(value: &str) -> Result<(SubjectKey, ModelSubject)> {
    let key = SubjectKey::new(value)?;
    let subject = key.parse().map_err(QualificationValueError::from)?;
    Ok((key, subject))
}

/// A serialized resumable fit checkpoint, opaque to storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FitCheckpoint(String);

impl FitCheckpoint {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Default)]
pub struct FitAttempt {
    pub states: BTreeMap<String, FitCheckpoint>,
    pub consecutive: u32,
    pub started_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FitHoldStatus {
    Pending,
    Resolved,
}

impl FitHoldStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Resolved => "resolved",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EvidenceGroupId(String);

impl EvidenceGroupId {
    /// # Errors
    /// Empty, whitespace-bearing or over-long text.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() || value.len() > 256 || value.chars().any(char::is_whitespace) {
            return Err(StoreError::InvalidInput(
                "evidence group ids are non-empty single tokens".to_owned(),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceRole {
    Training,
    Assessment,
    Excluded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBlob {
    pub image: ImageRef,
    pub samples_hash: Sha256Hex,
    pub role: EvidenceRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceGroup {
    pub group: EvidenceGroupId,
    pub role: EvidenceRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRoleAssignment {
    pub contribution: ContributionRef,
    pub role: EvidenceRole,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceMembership {
    pub blobs: Vec<EvidenceBlob>,
    pub groups: Vec<EvidenceGroup>,
    pub roles: Vec<EvidenceRoleAssignment>,
    #[serde(default)]
    pub evaluated_images: Vec<ImageRef>,
    #[serde(default)]
    pub evaluated_contributions: Vec<ContributionRef>,
}

impl EvidenceMembership {
    pub fn digest(&self) -> Sha256Hex {
        let bytes =
            serde_json::to_vec(self).expect("evidence membership has no unserializable value");
        Sha256Hex::from_bytes(Sha256::digest(bytes).into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticVerdict {
    Accepted,
    DownWeighted,
    Quarantined,
}

impl DiagnosticVerdict {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::DownWeighted => "down_weighted",
            Self::Quarantined => "quarantined",
        }
    }

    /// # Errors
    /// A value outside the closed set.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "accepted" => Ok(Self::Accepted),
            "down_weighted" => Ok(Self::DownWeighted),
            "quarantined" => Ok(Self::Quarantined),
            other => Err(StoreError::InvalidInput(format!(
                "unknown diagnostic verdict `{other}`"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticWeightComponents {
    pub look: f64,
    pub body: f64,
    pub contributor: looklab_schema::ContributorRef,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticImageStats {
    pub image: ImageRef,
    pub rms_de: f64,
    pub z: f64,
    pub verdict: DiagnosticVerdict,
    pub reason: Option<String>,
    pub weight_components: DiagnosticWeightComponents,
}

/// Qualified pool-weight input; never read from the diagnostic or global namespaces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualifiedImageStats {
    pub image: ImageRef,
    pub rms_de: f64,
    pub z: f64,
    pub verdict: DiagnosticVerdict,
    pub reason: Option<String>,
    pub weight_components: DiagnosticWeightComponents,
}

pub struct QualifiedImageWrite<'a> {
    pub subject: &'a SubjectKey,
    pub sources: &'a NumericalSources,
    pub candidate: &'a ModelVersionId,
    pub scope_key: &'a ScopeKey,
    pub report: &'a ModelReportId,
    pub stats: &'a QualifiedImageStats,
    pub lease: JobLease<'a>,
    pub snapshot: &'a [DependencyGeneration],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QualifiedImageRejection {
    LeaseLost,
    SupersededSources,
    UnknownReport(ModelReportId),
    ReportNotPass(AssessmentOutcome),
    SubjectMismatch,
    CandidateMismatch,
    ScopeMismatch,
    ReportInapplicable(ModelReportId),
    Invalidated,
    NotCurrent,
    DependencyAdvanced(DependencyRef),
    DependencyUnregistered(DependencyRef),
    RevokedDependency(DependencyRef),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QualifiedImageOutcome {
    Written,
    Rejected(QualifiedImageRejection),
}

#[derive(Debug, Clone, Copy)]
pub struct JobLease<'a> {
    pub job: &'a JobId,
    pub attempts: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct SelectionRef<'a> {
    pub revision: &'a RealizationRevisionId,
    pub report: Option<&'a ModelReportId>,
}

#[derive(Debug, Clone, Copy)]
pub enum ExpectedSelection<'a> {
    Absent,
    Selected(SelectionRef<'a>),
}

#[derive(Debug, Clone, Copy)]
pub struct SupersedeExpectation<'a> {
    pub scope_key: &'a ScopeKey,
    pub expected: ExpectedSelection<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionRefOwned {
    pub revision: RealizationRevisionId,
    pub report: Option<ModelReportId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionRejection {
    LeaseLost,
    UnknownRevision(RealizationRevisionId),
    SupersededSources,
    AnchorChanged,
    WrongSubject {
        expected: SubjectKey,
        actual: SubjectKey,
    },
    Invalidated(RealizationRevisionId),
    TargetMismatch {
        pinned: FamilyTargetId,
        requested: FamilyTargetId,
    },
    FamilyTargetMismatch {
        expected: LookId,
        target: FamilyTargetId,
    },
    UnknownFamilyTarget(FamilyTargetId),
    TargetUnpublished(FamilyTargetId),
    TargetIneligible(FamilyTargetId),
    TargetInvalidated(FamilyTargetId),
    FamilyHeadInapplicable(LookId),
    FamilyHeadMismatch {
        expected: FamilyTargetId,
        actual: Option<FamilyTargetId>,
    },
    EnginePrerequisiteFailed(ModelReportId),
    PendingRegressed {
        selected: PendingGeneration,
        observed: PendingGeneration,
    },
    NotAccepted(RealizationStatus),
    SafetyFailed(CandidateSafety),
    MissingReport,
    UnknownReport(ModelReportId),
    ReportSubjectMismatch(ModelReportId),
    ReportCandidateMismatch(ModelReportId),
    ReportTargetMismatch(ModelReportId),
    ReportScopeMismatch(ModelReportId),
    ReportInapplicable(ModelReportId),
    SupersessionStale(ScopeKey),
    DisjointSupersession(ScopeKey),
    SupersessionRequiresReport,
    OutcomeNotPass(AssessmentOutcome),
    CapabilityMismatch {
        required: QualificationCapability,
        actual: QualificationCapability,
    },
    DependencyAdvanced(DependencyRef),
    DependencyUnregistered(DependencyRef),
    RevokedDependency(DependencyRef),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionOutcome {
    Selected,
    Stale { actual: Option<SelectionRefOwned> },
    Rejected(SelectionRejection),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FamilySelectionOutcome {
    Selected,
    Stale { actual: Option<TargetVersion> },
    Rejected(SelectionRejection),
}

#[derive(Debug, Clone, Copy)]
pub struct FamilySelectionRef<'a> {
    pub target: &'a FamilyTargetId,
    pub report: Option<&'a ModelReportId>,
}

#[derive(Debug, Clone, Copy)]
pub enum ExpectedFamilySelection<'a> {
    Absent,
    Selected(FamilySelectionRef<'a>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FamilySelectionRefOwned {
    pub target: FamilyTargetId,
    pub report: Option<ModelReportId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FamilyProjectionOutcome {
    Selected,
    Stale {
        actual: Option<FamilySelectionRefOwned>,
    },
    Rejected(SelectionRejection),
}

pub struct FamilySelection<'a> {
    pub family: &'a LookId,
    pub sources: &'a NumericalSources,
    pub expected_anchor: Option<u32>,
    pub target: &'a FamilyTargetId,
    pub report: &'a ModelReportId,
    pub scope_key: &'a ScopeKey,
    pub expected: ExpectedFamilySelection<'a>,
    pub lease: JobLease<'a>,
    pub snapshot: &'a [DependencyGeneration],
    pub observed_pending: PendingGeneration,
}

pub struct RealizationSelection<'a> {
    pub subject: &'a SubjectKey,
    pub sources: &'a NumericalSources,
    pub expected_anchor: Option<u32>,
    pub use_: ProjectionUse,
    pub capability: QualificationCapability,
    pub scope_key: &'a ScopeKey,
    pub revision: &'a RealizationRevisionId,
    pub target: &'a FamilyTargetId,
    pub report: Option<&'a ModelReportId>,
    pub supersedes: &'a [SupersedeExpectation<'a>],
    pub expected: ExpectedSelection<'a>,
    pub lease: JobLease<'a>,
    pub snapshot: &'a [DependencyGeneration],
    pub observed_pending: PendingGeneration,
}

pub struct FamilySeed<'a> {
    pub family: &'a LookId,
    pub sources: &'a NumericalSources,
    pub expected_anchor: Option<u32>,
    pub target: &'a FamilyTargetId,
    pub safety: CandidateSafety,
    pub source_limited: bool,
    pub seed_bodies: &'a [BodyId],
    pub dependencies: &'a [DependencyRef],
    pub lease: JobLease<'a>,
    pub snapshot: &'a [DependencyGeneration],
    pub observed_pending: PendingGeneration,
}

pub struct FamilyAdvance<'a> {
    pub family: &'a LookId,
    pub sources: &'a NumericalSources,
    pub expected_anchor: Option<u32>,
    pub expected: TargetVersion,
    pub target: &'a FamilyTargetId,
    pub safety: CandidateSafety,
    pub source_limited: bool,
    pub report: Option<&'a ModelReportId>,
    pub dependencies: &'a [DependencyRef],
    pub lease: JobLease<'a>,
    pub snapshot: &'a [DependencyGeneration],
    pub observed_pending: PendingGeneration,
}

/// Head metadata a moderator restore records for its target.
pub struct RestoredHeadMetadata {
    pub source_limited: bool,
    pub safety: CandidateSafety,
    pub assessed_generation: PendingGeneration,
}

impl RestoredHeadMetadata {
    /// Metadata for a restored target accepted by a seed or advance: a qualified target carries a required report dependency, and acceptance records only passed safety checks.
    pub fn for_restored_target(
        eligibility: &TargetEligibility,
        assessed_generation: PendingGeneration,
    ) -> Self {
        Self {
            source_limited: !eligibility
                .dependencies
                .iter()
                .any(|dependency| dependency.required && dependency.kind == DependencyKind::Report),
            safety: CandidateSafety {
                numerical: SafetyCheck::Passed,
                release_gates: SafetyCheck::Passed,
            },
            assessed_generation,
        }
    }
}

pub struct BundleSelection<'a> {
    pub body: &'a BodyId,
    pub epoch: u32,
    pub use_: ProjectionUse,
    pub id: &'a MatchBundleRevisionId,
    pub expected: Option<&'a MatchBundleRevisionId>,
    pub lease: JobLease<'a>,
    pub sources: &'a NumericalSources,
    pub snapshot: &'a [DependencyGeneration],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidationSummary {
    pub projections: u64,
    pub family_heads: u64,
    pub target_eligibilities: u64,
    pub bundles: u64,
    pub reports: u64,
}

impl InvalidationSummary {
    fn merge(&mut self, other: Self) {
        self.projections += other.projections;
        self.family_heads += other.family_heads;
        self.target_eligibilities += other.target_eligibilities;
        self.bundles += other.bundles;
        self.reports += other.reports;
    }

    fn any(&self) -> bool {
        self.projections > 0
            || self.family_heads > 0
            || self.target_eligibilities > 0
            || self.bundles > 0
            || self.reports > 0
    }
}

pub struct NewBundleRevision<'a> {
    pub body: &'a BodyId,
    pub epoch: u32,
    pub manifest: &'a str,
    pub artifacts: &'a [ArtifactBinding],
    pub dependencies: &'a [DependencyRef],
}

pub struct StoredBundleRevision {
    pub id: MatchBundleRevisionId,
    pub revision: MatchBundleRevision,
    pub manifest: String,
    pub artifacts: Vec<ArtifactBinding>,
    pub dependencies: Vec<DependencyRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentBundle {
    pub body: BodyId,
    pub epoch: u32,
    pub use_: ProjectionUse,
    pub bundle: MatchBundleRevisionId,
    pub applicability: Applicability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleSelectionRejection {
    LeaseLost,
    SupersededSources,
    UnknownBundle(MatchBundleRevisionId),
    Invalidated(MatchBundleRevisionId),
    BodyMismatch { expected: BodyId, actual: BodyId },
    EpochMismatch { expected: u32, actual: u32 },
    UseNotSelectable(ProjectionUse),
    DependencyAdvanced(DependencyRef),
    DependencyUnregistered(DependencyRef),
    RevokedDependency(DependencyRef),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleSelectionOutcome {
    Selected,
    Stale {
        actual: Option<MatchBundleRevisionId>,
    },
    Rejected(BundleSelectionRejection),
}

/// Whether a family cohort's latest revision may still be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FamilyCohortState {
    /// No cohort revision exists for the family and role.
    Empty,
    /// The latest revision is usable; `members` is its membership.
    Current,
    /// The latest admitted cohort's admission no longer applies; `members` is empty.
    Inapplicable {
        report: Option<ModelReportId>,
        reason: FamilyCohortInapplicable,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FamilyCohortInapplicable {
    /// The cohort revision has no admission row.
    MissingAdmission,
    /// The admission report row is absent.
    UnknownReport,
    /// The admission report does not bind this family, target and capability with a pass.
    WrongBinding,
    /// The admission report's dependencies changed or were revoked.
    NotApplicable,
    /// The admission report's engine prerequisite no longer passes.
    EnginePrerequisiteFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FamilyCohort {
    pub state: FamilyCohortState,
    pub report: Option<ModelReportId>,
    pub members: Vec<CohortMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceRequestOrigin {
    Report(ModelReportId),
    PreCandidate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEvidenceRequest {
    pub origin: EvidenceRequestOrigin,
    pub subject: ModelSubject,
    pub subject_key: SubjectKey,
    pub capability: QualificationCapability,
    pub request: EvidenceRequest,
    pub observed_groups: Option<u32>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservedVersionKind {
    Family,
    Realization,
}

impl ReservedVersionKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Family => "family",
            Self::Realization => "realization",
        }
    }

    /// # Errors
    /// A value outside the closed set.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "family" => Ok(Self::Family),
            "realization" => Ok(Self::Realization),
            other => Err(StoreError::InvalidInput(format!(
                "unknown reserved version kind `{other}`"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservedVersion {
    pub kind: ReservedVersionKind,
    pub version: u32,
    pub target: Option<FamilyTargetId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealizationStage {
    Native,
    Recipe,
}

pub struct NewRealizationRevision<'a> {
    pub subject: &'a BodyRealizationId,
    pub version: RealizationVersion,
    pub stage: RealizationStage,
    pub target: &'a FamilyTargetId,
    pub body_state_version: BodyStateVersion,
    pub input_transform_version: u32,
    pub realization_params_version: u32,
    pub control_response_version: u32,
    pub clip_k: f64,
    pub status: RealizationStatus,
    pub status_reason: Option<&'a str>,
    pub safety: CandidateSafety,
    pub artifacts: &'a [ArtifactBinding],
    pub dependencies: &'a [DependencyRef],
}

pub struct StoredRealizationRevision {
    pub id: RealizationRevisionId,
    pub subject: BodyRealizationId,
    pub version: RealizationVersion,
    pub target: FamilyTargetId,
    pub body_state_version: BodyStateVersion,
    pub input_transform_version: u32,
    pub realization_params_version: u32,
    pub control_response_version: u32,
    pub clip_k: f64,
    pub status: RealizationStatus,
    pub safety: CandidateSafety,
    pub artifacts: Vec<ArtifactBinding>,
    pub dependencies: Vec<DependencyRef>,
}

pub struct StoredReport {
    pub id: ModelReportId,
    pub report: ModelReport,
    pub dependencies: Vec<DependencyRef>,
    pub artifacts: Vec<ArtifactBinding>,
}

pub struct QualifiedSelection {
    pub projection: looklab_schema::CurrentProjection,
    pub report: ModelReport,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubjectState {
    pub anchor_revision: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectRevisionRole {
    Target,
    Native,
    Recipe,
    Diagnostic,
}

impl SubjectRevisionRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Target => "target",
            Self::Native => "native",
            Self::Recipe => "recipe",
            Self::Diagnostic => "diagnostic",
        }
    }
}

pub async fn register_subject(
    conn: &mut SqliteConnection,
    subject: &ModelSubject,
    now: i64,
) -> Result<SubjectKey> {
    let key = subject.key();
    let (kind, family, body, epoch, native_sim) = match subject {
        ModelSubject::SharedFamily { family } => {
            ("shared_family", Some(family.as_str()), None, None, None)
        }
        ModelSubject::BodyRealization { realization } => (
            "body_realization",
            None,
            Some(realization.body.as_str()),
            Some(i64::from(realization.epoch.get())),
            Some(realization.native_sim.as_str()),
        ),
    };
    sqlx::query(
        "INSERT INTO model_subjects (key, kind, family, body, epoch, native_sim, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (key) DO NOTHING",
    )
    .bind(key.as_str())
    .bind(kind)
    .bind(family)
    .bind(body)
    .bind(epoch)
    .bind(native_sim)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(key)
}

pub async fn register_body_realization(
    conn: &mut SqliteConnection,
    schema: &ControlSchema,
    body: &BodyId,
    epoch: u32,
    native_sim: &SimKey,
    now: i64,
) -> Result<BodyRealizationId> {
    if schema.body != *body {
        return Err(StoreError::SchemaBodyMismatch {
            schema: schema.body.clone(),
            body: body.clone(),
        });
    }
    let Some(epoch_entry) = schema.epochs.iter().find(|candidate| candidate.id == epoch) else {
        return Err(StoreError::UnknownEpoch {
            body: body.clone(),
            epoch,
        });
    };
    if epoch_entry.native_sim(native_sim).is_none() {
        return Err(StoreError::UnknownNativeSim {
            body: body.clone(),
            epoch,
            sim: native_sim.clone(),
        });
    }
    let realization = BodyRealizationId {
        body: body.clone(),
        epoch: ControlEpochId::new(epoch),
        native_sim: native_sim.clone(),
    };
    register_subject(
        conn,
        &ModelSubject::BodyRealization {
            realization: realization.clone(),
        },
        now,
    )
    .await?;
    Ok(realization)
}

pub async fn reserved_version(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
) -> Result<Option<ReservedVersion>> {
    let row = sqlx::query(
        "SELECT kind, version, target FROM model_reserved_versions WHERE subject_key = ?",
    )
    .bind(subject.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| {
        let version: i64 = row.try_get("version")?;
        let target: Option<String> = row.try_get("target")?;
        Ok(ReservedVersion {
            kind: ReservedVersionKind::parse(row.try_get::<String, _>("kind")?.as_str())?,
            version: u32::try_from(version).unwrap_or(0),
            target: target
                .map(|value| FamilyTargetId::parse(&value))
                .transpose()?,
        })
    })
    .transpose()
}

pub async fn release_reserved_version(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
) -> Result<()> {
    sqlx::query("DELETE FROM model_reserved_versions WHERE subject_key = ?")
        .bind(subject.as_str())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub async fn reserve_family_version(
    conn: &mut SqliteConnection,
    family: &LookId,
    now: i64,
) -> Result<FamilyTargetId> {
    let key = register_subject(
        conn,
        &ModelSubject::SharedFamily {
            family: family.clone(),
        },
        now,
    )
    .await?;
    if let Some(reserved) = reserved_version(conn, &key).await?
        && reserved.kind == ReservedVersionKind::Family
        && let Some(version) = TargetVersion::new(reserved.version)
    {
        return Ok(FamilyTargetId {
            family: family.clone(),
            version,
        });
    }
    let next: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(version), 0) + 1 FROM look_versions WHERE look = ?",
    )
    .bind(family.as_str())
    .fetch_one(&mut *conn)
    .await?;
    let version = TargetVersion::new(u32::try_from(next).unwrap_or(0))
        .ok_or_else(|| StoreError::InvalidInput("family version overflow".to_owned()))?;
    let target = FamilyTargetId {
        family: family.clone(),
        version,
    };
    sqlx::query(
        "INSERT INTO look_versions (id, look, version, status, created_at)
         VALUES (?, ?, ?, 'provisional', ?)",
    )
    .bind(format!("{family}-{}", version.get()))
    .bind(family.as_str())
    .bind(i64::from(version.get()))
    .bind(now)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO model_reserved_versions (subject_key, kind, version, target, reserved_at)
         VALUES (?, 'family', ?, ?, ?)
         ON CONFLICT (subject_key) DO UPDATE
         SET kind = excluded.kind, version = excluded.version,
             target = excluded.target, reserved_at = excluded.reserved_at",
    )
    .bind(key.as_str())
    .bind(i64::from(version.get()))
    .bind(target.to_key())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(target)
}

pub async fn reserve_realization_version(
    conn: &mut SqliteConnection,
    subject: &BodyRealizationId,
    target: &FamilyTargetId,
    now: i64,
) -> Result<(RealizationRevisionId, RealizationVersion)> {
    let key = register_subject(
        conn,
        &ModelSubject::BodyRealization {
            realization: subject.clone(),
        },
        now,
    )
    .await?;
    let existing: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(version), 0) FROM model_realization_revisions
         WHERE subject_key = ?",
    )
    .bind(key.as_str())
    .fetch_one(&mut *conn)
    .await?;
    let reserved = reserved_version(conn, &key).await?;
    let version = match reserved {
        Some(reserved)
            if reserved.kind == ReservedVersionKind::Realization
                && i64::from(reserved.version) > existing =>
        {
            RealizationVersion::new(reserved.version)
        }
        _ => u32::try_from(existing)
            .ok()
            .and_then(|existing| existing.checked_add(1))
            .and_then(RealizationVersion::new),
    }
    .ok_or_else(|| StoreError::InvalidInput("realization version overflow".to_owned()))?;
    sqlx::query(
        "INSERT INTO model_reserved_versions (subject_key, kind, version, target, reserved_at)
         VALUES (?, 'realization', ?, ?, ?)
         ON CONFLICT (subject_key) DO UPDATE
         SET kind = excluded.kind, version = excluded.version,
             target = excluded.target, reserved_at = excluded.reserved_at",
    )
    .bind(key.as_str())
    .bind(i64::from(version.get()))
    .bind(target.to_key())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    let id = RealizationRevisionId::for_version(&looklab_schema::BodyRealizationVersionId {
        realization: subject.clone(),
        version,
        target: target.clone(),
    });
    Ok((id, version))
}

pub async fn reserve_body_state(
    conn: &mut SqliteConnection,
    body: &BodyId,
    stage: BodyStateStage,
    now: i64,
) -> Result<BodyStateVersion> {
    let reserved: Option<(i64, String)> =
        sqlx::query_as("SELECT version, stage FROM model_body_state_reservations WHERE body = ?")
            .bind(body.as_str())
            .fetch_optional(&mut *conn)
            .await?;
    if let Some((version, reserved_stage)) = reserved
        && reserved_stage == stage.as_str()
        && let Some(version) = BodyStateVersion::new(u32::try_from(version).unwrap_or(0))
    {
        return Ok(version);
    }
    let states: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(version), 0) FROM model_body_states WHERE body = ?",
    )
    .bind(body.as_str())
    .fetch_one(&mut *conn)
    .await?;
    let reservations: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(version), 0) FROM model_body_state_reservations WHERE body = ?",
    )
    .bind(body.as_str())
    .fetch_one(&mut *conn)
    .await?;
    let next = states.max(reservations) + 1;
    let version = BodyStateVersion::new(u32::try_from(next).unwrap_or(0))
        .ok_or_else(|| StoreError::InvalidInput("body state version overflow".to_owned()))?;
    sqlx::query(
        "INSERT INTO model_body_state_reservations (body, version, stage, reserved_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT (body) DO UPDATE
         SET version = excluded.version, stage = excluded.stage, reserved_at = excluded.reserved_at",
    )
    .bind(body.as_str())
    .bind(i64::from(version.get()))
    .bind(stage.as_str())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(version)
}

pub async fn insert_body_state(
    conn: &mut SqliteConnection,
    record: &BodyStateRecord,
    snapshot: &[DependencyGeneration],
    now: i64,
) -> Result<BodyStateVersion> {
    if record.pins.input_transform.role != ArtifactRole::InputTransform
        || record.pins.body_params.role != ArtifactRole::BodyRealizationParams
        || record.pins.gauge.role != ArtifactRole::Gauge
        || record
            .pins
            .profile
            .as_ref()
            .is_some_and(|profile| profile.role != ArtifactRole::CalibrationProfile)
    {
        return Err(StoreError::InvalidInput(
            "body state pins must use transform, body-params, gauge and calibration-profile roles"
                .to_owned(),
        ));
    }
    let expected = snapshot_map(snapshot);
    for dependency in record
        .dependencies
        .iter()
        .filter(|dependency| dependency.required)
    {
        if revoked_dependency(conn, dependency).await? {
            return Err(StoreError::RevokedDependency {
                dependency: dependency.clone(),
            });
        }
        let Some(observed) = expected.get(&(dependency.kind, dependency.target.clone())) else {
            return Err(StoreError::UnregisteredDependency {
                dependency: dependency.clone(),
            });
        };
        let Some(current) = dependency_generation(conn, dependency).await? else {
            return Err(StoreError::UnregisteredDependency {
                dependency: dependency.clone(),
            });
        };
        if current != *observed {
            return Err(StoreError::StaleDependency {
                dependency: dependency.clone(),
            });
        }
    }
    let reserved: Option<(i64, String)> =
        sqlx::query_as("SELECT version, stage FROM model_body_state_reservations WHERE body = ?")
            .bind(record.body.as_str())
            .fetch_optional(&mut *conn)
            .await?;
    match reserved {
        Some((version, stage))
            if i64::from(record.version.get()) == version && stage == record.stage.as_str() => {}
        Some(_) => {
            return Err(StoreError::InvalidInput(
                "body state version does not match its reservation".to_owned(),
            ));
        }
        None => {
            let existing: i64 = sqlx::query_scalar(
                "SELECT COALESCE(MAX(version), 0) FROM model_body_states WHERE body = ?",
            )
            .bind(record.body.as_str())
            .fetch_one(&mut *conn)
            .await?;
            if i64::from(record.version.get()) <= existing {
                return Err(StoreError::InvalidInput(
                    "body state version was already used".to_owned(),
                ));
            }
        }
    }
    let inserted = sqlx::query(
        "INSERT INTO model_body_states
         (body, version, stage, profile_sha256, input_transform_sha256, body_params_sha256,
          gauge_sha256, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (body, version) DO NOTHING",
    )
    .bind(record.body.as_str())
    .bind(i64::from(record.version.get()))
    .bind(record.stage.as_str())
    .bind(
        record
            .pins
            .profile
            .as_ref()
            .map(|profile| profile.sha256.to_hex()),
    )
    .bind(record.pins.input_transform.sha256.to_hex())
    .bind(record.pins.body_params.sha256.to_hex())
    .bind(record.pins.gauge.sha256.to_hex())
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if inserted == 0
        && body_state(conn, &record.body, record.version)
            .await?
            .as_ref()
            != Some(record)
    {
        return Err(StoreError::InvalidInput(
            "body state version already holds different pins".to_owned(),
        ));
    }
    insert_dependency_edges(
        conn,
        "body_state",
        &DependencyTarget::body_state(&record.body, record.version).to_string(),
        &record.dependencies,
        now,
    )
    .await?;
    sqlx::query("DELETE FROM model_body_state_reservations WHERE body = ?")
        .bind(record.body.as_str())
        .execute(&mut *conn)
        .await?;
    Ok(record.version)
}

pub async fn body_state(
    conn: &mut SqliteConnection,
    body: &BodyId,
    version: BodyStateVersion,
) -> Result<Option<BodyStateRecord>> {
    let row = sqlx::query(
        "SELECT stage, profile_sha256, input_transform_sha256, body_params_sha256,
                gauge_sha256
         FROM model_body_states WHERE body = ? AND version = ?",
    )
    .bind(body.as_str())
    .bind(i64::from(version.get()))
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let owner = DependencyTarget::body_state(body, version).to_string();
    Ok(Some(BodyStateRecord {
        body: body.clone(),
        version,
        stage: BodyStateStage::parse(row.try_get::<String, _>("stage")?.as_str())?,
        pins: BodyStatePins {
            profile: row
                .try_get::<Option<String>, _>("profile_sha256")?
                .map(|sha256| {
                    Ok::<ArtifactBinding, StoreError>(ArtifactBinding {
                        sha256: Sha256Hex::parse(&sha256)
                            .map_err(|error| StoreError::InvalidInput(error.to_string()))?,
                        role: ArtifactRole::CalibrationProfile,
                    })
                })
                .transpose()?,
            input_transform: ArtifactBinding {
                sha256: Sha256Hex::parse(&row.try_get::<String, _>("input_transform_sha256")?)
                    .map_err(|error| StoreError::InvalidInput(error.to_string()))?,
                role: ArtifactRole::InputTransform,
            },
            body_params: ArtifactBinding {
                sha256: Sha256Hex::parse(&row.try_get::<String, _>("body_params_sha256")?)
                    .map_err(|error| StoreError::InvalidInput(error.to_string()))?,
                role: ArtifactRole::BodyRealizationParams,
            },
            gauge: ArtifactBinding {
                sha256: Sha256Hex::parse(&row.try_get::<String, _>("gauge_sha256")?)
                    .map_err(|error| StoreError::InvalidInput(error.to_string()))?,
                role: ArtifactRole::Gauge,
            },
        },
        dependencies: dependencies(conn, "body_state", &owner).await?,
    }))
}

pub async fn latest_body_state(
    conn: &mut SqliteConnection,
    body: &BodyId,
    stage: BodyStateStage,
) -> Result<Option<BodyStateRecord>> {
    let version: Option<i64> = sqlx::query_scalar(
        "SELECT MAX(version) FROM model_body_states WHERE body = ? AND stage = ?",
    )
    .bind(body.as_str())
    .bind(stage.as_str())
    .fetch_one(&mut *conn)
    .await?;
    let Some(version) =
        version.and_then(|version| BodyStateVersion::new(u32::try_from(version).unwrap_or(0)))
    else {
        return Ok(None);
    };
    body_state(conn, body, version).await
}

pub async fn body_states(
    conn: &mut SqliteConnection,
    body: Option<&BodyId>,
) -> Result<Vec<BodyStateRecord>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT body, version FROM model_body_states
         WHERE (? IS NULL OR body = ?)
         ORDER BY body, version",
    )
    .bind(body.map(BodyId::as_str))
    .bind(body.map(BodyId::as_str))
    .fetch_all(&mut *conn)
    .await?;
    let mut states = Vec::new();
    for (body, version) in rows {
        let version = BodyStateVersion::new(u32::try_from(version).unwrap_or(0))
            .ok_or_else(|| StoreError::InvalidInput("invalid body state version".to_owned()))?;
        if let Some(state) = body_state(conn, &body_id(&body)?, version).await? {
            states.push(state);
        }
    }
    Ok(states)
}

pub async fn insert_realization_revision(
    conn: &mut SqliteConnection,
    revision: &NewRealizationRevision<'_>,
    now: i64,
) -> Result<RealizationVersion> {
    if !(revision.clip_k.is_finite() && revision.clip_k > 0.0) {
        return Err(StoreError::InvalidInput(
            "clip_k must be finite and positive".to_owned(),
        ));
    }
    let subject_key = register_subject(
        conn,
        &ModelSubject::BodyRealization {
            realization: revision.subject.clone(),
        },
        now,
    )
    .await?;
    let target_exists: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM look_versions WHERE look = ? AND version = ?")
            .bind(revision.target.family.as_str())
            .bind(i64::from(revision.target.version.get()))
            .fetch_one(&mut *conn)
            .await?;
    if target_exists == 0 {
        return Err(StoreError::UnknownFamilyTarget(revision.target.clone()));
    }
    if body_state(conn, &revision.subject.body, revision.body_state_version)
        .await?
        .is_none()
    {
        return Err(StoreError::UnknownBodyState {
            body: revision.subject.body.clone(),
            version: revision.body_state_version,
        });
    }
    let version = revision.version;
    let existing: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(version), 0) FROM model_realization_revisions
         WHERE subject_key = ?",
    )
    .bind(subject_key.as_str())
    .fetch_one(&mut *conn)
    .await?;
    if i64::from(version.get()) <= existing {
        return Err(StoreError::InvalidInput(
            "realization version was already used".to_owned(),
        ));
    }
    if let Some(reserved) = reserved_version(conn, &subject_key).await?
        && reserved.kind == ReservedVersionKind::Realization
        && reserved.version != version.get()
    {
        return Err(StoreError::InvalidInput(
            "realization version does not match its reservation".to_owned(),
        ));
    }
    let id = RealizationRevisionId::for_version(&looklab_schema::BodyRealizationVersionId {
        realization: revision.subject.clone(),
        version,
        target: revision.target.clone(),
    });
    sqlx::query(
        "INSERT INTO model_realization_revisions
         (id, subject_key, body, version, family, target_version, body_state_version,
          input_transform_version, realization_params_version, control_response_version, clip_k,
          status, status_reason, numerical, release_gates, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id.as_str())
    .bind(subject_key.as_str())
    .bind(revision.subject.body.as_str())
    .bind(i64::from(version.get()))
    .bind(revision.target.family.as_str())
    .bind(i64::from(revision.target.version.get()))
    .bind(i64::from(revision.body_state_version.get()))
    .bind(i64::from(revision.input_transform_version))
    .bind(i64::from(revision.realization_params_version))
    .bind(i64::from(revision.control_response_version))
    .bind(revision.clip_k)
    .bind(revision.status.as_str())
    .bind(revision.status_reason)
    .bind(revision.safety.numerical.as_str())
    .bind(revision.safety.release_gates.as_str())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    for artifact in revision.artifacts {
        insert_artifact(conn, "realization_revision", id.as_str(), *artifact, now).await?;
    }
    let mut edges = revision.dependencies.to_vec();
    let family_dependency = DependencyRef::required(
        DependencyKind::FamilyVersion,
        DependencyTarget::family_version(revision.target),
    );
    if !edges.contains(&family_dependency) {
        edges.push(family_dependency);
    }
    let body_dependency = DependencyRef::required(
        DependencyKind::BodyState,
        DependencyTarget::body_state(&revision.subject.body, revision.body_state_version),
    );
    if !edges.contains(&body_dependency) {
        edges.push(body_dependency);
    }
    insert_dependency_edges(conn, "realization_revision", id.as_str(), &edges, now).await?;
    let role = match (revision.status, revision.stage) {
        (RealizationStatus::Diagnostic, _) => SubjectRevisionRole::Diagnostic,
        (RealizationStatus::Candidate, RealizationStage::Native) => SubjectRevisionRole::Native,
        (RealizationStatus::Candidate, RealizationStage::Recipe) => SubjectRevisionRole::Recipe,
    };
    record_subject_revision(conn, &subject_key, version.get(), role, None, None, now).await?;
    release_reserved_version(conn, &subject_key).await?;
    Ok(version)
}

pub async fn realization_revision(
    conn: &mut SqliteConnection,
    id: &RealizationRevisionId,
) -> Result<Option<StoredRealizationRevision>> {
    let row = sqlx::query(
        "SELECT subject_key, version, family, target_version, body_state_version,
                input_transform_version, realization_params_version, control_response_version,
                clip_k, status, numerical, release_gates
         FROM model_realization_revisions WHERE id = ?",
    )
    .bind(id.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let subject_key: String = row.try_get("subject_key")?;
    let subject = parse_subject(&subject_key)?;
    let ModelSubject::BodyRealization { realization } = subject else {
        return Err(StoreError::InvalidInput(
            "realization revision subject is not a body realization".to_owned(),
        ));
    };
    let version: i64 = row.try_get("version")?;
    let target = FamilyTargetId {
        family: look_id(&row.try_get::<String, _>("family")?)?,
        version: TargetVersion::new(
            u32::try_from(row.try_get::<i64, _>("target_version")?).unwrap_or(0),
        )
        .ok_or_else(|| StoreError::InvalidInput("invalid target version".to_owned()))?,
    };
    let version = RealizationVersion::new(u32::try_from(version).unwrap_or(0))
        .ok_or_else(|| StoreError::InvalidInput("invalid realization version".to_owned()))?;
    let body_state_version = BodyStateVersion::new(
        u32::try_from(row.try_get::<i64, _>("body_state_version")?).unwrap_or(0),
    )
    .ok_or_else(|| StoreError::InvalidInput("invalid body state version".to_owned()))?;
    let status = RealizationStatus::parse(row.try_get::<String, _>("status")?.as_str())?;
    let safety = CandidateSafety {
        numerical: SafetyCheck::parse(row.try_get::<String, _>("numerical")?.as_str())?,
        release_gates: SafetyCheck::parse(row.try_get::<String, _>("release_gates")?.as_str())?,
    };
    let artifacts = public_artifacts(conn, "realization_revision", id.as_str()).await?;
    let dependencies = dependencies(conn, "realization_revision", id.as_str()).await?;
    Ok(Some(StoredRealizationRevision {
        id: id.clone(),
        subject: realization,
        version,
        target,
        body_state_version,
        input_transform_version: u32::try_from(row.try_get::<i64, _>("input_transform_version")?)
            .unwrap_or(0),
        realization_params_version: u32::try_from(
            row.try_get::<i64, _>("realization_params_version")?,
        )
        .unwrap_or(0),
        control_response_version: u32::try_from(row.try_get::<i64, _>("control_response_version")?)
            .unwrap_or(0),
        clip_k: row.try_get("clip_k")?,
        status,
        safety,
        artifacts,
        dependencies,
    }))
}

async fn insert_artifact(
    conn: &mut SqliteConnection,
    owner_kind: &str,
    owner_id: &str,
    artifact: ArtifactBinding,
    now: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO model_artifact_bindings
         (owner_kind, owner_id, artifact_sha256, role, created_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT DO NOTHING",
    )
    .bind(owner_kind)
    .bind(owner_id)
    .bind(artifact.sha256.to_hex())
    .bind(artifact.role.as_str())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn artifacts_all(
    conn: &mut SqliteConnection,
    owner_kind: &str,
    owner_id: &str,
) -> Result<Vec<ArtifactBinding>> {
    let rows = sqlx::query(
        "SELECT artifact_sha256, role FROM model_artifact_bindings
         WHERE owner_kind = ? AND owner_id = ?
         ORDER BY role, artifact_sha256",
    )
    .bind(owner_kind)
    .bind(owner_id)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(ArtifactBinding {
                sha256: Sha256Hex::parse(&row.try_get::<String, _>("artifact_sha256")?)
                    .map_err(|error| StoreError::InvalidInput(error.to_string()))?,
                role: ArtifactRole::parse(row.try_get::<String, _>("role")?.as_str())?,
            })
        })
        .collect()
}

pub async fn artifacts(
    conn: &mut SqliteConnection,
    owner_kind: &str,
    owner_id: &str,
) -> Result<Vec<ArtifactBinding>> {
    artifacts_all(conn, owner_kind, owner_id).await
}

async fn artifacts_with_visibility(
    conn: &mut SqliteConnection,
    owner_kind: &str,
    owner_id: &str,
    visibility: ArtifactVisibility,
) -> Result<Vec<ArtifactBinding>> {
    Ok(artifacts_all(conn, owner_kind, owner_id)
        .await?
        .into_iter()
        .filter(|artifact| artifact.role.visibility() == visibility)
        .collect())
}

async fn public_artifacts(
    conn: &mut SqliteConnection,
    owner_kind: &str,
    owner_id: &str,
) -> Result<Vec<ArtifactBinding>> {
    artifacts_with_visibility(conn, owner_kind, owner_id, ArtifactVisibility::Public).await
}

pub async fn private_artifact_hashes(
    conn: &mut SqliteConnection,
    owner_kind: &str,
    owner_id: &str,
) -> Result<Vec<Sha256Hex>> {
    Ok(
        artifacts_with_visibility(conn, owner_kind, owner_id, ArtifactVisibility::Private)
            .await?
            .into_iter()
            .map(|artifact| artifact.sha256)
            .collect(),
    )
}

async fn insert_dependency_edges(
    conn: &mut SqliteConnection,
    owner_kind: &str,
    owner_id: &str,
    dependencies: &[DependencyRef],
    now: i64,
) -> Result<()> {
    insert_dependency_edges_internal(conn, owner_kind, owner_id, dependencies, now, false).await
}

async fn insert_dependency_edges_internal(
    conn: &mut SqliteConnection,
    owner_kind: &str,
    owner_id: &str,
    dependencies: &[DependencyRef],
    now: i64,
    allow_revoked: bool,
) -> Result<()> {
    for dependency in dependencies {
        if !allow_revoked && revoked_dependency(conn, dependency).await? {
            return Err(StoreError::RevokedDependency {
                dependency: dependency.clone(),
            });
        }
        let generation = register_dependency(conn, dependency, now).await?;
        sqlx::query(
            "INSERT INTO model_dependencies
             (owner_kind, owner_id, kind, target, required, generation)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT DO NOTHING",
        )
        .bind(owner_kind)
        .bind(owner_id)
        .bind(dependency.kind.as_str())
        .bind(dependency.target.as_str())
        .bind(if dependency.required { 1 } else { 0 })
        .bind(i64::try_from(generation).unwrap_or(i64::MAX))
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

pub async fn dependencies(
    conn: &mut SqliteConnection,
    owner_kind: &str,
    owner_id: &str,
) -> Result<Vec<DependencyRef>> {
    let rows = sqlx::query(
        "SELECT kind, target, required FROM model_dependencies
         WHERE owner_kind = ? AND owner_id = ?
         ORDER BY kind, target",
    )
    .bind(owner_kind)
    .bind(owner_id)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(DependencyRef {
                kind: DependencyKind::parse(row.try_get::<String, _>("kind")?.as_str())?,
                target: DependencyTarget::new(row.try_get::<String, _>("target")?)?,
                required: row.try_get::<i64, _>("required")? != 0,
            })
        })
        .collect()
}

pub async fn register_dependency(
    conn: &mut SqliteConnection,
    dependency: &DependencyRef,
    now: i64,
) -> Result<u64> {
    sqlx::query(
        "INSERT INTO model_dependency_generations
         (kind, target, generation, reason, updated_at)
         VALUES (?, ?, 0, NULL, ?)
         ON CONFLICT (kind, target) DO NOTHING",
    )
    .bind(dependency.kind.as_str())
    .bind(dependency.target.as_str())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    let generation: i64 = sqlx::query_scalar(
        "SELECT generation FROM model_dependency_generations WHERE kind = ? AND target = ?",
    )
    .bind(dependency.kind.as_str())
    .bind(dependency.target.as_str())
    .fetch_one(&mut *conn)
    .await?;
    Ok(u64::try_from(generation).unwrap_or(0))
}

pub async fn dependency_generation(
    conn: &mut SqliteConnection,
    dependency: &DependencyRef,
) -> Result<Option<u64>> {
    let generation: Option<i64> = sqlx::query_scalar(
        "SELECT generation FROM model_dependency_generations WHERE kind = ? AND target = ?",
    )
    .bind(dependency.kind.as_str())
    .bind(dependency.target.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    Ok(generation.and_then(|value| u64::try_from(value).ok()))
}

async fn revoked_dependency(
    conn: &mut SqliteConnection,
    dependency: &DependencyRef,
) -> Result<bool> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM model_revoked_dependencies WHERE kind = ? AND target = ?",
    )
    .bind(dependency.kind.as_str())
    .bind(dependency.target.as_str())
    .fetch_one(&mut *conn)
    .await?;
    Ok(count > 0)
}

/// Registers any missing generation and returns the snapshot in the caller's
/// transaction.
pub async fn observe_dependency_snapshot(
    conn: &mut SqliteConnection,
    dependencies: &[DependencyRef],
    now: i64,
) -> Result<Vec<DependencyGeneration>> {
    for dependency in dependencies {
        if revoked_dependency(conn, dependency).await? {
            return Err(StoreError::RevokedDependency {
                dependency: dependency.clone(),
            });
        }
        register_dependency(conn, dependency, now).await?;
    }
    dependency_snapshot(conn, dependencies).await
}

pub async fn dependency_snapshot(
    conn: &mut SqliteConnection,
    dependencies: &[DependencyRef],
) -> Result<Vec<DependencyGeneration>> {
    let mut seen = BTreeSet::new();
    let mut snapshot = Vec::new();
    for dependency in dependencies {
        if !seen.insert((dependency.kind, dependency.target.clone())) {
            continue;
        }
        if revoked_dependency(conn, dependency).await? {
            return Err(StoreError::RevokedDependency {
                dependency: dependency.clone(),
            });
        }
        let generation = dependency_generation(conn, dependency)
            .await?
            .ok_or_else(|| StoreError::UnregisteredDependency {
                dependency: dependency.clone(),
            })?;
        snapshot.push(DependencyGeneration {
            dependency: dependency.clone(),
            generation,
        });
    }
    Ok(snapshot)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NumericalSources {
    pub fitter: SourceRef,
    pub evaluator: SourceRef,
    pub model: SourceRef,
}

impl NumericalSources {
    pub fn dependencies(&self) -> Vec<DependencyRef> {
        [&self.fitter, &self.evaluator, &self.model]
            .into_iter()
            .map(|source| {
                DependencyRef::required(
                    DependencyKind::ModelSource,
                    DependencyTarget::model_source(source),
                )
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceActivation {
    pub previous: Option<NumericalSources>,
    pub current: NumericalSources,
    pub generation: u64,
    pub invalidations: InvalidationSummary,
}

pub async fn sync_active_numerical_sources(
    conn: &mut SqliteConnection,
    sources: &NumericalSources,
    now: i64,
) -> Result<SourceActivation> {
    let row: Option<(String, String, String, i64)> = sqlx::query_as(
        "SELECT fitter, evaluator, model, activated_generation
         FROM model_active_numerical_sources WHERE id = 1",
    )
    .fetch_optional(&mut *conn)
    .await?;
    let current = sources.clone();
    let mut invalidations = InvalidationSummary {
        projections: 0,
        family_heads: 0,
        target_eligibilities: 0,
        bundles: 0,
        reports: 0,
    };
    let Some((fitter, evaluator, model, generation)) = row else {
        sqlx::query(
            "INSERT INTO model_active_numerical_sources
             (id, fitter, evaluator, model, activated_generation, activated_at)
             VALUES (1, ?, ?, ?, 1, ?)",
        )
        .bind(current.fitter.as_str())
        .bind(current.evaluator.as_str())
        .bind(current.model.as_str())
        .bind(now)
        .execute(&mut *conn)
        .await?;
        for dependency in current.dependencies() {
            register_dependency(conn, &dependency, now).await?;
        }
        return Ok(SourceActivation {
            previous: None,
            current,
            generation: 1,
            invalidations,
        });
    };
    let previous = NumericalSources {
        fitter: SourceRef::new(fitter)?,
        evaluator: SourceRef::new(evaluator)?,
        model: SourceRef::new(model)?,
    };
    let generation = u64::try_from(generation).unwrap_or(1);
    if previous == current {
        for dependency in current.dependencies() {
            register_dependency(conn, &dependency, now).await?;
        }
        return Ok(SourceActivation {
            previous: Some(previous),
            current,
            generation,
            invalidations,
        });
    }
    let current_targets: BTreeSet<DependencyTarget> = current
        .dependencies()
        .into_iter()
        .map(|dependency| dependency.target)
        .collect();
    let mut affected_subjects: BTreeSet<String> = BTreeSet::new();
    for dependency in previous.dependencies() {
        if !current_targets.contains(&dependency.target) {
            invalidations.merge(
                invalidate(
                    conn,
                    &dependency,
                    InvalidationReason::ModelSourceChange,
                    now,
                )
                .await?,
            );
            let subjects: Vec<String> = sqlx::query_scalar(
                "SELECT DISTINCT r.subject_key FROM model_reports r
                 JOIN model_dependencies d
                   ON d.owner_kind = 'report' AND d.owner_id = r.id
                 WHERE d.kind = ? AND d.target = ? AND d.required = 1",
            )
            .bind(DependencyKind::ModelSource.as_str())
            .bind(dependency.target.as_str())
            .fetch_all(&mut *conn)
            .await?;
            affected_subjects.extend(subjects);
        }
    }
    for subject in affected_subjects {
        note_evidence_arrival(conn, &SubjectKey::new(subject)?, now).await?;
    }
    for dependency in current.dependencies() {
        register_dependency(conn, &dependency, now).await?;
    }
    let next = generation.saturating_add(1);
    sqlx::query(
        "UPDATE model_active_numerical_sources
         SET fitter = ?, evaluator = ?, model = ?, activated_generation = ?, activated_at = ?
         WHERE id = 1",
    )
    .bind(current.fitter.as_str())
    .bind(current.evaluator.as_str())
    .bind(current.model.as_str())
    .bind(i64::try_from(next).unwrap_or(i64::MAX))
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(SourceActivation {
        previous: Some(previous),
        current,
        generation: next,
        invalidations,
    })
}

pub async fn sync_active_numerical_sources_fenced(
    pool: &SqlitePool,
    sources: &NumericalSources,
    now: i64,
) -> Result<SourceActivation> {
    let mut tx = begin_immediate(pool).await?;
    let activation = sync_active_numerical_sources(&mut tx, sources, now).await?;
    tx.commit().await?;
    Ok(activation)
}

async fn sources_current(conn: &mut SqliteConnection, sources: &NumericalSources) -> Result<bool> {
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT fitter, evaluator, model FROM model_active_numerical_sources WHERE id = 1",
    )
    .fetch_optional(&mut *conn)
    .await?;
    let Some((fitter, evaluator, model)) = row else {
        return Ok(true);
    };
    Ok(fitter == sources.fitter.as_str()
        && evaluator == sources.evaluator.as_str()
        && model == sources.model.as_str())
}

pub async fn numerical_sources_current(
    conn: &mut SqliteConnection,
    sources: &NumericalSources,
) -> Result<bool> {
    sources_current(conn, sources).await
}

async fn anchor_matches(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
    expected: Option<u32>,
) -> Result<bool> {
    let state = subject_state(conn, subject).await?;
    Ok(state.and_then(|state| state.anchor_revision) == expected)
}

fn snapshot_map(
    snapshot: &[DependencyGeneration],
) -> BTreeMap<(DependencyKind, DependencyTarget), u64> {
    snapshot
        .iter()
        .map(|entry| {
            (
                (entry.dependency.kind, entry.dependency.target.clone()),
                entry.generation,
            )
        })
        .collect()
}

async fn verify_owner_snapshot(
    conn: &mut SqliteConnection,
    owner_kind: &str,
    owner_id: &str,
    snapshot: &[DependencyGeneration],
) -> Result<Option<SelectionRejection>> {
    let rows = sqlx::query(
        "SELECT d.kind, d.target, d.generation, g.generation AS current
         FROM model_dependencies d
         LEFT JOIN model_dependency_generations g
                ON g.kind = d.kind AND g.target = d.target
         WHERE d.owner_kind = ? AND d.owner_id = ? AND d.required = 1",
    )
    .bind(owner_kind)
    .bind(owner_id)
    .fetch_all(&mut *conn)
    .await?;
    let expected = snapshot_map(snapshot);
    for row in &rows {
        let dependency = DependencyRef::required(
            DependencyKind::parse(row.try_get::<String, _>("kind")?.as_str())?,
            DependencyTarget::new(row.try_get::<String, _>("target")?)?,
        );
        if revoked_dependency(conn, &dependency).await? {
            return Ok(Some(SelectionRejection::RevokedDependency(dependency)));
        }
        let recorded: i64 = row.try_get("generation")?;
        let current: Option<i64> = row.try_get("current")?;
        let Some(current) = current else {
            return Ok(Some(SelectionRejection::DependencyUnregistered(dependency)));
        };
        if current != recorded {
            return Ok(Some(SelectionRejection::DependencyAdvanced(dependency)));
        }
        let Some(observed) = expected.get(&(dependency.kind, dependency.target.clone())) else {
            return Ok(Some(SelectionRejection::DependencyUnregistered(dependency)));
        };
        if *observed != u64::try_from(current).unwrap_or(0) {
            return Ok(Some(SelectionRejection::DependencyAdvanced(dependency)));
        }
    }
    Ok(None)
}

async fn verify_dependency_snapshot(
    conn: &mut SqliteConnection,
    dependencies: &[DependencyRef],
    snapshot: &[DependencyGeneration],
) -> Result<Option<SelectionRejection>> {
    let expected = snapshot_map(snapshot);
    for dependency in dependencies.iter().filter(|dependency| dependency.required) {
        let Some(observed) = expected.get(&(dependency.kind, dependency.target.clone())) else {
            return Ok(Some(SelectionRejection::DependencyUnregistered(
                dependency.clone(),
            )));
        };
        let Some(current) = dependency_generation(conn, dependency).await? else {
            return Ok(Some(SelectionRejection::DependencyUnregistered(
                dependency.clone(),
            )));
        };
        if current != *observed {
            return Ok(Some(SelectionRejection::DependencyAdvanced(
                dependency.clone(),
            )));
        }
    }
    Ok(None)
}

/// The lease, numerical sources and accepted evidence snapshot are still current.
pub async fn execution_current(
    conn: &mut SqliteConnection,
    lease: JobLease<'_>,
    sources: &NumericalSources,
    dependencies: &[DependencyRef],
    snapshot: &[DependencyGeneration],
) -> Result<bool> {
    if !lease_valid(&mut *conn, lease).await? {
        return Ok(false);
    }
    if !sources_current(&mut *conn, sources).await? {
        return Ok(false);
    }
    Ok(
        verify_dependency_snapshot(&mut *conn, dependencies, snapshot)
            .await?
            .is_none(),
    )
}

async fn report_invalidated(conn: &mut SqliteConnection, id: &ModelReportId) -> Result<bool> {
    let generation: Option<i64> = sqlx::query_scalar(
        "SELECT generation FROM model_dependency_generations WHERE kind = ? AND target = ?",
    )
    .bind(DependencyKind::Report.as_str())
    .bind(id.to_hex())
    .fetch_optional(&mut *conn)
    .await?;
    Ok(generation.unwrap_or(0) > 0)
}

async fn revision_invalidated(
    conn: &mut SqliteConnection,
    id: &RealizationRevisionId,
) -> Result<bool> {
    let generation: Option<i64> = sqlx::query_scalar(
        "SELECT generation FROM model_dependency_generations WHERE kind = ? AND target = ?",
    )
    .bind(DependencyKind::RealizationRevision.as_str())
    .bind(id.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    Ok(generation.unwrap_or(0) > 0)
}

async fn bundle_invalidated(
    conn: &mut SqliteConnection,
    id: &MatchBundleRevisionId,
) -> Result<bool> {
    let generation: Option<i64> = sqlx::query_scalar(
        "SELECT generation FROM model_dependency_generations WHERE kind = ? AND target = ?",
    )
    .bind(DependencyKind::BundleRevision.as_str())
    .bind(id.to_hex())
    .fetch_optional(&mut *conn)
    .await?;
    if generation.unwrap_or(0) > 0 {
        return Ok(true);
    }
    let realizations: Vec<String> = sqlx::query_scalar(
        "SELECT realization_id FROM model_bundle_revision_realizations WHERE bundle_id = ?",
    )
    .bind(id.to_hex())
    .fetch_all(&mut *conn)
    .await?;
    for realization in realizations {
        let realization = RealizationRevisionId::new(realization)?;
        if revision_invalidated(conn, &realization).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub async fn report_applicable(conn: &mut SqliteConnection, id: &ModelReportId) -> Result<bool> {
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM model_reports WHERE id = ?")
        .bind(id.to_hex())
        .fetch_one(&mut *conn)
        .await?;
    if exists == 0 {
        return Ok(false);
    }
    if report_invalidated(conn, id).await? {
        return Ok(false);
    }
    let rows = sqlx::query(
        "SELECT d.kind, d.target, d.generation, g.generation AS current
         FROM model_dependencies d
         LEFT JOIN model_dependency_generations g
                ON g.kind = d.kind AND g.target = d.target
         WHERE d.owner_kind = 'report' AND d.owner_id = ? AND d.required = 1",
    )
    .bind(id.to_hex())
    .fetch_all(&mut *conn)
    .await?;
    for row in &rows {
        let recorded: i64 = row.try_get("generation")?;
        let current: Option<i64> = row.try_get("current")?;
        if current != Some(recorded) {
            return Ok(false);
        }
        let dependency = DependencyRef::required(
            DependencyKind::parse(row.try_get::<String, _>("kind")?.as_str())?,
            DependencyTarget::new(row.try_get::<String, _>("target")?)?,
        );
        if revoked_dependency(conn, &dependency).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

pub async fn insert_report(
    conn: &mut SqliteConnection,
    report: &ModelReport,
    dependencies: &[DependencyRef],
    artifacts: &[ArtifactBinding],
    evidence: &EvidenceMembership,
    now: i64,
) -> Result<ModelReportId> {
    let mut edges = dependencies.to_vec();
    match &report.candidate {
        ModelVersionId::SharedFamily { target } => {
            let dependency = DependencyRef::required(
                DependencyKind::FamilyVersion,
                DependencyTarget::family_version(target),
            );
            if !edges.contains(&dependency) {
                edges.push(dependency);
            }
        }
        ModelVersionId::BodyRealization { realization } => {
            let revision = RealizationRevisionId::for_version(realization);
            let dependency = DependencyRef::required(
                DependencyKind::RealizationRevision,
                DependencyTarget::realization_revision(&revision),
            );
            if !edges.contains(&dependency) {
                edges.push(dependency);
            }
            let dependency = DependencyRef::required(
                DependencyKind::FamilyVersion,
                DependencyTarget::family_version(&realization.target),
            );
            if !edges.contains(&dependency) {
                edges.push(dependency);
            }
        }
    }
    report.validate(&edges)?;
    let report_sources = NumericalSources {
        fitter: report.provenance.fitter.clone(),
        evaluator: report.provenance.evaluator.clone(),
        model: report.provenance.model.clone(),
    };
    if !sources_current(conn, &report_sources).await? {
        return Err(StoreError::SupersededSources);
    }
    register_subject(conn, &report.subject, now).await?;
    let public = report.semantic_id(dependencies);
    let mut hasher = Sha256::new();
    hasher.update(public.digest().as_bytes());
    hasher.update(evidence.digest().as_bytes());
    let id = ModelReportId::from_digest(hasher.finalize().into());
    let (candidate_kind, candidate_id, candidate_target, candidate_version) =
        match &report.candidate {
            ModelVersionId::SharedFamily { target } => (
                ModelVersionKind::SharedFamily.as_str(),
                target.to_key(),
                target.to_key(),
                i64::from(target.version.get()),
            ),
            ModelVersionId::BodyRealization { realization } => (
                ModelVersionKind::BodyRealization.as_str(),
                RealizationRevisionId::for_version(realization).to_string(),
                realization.target.to_key(),
                i64::from(realization.version.get()),
            ),
        };
    let inserted = sqlx::query(
        "INSERT INTO model_reports
         (id, subject_key, candidate_kind, candidate_id, candidate_target, candidate_version,
          capability, scope_key, scope, rule_revision, fitter, evaluator, model, probe_set,
          effective_options, partition_policy, assessment_seeds, evidence_membership, outcome,
          metrics, supported_mask, limitations, evidence_requests, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(id.to_hex())
    .bind(report.subject.key().as_str())
    .bind(candidate_kind)
    .bind(candidate_id)
    .bind(candidate_target)
    .bind(candidate_version)
    .bind(report.capability.as_str())
    .bind(report.scope.key().as_str())
    .bind(serde_json::to_string(&report.scope)?)
    .bind(report.provenance.rule_revision.as_str())
    .bind(report.provenance.fitter.as_str())
    .bind(report.provenance.evaluator.as_str())
    .bind(report.provenance.model.as_str())
    .bind(report.provenance.probe_set.as_str())
    .bind(report.provenance.effective_options.to_hex())
    .bind(report.provenance.partition_policy.to_hex())
    .bind(serde_json::to_string(&report.provenance.assessment_seeds)?)
    .bind(serde_json::to_string(evidence)?)
    .bind(report.outcome.as_str())
    .bind(serde_json::to_string(&report.metrics)?)
    .bind(serde_json::to_string(&report.supported_mask)?)
    .bind(serde_json::to_string(&report.limitations)?)
    .bind(serde_json::to_string(&report.evidence_requests)?)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if inserted == 0 {
        return Ok(id);
    }
    for artifact in artifacts {
        insert_artifact(conn, "report", &id.to_hex(), *artifact, now).await?;
    }
    insert_dependency_edges(conn, "report", &id.to_hex(), &edges, now).await?;
    bump_publication(conn).await?;
    Ok(id)
}

pub async fn report(
    conn: &mut SqliteConnection,
    id: &ModelReportId,
) -> Result<Option<StoredReport>> {
    let row = sqlx::query(
        "SELECT subject_key, candidate_kind, candidate_id, candidate_target, candidate_version,
                capability, scope, rule_revision, fitter, evaluator, model, probe_set,
                effective_options, partition_policy, assessment_seeds, outcome, metrics,
                supported_mask, limitations, evidence_requests
         FROM model_reports WHERE id = ?",
    )
    .bind(id.to_hex())
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let subject_key: String = row.try_get("subject_key")?;
    let subject = parse_subject(&subject_key)?;
    let candidate = candidate_from_columns(
        &row.try_get::<String, _>("candidate_kind")?,
        &row.try_get::<String, _>("candidate_id")?,
        &row.try_get::<String, _>("candidate_target")?,
        row.try_get("candidate_version")?,
        &subject_key,
    )?;
    let report = ModelReport {
        subject,
        candidate,
        capability: QualificationCapability::parse(
            row.try_get::<String, _>("capability")?.as_str(),
        )?,
        scope: serde_json::from_str(&row.try_get::<String, _>("scope")?)?,
        provenance: looklab_schema::ReportProvenance {
            rule_revision: looklab_schema::RuleRevision::new(
                row.try_get::<String, _>("rule_revision")?,
            )?,
            fitter: looklab_schema::SourceRef::new(row.try_get::<String, _>("fitter")?)?,
            evaluator: looklab_schema::SourceRef::new(row.try_get::<String, _>("evaluator")?)?,
            model: looklab_schema::SourceRef::new(row.try_get::<String, _>("model")?)?,
            probe_set: ProbeSetId::new(row.try_get::<String, _>("probe_set")?)?,
            effective_options: Sha256Hex::parse(&row.try_get::<String, _>("effective_options")?)
                .map_err(|error| StoreError::InvalidInput(error.to_string()))?,
            partition_policy: Sha256Hex::parse(&row.try_get::<String, _>("partition_policy")?)
                .map_err(|error| StoreError::InvalidInput(error.to_string()))?,
            assessment_seeds: serde_json::from_str(&row.try_get::<String, _>("assessment_seeds")?)?,
        },
        outcome: AssessmentOutcome::parse(row.try_get::<String, _>("outcome")?.as_str())?,
        metrics: serde_json::from_str(&row.try_get::<String, _>("metrics")?)?,
        supported_mask: serde_json::from_str(&row.try_get::<String, _>("supported_mask")?)?,
        limitations: serde_json::from_str(&row.try_get::<String, _>("limitations")?)?,
        evidence_requests: serde_json::from_str(&row.try_get::<String, _>("evidence_requests")?)?,
    };
    Ok(Some(StoredReport {
        id: *id,
        report,
        dependencies: dependencies(conn, "report", &id.to_hex()).await?,
        artifacts: public_artifacts(conn, "report", &id.to_hex()).await?,
    }))
}

pub async fn report_evidence(
    conn: &mut SqliteConnection,
    id: &ModelReportId,
) -> Result<Option<EvidenceMembership>> {
    let evidence: Option<String> =
        sqlx::query_scalar("SELECT evidence_membership FROM model_reports WHERE id = ?")
            .bind(id.to_hex())
            .fetch_optional(&mut *conn)
            .await?;
    evidence
        .map(|evidence| serde_json::from_str(&evidence).map_err(StoreError::from))
        .transpose()
}

fn candidate_from_columns(
    kind: &str,
    _id: &str,
    target: &str,
    version: i64,
    subject_key: &str,
) -> Result<ModelVersionId> {
    let target = FamilyTargetId::parse(target)?;
    match ModelVersionKind::parse(kind)? {
        ModelVersionKind::SharedFamily => Ok(ModelVersionId::SharedFamily { target }),
        ModelVersionKind::BodyRealization => {
            let subject = parse_subject(subject_key)?;
            let ModelSubject::BodyRealization { realization } = subject else {
                return Err(StoreError::InvalidInput(
                    "realization candidate has a non-realization subject".to_owned(),
                ));
            };
            let version = RealizationVersion::new(u32::try_from(version).unwrap_or(0))
                .ok_or_else(|| StoreError::InvalidInput("invalid candidate version".to_owned()))?;
            Ok(ModelVersionId::BodyRealization {
                realization: looklab_schema::BodyRealizationVersionId {
                    realization,
                    version,
                    target,
                },
            })
        }
    }
}

#[derive(Debug, Clone)]
struct ReportHeader {
    subject_key: String,
    candidate_kind: String,
    candidate_id: String,
    candidate_target: String,
    candidate_version: i64,
    scope_key: String,
    capability: QualificationCapability,
    outcome: AssessmentOutcome,
}

async fn report_header(
    conn: &mut SqliteConnection,
    id: &ModelReportId,
) -> Result<Option<ReportHeader>> {
    let row = sqlx::query(
        "SELECT subject_key, candidate_kind, candidate_id, candidate_target, candidate_version,
                scope_key, capability, outcome
         FROM model_reports WHERE id = ?",
    )
    .bind(id.to_hex())
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| {
        Ok(ReportHeader {
            subject_key: row.try_get("subject_key")?,
            candidate_kind: row.try_get("candidate_kind")?,
            candidate_id: row.try_get("candidate_id")?,
            candidate_target: row.try_get("candidate_target")?,
            candidate_version: row.try_get("candidate_version")?,
            scope_key: row.try_get("scope_key")?,
            capability: QualificationCapability::parse(
                row.try_get::<String, _>("capability")?.as_str(),
            )?,
            outcome: AssessmentOutcome::parse(row.try_get::<String, _>("outcome")?.as_str())?,
        })
    })
    .transpose()
}

pub async fn insert_engine_prerequisite(
    conn: &mut SqliteConnection,
    prerequisite: &EnginePrerequisite,
    now: i64,
) -> Result<EnginePrerequisiteId> {
    prerequisite.validate()?;
    let id = prerequisite.id();
    let dependency = DependencyRef::required(
        DependencyKind::EngineQualification,
        DependencyTarget::engine_qualification(&id),
    );
    let generation = register_dependency(conn, &dependency, now).await?;
    sqlx::query(
        "INSERT INTO model_engine_prerequisites
         (engine, rule_revision, outcome, metrics, supported_mask, limitations, generation,
          recorded_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (engine, rule_revision) DO NOTHING",
    )
    .bind(id.engine.as_str())
    .bind(id.rule_revision.as_str())
    .bind(prerequisite.outcome.as_str())
    .bind(serde_json::to_string(&prerequisite.metrics)?)
    .bind(serde_json::to_string(&prerequisite.supported_mask)?)
    .bind(serde_json::to_string(&prerequisite.limitations)?)
    .bind(i64::try_from(generation).unwrap_or(i64::MAX))
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(id)
}

pub async fn engine_prerequisite(
    conn: &mut SqliteConnection,
    id: &EnginePrerequisiteId,
) -> Result<Option<EnginePrerequisite>> {
    let row = sqlx::query(
        "SELECT outcome, metrics, supported_mask, limitations
         FROM model_engine_prerequisites WHERE engine = ? AND rule_revision = ?",
    )
    .bind(id.engine.as_str())
    .bind(id.rule_revision.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| {
        Ok(EnginePrerequisite {
            engine: id.engine.clone(),
            rule_revision: id.rule_revision.clone(),
            outcome: AssessmentOutcome::parse(row.try_get::<String, _>("outcome")?.as_str())?,
            metrics: serde_json::from_str(&row.try_get::<String, _>("metrics")?)?,
            supported_mask: serde_json::from_str(&row.try_get::<String, _>("supported_mask")?)?,
            limitations: serde_json::from_str(&row.try_get::<String, _>("limitations")?)?,
        })
    })
    .transpose()
}

pub async fn engine_prerequisite_applicable(
    conn: &mut SqliteConnection,
    id: &EnginePrerequisiteId,
) -> Result<Option<AssessmentOutcome>> {
    let row: Option<(String, i64)> = sqlx::query_as(
        "SELECT outcome, generation FROM model_engine_prerequisites
         WHERE engine = ? AND rule_revision = ?",
    )
    .bind(id.engine.as_str())
    .bind(id.rule_revision.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    let Some((outcome, recorded)) = row else {
        return Ok(None);
    };
    let dependency = DependencyRef::required(
        DependencyKind::EngineQualification,
        DependencyTarget::engine_qualification(id),
    );
    let current = dependency_generation(conn, &dependency).await?;
    if current == u64::try_from(recorded).ok() {
        Ok(Some(AssessmentOutcome::parse(&outcome)?))
    } else {
        Ok(None)
    }
}

pub async fn fit_attempt(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
) -> Result<Option<FitAttempt>> {
    let row: Option<(String, i64, i64)> = sqlx::query_as(
        "SELECT states, consecutive, started_at FROM model_fit_attempts WHERE subject_key = ?",
    )
    .bind(subject.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|(states, consecutive, started_at)| {
        let states: BTreeMap<String, FitCheckpoint> = serde_json::from_str(&states)?;
        Ok(FitAttempt {
            states,
            consecutive: u32::try_from(consecutive).unwrap_or(u32::MAX),
            started_at,
        })
    })
    .transpose()
}

pub async fn record_fit_attempt(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
    block: &str,
    state: Option<&FitCheckpoint>,
    now: i64,
) -> Result<u32> {
    let mut attempt = fit_attempt(conn, subject).await?.unwrap_or_default();
    if let Some(state) = state {
        attempt.states.insert(block.to_owned(), state.clone());
    }
    let consecutive = attempt.consecutive.saturating_add(1);
    let started_at = if attempt.consecutive == 0 {
        now
    } else {
        attempt.started_at
    };
    sqlx::query(
        "INSERT INTO model_fit_attempts (subject_key, states, consecutive, started_at, updated_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT (subject_key) DO UPDATE
         SET states = excluded.states,
             consecutive = excluded.consecutive,
             started_at = excluded.started_at,
             updated_at = excluded.updated_at",
    )
    .bind(subject.as_str())
    .bind(serde_json::to_string(&attempt.states)?)
    .bind(i64::from(consecutive))
    .bind(started_at)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(consecutive)
}

pub async fn clear_fit_attempt(conn: &mut SqliteConnection, subject: &SubjectKey) -> Result<()> {
    sqlx::query("DELETE FROM model_fit_attempts WHERE subject_key = ?")
        .bind(subject.as_str())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub async fn open_fit_hold(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
    consecutive: u32,
    first_attempt: i64,
    last_attempt: i64,
    diagnostics: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO model_fit_holds
         (subject_key, consecutive, first_attempt, last_attempt, diagnostics, status)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT (subject_key) DO UPDATE
         SET consecutive = excluded.consecutive,
             first_attempt = excluded.first_attempt,
             last_attempt = excluded.last_attempt,
             diagnostics = excluded.diagnostics",
    )
    .bind(subject.as_str())
    .bind(i64::from(consecutive))
    .bind(first_attempt)
    .bind(last_attempt)
    .bind(diagnostics)
    .bind(FitHoldStatus::Pending.as_str())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub async fn resolve_fit_hold(conn: &mut SqliteConnection, subject: &SubjectKey) -> Result<()> {
    sqlx::query("UPDATE model_fit_holds SET status = ? WHERE subject_key = ? AND status = ?")
        .bind(FitHoldStatus::Resolved.as_str())
        .bind(subject.as_str())
        .bind(FitHoldStatus::Pending.as_str())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub async fn pool_fingerprint(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT fingerprint FROM model_pool_state WHERE subject_key = ?")
            .bind(subject.as_str())
            .fetch_optional(&mut *conn)
            .await?,
    )
}

pub async fn record_pool_fingerprint(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
    fingerprint: &str,
    now: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO model_pool_state (subject_key, fingerprint, updated_at)
         VALUES (?, ?, ?)
         ON CONFLICT (subject_key) DO UPDATE
         SET fingerprint = excluded.fingerprint, updated_at = excluded.updated_at",
    )
    .bind(subject.as_str())
    .bind(fingerprint)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub async fn record_subject_revision(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
    revision: u32,
    role: SubjectRevisionRole,
    artifact: Option<Sha256Hex>,
    native_warm_state: Option<Sha256Hex>,
    now: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO model_subject_revisions
         (subject_key, revision, role, artifact_sha256, native_warm_state_sha256, recorded_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT DO NOTHING",
    )
    .bind(subject.as_str())
    .bind(i64::from(revision))
    .bind(role.as_str())
    .bind(artifact.map(Sha256Hex::to_hex))
    .bind(native_warm_state.map(Sha256Hex::to_hex))
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub async fn subject_state(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
) -> Result<Option<SubjectState>> {
    let row = sqlx::query("SELECT anchor_revision FROM model_subject_state WHERE subject_key = ?")
        .bind(subject.as_str())
        .fetch_optional(&mut *conn)
        .await?;
    row.map(|row| {
        let anchor_revision: Option<i64> = row.try_get("anchor_revision")?;
        Ok(SubjectState {
            anchor_revision: anchor_revision.and_then(|value| u32::try_from(value).ok()),
        })
    })
    .transpose()
}

async fn ensure_subject_state(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
    now: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO model_subject_state (subject_key, updated_at)
         VALUES (?, ?)
         ON CONFLICT (subject_key) DO NOTHING",
    )
    .bind(subject.as_str())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub async fn set_subject_anchor(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
    expected: Option<u32>,
    anchor: u32,
    now: i64,
) -> Result<bool> {
    ensure_subject_state(conn, subject, now).await?;
    let updated = sqlx::query(
        "UPDATE model_subject_state SET anchor_revision = ?, updated_at = ?
         WHERE subject_key = ? AND anchor_revision IS ?",
    )
    .bind(i64::from(anchor))
    .bind(now)
    .bind(subject.as_str())
    .bind(expected.map(i64::from))
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

pub async fn family_head(
    conn: &mut SqliteConnection,
    family: &LookId,
) -> Result<Option<FamilyHead>> {
    let row = sqlx::query(
        "SELECT head_version, source_limited, numerical, release_gates, report_id,
                applicability, invalidated_reason
         FROM model_family_heads WHERE family = ?",
    )
    .bind(family.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| {
        let version: i64 = row.try_get("head_version")?;
        let reason: Option<String> = row.try_get("invalidated_reason")?;
        Ok(FamilyHead {
            target: FamilyTargetId {
                family: family.clone(),
                version: TargetVersion::new(u32::try_from(version).unwrap_or(0))
                    .ok_or_else(|| StoreError::InvalidInput("invalid head version".to_owned()))?,
            },
            source_limited: row.try_get::<i64, _>("source_limited")? != 0,
            safety: CandidateSafety {
                numerical: SafetyCheck::parse(row.try_get::<String, _>("numerical")?.as_str())?,
                release_gates: SafetyCheck::parse(
                    row.try_get::<String, _>("release_gates")?.as_str(),
                )?,
            },
            report: row
                .try_get::<Option<String>, _>("report_id")?
                .map(|value| report_id(&value))
                .transpose()?,
            applicability: parse_applicability(
                row.try_get::<String, _>("applicability")?.as_str(),
                reason.as_deref(),
            )?,
        })
    })
    .transpose()
}

pub async fn family_target(
    conn: &mut SqliteConnection,
    family: &LookId,
) -> Result<Option<FamilyTargetId>> {
    Ok(family_head(conn, family).await?.map(|head| head.target))
}

pub async fn family_cross_body_qualified(
    conn: &mut SqliteConnection,
    family: &LookId,
) -> Result<bool> {
    let Some(head) = family_head(conn, family).await? else {
        return Ok(false);
    };
    if !head.applicability.is_current()
        || !target_published(conn, &head.target).await?
        || !target_currently_eligible(conn, &head.target).await?
        || !target_applicable(conn, &head.target).await?
    {
        return Ok(false);
    }
    let Some(report_id) = head.report else {
        return Ok(false);
    };
    let Some(stored) = report(conn, &report_id).await? else {
        return Ok(false);
    };
    Ok(matches!(stored.report.outcome, AssessmentOutcome::Pass)
        && stored.report.capability == QualificationCapability::CrossBodyAdmission
        && report_applicable(conn, &report_id).await?)
}

pub async fn record_cohort_revision(
    conn: &mut SqliteConnection,
    family: &LookId,
    role: CohortRole,
    basis: CohortBasis,
    digest: Sha256Hex,
    members: &[CohortMember],
    now: i64,
) -> Result<CohortRevision> {
    let next: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(revision), 0) + 1 FROM model_cohort_revisions WHERE family = ?",
    )
    .bind(family.as_str())
    .fetch_one(&mut *conn)
    .await?;
    let revision = CohortRevision::new(u32::try_from(next).unwrap_or(0))
        .ok_or_else(|| StoreError::InvalidInput("cohort revision overflow".to_owned()))?;
    sqlx::query(
        "INSERT INTO model_cohort_revisions (family, revision, role, digest, basis, recorded_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(family.as_str())
    .bind(i64::from(revision.get()))
    .bind(role.as_str())
    .bind(digest.to_hex())
    .bind(basis.as_str())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    let mut seen = BTreeSet::new();
    for member in members {
        let member_key = cohort_member_key(member);
        if !seen.insert(member_key.clone()) {
            continue;
        }
        let (body, realization, scope, supported_mask, report) = match member {
            CohortMember::Seed { body } => (body.clone(), None, None, None, None),
            CohortMember::Admitted {
                realization,
                report,
                scope,
                supported_mask,
            } => (
                realization.realization.body.clone(),
                Some(realization),
                Some(scope),
                Some(supported_mask),
                Some(*report),
            ),
        };
        sqlx::query(
            "INSERT INTO model_family_cohort
             (family, cohort_revision, member_key, body, realization_epoch, realization_sim,
              realization_version, target, scope_key, scope, supported_mask, report_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(family.as_str())
        .bind(i64::from(revision.get()))
        .bind(member_key)
        .bind(body.as_str())
        .bind(realization.map(|realization| i64::from(realization.realization.epoch.get())))
        .bind(realization.map(|realization| realization.realization.native_sim.as_str()))
        .bind(realization.map(|realization| i64::from(realization.version.get())))
        .bind(realization.map(|realization| realization.target.to_key()))
        .bind(scope.map(|scope| scope.key()).map(|key| key.to_string()))
        .bind(
            scope
                .map(|scope| serde_json::to_string(scope.as_ref()))
                .transpose()?,
        )
        .bind(supported_mask.map(serde_json::to_string).transpose()?)
        .bind(report.map(ModelReportId::to_hex))
        .execute(&mut *conn)
        .await?;
    }
    Ok(revision)
}

pub async fn family_cohort(
    conn: &mut SqliteConnection,
    family: &LookId,
    role: CohortRole,
) -> Result<FamilyCohort> {
    let revision: Option<i64> = sqlx::query_scalar(
        "SELECT MAX(revision) FROM model_cohort_revisions WHERE family = ? AND role = ?",
    )
    .bind(family.as_str())
    .bind(role.as_str())
    .fetch_one(&mut *conn)
    .await?;
    let Some(revision) = revision else {
        return Ok(FamilyCohort {
            state: FamilyCohortState::Empty,
            report: None,
            members: Vec::new(),
        });
    };
    let rows = sqlx::query(
        "SELECT body, realization_epoch, realization_sim, realization_version, target,
                scope, supported_mask, report_id
         FROM model_family_cohort
         WHERE family = ? AND cohort_revision = ?
         ORDER BY member_key",
    )
    .bind(family.as_str())
    .bind(revision)
    .fetch_all(&mut *conn)
    .await?;
    let members: Vec<CohortMember> = rows
        .iter()
        .map(|row| {
            let body = body_id(&row.try_get::<String, _>("body")?)?;
            let epoch: Option<i64> = row.try_get("realization_epoch")?;
            let native_sim: Option<String> = row.try_get("realization_sim")?;
            let version: Option<i64> = row.try_get("realization_version")?;
            let target: Option<String> = row.try_get("target")?;
            let scope: Option<String> = row.try_get("scope")?;
            let supported_mask: Option<String> = row.try_get("supported_mask")?;
            let report: Option<String> = row.try_get("report_id")?;
            match (epoch, native_sim, version, target, scope, report) {
                (
                    Some(epoch),
                    Some(native_sim),
                    Some(version),
                    Some(target),
                    Some(scope),
                    Some(report),
                ) => Ok(CohortMember::Admitted {
                    realization: looklab_schema::BodyRealizationVersionId {
                        realization: BodyRealizationId {
                            body,
                            epoch: ControlEpochId::new(u32::try_from(epoch).unwrap_or(0)),
                            native_sim: SimKey::new(&native_sim)
                                .map_err(|error| StoreError::InvalidInput(error.to_string()))?,
                        },
                        version: RealizationVersion::new(u32::try_from(version).unwrap_or(0))
                            .ok_or_else(|| {
                                StoreError::InvalidInput(
                                    "invalid cohort realization version".to_owned(),
                                )
                            })?,
                        target: FamilyTargetId::parse(&target)?,
                    },
                    report: report_id(&report)?,
                    scope: serde_json::from_str(&scope)?,
                    supported_mask: serde_json::from_str(
                        &supported_mask.unwrap_or_else(|| "[]".to_owned()),
                    )?,
                }),
                _ => Ok(CohortMember::Seed { body }),
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let admission_report = if role == CohortRole::Admitted {
        let admission: Option<(String, i64)> = sqlx::query_as(
            "SELECT report_id, target_version FROM model_cohort_admissions
             WHERE family = ? AND cohort_revision = ?",
        )
        .bind(family.as_str())
        .bind(revision)
        .fetch_optional(&mut *conn)
        .await?;
        let Some((report_text, target_version)) = admission else {
            return Ok(FamilyCohort {
                state: FamilyCohortState::Inapplicable {
                    report: None,
                    reason: FamilyCohortInapplicable::MissingAdmission,
                },
                report: None,
                members: Vec::new(),
            });
        };
        let admission_report = report_id(&report_text)?;
        let Some(stored) = report(conn, &admission_report).await? else {
            return Ok(FamilyCohort {
                state: FamilyCohortState::Inapplicable {
                    report: Some(admission_report),
                    reason: FamilyCohortInapplicable::UnknownReport,
                },
                report: None,
                members: Vec::new(),
            });
        };
        let target = FamilyTargetId {
            family: family.clone(),
            version: TargetVersion::new(u32::try_from(target_version).unwrap_or(0)).ok_or_else(
                || StoreError::InvalidInput("invalid cohort admission target".to_owned()),
            )?,
        };
        let subject_matches = matches!(
            &stored.report.subject,
            ModelSubject::SharedFamily { family: subject } if subject == family
        );
        let candidate_matches = matches!(
            &stored.report.candidate,
            ModelVersionId::SharedFamily { target: candidate } if candidate == &target
        );
        if !subject_matches
            || !candidate_matches
            || stored.report.capability != QualificationCapability::CrossBodyAdmission
            || !matches!(stored.report.outcome, AssessmentOutcome::Pass)
        {
            return Ok(FamilyCohort {
                state: FamilyCohortState::Inapplicable {
                    report: Some(admission_report),
                    reason: FamilyCohortInapplicable::WrongBinding,
                },
                report: None,
                members: Vec::new(),
            });
        }
        if !report_applicable(conn, &admission_report).await? {
            return Ok(FamilyCohort {
                state: FamilyCohortState::Inapplicable {
                    report: Some(admission_report),
                    reason: FamilyCohortInapplicable::NotApplicable,
                },
                report: None,
                members: Vec::new(),
            });
        }
        if !engine_prerequisite_passes(conn, &admission_report).await? {
            return Ok(FamilyCohort {
                state: FamilyCohortState::Inapplicable {
                    report: Some(admission_report),
                    reason: FamilyCohortInapplicable::EnginePrerequisiteFailed,
                },
                report: None,
                members: Vec::new(),
            });
        }
        Some(admission_report)
    } else {
        None
    };
    Ok(FamilyCohort {
        state: FamilyCohortState::Current,
        report: admission_report,
        members,
    })
}

pub async fn record_family_cohort_member(
    conn: &mut SqliteConnection,
    family: &LookId,
    member: &CohortMember,
    role: CohortRole,
    now: i64,
) -> Result<CohortRevision> {
    if role == CohortRole::Admitted {
        return Err(StoreError::InvalidInput(
            "an admitted cohort revision is recorded by its admission report".to_owned(),
        ));
    }
    let mut members = family_cohort(conn, family, role).await?.members;
    let key = cohort_member_key(member);
    if !members
        .iter()
        .any(|existing| cohort_member_key(existing) == key)
    {
        members.push(member.clone());
    }
    let digest = cohort_digest(&members);
    let basis = match role {
        CohortRole::Seed => CohortBasis::SeedEvidence,
        CohortRole::Admitted => CohortBasis::CrossBodyAdmission,
    };
    record_cohort_revision(conn, family, role, basis, digest, &members, now).await
}

fn cohort_member_key(member: &CohortMember) -> String {
    serde_json::to_string(member).expect("a cohort member has no unserializable value")
}

fn cohort_digest(members: &[CohortMember]) -> Sha256Hex {
    let mut keys: Vec<String> = members.iter().map(cohort_member_key).collect();
    keys.sort_unstable();
    keys.dedup();
    let mut hasher = Sha256::new();
    for key in keys {
        hasher.update(key.as_bytes());
        hasher.update([0]);
    }
    Sha256Hex::from_bytes(hasher.finalize().into())
}

async fn engine_prerequisite_passes(
    conn: &mut SqliteConnection,
    report: &ModelReportId,
) -> Result<bool> {
    for dependency in dependencies(conn, "report", &report.to_hex()).await? {
        if dependency.kind != DependencyKind::EngineQualification {
            continue;
        }
        let Some((engine, rule)) = dependency.target.as_str().rsplit_once('@') else {
            continue;
        };
        let id = EnginePrerequisiteId {
            engine: SourceRef::new(engine)?,
            rule_revision: looklab_schema::RuleRevision::new(rule)?,
        };
        if matches!(
            engine_prerequisite_applicable(conn, &id).await?,
            Some(AssessmentOutcome::Pass)
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn project_member_claim(global: &ReportScope, realization: &BodyRealizationId) -> ReportScope {
    let points = global
        .points
        .iter()
        .filter(|point| match &point.firmware {
            Some(firmware) => {
                firmware.body == realization.body && firmware.epoch == realization.epoch
            }
            None => {
                global.assessed_bodies.len() == 1
                    && global.assessed_bodies.first() == Some(&realization.body)
            }
        })
        .cloned()
        .collect();
    ReportScope {
        points,
        combination_basis: global.combination_basis,
        defaults_assessed: global.defaults_assessed,
        output_encoding: global.output_encoding,
        colour_cells: global.colour_cells.clone(),
        exposure_cells: global.exposure_cells.clone(),
        assessed_bodies: vec![realization.body.clone()],
        assessed_domain: global.assessed_domain.clone(),
    }
}

fn scope_covers(member: &ReportScope, claim: &ReportScope) -> bool {
    member.output_encoding == claim.output_encoding
        && claim
            .points
            .iter()
            .all(|point| member.points.contains(point))
        && claim
            .colour_cells
            .iter()
            .all(|cell| member.colour_cells.contains(cell))
        && (claim.exposure_cells.is_empty()
            || member.exposure_cells.is_empty()
            || claim
                .exposure_cells
                .iter()
                .all(|cell| member.exposure_cells.contains(cell)))
}

fn mask_covers(mask: &SupportedMask, claim: &ReportScope) -> bool {
    claim.colour_cells.iter().all(|cell| mask.contains(cell))
        && claim.exposure_cells.iter().all(|cell| mask.contains(cell))
}

pub async fn record_cohort_admission(
    conn: &mut SqliteConnection,
    request: &CohortAdmissionRequest,
    now: i64,
) -> Result<()> {
    let stored = report(conn, &request.report)
        .await?
        .ok_or_else(|| StoreError::InvalidInput("unknown cohort admission report".to_owned()))?;
    let subject_matches = matches!(
        &stored.report.subject,
        ModelSubject::SharedFamily { family } if family == &request.family
    );
    let candidate_matches = matches!(
        &stored.report.candidate,
        ModelVersionId::SharedFamily { target } if target == &request.target
    );
    if !subject_matches
        || !candidate_matches
        || stored.report.capability != QualificationCapability::CrossBodyAdmission
        || !matches!(stored.report.outcome, AssessmentOutcome::Pass)
        || stored.report.scope.assessed_domain != request.assessed_domain
        || !report_applicable(conn, &request.report).await?
        || !engine_prerequisite_passes(conn, &request.report).await?
    {
        return Err(StoreError::InvalidInput(
            "cohort admission requires an applicable cross-body pass, its domain and a passing engine prerequisite"
                .to_owned(),
        ));
    }
    if request.members.is_empty() {
        return Err(StoreError::InvalidInput(
            "cohort admission requires at least one member".to_owned(),
        ));
    }
    let proof_scope = stored.report.scope.clone();
    let mut admitted_members = Vec::new();
    for member in &request.members {
        let CohortMember::Admitted {
            realization,
            report: member_report_id,
            ..
        } = member
        else {
            return Err(StoreError::InvalidInput(
                "cohort admission members must carry an exact proof binding".to_owned(),
            ));
        };
        if realization.target != request.target {
            return Err(StoreError::InvalidInput(
                "an admitted cohort member pins another family target".to_owned(),
            ));
        }
        let Some(member_report) = report(conn, member_report_id).await? else {
            return Err(StoreError::InvalidInput(
                "an admitted cohort member's report is unknown".to_owned(),
            ));
        };
        if member_report.report.candidate
            != (ModelVersionId::BodyRealization {
                realization: realization.clone(),
            })
            || member_report.report.subject.key()
                != SubjectKey::for_realization(&realization.realization)
            || !matches!(member_report.report.outcome, AssessmentOutcome::Pass)
            || !matches!(
                member_report.report.capability,
                QualificationCapability::NativePrediction
                    | QualificationCapability::ControlsMatching
            )
            || !report_applicable(conn, member_report_id).await?
        {
            return Err(StoreError::InvalidInput(
                "an admitted cohort member needs a passing report for that exact realization and claimed scope"
                    .to_owned(),
            ));
        }
        let claim = project_member_claim(&proof_scope, &realization.realization);
        if claim.points.is_empty()
            || !scope_covers(&member_report.report.scope, &claim)
            || !mask_covers(&member_report.report.supported_mask, &claim)
        {
            return Err(StoreError::InvalidInput(
                "an admitted cohort member's report does not cover the projected proof".to_owned(),
            ));
        }
        let claim_cells: Vec<CellId> = claim
            .colour_cells
            .iter()
            .chain(claim.exposure_cells.iter())
            .cloned()
            .collect();
        let supported_mask = member_report
            .report
            .supported_mask
            .restricted_to(&claim_cells)
            .intersection(&stored.report.supported_mask.restricted_to(&claim_cells));
        if supported_mask.is_empty() {
            return Err(StoreError::InvalidInput(
                "an admitted cohort member has no shared supported cells with the proof".to_owned(),
            ));
        }
        admitted_members.push(CohortMember::Admitted {
            realization: realization.clone(),
            report: *member_report_id,
            scope: Box::new(claim),
            supported_mask,
        });
        insert_dependency_edges_internal(
            conn,
            "report",
            &request.report.to_hex(),
            &[DependencyRef::required(
                DependencyKind::Report,
                DependencyTarget::report(member_report_id),
            )],
            now,
            false,
        )
        .await?;
    }
    let digest = cohort_digest(&admitted_members);
    let revision = record_cohort_revision(
        conn,
        &request.family,
        CohortRole::Admitted,
        CohortBasis::CrossBodyAdmission,
        digest,
        &admitted_members,
        now,
    )
    .await?;
    sqlx::query(
        "INSERT INTO model_cohort_admissions
         (family, target_version, report_id, cohort_revision, assessed_domain, recorded_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT DO NOTHING",
    )
    .bind(request.family.as_str())
    .bind(i64::from(request.target.version.get()))
    .bind(request.report.to_hex())
    .bind(i64::from(revision.get()))
    .bind(request.assessed_domain.as_str())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    note_evidence_arrival(conn, &SubjectKey::for_family(&request.family), now).await?;
    Ok(())
}

async fn insert_eligibility_snapshot(
    conn: &mut SqliteConnection,
    eligibility: &TargetEligibility,
    now: i64,
) -> Result<EligibilitySnapshotId> {
    let eligibility = eligibility.clone().sealed();
    let id = eligibility.id;
    sqlx::query(
        "INSERT INTO model_target_eligibility_snapshots
         (id, target, basis, artifacts, base_pin, support_uncertainty_source, status, reasons,
          recorded_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(id.to_hex())
    .bind(eligibility.target.to_key())
    .bind(eligibility.basis.as_str())
    .bind(serde_json::to_string(&eligibility.artifacts)?)
    .bind(serde_json::to_string(&eligibility.base_pin)?)
    .bind(
        eligibility
            .support_uncertainty_source
            .as_ref()
            .map(looklab_schema::SourceRef::as_str),
    )
    .bind(eligibility.status.as_str())
    .bind(serde_json::to_string(&eligibility.reasons)?)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    insert_dependency_edges_internal(
        conn,
        "target_eligibility",
        &id.to_hex(),
        &eligibility.dependencies,
        now,
        true,
    )
    .await?;
    Ok(id)
}

pub async fn record_target_eligibility(
    conn: &mut SqliteConnection,
    eligibility: &TargetEligibility,
    now: i64,
) -> Result<EligibilitySnapshotId> {
    let target_exists: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM look_versions WHERE look = ? AND version = ?")
            .bind(eligibility.target.look.as_str())
            .bind(i64::from(eligibility.target.version.get()))
            .fetch_one(&mut *conn)
            .await?;
    if target_exists == 0 {
        return Err(StoreError::UnknownPublishedTarget(
            eligibility.target.clone(),
        ));
    }
    let previous: Option<String> = sqlx::query_scalar(
        "SELECT snapshot_id FROM model_target_eligibility_current WHERE target = ?",
    )
    .bind(eligibility.target.to_key())
    .fetch_optional(&mut *conn)
    .await?;
    let id = insert_eligibility_snapshot(conn, eligibility, now).await?;
    sqlx::query(
        "INSERT INTO model_target_eligibility_current (target, snapshot_id, updated_at)
         VALUES (?, ?, ?)
         ON CONFLICT (target) DO UPDATE
         SET snapshot_id = excluded.snapshot_id, updated_at = excluded.updated_at",
    )
    .bind(eligibility.target.to_key())
    .bind(id.to_hex())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    if previous.as_deref() != Some(id.to_hex().as_str()) {
        bump_publication(conn).await?;
    }
    Ok(id)
}

pub async fn target_eligibility(
    conn: &mut SqliteConnection,
    target: &PublishedTargetId,
) -> Result<Option<TargetEligibility>> {
    let snapshot: Option<String> = sqlx::query_scalar(
        "SELECT snapshot_id FROM model_target_eligibility_current WHERE target = ?",
    )
    .bind(target.to_key())
    .fetch_optional(&mut *conn)
    .await?;
    let Some(snapshot) = snapshot else {
        return Ok(None);
    };
    let id = EligibilitySnapshotId::parse(&snapshot)
        .map_err(|error| StoreError::InvalidInput(error.to_string()))?;
    target_eligibility_snapshot(conn, &id).await
}

pub async fn target_eligibility_snapshot(
    conn: &mut SqliteConnection,
    id: &EligibilitySnapshotId,
) -> Result<Option<TargetEligibility>> {
    let row = sqlx::query(
        "SELECT target, basis, artifacts, base_pin, support_uncertainty_source, status, reasons
         FROM model_target_eligibility_snapshots WHERE id = ?",
    )
    .bind(id.to_hex())
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(TargetEligibility {
        id: *id,
        target: PublishedTargetId::parse(&row.try_get::<String, _>("target")?)?,
        basis: TargetBasis::parse(row.try_get::<String, _>("basis")?.as_str())?,
        artifacts: serde_json::from_str(&row.try_get::<String, _>("artifacts")?)?,
        base_pin: serde_json::from_str(
            &row.try_get::<Option<String>, _>("base_pin")?
                .unwrap_or_else(|| "null".to_owned()),
        )?,
        support_uncertainty_source: row
            .try_get::<Option<String>, _>("support_uncertainty_source")?
            .map(looklab_schema::SourceRef::new)
            .transpose()?,
        status: TargetEligibilityStatus::parse(row.try_get::<String, _>("status")?.as_str())?,
        reasons: serde_json::from_str(&row.try_get::<String, _>("reasons")?)?,
        dependencies: dependencies(conn, "target_eligibility", &id.to_hex()).await?,
    }))
}

pub async fn target_eligibilities(conn: &mut SqliteConnection) -> Result<Vec<TargetEligibility>> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT snapshot_id FROM model_target_eligibility_current ORDER BY target",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut eligibilities = Vec::new();
    for id in ids {
        let id = EligibilitySnapshotId::parse(&id)
            .map_err(|error| StoreError::InvalidInput(error.to_string()))?;
        if let Some(eligibility) = target_eligibility_snapshot(conn, &id).await? {
            eligibilities.push(eligibility);
        }
    }
    Ok(eligibilities)
}

pub async fn note_evidence_arrival(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
    now: i64,
) -> Result<PendingGeneration> {
    let parsed = parse_subject(subject.as_str())?;
    register_subject(conn, &parsed, now).await?;
    let generation: i64 = sqlx::query_scalar(
        "INSERT INTO model_pending_work (subject_key, generation, updated_at)
         VALUES (?, 1, ?)
         ON CONFLICT (subject_key) DO UPDATE
         SET generation = model_pending_work.generation + 1,
             updated_at = excluded.updated_at
         RETURNING generation",
    )
    .bind(subject.as_str())
    .bind(now)
    .fetch_one(&mut *conn)
    .await?;
    Ok(PendingGeneration::new(
        u64::try_from(generation).unwrap_or(0),
    ))
}

pub async fn pending_generation(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
) -> Result<PendingGeneration> {
    let generation: Option<i64> =
        sqlx::query_scalar("SELECT generation FROM model_pending_work WHERE subject_key = ?")
            .bind(subject.as_str())
            .fetch_optional(&mut *conn)
            .await?;
    Ok(PendingGeneration::new(
        generation
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(0),
    ))
}

pub async fn current_projection(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
    use_: ProjectionUse,
    capability: QualificationCapability,
    scope_key: &ScopeKey,
) -> Result<Option<looklab_schema::CurrentProjection>> {
    let row = sqlx::query(
        "SELECT subject_key, \"use\", capability, scope_key, selected_report_id,
                selected_candidate_kind, selected_candidate_id, selected_candidate_target,
                selected_candidate_version, pending_generation, applicability,
                invalidated_reason
         FROM model_current_projections
         WHERE subject_key = ? AND \"use\" = ? AND capability = ? AND scope_key = ?",
    )
    .bind(subject.as_str())
    .bind(use_.as_str())
    .bind(capability.as_str())
    .bind(scope_key.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| projection_from_row(&row)).transpose()
}

pub async fn current_projections(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
    use_: ProjectionUse,
) -> Result<Vec<looklab_schema::CurrentProjection>> {
    let rows = sqlx::query(
        "SELECT subject_key, \"use\", capability, scope_key, selected_report_id,
                selected_candidate_kind, selected_candidate_id, selected_candidate_target,
                selected_candidate_version, pending_generation, applicability,
                invalidated_reason
         FROM model_current_projections
         WHERE subject_key = ? AND \"use\" = ?
         ORDER BY capability, scope_key",
    )
    .bind(subject.as_str())
    .bind(use_.as_str())
    .fetch_all(&mut *conn)
    .await?;
    rows.iter().map(projection_from_row).collect()
}

pub async fn applicability(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
    use_: ProjectionUse,
    capability: QualificationCapability,
    scope_key: &ScopeKey,
) -> Result<Applicability> {
    Ok(
        current_projection(conn, subject, use_, capability, scope_key)
            .await?
            .map_or(Applicability::Unassessed, |projection| {
                projection.applicability
            }),
    )
}

fn projection_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<looklab_schema::CurrentProjection> {
    let subject_key: String = row.try_get("subject_key")?;
    let selected_report: Option<String> = row.try_get("selected_report_id")?;
    let candidate_kind: Option<String> = row.try_get("selected_candidate_kind")?;
    let candidate_id: Option<String> = row.try_get("selected_candidate_id")?;
    let candidate_target: Option<String> = row.try_get("selected_candidate_target")?;
    let candidate_version: Option<i64> = row.try_get("selected_candidate_version")?;
    let selected_candidate = match (
        candidate_kind,
        candidate_id,
        candidate_target,
        candidate_version,
    ) {
        (Some(kind), Some(id), Some(target), Some(version)) => Some(candidate_from_columns(
            &kind,
            &id,
            &target,
            version,
            &subject_key,
        )?),
        _ => None,
    };
    let pending: i64 = row.try_get("pending_generation")?;
    Ok(looklab_schema::CurrentProjection {
        subject: SubjectKey::new(subject_key)?,
        use_: ProjectionUse::parse(row.try_get::<String, _>("use")?.as_str())?,
        capability: QualificationCapability::parse(
            row.try_get::<String, _>("capability")?.as_str(),
        )?,
        scope_key: ScopeKey::parse(&row.try_get::<String, _>("scope_key")?)?,
        selected_report: selected_report.map(|value| report_id(&value)).transpose()?,
        selected_candidate,
        pending_generation: PendingGeneration::new(u64::try_from(pending).unwrap_or(0)),
        applicability: parse_applicability(
            row.try_get::<String, _>("applicability")?.as_str(),
            row.try_get::<Option<String>, _>("invalidated_reason")?
                .as_deref(),
        )?,
    })
}

fn parse_applicability(text: &str, reason: Option<&str>) -> Result<Applicability> {
    match text {
        "current" => Ok(Applicability::Current),
        "invalidated" => Ok(Applicability::Invalidated {
            reason: InvalidationReason::parse(reason.unwrap_or_default())?,
        }),
        "unassessed" => Ok(Applicability::Unassessed),
        other => Err(StoreError::InvalidInput(format!(
            "unknown applicability `{other}`"
        ))),
    }
}

pub async fn qualified_selection(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
    capability: QualificationCapability,
    scope_key: &ScopeKey,
) -> Result<Option<QualifiedSelection>> {
    let Some(projection) = current_projection(
        conn,
        subject,
        ProjectionUse::QualifiedMatch,
        capability,
        scope_key,
    )
    .await?
    else {
        return Ok(None);
    };
    if !projection.applicability.is_current() {
        return Ok(None);
    }
    let Some(report_id) = projection.selected_report else {
        return Ok(None);
    };
    let Some(candidate) = projection.selected_candidate.as_ref() else {
        return Ok(None);
    };
    let Some(stored) = report(conn, &report_id).await? else {
        return Ok(None);
    };
    if stored.report.candidate != *candidate
        || !matches!(stored.report.outcome, AssessmentOutcome::Pass)
        || stored.report.capability != capability
        || stored.report.scope.key() != *scope_key
        || !report_applicable(conn, &report_id).await?
    {
        return Ok(None);
    }
    let target = match candidate {
        ModelVersionId::SharedFamily { target } => target,
        ModelVersionId::BodyRealization { realization } => &realization.target,
    };
    if !target_published(conn, target).await?
        || !target_currently_eligible(conn, target).await?
        || !target_applicable(conn, target).await?
    {
        return Ok(None);
    }
    Ok(Some(QualifiedSelection {
        projection,
        report: stored.report,
    }))
}

async fn lease_valid(conn: &mut SqliteConnection, lease: JobLease<'_>) -> Result<bool> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE id = ? AND attempts = ? AND status = 'running'",
    )
    .bind(lease.job.as_str())
    .bind(lease.attempts)
    .fetch_one(&mut *conn)
    .await?;
    Ok(count == 1)
}

/// The target has a version manifest under a lifecycle status new selections may use.
async fn target_published(conn: &mut SqliteConnection, target: &FamilyTargetId) -> Result<bool> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM version_manifests m
         JOIN look_versions v ON v.id = m.look_version_id
         WHERE v.look = ? AND v.version = ?
           AND v.status IN ('provisional', 'rooted', 'published')",
    )
    .bind(target.family.as_str())
    .bind(i64::from(target.version.get()))
    .fetch_one(&mut *conn)
    .await?;
    Ok(count > 0)
}

/// The target's publication-eligibility snapshot is current and eligible.
async fn target_currently_eligible(
    conn: &mut SqliteConnection,
    target: &FamilyTargetId,
) -> Result<bool> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM model_target_eligibility_current c
         JOIN model_target_eligibility_snapshots s ON s.id = c.snapshot_id
         WHERE c.target = ? AND s.target = c.target AND s.status = 'eligible'",
    )
    .bind(target.to_key())
    .fetch_one(&mut *conn)
    .await?;
    Ok(count > 0)
}

/// The exact target version was revoked as a family version.
async fn target_revoked(conn: &mut SqliteConnection, target: &FamilyTargetId) -> Result<bool> {
    let revoked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM model_revoked_dependencies
         WHERE kind = 'family_version' AND target = ?",
    )
    .bind(target.to_key())
    .fetch_one(&mut *conn)
    .await?;
    Ok(revoked > 0)
}

/// The exact target version is not revoked and, when it is the family head, remains applicable.
async fn target_applicable(conn: &mut SqliteConnection, target: &FamilyTargetId) -> Result<bool> {
    if target_revoked(conn, target).await? {
        return Ok(false);
    }
    let head: Option<String> = sqlx::query_scalar(
        "SELECT applicability FROM model_family_heads
         WHERE family = ? AND head_version = ?",
    )
    .bind(target.family.as_str())
    .bind(i64::from(target.version.get()))
    .fetch_optional(&mut *conn)
    .await?;
    Ok(head.as_deref() != Some("invalidated"))
}

async fn family_target_exists(
    conn: &mut SqliteConnection,
    target: &FamilyTargetId,
) -> Result<bool> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM look_versions WHERE look = ? AND version = ?")
            .bind(target.family.as_str())
            .bind(i64::from(target.version.get()))
            .fetch_one(&mut *conn)
            .await?;
    Ok(count > 0)
}

struct RevisionRow {
    subject_key: String,
    target: FamilyTargetId,
    status: RealizationStatus,
    safety: CandidateSafety,
}

async fn revision_row(
    conn: &mut SqliteConnection,
    id: &RealizationRevisionId,
) -> Result<Option<RevisionRow>> {
    let row = sqlx::query(
        "SELECT subject_key, family, target_version, status, numerical, release_gates
         FROM model_realization_revisions WHERE id = ?",
    )
    .bind(id.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| {
        Ok(RevisionRow {
            subject_key: row.try_get("subject_key")?,
            target: FamilyTargetId {
                family: look_id(&row.try_get::<String, _>("family")?)?,
                version: TargetVersion::new(
                    u32::try_from(row.try_get::<i64, _>("target_version")?).unwrap_or(0),
                )
                .ok_or_else(|| StoreError::InvalidInput("invalid target version".to_owned()))?,
            },
            status: RealizationStatus::parse(row.try_get::<String, _>("status")?.as_str())?,
            safety: CandidateSafety {
                numerical: SafetyCheck::parse(row.try_get::<String, _>("numerical")?.as_str())?,
                release_gates: SafetyCheck::parse(
                    row.try_get::<String, _>("release_gates")?.as_str(),
                )?,
            },
        })
    })
    .transpose()
}

fn selection_matches(
    projection: Option<&looklab_schema::CurrentProjection>,
    expected: ExpectedSelection<'_>,
) -> bool {
    match expected {
        ExpectedSelection::Absent => projection.is_none_or(|projection| {
            projection.selected_candidate.is_none() && projection.selected_report.is_none()
        }),
        ExpectedSelection::Selected(selection) => projection.is_some_and(|projection| {
            projection.selected_report == selection.report.copied()
                && projection
                    .selected_candidate
                    .as_ref()
                    .map(ModelVersionId::candidate_id)
                    == Some(selection.revision.to_string())
        }),
    }
}

async fn check_supersession(
    conn: &mut SqliteConnection,
    request: &RealizationSelection<'_>,
    destination: Option<&ModelReport>,
) -> Result<Option<SelectionRejection>> {
    for expectation in request.supersedes {
        let projection = current_projection(
            conn,
            request.subject,
            request.use_,
            request.capability,
            expectation.scope_key,
        )
        .await?;
        if !selection_matches(projection.as_ref(), expectation.expected) {
            return Ok(Some(SelectionRejection::SupersessionStale(
                expectation.scope_key.clone(),
            )));
        }
        if let ExpectedSelection::Selected(selection) = expectation.expected {
            let Some(destination) = destination else {
                return Ok(Some(SelectionRejection::SupersessionRequiresReport));
            };
            let Some(report_id) = selection.report else {
                return Ok(Some(SelectionRejection::SupersessionStale(
                    expectation.scope_key.clone(),
                )));
            };
            let Some(stored) = report(conn, report_id).await? else {
                return Ok(Some(SelectionRejection::SupersessionStale(
                    expectation.scope_key.clone(),
                )));
            };
            if !destination.scope.overlaps(&stored.report.scope)
                || !destination
                    .supported_mask
                    .overlaps(&stored.report.supported_mask)
            {
                return Ok(Some(SelectionRejection::DisjointSupersession(
                    expectation.scope_key.clone(),
                )));
            }
        }
    }
    Ok(None)
}

pub async fn select_realization(
    conn: &mut SqliteConnection,
    request: &RealizationSelection<'_>,
    now: i64,
) -> Result<SelectionOutcome> {
    if !lease_valid(conn, request.lease).await? {
        return Ok(SelectionOutcome::Rejected(SelectionRejection::LeaseLost));
    }
    if !sources_current(conn, request.sources).await? {
        return Ok(SelectionOutcome::Rejected(
            SelectionRejection::SupersededSources,
        ));
    }
    if !anchor_matches(
        conn,
        &SubjectKey::for_family(&request.target.family),
        request.expected_anchor,
    )
    .await?
    {
        return Ok(SelectionOutcome::Rejected(
            SelectionRejection::AnchorChanged,
        ));
    }
    let Some(revision) = revision_row(conn, request.revision).await? else {
        return Ok(SelectionOutcome::Rejected(
            SelectionRejection::UnknownRevision(request.revision.clone()),
        ));
    };
    if revision_invalidated(conn, request.revision).await? {
        return Ok(SelectionOutcome::Rejected(SelectionRejection::Invalidated(
            request.revision.clone(),
        )));
    }
    if revision.subject_key != request.subject.as_str() {
        return Ok(SelectionOutcome::Rejected(
            SelectionRejection::WrongSubject {
                expected: request.subject.clone(),
                actual: SubjectKey::new(revision.subject_key)?,
            },
        ));
    }
    if revision.target != *request.target {
        return Ok(SelectionOutcome::Rejected(
            SelectionRejection::TargetMismatch {
                pinned: revision.target,
                requested: request.target.clone(),
            },
        ));
    }
    if revision.status != RealizationStatus::Candidate {
        return Ok(SelectionOutcome::Rejected(SelectionRejection::NotAccepted(
            revision.status,
        )));
    }
    if !revision.safety.accepted() {
        return Ok(SelectionOutcome::Rejected(
            SelectionRejection::SafetyFailed(revision.safety),
        ));
    }
    if !target_published(conn, request.target).await? {
        return Ok(SelectionOutcome::Rejected(
            SelectionRejection::TargetUnpublished(request.target.clone()),
        ));
    }
    if !target_currently_eligible(conn, request.target).await? {
        return Ok(SelectionOutcome::Rejected(
            SelectionRejection::TargetIneligible(request.target.clone()),
        ));
    }
    if !target_applicable(conn, request.target).await? {
        return Ok(SelectionOutcome::Rejected(
            SelectionRejection::TargetInvalidated(request.target.clone()),
        ));
    }
    let selected_report = match request.use_ {
        ProjectionUse::QualifiedMatch => {
            let Some(report_id) = request.report else {
                return Ok(SelectionOutcome::Rejected(
                    SelectionRejection::MissingReport,
                ));
            };
            let Some(header) = report_header(conn, report_id).await? else {
                return Ok(SelectionOutcome::Rejected(
                    SelectionRejection::UnknownReport(*report_id),
                ));
            };
            if header.subject_key != request.subject.as_str() {
                return Ok(SelectionOutcome::Rejected(
                    SelectionRejection::ReportSubjectMismatch(*report_id),
                ));
            }
            if header.candidate_kind != ModelVersionKind::BodyRealization.as_str()
                || header.candidate_id != request.revision.as_str()
                || header.candidate_target != request.target.to_key()
                || Some(header.candidate_version)
                    != request
                        .revision
                        .as_str()
                        .rsplit_once('@')
                        .and_then(|(_, version)| version.parse::<u32>().ok())
                        .map(i64::from)
            {
                return Ok(SelectionOutcome::Rejected(
                    SelectionRejection::ReportCandidateMismatch(*report_id),
                ));
            }
            if header.capability != request.capability {
                return Ok(SelectionOutcome::Rejected(
                    SelectionRejection::CapabilityMismatch {
                        required: request.capability,
                        actual: header.capability,
                    },
                ));
            }
            if header.scope_key != request.scope_key.as_str() {
                return Ok(SelectionOutcome::Rejected(
                    SelectionRejection::ReportScopeMismatch(*report_id),
                ));
            }
            if !matches!(header.outcome, AssessmentOutcome::Pass) {
                return Ok(SelectionOutcome::Rejected(
                    SelectionRejection::OutcomeNotPass(header.outcome),
                ));
            }
            if !report_applicable(conn, report_id).await? {
                return Ok(SelectionOutcome::Rejected(
                    SelectionRejection::ReportInapplicable(*report_id),
                ));
            }
            if let Some(rejection) =
                verify_owner_snapshot(conn, "report", &report_id.to_hex(), request.snapshot).await?
            {
                return Ok(SelectionOutcome::Rejected(rejection));
            }
            Some(*report_id)
        }
        ProjectionUse::ExperimentalMatch | ProjectionUse::Preview => None,
    };
    let destination_report = if let Some(report_id) = request.report {
        report(conn, report_id).await?.map(|stored| stored.report)
    } else {
        None
    };
    if let Some(rejection) = check_supersession(conn, request, destination_report.as_ref()).await? {
        return Ok(SelectionOutcome::Rejected(rejection));
    }
    if let Some(rejection) = verify_owner_snapshot(
        conn,
        "realization_revision",
        request.revision.as_str(),
        request.snapshot,
    )
    .await?
    {
        return Ok(SelectionOutcome::Rejected(rejection));
    }
    sqlx::query(
        "INSERT INTO model_current_projections
         (subject_key, \"use\", capability, scope_key, pending_generation, applicability,
          updated_at)
         VALUES (?, ?, ?, ?, 0, 'current', ?)
         ON CONFLICT DO NOTHING",
    )
    .bind(request.subject.as_str())
    .bind(request.use_.as_str())
    .bind(request.capability.as_str())
    .bind(request.scope_key.as_str())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    let (expected_kind, expected_id, expected_report) = match request.expected {
        ExpectedSelection::Absent => (None, None, None),
        ExpectedSelection::Selected(selection) => (
            Some(ModelVersionKind::BodyRealization.as_str()),
            Some(selection.revision.as_str()),
            selection.report.copied().map(ModelReportId::to_hex),
        ),
    };
    let updated = sqlx::query(
        "UPDATE model_current_projections
         SET selected_report_id = ?, selected_candidate_kind = 'body_realization',
             selected_candidate_id = ?, selected_candidate_target = ?,
             selected_candidate_version = ?, pending_generation = ?, applicability = 'current',
             invalidated_reason = NULL, updated_at = ?
         WHERE subject_key = ? AND \"use\" = ? AND capability = ? AND scope_key = ?
           AND selected_candidate_kind IS ? AND selected_candidate_id IS ?
           AND selected_report_id IS ?",
    )
    .bind(selected_report.map(ModelReportId::to_hex))
    .bind(request.revision.as_str())
    .bind(request.target.to_key())
    .bind(i64::from(
        request
            .revision
            .as_str()
            .rsplit_once('@')
            .and_then(|(_, version)| version.parse::<u32>().ok())
            .unwrap_or(0),
    ))
    .bind(i64::try_from(request.observed_pending.get()).unwrap_or(i64::MAX))
    .bind(now)
    .bind(request.subject.as_str())
    .bind(request.use_.as_str())
    .bind(request.capability.as_str())
    .bind(request.scope_key.as_str())
    .bind(expected_kind)
    .bind(expected_id)
    .bind(expected_report)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if updated == 0 {
        let actual: Option<(Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT selected_candidate_id, selected_report_id FROM model_current_projections
             WHERE subject_key = ? AND \"use\" = ? AND capability = ? AND scope_key = ?",
        )
        .bind(request.subject.as_str())
        .bind(request.use_.as_str())
        .bind(request.capability.as_str())
        .bind(request.scope_key.as_str())
        .fetch_optional(&mut *conn)
        .await?;
        let actual = match actual {
            Some((Some(revision), report)) => Some(SelectionRefOwned {
                revision: RealizationRevisionId::new(revision)?,
                report: report.map(|value| report_id(&value)).transpose()?,
            }),
            _ => None,
        };
        return Ok(SelectionOutcome::Stale { actual });
    }
    for expectation in request.supersedes {
        sqlx::query(
            "UPDATE model_current_projections
             SET applicability = 'invalidated', invalidated_reason = 'superseded', updated_at = ?
             WHERE subject_key = ? AND \"use\" = ? AND capability = ? AND scope_key = ?
               AND applicability = 'current'",
        )
        .bind(now)
        .bind(request.subject.as_str())
        .bind(request.use_.as_str())
        .bind(request.capability.as_str())
        .bind(expectation.scope_key.as_str())
        .execute(&mut *conn)
        .await?;
    }
    bump_publication(conn).await?;
    Ok(SelectionOutcome::Selected)
}

pub async fn select_family(
    conn: &mut SqliteConnection,
    request: &FamilySelection<'_>,
    now: i64,
) -> Result<FamilyProjectionOutcome> {
    if !lease_valid(conn, request.lease).await? {
        return Ok(FamilyProjectionOutcome::Rejected(
            SelectionRejection::LeaseLost,
        ));
    }
    if !sources_current(conn, request.sources).await? {
        return Ok(FamilyProjectionOutcome::Rejected(
            SelectionRejection::SupersededSources,
        ));
    }
    if request.target.family != *request.family {
        return Ok(FamilyProjectionOutcome::Rejected(
            SelectionRejection::FamilyTargetMismatch {
                expected: request.family.clone(),
                target: request.target.clone(),
            },
        ));
    }
    if !anchor_matches(
        conn,
        &SubjectKey::for_family(request.family),
        request.expected_anchor,
    )
    .await?
    {
        return Ok(FamilyProjectionOutcome::Rejected(
            SelectionRejection::AnchorChanged,
        ));
    }
    if !target_published(conn, request.target).await? {
        return Ok(FamilyProjectionOutcome::Rejected(
            SelectionRejection::TargetUnpublished(request.target.clone()),
        ));
    }
    if !target_currently_eligible(conn, request.target).await? {
        return Ok(FamilyProjectionOutcome::Rejected(
            SelectionRejection::TargetIneligible(request.target.clone()),
        ));
    }
    if target_revoked(conn, request.target).await? {
        return Ok(FamilyProjectionOutcome::Rejected(
            SelectionRejection::TargetInvalidated(request.target.clone()),
        ));
    }
    let Some(head) = family_head(conn, request.family).await? else {
        return Ok(FamilyProjectionOutcome::Rejected(
            SelectionRejection::FamilyHeadInapplicable(request.family.clone()),
        ));
    };
    if head.target != *request.target {
        return Ok(FamilyProjectionOutcome::Rejected(
            SelectionRejection::FamilyHeadMismatch {
                expected: request.target.clone(),
                actual: Some(head.target),
            },
        ));
    }
    let Some(header) = report_header(conn, request.report).await? else {
        return Ok(FamilyProjectionOutcome::Rejected(
            SelectionRejection::UnknownReport(*request.report),
        ));
    };
    if header.subject_key != SubjectKey::for_family(request.family).as_str() {
        return Ok(FamilyProjectionOutcome::Rejected(
            SelectionRejection::ReportSubjectMismatch(*request.report),
        ));
    }
    if header.candidate_kind != ModelVersionKind::SharedFamily.as_str()
        || header.candidate_id != request.target.to_key()
        || header.candidate_target != request.target.to_key()
        || header.candidate_version != i64::from(request.target.version.get())
    {
        return Ok(FamilyProjectionOutcome::Rejected(
            SelectionRejection::ReportCandidateMismatch(*request.report),
        ));
    }
    if header.capability != QualificationCapability::CrossBodyAdmission {
        return Ok(FamilyProjectionOutcome::Rejected(
            SelectionRejection::CapabilityMismatch {
                required: QualificationCapability::CrossBodyAdmission,
                actual: header.capability,
            },
        ));
    }
    if header.scope_key != request.scope_key.as_str() {
        return Ok(FamilyProjectionOutcome::Rejected(
            SelectionRejection::ReportScopeMismatch(*request.report),
        ));
    }
    if !matches!(header.outcome, AssessmentOutcome::Pass) {
        return Ok(FamilyProjectionOutcome::Rejected(
            SelectionRejection::OutcomeNotPass(header.outcome),
        ));
    }
    if !report_applicable(conn, request.report).await? {
        return Ok(FamilyProjectionOutcome::Rejected(
            SelectionRejection::ReportInapplicable(*request.report),
        ));
    }
    if !engine_prerequisite_passes(conn, request.report).await? {
        return Ok(FamilyProjectionOutcome::Rejected(
            SelectionRejection::EnginePrerequisiteFailed(*request.report),
        ));
    }
    if let Some(rejection) =
        verify_owner_snapshot(conn, "report", &request.report.to_hex(), request.snapshot).await?
    {
        return Ok(FamilyProjectionOutcome::Rejected(rejection));
    }
    let subject = SubjectKey::for_family(request.family);
    sqlx::query(
        "INSERT INTO model_current_projections
         (subject_key, \"use\", capability, scope_key, pending_generation, applicability,
          updated_at)
         VALUES (?, 'qualified_match', 'cross_body_admission', ?, 0, 'current', ?)
         ON CONFLICT DO NOTHING",
    )
    .bind(subject.as_str())
    .bind(request.scope_key.as_str())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    let (expected_kind, expected_id, expected_report) = match request.expected {
        ExpectedFamilySelection::Absent => (None, None, None),
        ExpectedFamilySelection::Selected(selection) => (
            Some(ModelVersionKind::SharedFamily.as_str()),
            Some(selection.target.to_key()),
            selection.report.copied().map(ModelReportId::to_hex),
        ),
    };
    let updated = sqlx::query(
        "UPDATE model_current_projections
         SET selected_report_id = ?, selected_candidate_kind = 'shared_family',
             selected_candidate_id = ?, selected_candidate_target = ?,
             selected_candidate_version = ?, pending_generation = ?, applicability = 'current',
             invalidated_reason = NULL, updated_at = ?
         WHERE subject_key = ? AND \"use\" = 'qualified_match'
           AND capability = 'cross_body_admission' AND scope_key = ?
           AND pending_generation <= ?
           AND selected_candidate_kind IS ? AND selected_candidate_id IS ?
           AND selected_report_id IS ?",
    )
    .bind(request.report.to_hex())
    .bind(request.target.to_key())
    .bind(request.target.to_key())
    .bind(i64::from(request.target.version.get()))
    .bind(i64::try_from(request.observed_pending.get()).unwrap_or(i64::MAX))
    .bind(now)
    .bind(subject.as_str())
    .bind(request.scope_key.as_str())
    .bind(i64::try_from(request.observed_pending.get()).unwrap_or(i64::MAX))
    .bind(expected_kind)
    .bind(expected_id)
    .bind(expected_report)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if updated == 0 {
        let row: Option<(Option<String>, Option<String>, i64)> = sqlx::query_as(
            "SELECT selected_candidate_id, selected_report_id, pending_generation
             FROM model_current_projections
             WHERE subject_key = ? AND \"use\" = 'qualified_match'
               AND capability = 'cross_body_admission' AND scope_key = ?",
        )
        .bind(subject.as_str())
        .bind(request.scope_key.as_str())
        .fetch_optional(&mut *conn)
        .await?;
        let stored = row
            .as_ref()
            .and_then(|(_, _, stored)| u64::try_from(*stored).ok())
            .unwrap_or(0);
        let actual = match row {
            Some((Some(target), report, _)) => Some(FamilySelectionRefOwned {
                target: FamilyTargetId::parse(&target)?,
                report: report.map(|value| report_id(&value)).transpose()?,
            }),
            _ => None,
        };
        let matches_expected = match (request.expected, actual.as_ref()) {
            (ExpectedFamilySelection::Absent, None) => true,
            (ExpectedFamilySelection::Selected(expected), Some(actual)) => {
                actual.target == *expected.target && actual.report.as_ref() == expected.report
            }
            _ => false,
        };
        if matches_expected && stored > request.observed_pending.get() {
            return Ok(FamilyProjectionOutcome::Rejected(
                SelectionRejection::PendingRegressed {
                    selected: PendingGeneration::new(stored),
                    observed: request.observed_pending,
                },
            ));
        }
        return Ok(FamilyProjectionOutcome::Stale { actual });
    }
    sqlx::query(
        "UPDATE model_family_heads
         SET report_id = ?, applicability = 'current', invalidated_reason = NULL,
             assessed_generation = ?, updated_at = ?
         WHERE family = ? AND head_version = ?
           AND (assessed_generation < ?
                OR (assessed_generation = ?
                    AND COALESCE((SELECT created_at FROM model_reports
                                  WHERE id = model_family_heads.report_id), 0)
                        <= (SELECT created_at FROM model_reports WHERE id = ?)))",
    )
    .bind(request.report.to_hex())
    .bind(i64::try_from(request.observed_pending.get()).unwrap_or(i64::MAX))
    .bind(now)
    .bind(request.family.as_str())
    .bind(i64::from(request.target.version.get()))
    .bind(i64::try_from(request.observed_pending.get()).unwrap_or(i64::MAX))
    .bind(i64::try_from(request.observed_pending.get()).unwrap_or(i64::MAX))
    .bind(request.report.to_hex())
    .execute(&mut *conn)
    .await?;
    bump_publication(conn).await?;
    Ok(FamilyProjectionOutcome::Selected)
}

pub async fn seed_family_target(
    conn: &mut SqliteConnection,
    seed: &FamilySeed<'_>,
    now: i64,
) -> Result<FamilySelectionOutcome> {
    if !lease_valid(conn, seed.lease).await? {
        return Ok(FamilySelectionOutcome::Rejected(
            SelectionRejection::LeaseLost,
        ));
    }
    if !sources_current(conn, seed.sources).await? {
        return Ok(FamilySelectionOutcome::Rejected(
            SelectionRejection::SupersededSources,
        ));
    }
    if !anchor_matches(
        conn,
        &SubjectKey::for_family(seed.family),
        seed.expected_anchor,
    )
    .await?
    {
        return Ok(FamilySelectionOutcome::Rejected(
            SelectionRejection::AnchorChanged,
        ));
    }
    if seed.target.family != *seed.family {
        return Ok(FamilySelectionOutcome::Rejected(
            SelectionRejection::FamilyTargetMismatch {
                expected: seed.family.clone(),
                target: seed.target.clone(),
            },
        ));
    }
    if !seed.safety.accepted() {
        return Ok(FamilySelectionOutcome::Rejected(
            SelectionRejection::SafetyFailed(seed.safety),
        ));
    }
    if !family_target_exists(conn, seed.target).await? {
        return Ok(FamilySelectionOutcome::Rejected(
            SelectionRejection::UnknownFamilyTarget(seed.target.clone()),
        ));
    }
    if let Some(rejection) =
        verify_dependency_snapshot(conn, seed.dependencies, seed.snapshot).await?
    {
        return Ok(FamilySelectionOutcome::Rejected(rejection));
    }
    register_subject(
        conn,
        &ModelSubject::SharedFamily {
            family: seed.family.clone(),
        },
        now,
    )
    .await?;
    let inserted = sqlx::query(
        "INSERT INTO model_family_heads
         (family, head_version, source_limited, numerical, release_gates, report_id,
          assessed_generation, applicability, invalidated_reason, updated_at)
         VALUES (?, ?, ?, ?, ?, NULL, ?, 'current', NULL, ?)
         ON CONFLICT (family) DO NOTHING",
    )
    .bind(seed.family.as_str())
    .bind(i64::from(seed.target.version.get()))
    .bind(if seed.source_limited { 1 } else { 0 })
    .bind(seed.safety.numerical.as_str())
    .bind(seed.safety.release_gates.as_str())
    .bind(i64::try_from(seed.observed_pending.get()).unwrap_or(i64::MAX))
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if inserted == 0 {
        let actual: Option<i64> =
            sqlx::query_scalar("SELECT head_version FROM model_family_heads WHERE family = ?")
                .bind(seed.family.as_str())
                .fetch_optional(&mut *conn)
                .await?;
        return Ok(FamilySelectionOutcome::Stale {
            actual: actual
                .and_then(|value| u32::try_from(value).ok())
                .and_then(TargetVersion::new),
        });
    }
    let owner = seed.target.to_key();
    insert_dependency_edges(conn, "family_head", &owner, seed.dependencies, now).await?;
    record_subject_revision(
        conn,
        &SubjectKey::for_family(seed.family),
        seed.target.version.get(),
        SubjectRevisionRole::Target,
        None,
        None,
        now,
    )
    .await?;
    let seed_members: Vec<CohortMember> = seed
        .seed_bodies
        .iter()
        .map(|body| CohortMember::Seed { body: body.clone() })
        .collect();
    let digest = cohort_digest(&seed_members);
    record_cohort_revision(
        conn,
        seed.family,
        CohortRole::Seed,
        CohortBasis::SeedEvidence,
        digest,
        &seed_members,
        now,
    )
    .await?;
    bump_publication(conn).await?;
    release_reserved_version(conn, &SubjectKey::for_family(seed.family)).await?;
    Ok(FamilySelectionOutcome::Selected)
}

pub async fn advance_family_head(
    conn: &mut SqliteConnection,
    advance: &FamilyAdvance<'_>,
    now: i64,
) -> Result<FamilySelectionOutcome> {
    if !lease_valid(conn, advance.lease).await? {
        return Ok(FamilySelectionOutcome::Rejected(
            SelectionRejection::LeaseLost,
        ));
    }
    if !sources_current(conn, advance.sources).await? {
        return Ok(FamilySelectionOutcome::Rejected(
            SelectionRejection::SupersededSources,
        ));
    }
    if !anchor_matches(
        conn,
        &SubjectKey::for_family(advance.family),
        advance.expected_anchor,
    )
    .await?
    {
        return Ok(FamilySelectionOutcome::Rejected(
            SelectionRejection::AnchorChanged,
        ));
    }
    if advance.target.family != *advance.family {
        return Ok(FamilySelectionOutcome::Rejected(
            SelectionRejection::FamilyTargetMismatch {
                expected: advance.family.clone(),
                target: advance.target.clone(),
            },
        ));
    }
    if !advance.safety.accepted() {
        return Ok(FamilySelectionOutcome::Rejected(
            SelectionRejection::SafetyFailed(advance.safety),
        ));
    }
    if !family_target_exists(conn, advance.target).await? {
        return Ok(FamilySelectionOutcome::Rejected(
            SelectionRejection::UnknownFamilyTarget(advance.target.clone()),
        ));
    }
    if let Some(report_id) = advance.report {
        let Some(header) = report_header(conn, report_id).await? else {
            return Ok(FamilySelectionOutcome::Rejected(
                SelectionRejection::UnknownReport(*report_id),
            ));
        };
        if header.subject_key != SubjectKey::for_family(advance.family).as_str() {
            return Ok(FamilySelectionOutcome::Rejected(
                SelectionRejection::ReportSubjectMismatch(*report_id),
            ));
        }
        if header.candidate_kind != ModelVersionKind::SharedFamily.as_str()
            || header.candidate_target != advance.target.to_key()
        {
            return Ok(FamilySelectionOutcome::Rejected(
                SelectionRejection::ReportCandidateMismatch(*report_id),
            ));
        }
        if !matches!(header.outcome, AssessmentOutcome::Pass) {
            return Ok(FamilySelectionOutcome::Rejected(
                SelectionRejection::OutcomeNotPass(header.outcome),
            ));
        }
        if !report_applicable(conn, report_id).await? {
            return Ok(FamilySelectionOutcome::Rejected(
                SelectionRejection::ReportInapplicable(*report_id),
            ));
        }
        if let Some(rejection) =
            verify_owner_snapshot(conn, "report", &report_id.to_hex(), advance.snapshot).await?
        {
            return Ok(FamilySelectionOutcome::Rejected(rejection));
        }
    }
    if let Some(rejection) =
        verify_dependency_snapshot(conn, advance.dependencies, advance.snapshot).await?
    {
        return Ok(FamilySelectionOutcome::Rejected(rejection));
    }
    let updated = sqlx::query(
        "UPDATE model_family_heads
         SET head_version = ?, source_limited = ?, numerical = ?, release_gates = ?,
             report_id = ?, assessed_generation = ?, applicability = 'current',
             invalidated_reason = NULL, updated_at = ?
         WHERE family = ? AND head_version = ?",
    )
    .bind(i64::from(advance.target.version.get()))
    .bind(if advance.source_limited { 1 } else { 0 })
    .bind(advance.safety.numerical.as_str())
    .bind(advance.safety.release_gates.as_str())
    .bind(advance.report.copied().map(ModelReportId::to_hex))
    .bind(i64::try_from(advance.observed_pending.get()).unwrap_or(i64::MAX))
    .bind(now)
    .bind(advance.family.as_str())
    .bind(i64::from(advance.expected.get()))
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if updated == 0 {
        let actual: Option<i64> =
            sqlx::query_scalar("SELECT head_version FROM model_family_heads WHERE family = ?")
                .bind(advance.family.as_str())
                .fetch_optional(&mut *conn)
                .await?;
        return Ok(FamilySelectionOutcome::Stale {
            actual: actual
                .and_then(|value| u32::try_from(value).ok())
                .and_then(TargetVersion::new),
        });
    }
    let owner = advance.target.to_key();
    insert_dependency_edges(conn, "family_head", &owner, advance.dependencies, now).await?;
    record_subject_revision(
        conn,
        &SubjectKey::for_family(advance.family),
        advance.target.version.get(),
        SubjectRevisionRole::Target,
        None,
        None,
        now,
    )
    .await?;
    bump_publication(conn).await?;
    release_reserved_version(conn, &SubjectKey::for_family(advance.family)).await?;
    Ok(FamilySelectionOutcome::Selected)
}

pub async fn advance_family_head_from_moderator(
    conn: &mut SqliteConnection,
    family: &LookId,
    target: &FamilyTargetId,
    eligibility: &TargetEligibility,
    manifest_hash: Sha256Hex,
    restore: &RestoredHeadMetadata,
    now: i64,
) -> Result<bool> {
    if target.family != *family {
        return Err(StoreError::InvalidInput(
            "moderator target names another family".to_owned(),
        ));
    }
    if eligibility.target.look != *family
        || eligibility.target.version != target.version
        || eligibility.basis != TargetBasis::NativeFamily
        || eligibility.status != TargetEligibilityStatus::Eligible
    {
        return Err(StoreError::InvalidInput(
            "moderator target eligibility is not an eligible native-family binding".to_owned(),
        ));
    }
    if !restore.safety.accepted() {
        return Err(StoreError::InvalidInput(
            "moderator restore carries a failed safety check".to_owned(),
        ));
    }
    let publishable: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM look_versions v
         JOIN version_manifests m ON m.look_version_id = v.id
         WHERE v.look = ? AND v.version = ? AND v.status IN ('published', 'rooted')",
    )
    .bind(family.as_str())
    .bind(i64::from(target.version.get()))
    .fetch_one(&mut *conn)
    .await?;
    if publishable == 0 {
        return Err(StoreError::InvalidInput(
            "moderator target is not a publishable resolved version".to_owned(),
        ));
    }
    let current: Option<i64> =
        sqlx::query_scalar("SELECT head_version FROM model_family_heads WHERE family = ?")
            .bind(family.as_str())
            .fetch_optional(&mut *conn)
            .await?;
    let Some(current) = current else {
        return Ok(false);
    };
    record_target_eligibility(conn, eligibility, now).await?;
    insert_artifact(
        conn,
        "family_target",
        &target.to_key(),
        ArtifactBinding {
            sha256: manifest_hash,
            role: ArtifactRole::Manifest,
        },
        now,
    )
    .await?;
    let updated = sqlx::query(
        "UPDATE model_family_heads
         SET head_version = ?, source_limited = ?, numerical = ?, release_gates = ?,
             report_id = NULL, assessed_generation = ?, applicability = 'current',
             invalidated_reason = NULL, updated_at = ?
         WHERE family = ? AND head_version = ?",
    )
    .bind(i64::from(target.version.get()))
    .bind(if restore.source_limited { 1 } else { 0 })
    .bind(restore.safety.numerical.as_str())
    .bind(restore.safety.release_gates.as_str())
    .bind(i64::try_from(restore.assessed_generation.get()).unwrap_or(i64::MAX))
    .bind(now)
    .bind(family.as_str())
    .bind(current)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if updated == 0 {
        return Ok(false);
    }
    let owner = target.to_key();
    insert_dependency_edges(conn, "family_head", &owner, &eligibility.dependencies, now).await?;
    register_subject(
        conn,
        &ModelSubject::SharedFamily {
            family: family.clone(),
        },
        now,
    )
    .await?;
    record_subject_revision(
        conn,
        &SubjectKey::for_family(family),
        target.version.get(),
        SubjectRevisionRole::Target,
        Some(manifest_hash),
        None,
        now,
    )
    .await?;
    bump_publication(conn).await?;
    Ok(true)
}

pub async fn invalidate(
    conn: &mut SqliteConnection,
    dependency: &DependencyRef,
    reason: InvalidationReason,
    now: i64,
) -> Result<InvalidationSummary> {
    let mut summary = InvalidationSummary {
        projections: 0,
        family_heads: 0,
        target_eligibilities: 0,
        bundles: 0,
        reports: 0,
    };
    let mut queue = VecDeque::new();
    queue.push_back((dependency.kind, dependency.target.clone(), true));
    let mut visited: BTreeSet<(DependencyKind, DependencyTarget)> = BTreeSet::new();
    while let Some((kind, target, root)) = queue.pop_front() {
        if !visited.insert((kind, target.clone())) {
            continue;
        }
        let revoke = root || kind != DependencyKind::RealizationRevision;
        if revoke {
            bump_dependency_generation(conn, kind, &target, reason, now).await?;
        }
        if revoke && kind.is_versioned() {
            sqlx::query(
                "INSERT INTO model_revoked_dependencies (kind, target, reason, revoked_at)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT DO NOTHING",
            )
            .bind(kind.as_str())
            .bind(target.as_str())
            .bind(reason.as_str())
            .bind(now)
            .execute(&mut *conn)
            .await?;
        }
        if root {
            sqlx::query(
                "INSERT INTO model_invalidation_events (kind, target, reason, recorded_at)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(kind.as_str())
            .bind(target.as_str())
            .bind(reason.as_str())
            .bind(now)
            .execute(&mut *conn)
            .await?;
        }
        summary.merge(mark_invalidated(conn, kind, &target, reason, now).await?);
        for (owner_kind, owner_id) in dependent_owners(conn, kind, &target).await? {
            let next = match owner_kind.as_str() {
                "report" => Some((DependencyKind::Report, owner_id)),
                "realization_revision" => Some((DependencyKind::RealizationRevision, owner_id)),
                "bundle_revision" => Some((DependencyKind::BundleRevision, owner_id)),
                "family_head" => Some((DependencyKind::FamilyVersion, owner_id)),
                "body_state" => Some((DependencyKind::BodyState, owner_id)),
                "target_eligibility" => {
                    let snapshot = EligibilitySnapshotId::parse(&owner_id)
                        .map_err(|error| StoreError::InvalidInput(error.to_string()))?;
                    target_eligibility_snapshot(conn, &snapshot)
                        .await?
                        .map(|eligibility| {
                            (DependencyKind::PublishedTarget, eligibility.target.to_key())
                        })
                }
                _ => None,
            };
            if let Some((next_kind, next_target)) = next {
                queue.push_back((next_kind, DependencyTarget::new(next_target)?, false));
            }
        }
    }
    if summary.any() {
        bump_publication(conn).await?;
    }
    Ok(summary)
}

pub async fn invalidate_schema_versions(
    conn: &mut SqliteConnection,
    body: &BodyId,
    versions: &[u64],
    reason: InvalidationReason,
    now: i64,
) -> Result<InvalidationSummary> {
    let prefix = format!("{body}@");
    let mut targets: BTreeSet<String> = versions
        .iter()
        .map(|version| format!("{body}@{version}"))
        .collect();
    if versions.is_empty() {
        let recorded: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT target FROM model_dependency_generations WHERE kind = ?",
        )
        .bind(DependencyKind::Schema.as_str())
        .fetch_all(&mut *conn)
        .await?;
        targets.extend(
            recorded
                .into_iter()
                .filter(|target| target.starts_with(&prefix)),
        );
        let recorded: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT target FROM model_dependencies WHERE kind = ?")
                .bind(DependencyKind::Schema.as_str())
                .fetch_all(&mut *conn)
                .await?;
        targets.extend(
            recorded
                .into_iter()
                .filter(|target| target.starts_with(&prefix)),
        );
    }
    let mut summary = InvalidationSummary {
        projections: 0,
        family_heads: 0,
        target_eligibilities: 0,
        bundles: 0,
        reports: 0,
    };
    for target in targets {
        let dependency =
            DependencyRef::required(DependencyKind::Schema, DependencyTarget::new(target)?);
        summary.merge(invalidate(conn, &dependency, reason, now).await?);
    }
    Ok(summary)
}

async fn bump_dependency_generation(
    conn: &mut SqliteConnection,
    kind: DependencyKind,
    target: &DependencyTarget,
    reason: InvalidationReason,
    now: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO model_dependency_generations
         (kind, target, generation, reason, updated_at)
         VALUES (?, ?, 1, ?, ?)
         ON CONFLICT (kind, target) DO UPDATE
         SET generation = model_dependency_generations.generation + 1,
             reason = excluded.reason,
             updated_at = excluded.updated_at",
    )
    .bind(kind.as_str())
    .bind(target.as_str())
    .bind(reason.as_str())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn dependent_owners(
    conn: &mut SqliteConnection,
    kind: DependencyKind,
    target: &DependencyTarget,
) -> Result<Vec<(String, String)>> {
    let counterpart = match kind {
        DependencyKind::FamilyVersion => Some(DependencyKind::PublishedTarget),
        DependencyKind::PublishedTarget => Some(DependencyKind::FamilyVersion),
        _ => None,
    };
    let rows = if let Some(counterpart) = counterpart {
        sqlx::query(
            "SELECT owner_kind, owner_id FROM model_dependencies
             WHERE required = 1
               AND ((kind = ? AND target = ?) OR (kind = ? AND target = ?))",
        )
        .bind(kind.as_str())
        .bind(target.as_str())
        .bind(counterpart.as_str())
        .bind(target.as_str())
        .fetch_all(&mut *conn)
        .await?
    } else {
        sqlx::query(
            "SELECT owner_kind, owner_id FROM model_dependencies
             WHERE kind = ? AND target = ? AND required = 1",
        )
        .bind(kind.as_str())
        .bind(target.as_str())
        .fetch_all(&mut *conn)
        .await?
    };
    rows.iter()
        .map(|row| Ok((row.try_get("owner_kind")?, row.try_get("owner_id")?)))
        .collect()
}

async fn mark_invalidated(
    conn: &mut SqliteConnection,
    kind: DependencyKind,
    target: &DependencyTarget,
    reason: InvalidationReason,
    now: i64,
) -> Result<InvalidationSummary> {
    let mut summary = InvalidationSummary {
        projections: 0,
        family_heads: 0,
        target_eligibilities: 0,
        bundles: 0,
        reports: 0,
    };
    summary.projections += sqlx::query(
        "UPDATE model_current_projections
         SET applicability = 'invalidated', invalidated_reason = ?, updated_at = ?
         WHERE selected_report_id IN (
                   SELECT owner_id FROM model_dependencies
                   WHERE owner_kind = 'report' AND kind = ? AND target = ? AND required = 1)
            OR (selected_candidate_kind = 'body_realization' AND selected_candidate_id IN (
                   SELECT owner_id FROM model_dependencies
                   WHERE owner_kind = 'realization_revision' AND kind = ? AND target = ?
                     AND required = 1))",
    )
    .bind(reason.as_str())
    .bind(now)
    .bind(kind.as_str())
    .bind(target.as_str())
    .bind(kind.as_str())
    .bind(target.as_str())
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if kind == DependencyKind::Report {
        summary.projections += sqlx::query(
            "UPDATE model_current_projections
             SET applicability = 'invalidated', invalidated_reason = ?, updated_at = ?
             WHERE selected_report_id = ?",
        )
        .bind(reason.as_str())
        .bind(now)
        .bind(target.as_str())
        .execute(&mut *conn)
        .await?
        .rows_affected();
        summary.family_heads += sqlx::query(
            "UPDATE model_family_heads
             SET applicability = 'invalidated', invalidated_reason = ?, updated_at = ?
             WHERE report_id = ?",
        )
        .bind(reason.as_str())
        .bind(now)
        .bind(target.as_str())
        .execute(&mut *conn)
        .await?
        .rows_affected();
    }
    if kind == DependencyKind::RealizationRevision {
        summary.projections += sqlx::query(
            "UPDATE model_current_projections
             SET applicability = 'invalidated', invalidated_reason = ?, updated_at = ?
             WHERE selected_candidate_kind = 'body_realization' AND selected_candidate_id = ?",
        )
        .bind(reason.as_str())
        .bind(now)
        .bind(target.as_str())
        .execute(&mut *conn)
        .await?
        .rows_affected();
    }
    summary.family_heads += sqlx::query(
        "UPDATE model_family_heads
         SET applicability = 'invalidated', invalidated_reason = ?, updated_at = ?
         WHERE report_id IN (
                   SELECT owner_id FROM model_dependencies
                   WHERE owner_kind = 'report' AND kind = ? AND target = ? AND required = 1)
            OR EXISTS (
                   SELECT 1 FROM model_dependencies d
                   WHERE d.owner_kind = 'family_head'
                     AND d.owner_id = model_family_heads.family || '@'
                                      || model_family_heads.head_version
                     AND d.kind = ? AND d.target = ? AND d.required = 1)",
    )
    .bind(reason.as_str())
    .bind(now)
    .bind(kind.as_str())
    .bind(target.as_str())
    .bind(kind.as_str())
    .bind(target.as_str())
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if kind == DependencyKind::FamilyVersion {
        summary.family_heads += sqlx::query(
            "UPDATE model_family_heads
             SET applicability = 'invalidated', invalidated_reason = ?, updated_at = ?
             WHERE family || '@' || head_version = ?",
        )
        .bind(reason.as_str())
        .bind(now)
        .bind(target.as_str())
        .execute(&mut *conn)
        .await?
        .rows_affected();
    }
    let mut snapshots: BTreeSet<String> = BTreeSet::new();
    if kind == DependencyKind::PublishedTarget {
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT snapshot_id FROM model_target_eligibility_current WHERE target = ?",
        )
        .bind(target.as_str())
        .fetch_all(&mut *conn)
        .await?;
        snapshots.extend(rows);
    }
    for (owner_kind, owner_id) in dependent_owners(conn, kind, target).await? {
        if owner_kind == "target_eligibility" {
            snapshots.insert(owner_id);
        }
    }
    for snapshot in snapshots {
        summary.target_eligibilities += mark_eligibility_snapshot(conn, &snapshot, now).await?;
    }
    summary.bundles += sqlx::query(
        "UPDATE model_bundle_current
         SET applicability = 'invalidated', invalidated_reason = ?, updated_at = ?
         WHERE bundle_id IN (
                   SELECT owner_id FROM model_dependencies
                   WHERE owner_kind = 'bundle_revision' AND kind = ? AND target = ?
                     AND required = 1)
            OR bundle_id IN (
                   SELECT r.bundle_id FROM model_bundle_revision_realizations r
                   WHERE r.realization_id IN (
                       SELECT owner_id FROM model_dependencies
                       WHERE owner_kind = 'realization_revision' AND kind = ? AND target = ?
                         AND required = 1))",
    )
    .bind(reason.as_str())
    .bind(now)
    .bind(kind.as_str())
    .bind(target.as_str())
    .bind(kind.as_str())
    .bind(target.as_str())
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if kind == DependencyKind::BundleRevision {
        summary.bundles += sqlx::query(
            "UPDATE model_bundle_current
             SET applicability = 'invalidated', invalidated_reason = ?, updated_at = ?
             WHERE bundle_id = ?",
        )
        .bind(reason.as_str())
        .bind(now)
        .bind(target.as_str())
        .execute(&mut *conn)
        .await?
        .rows_affected();
    }
    let reports: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT owner_id) FROM model_dependencies
         WHERE owner_kind = 'report' AND kind = ? AND target = ? AND required = 1",
    )
    .bind(kind.as_str())
    .bind(target.as_str())
    .fetch_one(&mut *conn)
    .await?;
    summary.reports = u64::try_from(reports).unwrap_or(0);
    Ok(summary)
}

async fn mark_eligibility_snapshot(
    conn: &mut SqliteConnection,
    snapshot: &str,
    now: i64,
) -> Result<u64> {
    let id = EligibilitySnapshotId::parse(snapshot)
        .map_err(|error| StoreError::InvalidInput(error.to_string()))?;
    let Some(mut eligibility) = target_eligibility_snapshot(conn, &id).await? else {
        return Ok(0);
    };
    eligibility.status = TargetEligibilityStatus::Invalidated;
    if !eligibility
        .reasons
        .contains(&TargetEligibilityReason::DependencyInvalidated)
    {
        eligibility
            .reasons
            .push(TargetEligibilityReason::DependencyInvalidated);
    }
    let new_id = insert_eligibility_snapshot(conn, &eligibility, now).await?;
    Ok(sqlx::query(
        "UPDATE model_target_eligibility_current
         SET snapshot_id = ?, updated_at = ?
         WHERE target = ? AND snapshot_id = ?",
    )
    .bind(new_id.to_hex())
    .bind(now)
    .bind(eligibility.target.to_key())
    .bind(snapshot)
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

async fn bump_publication(conn: &mut SqliteConnection) -> Result<()> {
    sqlx::query("UPDATE publication SET generation = generation + 1 WHERE id = 1")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub async fn write_diagnostic_image_stats(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
    stats: &DiagnosticImageStats,
    now: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO model_diagnostic_image_stats
         (subject_key, image_id, rms_de, z, verdict, reason, weight_components, computed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (subject_key, image_id) DO UPDATE
         SET rms_de = excluded.rms_de,
             z = excluded.z,
             verdict = excluded.verdict,
             reason = excluded.reason,
             weight_components = excluded.weight_components,
             computed_at = excluded.computed_at",
    )
    .bind(subject.as_str())
    .bind(stats.image.as_str())
    .bind(stats.rms_de)
    .bind(stats.z)
    .bind(stats.verdict.as_str())
    .bind(stats.reason.as_deref())
    .bind(serde_json::to_string(&stats.weight_components)?)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub async fn diagnostic_image_stats(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
) -> Result<Vec<DiagnosticImageStats>> {
    let rows = sqlx::query(
        "SELECT image_id, rms_de, z, verdict, reason, weight_components
         FROM model_diagnostic_image_stats WHERE subject_key = ? ORDER BY image_id",
    )
    .bind(subject.as_str())
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(DiagnosticImageStats {
                image: ImageRef::new(row.try_get::<String, _>("image_id")?)?,
                rms_de: row.try_get("rms_de")?,
                z: row.try_get("z")?,
                verdict: DiagnosticVerdict::parse(row.try_get::<String, _>("verdict")?.as_str())?,
                reason: row.try_get("reason")?,
                weight_components: serde_json::from_str(
                    &row.try_get::<String, _>("weight_components")?,
                )?,
            })
        })
        .collect()
}

pub async fn clear_diagnostic_image_stats(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
) -> Result<()> {
    sqlx::query("DELETE FROM model_diagnostic_image_stats WHERE subject_key = ?")
        .bind(subject.as_str())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

fn candidate_columns(candidate: &ModelVersionId) -> (String, String, String) {
    match candidate {
        ModelVersionId::SharedFamily { target } => (
            ModelVersionKind::SharedFamily.as_str().to_owned(),
            target.to_key(),
            target.to_key(),
        ),
        ModelVersionId::BodyRealization { realization } => (
            ModelVersionKind::BodyRealization.as_str().to_owned(),
            RealizationRevisionId::for_version(realization).to_string(),
            realization.target.to_key(),
        ),
    }
}

pub async fn write_qualified_image_stats(
    conn: &mut SqliteConnection,
    write: &QualifiedImageWrite<'_>,
    now: i64,
) -> Result<QualifiedImageOutcome> {
    if !lease_valid(conn, write.lease).await? {
        return Ok(QualifiedImageOutcome::Rejected(
            QualifiedImageRejection::LeaseLost,
        ));
    }
    if !sources_current(conn, write.sources).await? {
        return Ok(QualifiedImageOutcome::Rejected(
            QualifiedImageRejection::SupersededSources,
        ));
    }
    let Some(stored) = report(conn, write.report).await? else {
        return Ok(QualifiedImageOutcome::Rejected(
            QualifiedImageRejection::UnknownReport(*write.report),
        ));
    };
    if !matches!(stored.report.outcome, AssessmentOutcome::Pass) {
        return Ok(QualifiedImageOutcome::Rejected(
            QualifiedImageRejection::ReportNotPass(stored.report.outcome),
        ));
    }
    if stored.report.subject.key().as_str() != write.subject.as_str() {
        return Ok(QualifiedImageOutcome::Rejected(
            QualifiedImageRejection::SubjectMismatch,
        ));
    }
    if &stored.report.candidate != write.candidate {
        return Ok(QualifiedImageOutcome::Rejected(
            QualifiedImageRejection::CandidateMismatch,
        ));
    }
    if stored.report.scope.key().as_str() != write.scope_key.as_str() {
        return Ok(QualifiedImageOutcome::Rejected(
            QualifiedImageRejection::ScopeMismatch,
        ));
    }
    if !report_applicable(conn, write.report).await? {
        return Ok(QualifiedImageOutcome::Rejected(
            QualifiedImageRejection::ReportInapplicable(*write.report),
        ));
    }
    if let Some(rejection) =
        verify_owner_snapshot(conn, "report", &write.report.to_hex(), write.snapshot).await?
    {
        return Ok(QualifiedImageOutcome::Rejected(match rejection {
            SelectionRejection::DependencyAdvanced(dependency) => {
                QualifiedImageRejection::DependencyAdvanced(dependency)
            }
            SelectionRejection::DependencyUnregistered(dependency) => {
                QualifiedImageRejection::DependencyUnregistered(dependency)
            }
            _ => unreachable!("owner snapshot checks only dependency rejections"),
        }));
    }
    if let ModelVersionId::BodyRealization { realization } = write.candidate {
        let revision = RealizationRevisionId::for_version(realization);
        let Some(stored_revision) = realization_revision(conn, &revision).await? else {
            return Ok(QualifiedImageOutcome::Rejected(
                QualifiedImageRejection::CandidateMismatch,
            ));
        };
        if stored_revision.status != RealizationStatus::Candidate
            || !stored_revision.safety.accepted()
        {
            return Ok(QualifiedImageOutcome::Rejected(
                QualifiedImageRejection::CandidateMismatch,
            ));
        }
        if revision_invalidated(conn, &revision).await? {
            return Ok(QualifiedImageOutcome::Rejected(
                QualifiedImageRejection::Invalidated,
            ));
        }
        if let Some(rejection) = verify_owner_snapshot(
            conn,
            "realization_revision",
            revision.as_str(),
            write.snapshot,
        )
        .await?
        {
            return Ok(QualifiedImageOutcome::Rejected(match rejection {
                SelectionRejection::DependencyAdvanced(dependency) => {
                    QualifiedImageRejection::DependencyAdvanced(dependency)
                }
                SelectionRejection::DependencyUnregistered(dependency) => {
                    QualifiedImageRejection::DependencyUnregistered(dependency)
                }
                SelectionRejection::RevokedDependency(dependency) => {
                    QualifiedImageRejection::RevokedDependency(dependency)
                }
                _ => unreachable!("owner snapshot checks only dependency rejections"),
            }));
        }
    }
    let Some(projection) = current_projection(
        conn,
        write.subject,
        ProjectionUse::QualifiedMatch,
        stored.report.capability,
        write.scope_key,
    )
    .await?
    else {
        return Ok(QualifiedImageOutcome::Rejected(
            QualifiedImageRejection::NotCurrent,
        ));
    };
    if !projection.applicability.is_current()
        || projection.selected_report != Some(*write.report)
        || projection.selected_candidate.as_ref() != Some(write.candidate)
    {
        return Ok(QualifiedImageOutcome::Rejected(
            QualifiedImageRejection::NotCurrent,
        ));
    }
    let (candidate_kind, candidate_id, candidate_target) = candidate_columns(write.candidate);
    sqlx::query(
        "INSERT INTO model_qualified_image_stats
         (subject_key, candidate_kind, candidate_id, candidate_target, scope_key, image_id,
          rms_de, z, verdict, reason, weight_components, report_id, computed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (subject_key, candidate_kind, candidate_id, candidate_target, scope_key,
                      image_id)
         DO UPDATE
         SET rms_de = excluded.rms_de,
             z = excluded.z,
             verdict = excluded.verdict,
             reason = excluded.reason,
             weight_components = excluded.weight_components,
             report_id = excluded.report_id,
             computed_at = excluded.computed_at",
    )
    .bind(write.subject.as_str())
    .bind(candidate_kind)
    .bind(candidate_id)
    .bind(candidate_target)
    .bind(write.scope_key.as_str())
    .bind(write.stats.image.as_str())
    .bind(write.stats.rms_de)
    .bind(write.stats.z)
    .bind(write.stats.verdict.as_str())
    .bind(write.stats.reason.as_deref())
    .bind(serde_json::to_string(&write.stats.weight_components)?)
    .bind(write.report.to_hex())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(QualifiedImageOutcome::Written)
}

pub async fn write_qualified_image_stats_fenced(
    pool: &SqlitePool,
    write: &QualifiedImageWrite<'_>,
    now: i64,
) -> Result<QualifiedImageOutcome> {
    let mut tx = begin_immediate(pool).await?;
    let outcome = write_qualified_image_stats(&mut tx, write, now).await?;
    tx.commit().await?;
    Ok(outcome)
}

pub async fn qualified_image_stats(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
    candidate: &ModelVersionId,
    scope_key: &ScopeKey,
) -> Result<Vec<QualifiedImageStats>> {
    let (candidate_kind, candidate_id, candidate_target) = candidate_columns(candidate);
    let rows = sqlx::query(
        "SELECT image_id, rms_de, z, verdict, reason, weight_components
         FROM model_qualified_image_stats
         WHERE subject_key = ? AND candidate_kind = ? AND candidate_id = ?
           AND candidate_target = ? AND scope_key = ?
         ORDER BY image_id",
    )
    .bind(subject.as_str())
    .bind(candidate_kind)
    .bind(candidate_id)
    .bind(candidate_target)
    .bind(scope_key.as_str())
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(QualifiedImageStats {
                image: ImageRef::new(row.try_get::<String, _>("image_id")?)?,
                rms_de: row.try_get("rms_de")?,
                z: row.try_get("z")?,
                verdict: DiagnosticVerdict::parse(row.try_get::<String, _>("verdict")?.as_str())?,
                reason: row.try_get("reason")?,
                weight_components: serde_json::from_str(
                    &row.try_get::<String, _>("weight_components")?,
                )?,
            })
        })
        .collect()
}

pub async fn clear_qualified_image_stats(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
    candidate: &ModelVersionId,
    scope_key: &ScopeKey,
) -> Result<()> {
    let (candidate_kind, candidate_id, candidate_target) = candidate_columns(candidate);
    sqlx::query(
        "DELETE FROM model_qualified_image_stats
         WHERE subject_key = ? AND candidate_kind = ? AND candidate_id = ?
           AND candidate_target = ? AND scope_key = ?",
    )
    .bind(subject.as_str())
    .bind(candidate_kind)
    .bind(candidate_id)
    .bind(candidate_target)
    .bind(scope_key.as_str())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub async fn subjects(conn: &mut SqliteConnection) -> Result<Vec<ModelSubject>> {
    let keys: Vec<String> = sqlx::query_scalar("SELECT key FROM model_subjects ORDER BY key")
        .fetch_all(&mut *conn)
        .await?;
    keys.iter().map(|key| parse_subject(key)).collect()
}

pub async fn family_heads(conn: &mut SqliteConnection) -> Result<Vec<FamilyHead>> {
    let families: Vec<String> =
        sqlx::query_scalar("SELECT family FROM model_family_heads ORDER BY family")
            .fetch_all(&mut *conn)
            .await?;
    let mut heads = Vec::new();
    for family in families {
        if let Some(head) = family_head(conn, &look_id(&family)?).await? {
            heads.push(head);
        }
    }
    Ok(heads)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptedFamilyVersionStatus {
    Provisional,
    Published,
    Rooted,
    Deprecated,
    Invalidated(InvalidationReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedFamilyVersion {
    pub target: FamilyTargetId,
    pub accepted_at: i64,
    pub status: AcceptedFamilyVersionStatus,
}

/// Family versions accepted by a successful seed, advance or moderator revert.
pub async fn accepted_family_versions(
    conn: &mut SqliteConnection,
    family: Option<&LookId>,
) -> Result<Vec<AcceptedFamilyVersion>> {
    let rows = sqlx::query(
        "SELECT s.family AS family, r.revision AS version, r.recorded_at AS accepted_at,
                v.status AS look_status,
                h.applicability AS head_applicability,
                h.invalidated_reason AS head_reason,
                d.reason AS revoked_reason
         FROM model_subject_revisions r
         JOIN model_subjects s ON s.key = r.subject_key
         JOIN look_versions v ON v.look = s.family AND v.version = r.revision
         LEFT JOIN model_family_heads h
                ON h.family = s.family AND h.head_version = r.revision
         LEFT JOIN model_revoked_dependencies d
                ON d.kind = 'family_version' AND d.target = s.family || '@' || r.revision
         WHERE s.kind = 'shared_family' AND r.role = 'target'
           AND (?1 IS NULL OR s.family = ?1)
         ORDER BY s.family, r.revision",
    )
    .bind(family.map(LookId::as_str))
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|row| {
            let family = look_id(&row.try_get::<String, _>("family")?)?;
            let version = u32::try_from(row.try_get::<i64, _>("version")?)
                .ok()
                .and_then(TargetVersion::new)
                .ok_or_else(|| StoreError::InvalidInput("invalid accepted version".to_owned()))?;
            let head_applicability: Option<String> = row.try_get("head_applicability")?;
            let head_reason: Option<String> = row.try_get("head_reason")?;
            let revoked_reason: Option<String> = row.try_get("revoked_reason")?;
            let head_applicability = head_applicability
                .map(|applicability| parse_applicability(&applicability, head_reason.as_deref()))
                .transpose()?;
            let status = if let Some(Applicability::Invalidated { reason }) = head_applicability {
                AcceptedFamilyVersionStatus::Invalidated(reason)
            } else if let Some(reason) = revoked_reason {
                AcceptedFamilyVersionStatus::Invalidated(InvalidationReason::parse(&reason)?)
            } else {
                match row.try_get::<String, _>("look_status")?.as_str() {
                    "provisional" => AcceptedFamilyVersionStatus::Provisional,
                    "published" => AcceptedFamilyVersionStatus::Published,
                    "rooted" => AcceptedFamilyVersionStatus::Rooted,
                    "deprecated" => AcceptedFamilyVersionStatus::Deprecated,
                    other => {
                        return Err(StoreError::InvalidInput(format!(
                            "unknown look version status `{other}`"
                        )));
                    }
                }
            };
            Ok(AcceptedFamilyVersion {
                target: FamilyTargetId { family, version },
                accepted_at: row.try_get("accepted_at")?,
                status,
            })
        })
        .collect()
}

pub async fn accepted_family_version(
    conn: &mut SqliteConnection,
    family: &LookId,
    version: TargetVersion,
) -> Result<Option<AcceptedFamilyVersion>> {
    Ok(accepted_family_versions(conn, Some(family))
        .await?
        .into_iter()
        .find(|accepted| accepted.target.version == version))
}

pub async fn current_projections_all(
    conn: &mut SqliteConnection,
) -> Result<Vec<looklab_schema::CurrentProjection>> {
    let rows = sqlx::query(
        "SELECT subject_key, \"use\", capability, scope_key, selected_report_id,
                selected_candidate_kind, selected_candidate_id, selected_candidate_target,
                selected_candidate_version, pending_generation, applicability,
                invalidated_reason
         FROM model_current_projections
         ORDER BY subject_key, \"use\", capability, scope_key",
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.iter().map(projection_from_row).collect()
}

pub async fn report_ids(conn: &mut SqliteConnection) -> Result<Vec<ModelReportId>> {
    let ids: Vec<String> =
        sqlx::query_scalar("SELECT id FROM model_reports ORDER BY created_at, id")
            .fetch_all(&mut *conn)
            .await?;
    ids.iter().map(|id| report_id(id)).collect()
}

pub async fn reports(conn: &mut SqliteConnection) -> Result<Vec<StoredReport>> {
    let mut reports = Vec::new();
    for id in report_ids(conn).await? {
        if let Some(stored) = report(conn, &id).await? {
            reports.push(stored);
        }
    }
    Ok(reports)
}

pub async fn realization_revisions(
    conn: &mut SqliteConnection,
    subject: Option<&SubjectKey>,
) -> Result<Vec<StoredRealizationRevision>> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM model_realization_revisions
         WHERE (? IS NULL OR subject_key = ?)
         ORDER BY subject_key, version",
    )
    .bind(subject.map(SubjectKey::as_str))
    .bind(subject.map(SubjectKey::as_str))
    .fetch_all(&mut *conn)
    .await?;
    let mut revisions = Vec::new();
    for id in rows {
        let id = RealizationRevisionId::new(id)?;
        if let Some(revision) = realization_revision(conn, &id).await? {
            revisions.push(revision);
        }
    }
    Ok(revisions)
}

pub async fn legacy_scopes(conn: &mut SqliteConnection) -> Result<Vec<LegacyScope>> {
    let rows = sqlx::query(
        "SELECT kind, target, family, body, reason FROM model_legacy_scopes
         ORDER BY kind, target",
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|row| {
            let family: Option<String> = row.try_get("family")?;
            let body: Option<String> = row.try_get("body")?;
            Ok(LegacyScope {
                kind: LegacyScopeKind::parse(row.try_get::<String, _>("kind")?.as_str())?,
                target: LegacyTarget::new(row.try_get::<String, _>("target")?)?,
                family: family.map(|value| look_id(&value)).transpose()?,
                body: body.map(|value| body_id(&value)).transpose()?,
                applicability: Applicability::Unassessed,
                reason: LegacyScopeReason::parse(row.try_get::<String, _>("reason")?.as_str())?,
            })
        })
        .collect()
}

pub async fn insert_bundle_revision(
    conn: &mut SqliteConnection,
    bundle: &NewBundleRevision<'_>,
    now: i64,
) -> Result<MatchBundleRevisionId> {
    let revision = MatchBundleRevision::parse_manifest(bundle.manifest, now)?;
    if revision.body != *bundle.body {
        return Err(StoreError::InvalidInput(
            "bundle manifest body does not match the requested body".to_owned(),
        ));
    }
    if revision.epoch.get() != bundle.epoch {
        return Err(StoreError::InvalidInput(
            "bundle manifest epoch does not match the requested epoch".to_owned(),
        ));
    }
    if body_state(conn, &revision.body, revision.body_state_version)
        .await?
        .is_none()
    {
        return Err(StoreError::UnknownBodyState {
            body: revision.body.clone(),
            version: revision.body_state_version,
        });
    }
    let mut sims = BTreeSet::new();
    let Some(bundle_body_state) =
        body_state(conn, &revision.body, revision.body_state_version).await?
    else {
        return Err(StoreError::UnknownBodyState {
            body: revision.body.clone(),
            version: revision.body_state_version,
        });
    };
    for sim in &revision.sims {
        if !sims.insert(sim.native_sim.as_str().to_owned()) {
            return Err(StoreError::InvalidInput(
                "a bundle revision lists one entry per native sim".to_owned(),
            ));
        }
        let target_exists: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM look_versions WHERE look = ? AND version = ?")
                .bind(sim.family_version.family.as_str())
                .bind(i64::from(sim.family_version.version.get()))
                .fetch_one(&mut *conn)
                .await?;
        if target_exists == 0 {
            return Err(StoreError::UnknownFamilyTarget(sim.family_version.clone()));
        }
        let Some(realization) = realization_revision(conn, &sim.realization).await? else {
            return Err(StoreError::InvalidInput(format!(
                "unknown realization revision {}",
                sim.realization
            )));
        };
        let artifact = |role: ArtifactRole| {
            realization
                .artifacts
                .iter()
                .find(|artifact| artifact.role == role)
                .map(|artifact| artifact.sha256)
        };
        if realization.subject.body != revision.body
            || realization.subject.epoch.get() != revision.epoch.get()
            || realization.subject.native_sim != sim.native_sim
            || realization.target != sim.family_version
            || realization.body_state_version != revision.body_state_version
            || realization.control_response_version != sim.control_response_version
            || realization.realization_params_version != sim.realization_params_version
            || realization.clip_k != sim.clip_k
            || artifact(ArtifactRole::Look) != Some(sim.base_artifact)
            || artifact(ArtifactRole::ControlResponse) != Some(sim.control_response)
            || artifact(ArtifactRole::Covariance) != sim.covariance
            || artifact(ArtifactRole::RealizationParams) != sim.realization_params
            || sim.body_params != Some(bundle_body_state.pins.body_params.sha256)
            || realization.status != RealizationStatus::Candidate
            || !realization.safety.accepted()
        {
            return Err(StoreError::InvalidInput(format!(
                "bundle sim {} does not match its realization revision and body state",
                sim.native_sim
            )));
        }
    }
    let id = revision.id;
    let inserted = sqlx::query(
        "INSERT INTO model_bundle_revisions (id, body, epoch, manifest, created_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(id.to_hex())
    .bind(bundle.body.as_str())
    .bind(i64::from(bundle.epoch))
    .bind(bundle.manifest)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    for artifact in bundle.artifacts {
        insert_artifact(conn, "bundle_revision", &id.to_hex(), *artifact, now).await?;
    }
    let mut edges = bundle.dependencies.to_vec();
    let body_dependency = DependencyRef::required(
        DependencyKind::BodyState,
        DependencyTarget::body_state(&revision.body, revision.body_state_version),
    );
    if !edges.contains(&body_dependency) {
        edges.push(body_dependency);
    }
    for sim in &revision.sims {
        let dependency = DependencyRef::required(
            DependencyKind::RealizationRevision,
            DependencyTarget::realization_revision(&sim.realization),
        );
        if !edges.contains(&dependency) {
            edges.push(dependency);
        }
    }
    insert_dependency_edges(conn, "bundle_revision", &id.to_hex(), &edges, now).await?;
    for sim in &revision.sims {
        sqlx::query(
            "INSERT INTO model_bundle_revision_realizations (bundle_id, realization_id)
             VALUES (?, ?)
             ON CONFLICT DO NOTHING",
        )
        .bind(id.to_hex())
        .bind(sim.realization.as_str())
        .execute(&mut *conn)
        .await?;
    }
    if inserted > 0 {
        bump_publication(conn).await?;
    }
    Ok(id)
}

pub async fn bundle_revision(
    conn: &mut SqliteConnection,
    id: &MatchBundleRevisionId,
) -> Result<Option<StoredBundleRevision>> {
    let row = sqlx::query("SELECT manifest, created_at FROM model_bundle_revisions WHERE id = ?")
        .bind(id.to_hex())
        .fetch_optional(&mut *conn)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let manifest: String = row.try_get("manifest")?;
    let created_at: i64 = row.try_get("created_at")?;
    let revision = MatchBundleRevision::parse_manifest(&manifest, created_at)?;
    if revision.id != *id {
        return Err(StoreError::InvalidInput(
            "bundle manifest bytes do not hash to their id".to_owned(),
        ));
    }
    Ok(Some(StoredBundleRevision {
        id: *id,
        revision,
        manifest,
        artifacts: public_artifacts(conn, "bundle_revision", &id.to_hex()).await?,
        dependencies: dependencies(conn, "bundle_revision", &id.to_hex()).await?,
    }))
}

pub async fn bundle_revisions(conn: &mut SqliteConnection) -> Result<Vec<StoredBundleRevision>> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM model_bundle_revisions ORDER BY body, epoch, created_at",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut revisions = Vec::new();
    for id in ids {
        let id = MatchBundleRevisionId::parse(&id)
            .map_err(|error| StoreError::InvalidInput(error.to_string()))?;
        if let Some(revision) = bundle_revision(conn, &id).await? {
            revisions.push(revision);
        }
    }
    Ok(revisions)
}

pub async fn current_bundles(conn: &mut SqliteConnection) -> Result<Vec<CurrentBundle>> {
    let rows = sqlx::query(
        "SELECT body, epoch, \"use\", bundle_id, applicability, invalidated_reason
         FROM model_bundle_current ORDER BY body, epoch, \"use\"",
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|row| {
            let epoch: i64 = row.try_get("epoch")?;
            Ok(CurrentBundle {
                body: body_id(&row.try_get::<String, _>("body")?)?,
                epoch: u32::try_from(epoch).unwrap_or(0),
                use_: ProjectionUse::parse(row.try_get::<String, _>("use")?.as_str())?,
                bundle: MatchBundleRevisionId::parse(&row.try_get::<String, _>("bundle_id")?)
                    .map_err(|error| StoreError::InvalidInput(error.to_string()))?,
                applicability: parse_applicability(
                    row.try_get::<String, _>("applicability")?.as_str(),
                    row.try_get::<Option<String>, _>("invalidated_reason")?
                        .as_deref(),
                )?,
            })
        })
        .collect()
}

pub async fn select_bundle(
    conn: &mut SqliteConnection,
    request: &BundleSelection<'_>,
    now: i64,
) -> Result<BundleSelectionOutcome> {
    if !lease_valid(conn, request.lease).await? {
        return Ok(BundleSelectionOutcome::Rejected(
            BundleSelectionRejection::LeaseLost,
        ));
    }
    if !sources_current(conn, request.sources).await? {
        return Ok(BundleSelectionOutcome::Rejected(
            BundleSelectionRejection::SupersededSources,
        ));
    }
    if !matches!(
        request.use_,
        ProjectionUse::QualifiedMatch | ProjectionUse::ExperimentalMatch
    ) {
        return Ok(BundleSelectionOutcome::Rejected(
            BundleSelectionRejection::UseNotSelectable(request.use_),
        ));
    }
    let Some(stored) = bundle_revision(conn, request.id).await? else {
        return Ok(BundleSelectionOutcome::Rejected(
            BundleSelectionRejection::UnknownBundle(*request.id),
        ));
    };
    if bundle_invalidated(conn, request.id).await? {
        return Ok(BundleSelectionOutcome::Rejected(
            BundleSelectionRejection::Invalidated(*request.id),
        ));
    }
    if stored.revision.body != *request.body {
        return Ok(BundleSelectionOutcome::Rejected(
            BundleSelectionRejection::BodyMismatch {
                expected: request.body.clone(),
                actual: stored.revision.body,
            },
        ));
    }
    if stored.revision.epoch.get() != request.epoch {
        return Ok(BundleSelectionOutcome::Rejected(
            BundleSelectionRejection::EpochMismatch {
                expected: request.epoch,
                actual: stored.revision.epoch.get(),
            },
        ));
    }
    if let Some(rejection) = verify_owner_snapshot(
        conn,
        "bundle_revision",
        &request.id.to_hex(),
        request.snapshot,
    )
    .await?
    {
        return Ok(BundleSelectionOutcome::Rejected(match rejection {
            SelectionRejection::DependencyAdvanced(dependency) => {
                BundleSelectionRejection::DependencyAdvanced(dependency)
            }
            SelectionRejection::DependencyUnregistered(dependency) => {
                BundleSelectionRejection::DependencyUnregistered(dependency)
            }
            SelectionRejection::RevokedDependency(dependency) => {
                BundleSelectionRejection::RevokedDependency(dependency)
            }
            _ => unreachable!("owner snapshot checks only dependency rejections"),
        }));
    }
    let current: Option<String> = sqlx::query_scalar(
        "SELECT bundle_id FROM model_bundle_current
         WHERE body = ? AND epoch = ? AND \"use\" = ?",
    )
    .bind(request.body.as_str())
    .bind(i64::from(request.epoch))
    .bind(request.use_.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    let selected = match (current.as_deref(), request.expected) {
        (None, None) => {
            let inserted = sqlx::query(
                "INSERT INTO model_bundle_current
                 (body, epoch, \"use\", bundle_id, applicability, invalidated_reason, updated_at)
                 VALUES (?, ?, ?, ?, 'current', NULL, ?)
                 ON CONFLICT DO NOTHING",
            )
            .bind(request.body.as_str())
            .bind(i64::from(request.epoch))
            .bind(request.use_.as_str())
            .bind(request.id.to_hex())
            .bind(now)
            .execute(&mut *conn)
            .await?
            .rows_affected();
            inserted == 1
        }
        (Some(current), Some(expected)) if current == expected.to_hex() => {
            let updated = sqlx::query(
                "UPDATE model_bundle_current
                 SET bundle_id = ?, applicability = 'current', invalidated_reason = NULL,
                     updated_at = ?
                 WHERE body = ? AND epoch = ? AND \"use\" = ? AND bundle_id IS ?",
            )
            .bind(request.id.to_hex())
            .bind(now)
            .bind(request.body.as_str())
            .bind(i64::from(request.epoch))
            .bind(request.use_.as_str())
            .bind(current)
            .execute(&mut *conn)
            .await?
            .rows_affected();
            updated == 1
        }
        _ => false,
    };
    if selected {
        bump_publication(conn).await?;
        return Ok(BundleSelectionOutcome::Selected);
    }
    let actual = current
        .map(|value| MatchBundleRevisionId::parse(&value))
        .transpose()
        .map_err(|error| StoreError::InvalidInput(error.to_string()))?;
    Ok(BundleSelectionOutcome::Stale { actual })
}

pub async fn record_evidence_request(
    conn: &mut SqliteConnection,
    report: &ModelReportId,
    subject: &SubjectKey,
    capability: QualificationCapability,
    request: &EvidenceRequest,
    now: i64,
) -> Result<()> {
    let inserted = sqlx::query(
        "INSERT INTO model_evidence_bounties
         (report_id, subject_key, capability, request_kind, detail, created_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT DO NOTHING",
    )
    .bind(report.to_hex())
    .bind(subject.as_str())
    .bind(capability.as_str())
    .bind(request.kind.as_str())
    .bind(&request.detail)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if inserted > 0 {
        bump_publication(conn).await?;
    }
    Ok(())
}

pub async fn record_active_evidence_request(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
    capability: QualificationCapability,
    request: &EvidenceRequest,
    observed_groups: Option<u32>,
    now: i64,
) -> Result<()> {
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM model_subjects WHERE key = ?")
        .bind(subject.as_str())
        .fetch_one(&mut *conn)
        .await?;
    if exists == 0 {
        return Err(StoreError::UnknownSubject(subject.clone()));
    }
    let observed = observed_groups.map(i64::from);
    let previous: Option<(String, Option<i64>)> = sqlx::query_as(
        "SELECT detail, observed_groups FROM model_active_evidence_requests
         WHERE subject_key = ? AND capability = ? AND request_kind = ?",
    )
    .bind(subject.as_str())
    .bind(capability.as_str())
    .bind(request.kind.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    let changed = previous.as_ref() != Some(&(request.detail.clone(), observed));
    sqlx::query(
        "INSERT INTO model_active_evidence_requests
         (subject_key, capability, request_kind, detail, observed_groups, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (subject_key, capability, request_kind) DO UPDATE
         SET detail = excluded.detail, observed_groups = excluded.observed_groups,
             updated_at = excluded.updated_at",
    )
    .bind(subject.as_str())
    .bind(capability.as_str())
    .bind(request.kind.as_str())
    .bind(&request.detail)
    .bind(observed)
    .bind(now)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    if changed {
        bump_publication(conn).await?;
    }
    Ok(())
}

pub async fn resolve_active_evidence_request(
    conn: &mut SqliteConnection,
    subject: &SubjectKey,
    kind: EvidenceRequestKind,
) -> Result<bool> {
    let deleted = sqlx::query(
        "DELETE FROM model_active_evidence_requests
         WHERE subject_key = ? AND request_kind = ?",
    )
    .bind(subject.as_str())
    .bind(kind.as_str())
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if deleted > 0 {
        bump_publication(conn).await?;
    }
    Ok(deleted > 0)
}

pub async fn evidence_requests(conn: &mut SqliteConnection) -> Result<Vec<StoredEvidenceRequest>> {
    let report_rows = sqlx::query(
        "SELECT report_id, subject_key, capability, request_kind, detail, created_at
         FROM model_evidence_bounties ORDER BY created_at, report_id, request_kind",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut requests: Vec<StoredEvidenceRequest> = Vec::new();
    for row in &report_rows {
        let report = report_id(&row.try_get::<String, _>("report_id")?)?;
        if !report_applicable(conn, &report).await? {
            continue;
        }
        let (subject_key, subject) = stored_subject(&row.try_get::<String, _>("subject_key")?)?;
        requests.push(StoredEvidenceRequest {
            origin: EvidenceRequestOrigin::Report(report),
            subject,
            subject_key,
            capability: QualificationCapability::parse(
                row.try_get::<String, _>("capability")?.as_str(),
            )?,
            request: EvidenceRequest {
                kind: EvidenceRequestKind::parse(
                    row.try_get::<String, _>("request_kind")?.as_str(),
                )?,
                detail: row.try_get("detail")?,
            },
            observed_groups: None,
            created_at: row.try_get("created_at")?,
        });
    }
    let active_rows = sqlx::query(
        "SELECT subject_key, capability, request_kind, detail, observed_groups, created_at
         FROM model_active_evidence_requests ORDER BY created_at, subject_key, request_kind",
    )
    .fetch_all(&mut *conn)
    .await?;
    requests.extend(
        active_rows
            .iter()
            .map(|row| {
                let (subject_key, subject) =
                    stored_subject(&row.try_get::<String, _>("subject_key")?)?;
                Ok(StoredEvidenceRequest {
                    origin: EvidenceRequestOrigin::PreCandidate,
                    subject,
                    subject_key,
                    capability: QualificationCapability::parse(
                        row.try_get::<String, _>("capability")?.as_str(),
                    )?,
                    request: EvidenceRequest {
                        kind: EvidenceRequestKind::parse(
                            row.try_get::<String, _>("request_kind")?.as_str(),
                        )?,
                        detail: row.try_get("detail")?,
                    },
                    observed_groups: row
                        .try_get::<Option<i64>, _>("observed_groups")?
                        .and_then(|groups| u32::try_from(groups).ok()),
                    created_at: row.try_get("created_at")?,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    );
    requests.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.subject_key.as_str().cmp(right.subject_key.as_str()))
            .then_with(|| left.request.kind.as_str().cmp(right.request.kind.as_str()))
    });
    Ok(requests)
}

pub async fn prune_evidence_requests(
    conn: &mut SqliteConnection,
    report: &ModelReportId,
) -> Result<()> {
    let deleted = sqlx::query("DELETE FROM model_evidence_bounties WHERE report_id = ?")
        .bind(report.to_hex())
        .execute(&mut *conn)
        .await?
        .rows_affected();
    if deleted > 0 {
        bump_publication(conn).await?;
    }
    Ok(())
}
