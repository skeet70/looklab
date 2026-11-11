// SPDX-License-Identifier: AGPL-3.0-or-later
//! Scoped model qualification records (docs/03 §1, docs/model-qualification-plan.md §8).

use core::fmt;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ids::{BodyId, ControlKey, EnumValue, IdError, LookId, SimKey};
use crate::match_params::{FirmwareId, WbModeKey};
use crate::model_subject::{
    BodyRealizationId, BodyRealizationVersionId, BodyStateVersion, ControlEpochId, FamilyTargetId,
    ModelSubject, ModelVersionId, PublishedTargetId,
};
use crate::proposal::Sha256Hex;

const MAX_TEXT_ID_LEN: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QualificationValueError {
    #[error("unknown {field} `{value}`")]
    Unknown { field: &'static str, value: String },
    #[error("`{value}` is not a valid {field}")]
    Invalid { field: &'static str, value: String },
    #[error("identifier: {0}")]
    Identifier(#[from] IdError),
    #[error("subject key: {0}")]
    SubjectKey(#[from] SubjectKeyError),
}

macro_rules! text_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// # Errors
            /// Empty, whitespace-bearing or over-long text.
            pub fn new(value: impl Into<String>) -> Result<Self, QualificationValueError> {
                let value = value.into();
                if value.is_empty()
                    || value.len() > MAX_TEXT_ID_LEN
                    || value.chars().any(char::is_whitespace)
                {
                    return Err(QualificationValueError::Invalid {
                        field: stringify!($name),
                        value,
                    });
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = QualificationValueError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = QualificationValueError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

macro_rules! closed_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            /// # Errors
            /// A value outside the closed set.
            pub fn parse(value: &str) -> Result<Self, QualificationValueError> {
                match value {
                    $($text => Ok(Self::$variant),)+
                    other => Err(QualificationValueError::Unknown {
                        field: stringify!($name),
                        value: other.to_owned(),
                    }),
                }
            }
        }
    };
}

text_id!(
    /// A named colour, exposure or probe region of an assessed domain.
    CellId
);
text_id!(
    /// The name of an assessed domain, for example `supported-probes`.
    DomainId
);
text_id!(
    /// A versioned qualification rule, for example `native-prediction/1`.
    RuleRevision
);
text_id!(
    /// A probe-set identity.
    ProbeSetId
);
text_id!(
    /// A fitter, evaluator or model source identity.
    SourceRef
);
text_id!(
    /// An approved calibration profile id.
    ProfileId
);
text_id!(
    /// A cohort revision identity.
    CohortId
);
text_id!(
    /// An exposure-gauge identity.
    GaugeId
);
text_id!(
    /// An identity-resolution snapshot identity.
    IdentityResolutionRef
);
text_id!(
    /// A contributor standing or weighting snapshot identity.
    StandingRef
);
text_id!(RootWeightingRef);

impl RootWeightingRef {
    pub fn global() -> Self {
        Self("global".to_owned())
    }

    pub fn body(body: &BodyId) -> Self {
        Self(format!("body:{body}"))
    }

    pub fn family(family: &LookId) -> Self {
        Self(format!("family:{family}"))
    }
}
text_id!(ContributionRef);
text_id!(ImageRef);
text_id!(ContributorRef);
text_id!(RootAdmissionRef);
text_id!(LegacyTarget);

/// A positive cohort revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CohortRevision(core::num::NonZeroU32);

impl CohortRevision {
    pub const fn new(value: u32) -> Option<Self> {
        match core::num::NonZeroU32::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u32 {
        self.0.get()
    }

    pub const fn checked_next(self) -> Option<Self> {
        match self.get().checked_add(1) {
            Some(value) => Self::new(value),
            None => None,
        }
    }
}

/// A model candidate's kind, matching [`ModelVersionId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelVersionKind {
    SharedFamily,
    BodyRealization,
}

impl ModelVersionKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SharedFamily => "shared_family",
            Self::BodyRealization => "body_realization",
        }
    }

    /// # Errors
    /// A value outside the closed set.
    pub fn parse(value: &str) -> Result<Self, QualificationValueError> {
        match value {
            "shared_family" => Ok(Self::SharedFamily),
            "body_realization" => Ok(Self::BodyRealization),
            other => Err(QualificationValueError::Unknown {
                field: "ModelVersionKind",
                value: other.to_owned(),
            }),
        }
    }
}

impl ModelVersionId {
    pub const fn kind(&self) -> ModelVersionKind {
        match self {
            Self::SharedFamily { .. } => ModelVersionKind::SharedFamily,
            Self::BodyRealization { .. } => ModelVersionKind::BodyRealization,
        }
    }

    /// `look@version` for a family target, the revision id for a realization.
    pub fn candidate_id(&self) -> String {
        match self {
            Self::SharedFamily { target } => family_target_id(target),
            Self::BodyRealization { realization } => {
                RealizationRevisionId::for_version(realization).to_string()
            }
        }
    }
}

fn family_target_id(target: &FamilyTargetId) -> String {
    format!("{}@{}", target.family, target.version.get())
}

impl FamilyTargetId {
    pub fn to_key(&self) -> String {
        family_target_id(self)
    }

    /// # Errors
    /// Text that is not `look@version` with a positive version.
    pub fn parse(text: &str) -> Result<Self, QualificationValueError> {
        let (family, version) =
            text.split_once('@')
                .ok_or_else(|| QualificationValueError::Invalid {
                    field: "FamilyTargetId",
                    value: text.to_owned(),
                })?;
        let version = version
            .parse::<u32>()
            .ok()
            .and_then(crate::model_subject::TargetVersion::new)
            .ok_or_else(|| QualificationValueError::Invalid {
                field: "FamilyTargetId",
                value: text.to_owned(),
            })?;
        Ok(Self {
            family: LookId::new(family)?,
            version,
        })
    }
}

impl PublishedTargetId {
    pub fn to_key(&self) -> String {
        format!("{}@{}", self.look, self.version.get())
    }

    /// # Errors
    /// Text that is not `look@version` with a positive version.
    pub fn parse(text: &str) -> Result<Self, QualificationValueError> {
        let (look, version) =
            text.split_once('@')
                .ok_or_else(|| QualificationValueError::Invalid {
                    field: "PublishedTargetId",
                    value: text.to_owned(),
                })?;
        let version = version
            .parse::<u32>()
            .ok()
            .and_then(crate::model_subject::TargetVersion::new)
            .ok_or_else(|| QualificationValueError::Invalid {
                field: "PublishedTargetId",
                value: text.to_owned(),
            })?;
        Ok(Self {
            look: LookId::new(look)?,
            version,
        })
    }
}

/// `shared_family:<look>` or `body_realization:<body>/<epoch>/<sim>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubjectKey(String);

impl SubjectKey {
    /// # Errors
    /// Text that is not a canonical subject key.
    pub fn new(value: impl Into<String>) -> Result<Self, QualificationValueError> {
        let key = Self(value.into());
        key.parse().map_err(QualificationValueError::SubjectKey)?;
        Ok(key)
    }

    pub fn for_family(family: &LookId) -> Self {
        Self(format!("shared_family:{family}"))
    }

    pub fn for_realization(realization: &BodyRealizationId) -> Self {
        Self(format!(
            "body_realization:{}/{}/{}",
            realization.body,
            realization.epoch.get(),
            realization.native_sim
        ))
    }

    pub fn of(subject: &ModelSubject) -> Self {
        match subject {
            ModelSubject::SharedFamily { family } => Self::for_family(family),
            ModelSubject::BodyRealization { realization } => Self::for_realization(realization),
        }
    }

    /// # Errors
    /// An unknown prefix or a component that is not a valid id.
    pub fn parse(&self) -> Result<ModelSubject, SubjectKeyError> {
        let (kind, rest) = self.0.split_once(':').ok_or(SubjectKeyError::Malformed)?;
        match kind {
            "shared_family" => Ok(ModelSubject::SharedFamily {
                family: LookId::new(rest)?,
            }),
            "body_realization" => {
                let mut parts = rest.rsplitn(3, '/');
                let (Some(sim), Some(epoch), Some(body)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    return Err(SubjectKeyError::Malformed);
                };
                let epoch = epoch
                    .parse::<u32>()
                    .map_err(|_| SubjectKeyError::Epoch(epoch.to_owned()))?;
                Ok(ModelSubject::BodyRealization {
                    realization: BodyRealizationId {
                        body: BodyId::new(body)?,
                        epoch: ControlEpochId::new(epoch),
                        native_sim: SimKey::new(sim)?,
                    },
                })
            }
            other => Err(SubjectKeyError::UnknownKind(other.to_owned())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SubjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubjectKeyError {
    #[error("unknown subject kind `{0}`")]
    UnknownKind(String),
    #[error("malformed subject key")]
    Malformed,
    #[error("`{0}` is not a control epoch id")]
    Epoch(String),
    #[error("invalid id: {0}")]
    Id(#[from] IdError),
}

impl ModelSubject {
    pub fn key(&self) -> SubjectKey {
        SubjectKey::of(self)
    }
}

/// An immutable realization revision id. It is not a Look version.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RealizationRevisionId(String);

impl RealizationRevisionId {
    /// # Errors
    /// Empty, whitespace-bearing or over-long text.
    pub fn new(value: impl Into<String>) -> Result<Self, QualificationValueError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_TEXT_ID_LEN
            || value.chars().any(char::is_whitespace)
        {
            return Err(QualificationValueError::Invalid {
                field: "RealizationRevisionId",
                value,
            });
        }
        Ok(Self(value))
    }

    pub fn for_version(id: &BodyRealizationVersionId) -> Self {
        Self(format!(
            "{}@{}",
            SubjectKey::for_realization(&id.realization),
            id.version.get()
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RealizationRevisionId {
    type Error = QualificationValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<RealizationRevisionId> for String {
    fn from(value: RealizationRevisionId) -> Self {
        value.0
    }
}

impl fmt::Display for RealizationRevisionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The digest of a report record and its dependency edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelReportId(Sha256Hex);

impl ModelReportId {
    /// # Errors
    /// Anything but 64 lowercase hex characters.
    pub fn parse(value: &str) -> Result<Self, crate::proposal::Sha256Error> {
        Sha256Hex::parse(value).map(Self)
    }

    pub const fn from_digest(digest: [u8; 32]) -> Self {
        Self(Sha256Hex::from_bytes(digest))
    }

    pub const fn digest(self) -> Sha256Hex {
        self.0
    }

    pub fn to_hex(self) -> String {
        self.0.to_hex()
    }
}

impl fmt::Display for ModelReportId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.to_hex())
    }
}

/// An immutable content digest of a [`TargetEligibility`] snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EligibilitySnapshotId(Sha256Hex);

impl EligibilitySnapshotId {
    /// # Errors
    /// Anything but 64 lowercase hex characters.
    pub fn parse(value: &str) -> Result<Self, crate::proposal::Sha256Error> {
        Sha256Hex::parse(value).map(Self)
    }

    pub const fn from_digest(digest: [u8; 32]) -> Self {
        Self(Sha256Hex::from_bytes(digest))
    }

    pub fn to_hex(self) -> String {
        self.0.to_hex()
    }
}

impl fmt::Display for EligibilitySnapshotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.to_hex())
    }
}

closed_enum!(
    /// Which qualification protocol a report answers.
    QualificationCapability {
        NativePrediction => "native_prediction",
        ControlsMatching => "controls_matching",
        CrossBodyAdmission => "cross_body_admission",
    }
);

closed_enum!(
    /// A report's assessment outcome.
    AssessmentOutcome {
        Pass => "pass",
        InsufficientEvidence => "insufficient_evidence",
        Fail => "fail",
        ExecutionFailure => "execution_failure",
    }
);

closed_enum!(
    /// Which consumer a current projection serves.
    ProjectionUse {
        QualifiedMatch => "qualified_match",
        ExperimentalMatch => "experimental_match",
        Preview => "preview",
    }
);

closed_enum!(
    /// Why a dependency or assertion stopped applying.
    InvalidationReason {
        Withdrawal => "withdrawal",
        Exclusion => "exclusion",
        IdentityMerge => "identity_merge",
        RootRevocation => "root_revocation",
        EvidenceLoss => "evidence_loss",
        TransformReinterpretation => "transform_reinterpretation",
        ProfileReinterpretation => "profile_reinterpretation",
        SchemaReinterpretation => "schema_reinterpretation",
        RuleReinterpretation => "rule_reinterpretation",
        ModelSourceChange => "model_source_change",
        StandingReinterpretation => "standing_reinterpretation",
        CohortReinterpretation => "cohort_reinterpretation",
        GaugeReinterpretation => "gauge_reinterpretation",
        RootWeightingChange => "root_weighting_change",
        Superseded => "superseded",
    }
);

closed_enum!(
    /// What a report or realization revision depends on.
    DependencyKind {
        Contribution => "contribution",
        Image => "image",
        RootAdmission => "root_admission",
        Transform => "transform",
        Profile => "profile",
        Schema => "schema",
        FamilyVersion => "family_version",
        RealizationRevision => "realization_revision",
        RuleVersion => "rule_version",
        IdentityResolution => "identity_resolution",
        Standing => "standing",
        Cohort => "cohort",
        Gauge => "gauge",
        ModelSource => "model_source",
        EffectiveOptions => "effective_options",
        ProbeSet => "probe_set",
        PartitionPolicy => "partition_policy",
        EngineQualification => "engine_qualification",
        PublishedTarget => "published_target",
        Report => "report",
        BundleRevision => "bundle_revision",
        BodyState => "body_state",
        RootWeighting => "root_weighting",
    }
);

closed_enum!(
    /// A realization revision's creation status; selection is a current projection.
    RealizationStatus {
        Candidate => "candidate",
        Diagnostic => "diagnostic",
    }
);

impl DependencyKind {
    pub const fn is_versioned(self) -> bool {
        matches!(
            self,
            Self::Contribution
                | Self::Image
                | Self::RootAdmission
                | Self::Schema
                | Self::Profile
                | Self::Transform
                | Self::RuleVersion
                | Self::EngineQualification
                | Self::FamilyVersion
                | Self::PublishedTarget
                | Self::RealizationRevision
                | Self::Report
                | Self::BundleRevision
                | Self::BodyState
        )
    }
}

closed_enum!(
    /// A numerical or release-gate check outcome.
    SafetyCheck {
        Passed => "passed",
        Failed => "failed",
    }
);

closed_enum!(
    /// What kind of evidence a report requests.
    EvidenceRequestKind {
        MissingSettings => "missing_settings",
        MissingControlValues => "missing_control_values",
        MissingIndependentCaptures => "missing_independent_captures",
        MissingCalibrationSource => "missing_calibration_source",
        MissingCrossBodyEvidence => "missing_cross_body_evidence",
        MissingEngineQualification => "missing_engine_qualification",
    }
);

closed_enum!(
    /// The output encoding an assessment evaluated.
    OutputEncoding {
        Srgb => "srgb",
        AdobeRgb => "adobe_rgb",
        Rec2020 => "rec2020",
        DisplayP3 => "display_p3",
        ProPhoto => "prophoto",
    }
);

closed_enum!(
    /// A metric name a report may carry.
    MetricKey {
        MedianDe => "median_de",
        MedianDePostClip => "median_de_post_clip",
        Coverage => "coverage",
        AssessedCells => "assessed_cells",
        SupportedCells => "supported_cells",
        CrossBodyMedianDisagreementDe => "cross_body_median_disagreement_de",
        RecoveryRate => "recovery_rate",
    }
);

closed_enum!(
    /// Whether an artifact may appear in a public manifest.
    ArtifactVisibility {
        Public => "public",
        Private => "private",
    }
);

closed_enum!(
    /// The role an artifact plays in a candidate.
    ArtifactRole {
        Look => "look",
        ControlResponse => "control_response",
        Bundle => "bundle",
        Manifest => "manifest",
        Card => "card",
        Covariance => "covariance",
        Preview => "preview",
        Report => "report",
        InputTransform => "input_transform",
        CalibrationProfile => "calibration_profile",
        BodyRealizationParams => "body_realization_params",
        Gauge => "gauge",
        NativeWarmState => "native_warm_state",
        RecipeOutput => "recipe_output",
        RealizationParams => "realization_params",
        Release => "release",
        FitResume => "fit_resume",
        FitDiagnostic => "fit_diagnostic",
        ImageNuisance => "image_nuisance",
        EvidenceMembership => "evidence_membership",
    }
);

impl ArtifactRole {
    pub const fn visibility(self) -> ArtifactVisibility {
        match self {
            Self::NativeWarmState
            | Self::FitResume
            | Self::FitDiagnostic
            | Self::ImageNuisance
            | Self::EvidenceMembership => ArtifactVisibility::Private,
            Self::Look
            | Self::ControlResponse
            | Self::Bundle
            | Self::Manifest
            | Self::Card
            | Self::Covariance
            | Self::Preview
            | Self::Report
            | Self::InputTransform
            | Self::CalibrationProfile
            | Self::BodyRealizationParams
            | Self::RealizationParams
            | Self::Release
            | Self::Gauge
            | Self::RecipeOutput => ArtifactVisibility::Public,
        }
    }
}

closed_enum!(
    /// A family cohort a body belongs to.
    CohortRole {
        Seed => "seed",
        Admitted => "admitted",
    }
);

closed_enum!(
    /// The frame of a stored exposure gauge.
    GaugeConvention {
        Relative => "relative",
        LegacyAbsoluteHeader => "legacy_absolute_header",
    }
);

closed_enum!(
    /// A retained legacy record's kind.
    LegacyScopeKind {
        LookVersion => "look_version",
        BodyParams => "body_params",
        PoolState => "pool_state",
    }
);

closed_enum!(
    /// Why retained legacy evidence stays unassessed.
    LegacyScopeReason {
        EpochAndSimUnrecorded => "epoch_and_sim_unrecorded",
    }
);

/// Whether a current projection applies, and why it stopped applying.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Applicability {
    Current,
    Invalidated { reason: InvalidationReason },
    Unassessed,
}

impl Applicability {
    pub const fn is_current(&self) -> bool {
        matches!(self, Self::Current)
    }
}

/// Immutable numerical and release-gate facts of a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateSafety {
    pub numerical: SafetyCheck,
    pub release_gates: SafetyCheck,
}

impl CandidateSafety {
    pub const fn accepted(self) -> bool {
        matches!(self.numerical, SafetyCheck::Passed)
            && matches!(self.release_gates, SafetyCheck::Passed)
    }
}

/// A monotonic pending-work counter.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct PendingGeneration(u64);

impl PendingGeneration {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

/// The canonical target of a dependency edge.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DependencyTarget(String);

impl DependencyTarget {
    /// # Errors
    /// Empty, whitespace-bearing or over-long text.
    pub fn new(value: impl Into<String>) -> Result<Self, QualificationValueError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_TEXT_ID_LEN
            || value.chars().any(char::is_whitespace)
        {
            return Err(QualificationValueError::Invalid {
                field: "DependencyTarget",
                value,
            });
        }
        Ok(Self(value))
    }

    pub fn family_version(target: &FamilyTargetId) -> Self {
        Self(family_target_id(target))
    }

    pub fn realization_revision(revision: &RealizationRevisionId) -> Self {
        Self(revision.as_str().to_owned())
    }

    pub fn transform(body: &BodyId, version: u32) -> Self {
        Self(format!("{body}@{version}"))
    }

    pub fn profile(id: &ProfileId, version: u32) -> Self {
        Self(format!("{id}@{version}"))
    }

    pub fn schema(body: &BodyId, version: u64) -> Self {
        Self(format!("{body}@{version}"))
    }

    pub fn published_schema(snapshot: &PublishedSchemaSnapshot) -> Self {
        Self(snapshot.to_key())
    }

    pub fn rule(revision: &RuleRevision) -> Self {
        Self(revision.as_str().to_owned())
    }

    pub fn contribution(id: &ContributionRef) -> Self {
        Self(id.as_str().to_owned())
    }

    pub fn image(id: &ImageRef) -> Self {
        Self(id.as_str().to_owned())
    }

    pub fn root_admission(id: &RootAdmissionRef) -> Self {
        Self(id.as_str().to_owned())
    }

    pub fn identity_resolution(id: &IdentityResolutionRef) -> Self {
        Self(id.as_str().to_owned())
    }

    pub fn standing(id: &StandingRef) -> Self {
        Self(id.as_str().to_owned())
    }

    pub fn root_weighting(id: &RootWeightingRef) -> Self {
        Self(id.as_str().to_owned())
    }

    pub fn root_weighting_global() -> Self {
        Self::root_weighting(&RootWeightingRef::global())
    }

    pub fn root_weighting_body(body: &BodyId) -> Self {
        Self::root_weighting(&RootWeightingRef::body(body))
    }

    pub fn root_weighting_family(family: &LookId) -> Self {
        Self::root_weighting(&RootWeightingRef::family(family))
    }

    pub fn cohort(id: &CohortId) -> Self {
        Self(id.as_str().to_owned())
    }

    pub fn gauge(id: &GaugeId) -> Self {
        Self(id.as_str().to_owned())
    }

    pub fn model_source(source: &SourceRef) -> Self {
        Self(source.as_str().to_owned())
    }

    pub fn effective_options(digest: Sha256Hex) -> Self {
        Self(digest.to_hex())
    }

    pub fn probe_set(id: &ProbeSetId) -> Self {
        Self(id.as_str().to_owned())
    }

    pub fn partition_policy(digest: Sha256Hex) -> Self {
        Self(digest.to_hex())
    }

    pub fn engine_qualification(id: &EnginePrerequisiteId) -> Self {
        Self(format!("{}@{}", id.engine, id.rule_revision))
    }

    pub fn published_target(target: &PublishedTargetId) -> Self {
        Self(target.to_key())
    }

    pub fn report(id: &ModelReportId) -> Self {
        Self(id.to_hex())
    }

    pub fn bundle_revision(id: &MatchBundleRevisionId) -> Self {
        Self(id.to_hex())
    }

    pub fn body_state(body: &BodyId, version: BodyStateVersion) -> Self {
        Self(format!("{body}@{}", version.get()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DependencyTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One queryable dependency edge of a report or realization revision.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DependencyRef {
    pub kind: DependencyKind,
    pub target: DependencyTarget,
    pub required: bool,
}

impl DependencyRef {
    pub fn required(kind: DependencyKind, target: DependencyTarget) -> Self {
        Self {
            kind,
            target,
            required: true,
        }
    }

    pub fn optional(kind: DependencyKind, target: DependencyTarget) -> Self {
        Self {
            kind,
            target,
            required: false,
        }
    }
}

/// A dependency generation read when a run started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyGeneration {
    pub dependency: DependencyRef,
    pub generation: u64,
}

/// A value of one assessed control.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingValue {
    Step(i64),
    Enum(EnumValue),
    Bool(bool),
}

/// One settings combination an assessment covers.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SettingCombination(BTreeMap<ControlKey, SettingValue>);

impl SettingCombination {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, key: ControlKey, value: SettingValue) {
        self.0.insert(key, value);
    }

    pub fn get(&self, key: &ControlKey) -> Option<&SettingValue> {
        self.0.get(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ControlKey, &SettingValue)> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// One enumerated assessed settings/physical-context point.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AssessedPoint {
    pub settings: SettingCombination,
    pub firmware: Option<FirmwareId>,
    pub wb_mode: Option<WbModeKey>,
}

/// The assessed domain of a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportScope {
    pub points: Vec<AssessedPoint>,
    pub combination_basis: CombinationBasis,
    pub defaults_assessed: bool,
    pub output_encoding: OutputEncoding,
    pub colour_cells: Vec<CellId>,
    pub exposure_cells: Vec<CellId>,
    pub assessed_bodies: Vec<BodyId>,
    pub assessed_domain: DomainId,
}

impl ReportScope {
    pub fn key(&self) -> ScopeKey {
        ScopeKey::of(self)
    }

    pub fn overlaps(&self, other: &Self) -> bool {
        self.output_encoding == other.output_encoding
            && self.points.iter().any(|point| other.points.contains(point))
            && colour_cells_overlap(&self.colour_cells, &other.colour_cells)
            && exposure_cells_overlap(&self.exposure_cells, &other.exposure_cells)
    }
}

fn colour_cells_overlap(left: &[CellId], right: &[CellId]) -> bool {
    if left.is_empty() && right.is_empty() {
        return true;
    }
    left.iter().any(|cell| right.contains(cell))
}

fn exposure_cells_overlap(left: &[CellId], right: &[CellId]) -> bool {
    left.is_empty() || right.is_empty() || left.iter().any(|cell| right.contains(cell))
}

/// The current-projection key for an assessed scope, or `all` for unassessed model use.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScopeKey(String);

impl ScopeKey {
    pub fn all() -> Self {
        Self("all".to_owned())
    }

    pub fn of(scope: &ReportScope) -> Self {
        let mut scope = scope.clone();
        scope.points.sort();
        scope.colour_cells.sort();
        scope.exposure_cells.sort();
        scope.assessed_bodies.sort();
        let bytes = serde_json::to_vec(&scope).expect("a report scope has no unserializable value");
        Self(Sha256Hex::from_bytes(Sha256::digest(bytes).into()).to_hex())
    }

    /// # Errors
    /// Anything but `all` or 64 lowercase hex characters.
    pub fn parse(value: &str) -> Result<Self, QualificationValueError> {
        if value == "all" {
            return Ok(Self::all());
        }
        Sha256Hex::parse(value)
            .map(|digest| Self(digest.to_hex()))
            .map_err(|_| QualificationValueError::Invalid {
                field: "ScopeKey",
                value: value.to_owned(),
            })
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ScopeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Fitter, evaluator and model identity plus the options that produced a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportProvenance {
    pub rule_revision: RuleRevision,
    pub fitter: SourceRef,
    pub evaluator: SourceRef,
    pub model: SourceRef,
    pub probe_set: ProbeSetId,
    pub effective_options: Sha256Hex,
    pub partition_policy: Sha256Hex,
    pub assessment_seeds: Vec<u64>,
}

/// Aggregate metrics a report may publish.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReportMetrics(BTreeMap<MetricKey, f64>);

impl ReportMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, key: MetricKey, value: f64) {
        self.0.insert(key, value);
    }

    pub fn get(&self, key: MetricKey) -> Option<f64> {
        self.0.get(&key).copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = (MetricKey, f64)> {
        self.0.iter().map(|(key, value)| (*key, *value))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The cells a report's claim covers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SupportedMask(BTreeSet<CellId>);

impl SupportedMask {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, cell: CellId) {
        self.0.insert(cell);
    }

    pub fn contains(&self, cell: &CellId) -> bool {
        self.0.contains(cell)
    }

    pub fn iter(&self) -> impl Iterator<Item = &CellId> {
        self.0.iter()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn overlaps(&self, other: &Self) -> bool {
        self.0.iter().any(|cell| other.0.contains(cell))
    }

    pub fn restricted_to(&self, cells: &[CellId]) -> Self {
        let cells: BTreeSet<&CellId> = cells.iter().collect();
        Self(
            self.0
                .iter()
                .filter(|cell| cells.contains(cell))
                .cloned()
                .collect(),
        )
    }

    pub fn intersection(&self, other: &Self) -> Self {
        Self(self.0.intersection(&other.0).cloned().collect())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Limitation(String);

impl Limitation {
    /// # Errors
    /// Empty text.
    pub fn new(value: impl Into<String>) -> Result<Self, QualificationValueError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(QualificationValueError::Invalid {
                field: "Limitation",
                value,
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An actionable request for evidence a report could not assess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRequest {
    pub kind: EvidenceRequestKind,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ReportValidationError {
    #[error("{metric:?} is not finite")]
    NonFiniteMetric { metric: MetricKey },
    #[error("{capability:?} pass requires metric {metric:?}")]
    MissingMetric {
        capability: QualificationCapability,
        metric: MetricKey,
    },
    #[error("a pass requires a non-empty supported mask")]
    EmptySupportedMask,
    #[error("supported_cells {metric} does not match the {mask}-cell mask")]
    SupportedCellsMismatch { mask: usize, metric: f64 },
    #[error("assessed_cells {assessed} is below supported_cells {supported}")]
    AssessedBelowSupported { assessed: f64, supported: f64 },
    #[error("coverage {coverage} is outside [0, 1]")]
    CoverageOutOfRange { coverage: f64 },
    #[error("a pass requires at least one required dependency")]
    NoRequiredDependency,
    #[error("a controls pass requires at least one assessed settings combination")]
    NoSettingCombination,
    #[error("a pass requires enumerated settings combinations, not an implied product")]
    ImplicitCombinationProduct,
    #[error("an empty settings combination is defaults-only and requires defaults_assessed")]
    UnassessedDefaultCombination,
    #[error("assessed firmware {body}/{epoch} does not match the realization subject")]
    FirmwareScopeMismatch { body: BodyId, epoch: u32 },
    #[error("a cross-body pass requires at least two assessed bodies")]
    TooFewAssessedBodies { bodies: usize },
    #[error("insufficient evidence requires at least one evidence request")]
    NoEvidenceRequest,
    #[error("candidate {candidate} does not belong to the report's subject")]
    CandidateSubjectMismatch { candidate: String },
    #[error("engine pass requires metric {metric:?}")]
    MissingEngineMetric { metric: MetricKey },
}

/// The public part of an immutable qualification report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelReport {
    pub subject: ModelSubject,
    pub candidate: ModelVersionId,
    pub capability: QualificationCapability,
    pub scope: ReportScope,
    pub provenance: ReportProvenance,
    pub outcome: AssessmentOutcome,
    pub metrics: ReportMetrics,
    pub supported_mask: SupportedMask,
    pub limitations: Vec<Limitation>,
    pub evidence_requests: Vec<EvidenceRequest>,
}

impl ModelReport {
    /// # Errors
    /// An outcome whose checks, scope or metrics cannot support it.
    pub fn validate(&self, dependencies: &[DependencyRef]) -> Result<(), ReportValidationError> {
        for (metric, value) in self.metrics.iter() {
            if !value.is_finite() {
                return Err(ReportValidationError::NonFiniteMetric { metric });
            }
        }
        self.validate_candidate_subject()?;
        match self.outcome {
            AssessmentOutcome::Pass => {}
            AssessmentOutcome::InsufficientEvidence => {
                if self.evidence_requests.is_empty() {
                    return Err(ReportValidationError::NoEvidenceRequest);
                }
                return Ok(());
            }
            AssessmentOutcome::Fail | AssessmentOutcome::ExecutionFailure => return Ok(()),
        }
        if self.supported_mask.is_empty() {
            return Err(ReportValidationError::EmptySupportedMask);
        }
        if !dependencies.iter().any(|dependency| dependency.required) {
            return Err(ReportValidationError::NoRequiredDependency);
        }
        if self.scope.points.is_empty() {
            return Err(ReportValidationError::NoSettingCombination);
        }
        if self.scope.combination_basis != CombinationBasis::Enumerated {
            return Err(ReportValidationError::ImplicitCombinationProduct);
        }
        if self
            .scope
            .points
            .iter()
            .any(|point| point.settings.is_empty())
            && !self.scope.defaults_assessed
        {
            return Err(ReportValidationError::UnassessedDefaultCombination);
        }
        if let ModelSubject::BodyRealization { realization } = &self.subject {
            for point in &self.scope.points {
                if let Some(firmware) = &point.firmware
                    && (firmware.body != realization.body || firmware.epoch != realization.epoch)
                {
                    return Err(ReportValidationError::FirmwareScopeMismatch {
                        body: firmware.body.clone(),
                        epoch: firmware.epoch.get(),
                    });
                }
            }
        }
        let required: &[MetricKey] = match self.capability {
            QualificationCapability::NativePrediction => &[
                MetricKey::MedianDe,
                MetricKey::Coverage,
                MetricKey::AssessedCells,
                MetricKey::SupportedCells,
            ],
            QualificationCapability::ControlsMatching => &[
                MetricKey::MedianDePostClip,
                MetricKey::Coverage,
                MetricKey::AssessedCells,
                MetricKey::SupportedCells,
            ],
            QualificationCapability::CrossBodyAdmission => {
                if self.scope.assessed_bodies.len() < 2 {
                    return Err(ReportValidationError::TooFewAssessedBodies {
                        bodies: self.scope.assessed_bodies.len(),
                    });
                }
                &[
                    MetricKey::CrossBodyMedianDisagreementDe,
                    MetricKey::Coverage,
                    MetricKey::AssessedCells,
                    MetricKey::SupportedCells,
                ]
            }
        };
        for metric in required {
            if self.metrics.get(*metric).is_none() {
                return Err(ReportValidationError::MissingMetric {
                    capability: self.capability,
                    metric: *metric,
                });
            }
        }
        let supported = self.metrics.get(MetricKey::SupportedCells).unwrap_or(0.0);
        if supported != self.supported_mask.len() as f64 {
            return Err(ReportValidationError::SupportedCellsMismatch {
                mask: self.supported_mask.len(),
                metric: supported,
            });
        }
        let assessed = self.metrics.get(MetricKey::AssessedCells).unwrap_or(0.0);
        if assessed < supported {
            return Err(ReportValidationError::AssessedBelowSupported {
                assessed,
                supported,
            });
        }
        let coverage = self.metrics.get(MetricKey::Coverage).unwrap_or(0.0);
        if !(0.0..=1.0).contains(&coverage) {
            return Err(ReportValidationError::CoverageOutOfRange { coverage });
        }
        Ok(())
    }

    fn validate_candidate_subject(&self) -> Result<(), ReportValidationError> {
        let matches = match (&self.subject, &self.candidate) {
            (ModelSubject::SharedFamily { family }, ModelVersionId::SharedFamily { target }) => {
                target.family == *family
            }
            (
                ModelSubject::BodyRealization { realization },
                ModelVersionId::BodyRealization {
                    realization: candidate,
                },
            ) => candidate.realization == *realization,
            _ => false,
        };
        if matches {
            Ok(())
        } else {
            Err(ReportValidationError::CandidateSubjectMismatch {
                candidate: self.candidate.candidate_id(),
            })
        }
    }

    /// The digest that makes a repeated assessment idempotent.
    pub fn semantic_id(&self, dependencies: &[DependencyRef]) -> ModelReportId {
        #[derive(Serialize)]
        struct Identity<'a> {
            report: &'a ModelReport,
            dependencies: Vec<&'a DependencyRef>,
        }

        let mut dependencies: Vec<&DependencyRef> = dependencies.iter().collect();
        dependencies.sort();
        dependencies.dedup();
        let bytes = serde_json::to_vec(&Identity {
            report: self,
            dependencies,
        })
        .expect("a model report has no unserializable value");
        ModelReportId::from_digest(Sha256::digest(bytes).into())
    }
}

/// A version-scoped engine prerequisite, for example P4.3.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EnginePrerequisiteId {
    pub engine: SourceRef,
    pub rule_revision: RuleRevision,
}

/// Known-reference evidence for an engine version, not a camera subject.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnginePrerequisite {
    pub engine: SourceRef,
    pub rule_revision: RuleRevision,
    pub outcome: AssessmentOutcome,
    pub metrics: ReportMetrics,
    pub supported_mask: SupportedMask,
    pub limitations: Vec<Limitation>,
}

impl EnginePrerequisite {
    pub fn id(&self) -> EnginePrerequisiteId {
        EnginePrerequisiteId {
            engine: self.engine.clone(),
            rule_revision: self.rule_revision.clone(),
        }
    }

    /// # Errors
    /// A pass without its known-reference recovery and coverage checks.
    pub fn validate(&self) -> Result<(), ReportValidationError> {
        for (metric, value) in self.metrics.iter() {
            if !value.is_finite() {
                return Err(ReportValidationError::NonFiniteMetric { metric });
            }
        }
        if !matches!(self.outcome, AssessmentOutcome::Pass) {
            return Ok(());
        }
        if self.supported_mask.is_empty() {
            return Err(ReportValidationError::EmptySupportedMask);
        }
        for metric in [
            MetricKey::RecoveryRate,
            MetricKey::Coverage,
            MetricKey::AssessedCells,
            MetricKey::SupportedCells,
        ] {
            if self.metrics.get(metric).is_none() {
                return Err(ReportValidationError::MissingEngineMetric { metric });
            }
        }
        let supported = self.metrics.get(MetricKey::SupportedCells).unwrap_or(0.0);
        if supported != self.supported_mask.len() as f64 {
            return Err(ReportValidationError::SupportedCellsMismatch {
                mask: self.supported_mask.len(),
                metric: supported,
            });
        }
        let coverage = self.metrics.get(MetricKey::Coverage).unwrap_or(0.0);
        if !(0.0..=1.0).contains(&coverage) {
            return Err(ReportValidationError::CoverageOutOfRange { coverage });
        }
        Ok(())
    }
}

/// One artifact a candidate or report binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactBinding {
    pub sha256: Sha256Hex,
    pub role: ArtifactRole,
}

/// A seed body or an admitted member's exact proof binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CohortMember {
    Seed {
        body: BodyId,
    },
    Admitted {
        realization: BodyRealizationVersionId,
        report: ModelReportId,
        scope: Box<ReportScope>,
        supported_mask: SupportedMask,
    },
}

/// A passing cross-body report's request to admit members to a qualified cohort fit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CohortAdmissionRequest {
    pub family: LookId,
    pub target: FamilyTargetId,
    pub report: ModelReportId,
    pub members: Vec<CohortMember>,
    pub assessed_domain: DomainId,
}

closed_enum!(
    /// How a target's eligibility is derived.
    TargetBasis {
        NativeFamily => "native_family",
        LutComposition => "lut_composition",
    }
);

closed_enum!(
    /// A target's derived eligibility.
    TargetEligibilityStatus {
        Eligible => "eligible",
        Ineligible => "ineligible",
        Invalidated => "invalidated",
    }
);

closed_enum!(
    /// Why a target is not eligible.
    TargetEligibilityReason {
        NativeProjectionMissing => "native_projection_missing",
        NativeProjectionInapplicable => "native_projection_inapplicable",
        LutImportInvalid => "lut_import_invalid",
        LutApproximationUnfrozen => "lut_approximation_unfrozen",
        BasePinMissing => "base_pin_missing",
        BasePinInapplicable => "base_pin_inapplicable",
        SupportUncertaintyMissing => "support_uncertainty_missing",
        ArtifactMissing => "artifact_missing",
        DependencyInvalidated => "dependency_invalidated",
    }
);

closed_enum!(
    /// How a report's assessed settings combinations were enumerated.
    CombinationBasis {
        Enumerated => "enumerated",
        Product => "product",
    }
);

closed_enum!(
    /// What admitted a family cohort.
    CohortBasis {
        SeedEvidence => "seed_evidence",
        CrossBodyAdmission => "cross_body_admission",
    }
);

/// A LUT or declared-rendering base a target's composition is pinned to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BasePin {
    LookVersion(PublishedTargetId),
    DeclaredRendering(SourceRef),
}

/// A target's derived eligibility; recomputed, unlike an immutable report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetEligibility {
    pub id: EligibilitySnapshotId,
    pub target: PublishedTargetId,
    pub basis: TargetBasis,
    pub artifacts: Vec<ArtifactBinding>,
    pub base_pin: Option<BasePin>,
    pub support_uncertainty_source: Option<SourceRef>,
    pub status: TargetEligibilityStatus,
    pub reasons: Vec<TargetEligibilityReason>,
    pub dependencies: Vec<DependencyRef>,
}

impl TargetEligibility {
    pub fn snapshot_id(&self) -> EligibilitySnapshotId {
        #[derive(Serialize)]
        struct Content<'a> {
            target: &'a PublishedTargetId,
            basis: TargetBasis,
            artifacts: &'a [ArtifactBinding],
            base_pin: &'a Option<BasePin>,
            support_uncertainty_source: &'a Option<SourceRef>,
            status: TargetEligibilityStatus,
            reasons: &'a [TargetEligibilityReason],
            dependencies: Vec<&'a DependencyRef>,
        }

        let mut dependencies: Vec<&DependencyRef> = self.dependencies.iter().collect();
        dependencies.sort();
        dependencies.dedup();
        let bytes = serde_json::to_vec(&Content {
            target: &self.target,
            basis: self.basis,
            artifacts: &self.artifacts,
            base_pin: &self.base_pin,
            support_uncertainty_source: &self.support_uncertainty_source,
            status: self.status,
            reasons: &self.reasons,
            dependencies,
        })
        .expect("a target eligibility has no unserializable value");
        EligibilitySnapshotId::from_digest(Sha256::digest(bytes).into())
    }

    pub fn sealed(mut self) -> Self {
        self.id = self.snapshot_id();
        self
    }
}

/// An immutable content digest of a [`MatchBundleRevision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MatchBundleRevisionId(Sha256Hex);

impl MatchBundleRevisionId {
    /// # Errors
    /// Anything but 64 lowercase hex characters.
    pub fn parse(value: &str) -> Result<Self, crate::proposal::Sha256Error> {
        Sha256Hex::parse(value).map(Self)
    }

    pub const fn from_digest(digest: [u8; 32]) -> Self {
        Self(Sha256Hex::from_bytes(digest))
    }

    pub fn to_hex(self) -> String {
        self.0.to_hex()
    }
}

impl fmt::Display for MatchBundleRevisionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.to_hex())
    }
}

closed_enum!(
    /// Why a native sim is absent from a bundle revision.
    BundleExclusionReason {
        NoPublishedRealization => "no_published_realization",
        NoControlResponse => "no_control_response",
        MissingFirmware => "missing_firmware",
        IncompatibleRevision => "incompatible_revision",
    }
);

closed_enum!(
    /// The selection mode a card or page presents.
    SelectionScope {
        Qualified => "qualified",
        Experimental => "experimental",
        Unassessed => "unassessed",
    }
);

/// One ranked sim's immutable source references and pinned realization state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BundleSimEntry {
    pub native_sim: SimKey,
    pub realization: RealizationRevisionId,
    pub family_version: FamilyTargetId,
    pub base_artifact: Sha256Hex,
    pub control_response: Sha256Hex,
    pub covariance: Option<Sha256Hex>,
    pub control_response_version: u32,
    pub clip_k: f64,
    pub realization_params_version: u32,
    pub body_params: Option<Sha256Hex>,
    pub realization_params: Option<Sha256Hex>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleExclusion {
    pub native_sim: SimKey,
    pub reason: BundleExclusionReason,
}

/// A target-independent `(body, epoch)` bundle revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchBundleRevision {
    pub id: MatchBundleRevisionId,
    pub manifest_hash: MatchBundleRevisionId,
    pub body: BodyId,
    pub epoch: ControlEpochId,
    pub schema_snapshot_version: u64,
    pub input_transform_version: u32,
    pub body_state_version: BodyStateVersion,
    pub body_realization_params_version: u32,
    pub sims: Vec<BundleSimEntry>,
    pub excluded: Vec<BundleExclusion>,
    pub producer: SourceRef,
    pub created_at: i64,
}

#[derive(Serialize)]
struct BundleManifestRef<'a> {
    body: &'a BodyId,
    epoch: ControlEpochId,
    schema_snapshot_version: u64,
    input_transform_version: u32,
    body_state_version: BodyStateVersion,
    body_realization_params_version: u32,
    sims: &'a [BundleSimEntry],
    excluded: &'a [BundleExclusion],
    producer: &'a SourceRef,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleManifest {
    body: BodyId,
    epoch: ControlEpochId,
    schema_snapshot_version: u64,
    input_transform_version: u32,
    body_state_version: BodyStateVersion,
    body_realization_params_version: u32,
    sims: Vec<BundleSimEntry>,
    excluded: Vec<BundleExclusion>,
    producer: SourceRef,
}

impl MatchBundleRevision {
    pub fn manifest(&self) -> String {
        serde_json::to_string(&BundleManifestRef {
            body: &self.body,
            epoch: self.epoch,
            schema_snapshot_version: self.schema_snapshot_version,
            input_transform_version: self.input_transform_version,
            body_state_version: self.body_state_version,
            body_realization_params_version: self.body_realization_params_version,
            sims: &self.sims,
            excluded: &self.excluded,
            producer: &self.producer,
        })
        .expect("a match bundle revision has no unserializable value")
    }

    pub fn snapshot_id(&self) -> MatchBundleRevisionId {
        MatchBundleRevisionId::from_digest(Sha256::digest(self.manifest().as_bytes()).into())
    }

    pub fn sealed(mut self) -> Self {
        let id = self.snapshot_id();
        self.id = id;
        self.manifest_hash = id;
        self
    }

    /// # Errors
    /// Manifest text that is not a canonical bundle revision.
    pub fn parse_manifest(manifest: &str, created_at: i64) -> Result<Self, serde_json::Error> {
        let content: BundleManifest = serde_json::from_str(manifest)?;
        let id = MatchBundleRevisionId::from_digest(Sha256::digest(manifest.as_bytes()).into());
        Ok(Self {
            id,
            manifest_hash: id,
            body: content.body,
            epoch: content.epoch,
            schema_snapshot_version: content.schema_snapshot_version,
            input_transform_version: content.input_transform_version,
            body_state_version: content.body_state_version,
            body_realization_params_version: content.body_realization_params_version,
            sims: content.sims,
            excluded: content.excluded,
            producer: content.producer,
            created_at,
        })
    }
}

closed_enum!(
    /// Whether a body state was fitted natively or published as shared calibration.
    BodyStateStage {
        Native => "native",
        Published => "published",
    }
);

/// Content-addressed calibration, transform, body-parameter and gauge pins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodyStatePins {
    pub profile: Option<ArtifactBinding>,
    pub input_transform: ArtifactBinding,
    pub body_params: ArtifactBinding,
    pub gauge: ArtifactBinding,
}

/// One immutable shared body resource version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodyStateRecord {
    pub body: BodyId,
    pub version: BodyStateVersion,
    pub stage: BodyStateStage,
    pub pins: BodyStatePins,
    pub dependencies: Vec<DependencyRef>,
}

/// A schema snapshot version that the proposal store has published as authoritative.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PublishedSchemaSnapshot {
    pub body: BodyId,
    pub snapshot: u64,
}

impl PublishedSchemaSnapshot {
    pub fn new(body: BodyId, snapshot: u64) -> Self {
        Self { body, snapshot }
    }

    pub fn to_key(&self) -> String {
        format!("{}@{}", self.body, self.snapshot)
    }
}

/// The canonical family head, independent of consumer mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilyHead {
    pub target: FamilyTargetId,
    pub source_limited: bool,
    pub safety: CandidateSafety,
    pub report: Option<ModelReportId>,
    pub applicability: Applicability,
}

/// The small current selection for one realization subject and use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentProjection {
    pub subject: SubjectKey,
    #[serde(rename = "use")]
    pub use_: ProjectionUse,
    pub capability: QualificationCapability,
    pub scope_key: ScopeKey,
    pub selected_report: Option<ModelReportId>,
    pub selected_candidate: Option<ModelVersionId>,
    pub pending_generation: PendingGeneration,
    pub applicability: Applicability,
}

/// Retained legacy evidence whose scope cannot be resolved to a typed subject.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyScope {
    pub kind: LegacyScopeKind,
    pub target: LegacyTarget,
    pub family: Option<LookId>,
    pub body: Option<BodyId>,
    pub applicability: Applicability,
    pub reason: LegacyScopeReason,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_subject::{RealizationVersion, TargetVersion};

    fn look(name: &str) -> LookId {
        LookId::new(name).expect("look")
    }

    fn body(name: &str) -> BodyId {
        BodyId::new(name).expect("body")
    }

    fn sim(name: &str) -> SimKey {
        SimKey::new(name).expect("sim")
    }

    #[test]
    fn subject_keys_round_trip() {
        let family = ModelSubject::SharedFamily {
            family: look("classic-chrome"),
        };
        assert_eq!(family.key().as_str(), "shared_family:classic-chrome");
        assert_eq!(family.key().parse().expect("family"), family);

        let realization = ModelSubject::BodyRealization {
            realization: BodyRealizationId {
                body: body("fujifilm/x-t5"),
                epoch: ControlEpochId::new(3),
                native_sim: sim("classic_chrome"),
            },
        };
        assert_eq!(
            realization.key().as_str(),
            "body_realization:fujifilm/x-t5/3/classic_chrome"
        );
        assert_eq!(realization.key().parse().expect("realization"), realization);
    }

    #[test]
    fn distinct_scopes_have_distinct_keys() {
        let epoch_two = ModelSubject::BodyRealization {
            realization: BodyRealizationId {
                body: body("fujifilm/x-t5"),
                epoch: ControlEpochId::new(2),
                native_sim: sim("classic_chrome"),
            },
        };
        let other_sim = ModelSubject::BodyRealization {
            realization: BodyRealizationId {
                body: body("fujifilm/x-t5"),
                epoch: ControlEpochId::new(3),
                native_sim: sim("provia"),
            },
        };
        assert_ne!(epoch_two.key(), other_sim.key());
        assert_ne!(
            epoch_two.key().parse().expect("epoch"),
            other_sim.key().parse().expect("sim")
        );
    }

    #[test]
    fn malformed_subject_keys_are_rejected() {
        for text in [
            "",
            "family:classic-chrome",
            "shared_family:",
            "body_realization:fujifilm/x-t5",
            "body_realization:fujifilm/x-t5/not-a-number/classic_chrome",
        ] {
            assert!(
                SubjectKey(text.to_owned()).parse().is_err(),
                "{text} should be rejected"
            );
        }
    }

    #[test]
    fn candidate_ids_are_scope_specific() {
        let target = ModelVersionId::SharedFamily {
            target: FamilyTargetId {
                family: look("classic-chrome"),
                version: TargetVersion::new(4).expect("version"),
            },
        };
        assert_eq!(target.kind(), ModelVersionKind::SharedFamily);
        assert_eq!(target.candidate_id(), "classic-chrome@4");

        let realization = ModelVersionId::BodyRealization {
            realization: BodyRealizationVersionId {
                realization: BodyRealizationId {
                    body: body("fujifilm/x-t5"),
                    epoch: ControlEpochId::new(1),
                    native_sim: sim("provia"),
                },
                version: RealizationVersion::new(2).expect("version"),
                target: FamilyTargetId {
                    family: look("classic-chrome"),
                    version: TargetVersion::new(4).expect("version"),
                },
            },
        };
        assert_eq!(realization.kind(), ModelVersionKind::BodyRealization);
        assert_eq!(
            realization.candidate_id(),
            "body_realization:fujifilm/x-t5/1/provia@2"
        );
    }

    fn scope() -> ReportScope {
        ReportScope {
            points: vec![AssessedPoint {
                settings: SettingCombination::new(),
                firmware: None,
                wb_mode: None,
            }],
            combination_basis: CombinationBasis::Enumerated,
            defaults_assessed: true,
            output_encoding: OutputEncoding::Srgb,
            colour_cells: vec![CellId::new("neutral").expect("cell")],
            exposure_cells: vec![CellId::new("ev0").expect("cell")],
            assessed_bodies: vec![body("fujifilm/x-t5")],
            assessed_domain: DomainId::new("supported-probes").expect("domain"),
        }
    }

    fn target() -> FamilyTargetId {
        FamilyTargetId {
            family: look("classic-chrome"),
            version: TargetVersion::new(1).expect("version"),
        }
    }

    fn report() -> ModelReport {
        ModelReport {
            subject: ModelSubject::SharedFamily {
                family: look("classic-chrome"),
            },
            candidate: ModelVersionId::SharedFamily { target: target() },
            capability: QualificationCapability::NativePrediction,
            scope: scope(),
            provenance: ReportProvenance {
                rule_revision: RuleRevision::new("native-prediction/1").expect("rule"),
                fitter: SourceRef::new("looklab-fit@dev").expect("fitter"),
                evaluator: SourceRef::new("looklab-model@dev").expect("evaluator"),
                model: SourceRef::new("looklab-model@dev").expect("model"),
                probe_set: ProbeSetId::new("v1").expect("probes"),
                effective_options: Sha256Hex::from_bytes([0; 32]),
                partition_policy: Sha256Hex::from_bytes([1; 32]),
                assessment_seeds: vec![7],
            },
            outcome: AssessmentOutcome::Pass,
            metrics: ReportMetrics::new(),
            supported_mask: SupportedMask::new(),
            limitations: vec![Limitation::new("single group").expect("limitation")],
            evidence_requests: Vec::new(),
        }
    }

    fn dependencies() -> Vec<DependencyRef> {
        vec![DependencyRef::required(
            DependencyKind::Transform,
            DependencyTarget::transform(&body("fujifilm/x-t5"), 2),
        )]
    }

    #[test]
    fn report_identity_ignores_dependency_order_but_not_content() {
        let report = report();
        let transform = DependencyRef::required(
            DependencyKind::Transform,
            DependencyTarget::transform(&body("fujifilm/x-t5"), 2),
        );
        let rule = DependencyRef::required(
            DependencyKind::RuleVersion,
            DependencyTarget::rule(&RuleRevision::new("native-prediction/1").expect("rule")),
        );
        let mut dependencies = [transform, rule];
        let ordered = report.semantic_id(&dependencies);
        dependencies.reverse();
        assert_eq!(ordered, report.semantic_id(&dependencies));

        let mut changed = report.clone();
        changed.outcome = AssessmentOutcome::Fail;
        assert_ne!(ordered, changed.semantic_id(&dependencies));

        let mut wider = report;
        wider.scope.assessed_bodies.push(body("sony/a7"));
        assert_ne!(ordered, wider.semantic_id(&dependencies));
    }

    #[test]
    fn invalidated_applicability_carries_its_reason() {
        let applicability = Applicability::Invalidated {
            reason: InvalidationReason::Withdrawal,
        };
        assert!(!applicability.is_current());
        let encoded = serde_json::to_string(&applicability).expect("encode");
        assert_eq!(
            serde_json::from_str::<Applicability>(&encoded).expect("decode"),
            applicability
        );
    }

    #[test]
    fn a_pass_needs_its_checks_metrics_and_domain() {
        let report = report();
        assert_eq!(
            report.validate(&dependencies()),
            Err(ReportValidationError::EmptySupportedMask)
        );

        let mut complete = report;
        complete
            .supported_mask
            .insert(CellId::new("neutral").expect("cell"));
        complete.metrics.insert(MetricKey::SupportedCells, 1.0);
        complete.metrics.insert(MetricKey::AssessedCells, 3.0);
        complete.metrics.insert(MetricKey::Coverage, 1.0 / 3.0);
        complete.metrics.insert(MetricKey::MedianDe, 1.0);
        assert_eq!(complete.validate(&dependencies()), Ok(()));

        let mut no_dependencies = complete.clone();
        no_dependencies.metrics.insert(MetricKey::MedianDe, 1.0);
        assert_eq!(
            no_dependencies.validate(&[]),
            Err(ReportValidationError::NoRequiredDependency)
        );

        let mut missing = complete;
        missing.metrics = ReportMetrics::new();
        assert_eq!(
            missing.validate(&dependencies()),
            Err(ReportValidationError::MissingMetric {
                capability: QualificationCapability::NativePrediction,
                metric: MetricKey::MedianDe,
            })
        );
    }

    #[test]
    fn insufficient_evidence_requires_an_actionable_request() {
        let mut report = report();
        report.outcome = AssessmentOutcome::InsufficientEvidence;
        assert_eq!(
            report.validate(&dependencies()),
            Err(ReportValidationError::NoEvidenceRequest)
        );
        report.evidence_requests.push(EvidenceRequest {
            kind: EvidenceRequestKind::MissingIndependentCaptures,
            detail: "a second independent capture at EV 0".to_owned(),
        });
        assert_eq!(report.validate(&dependencies()), Ok(()));
    }

    #[test]
    fn a_report_cannot_name_another_subjects_candidate() {
        let mut report = report();
        report.subject = ModelSubject::BodyRealization {
            realization: BodyRealizationId {
                body: body("fujifilm/x-t5"),
                epoch: ControlEpochId::new(1),
                native_sim: sim("provia"),
            },
        };
        assert!(matches!(
            report.validate(&dependencies()),
            Err(ReportValidationError::CandidateSubjectMismatch { .. })
        ));
    }

    #[test]
    fn bundle_manifest_round_trips_with_its_content_id() {
        let revision = MatchBundleRevision {
            id: MatchBundleRevisionId::from_digest([0; 32]),
            body: body("fujifilm/x-t5"),
            epoch: ControlEpochId::new(1),
            schema_snapshot_version: 1,
            input_transform_version: 2,
            body_state_version: BodyStateVersion::new(1).expect("body state"),
            body_realization_params_version: 3,
            sims: Vec::new(),
            excluded: Vec::new(),
            manifest_hash: MatchBundleRevisionId::from_digest([0; 32]),
            producer: SourceRef::new("worker@1").expect("producer"),
            created_at: 9,
        }
        .sealed();
        let manifest = revision.manifest();
        let parsed = MatchBundleRevision::parse_manifest(&manifest, 9).expect("manifest");
        assert_eq!(parsed.id, revision.id);
        assert_eq!(parsed.manifest_hash, revision.id);
        assert_eq!(parsed.manifest(), manifest);
        assert_eq!(
            parsed.id,
            MatchBundleRevisionId::from_digest(Sha256::digest(manifest.as_bytes()).into())
        );
    }

    #[test]
    fn engine_pass_needs_recovery_and_coverage() {
        let mut prerequisite = EnginePrerequisite {
            engine: SourceRef::new("looklab-fit@dev").expect("engine"),
            rule_revision: RuleRevision::new("p4.3/1").expect("rule"),
            outcome: AssessmentOutcome::Pass,
            metrics: ReportMetrics::new(),
            supported_mask: SupportedMask::new(),
            limitations: Vec::new(),
        };
        assert_eq!(
            prerequisite.validate(),
            Err(ReportValidationError::EmptySupportedMask)
        );
        prerequisite
            .supported_mask
            .insert(CellId::new("latent-1").expect("cell"));
        prerequisite.metrics.insert(MetricKey::SupportedCells, 1.0);
        assert_eq!(
            prerequisite.validate(),
            Err(ReportValidationError::MissingEngineMetric {
                metric: MetricKey::RecoveryRate
            })
        );
    }
}
