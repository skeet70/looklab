// SPDX-License-Identifier: AGPL-3.0-or-later
//! Published qualification records and scoped selection (docs/03 §5).

use looklab_model::file::LookVersion;
use looklab_schema::{
    Applicability, ArtifactBinding, AssessmentOutcome, BasePin, BodyId, BodyRealizationId,
    BodyStateStage, BodyStateVersion, CandidateSafety, EligibilitySnapshotId, EvidenceRequest,
    FamilyHead, FamilyTargetId, Limitation, MatchBundleRevision, MatchBundleRevisionId,
    ModelReport, ModelReportId, ModelSubject, ModelVersionId, PendingGeneration, ProjectionUse,
    PublishedTargetId, QualificationCapability, RealizationRevisionId, RealizationStatus,
    RealizationVersion, ReportMetrics, ReportScope, RuleRevision, ScopeKey, SelectionScope,
    SourceRef, SubjectKey, SupportedMask, TargetBasis, TargetEligibility, TargetEligibilityReason,
    TargetEligibilityStatus,
};
use serde::{Deserialize, Serialize};

use crate::PublishError;
use crate::hash::{ArtifactHash, SitePath};
use crate::manifest::{ArtifactBlob, ArtifactEntry, HashMismatch};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportFormat {
    #[serde(rename = "looklab-model-report/1")]
    V1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EligibilityFormat {
    #[serde(rename = "looklab-target-eligibility/1")]
    V1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CurrentFormat {
    #[serde(rename = "looklab-current/1")]
    V1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BodyStateFormat {
    #[serde(rename = "looklab-body-state/1")]
    V1,
}

/// # Errors
/// Rejects a report id that is not a safe path component.
pub fn report_path(id: &ModelReportId) -> Result<SitePath, PublishError> {
    SitePath::new(format!("data/reports/{}.json", id.to_hex()))
}

/// # Errors
/// Rejects a snapshot id that is not a safe path component.
pub fn eligibility_path(id: &EligibilitySnapshotId) -> Result<SitePath, PublishError> {
    SitePath::new(format!("data/eligibility/{id}.json"))
}

/// # Errors
/// Rejects a subject key that is not a safe path component.
pub fn current_path(subject: &SubjectKey) -> Result<SitePath, PublishError> {
    SitePath::new(format!("data/current/{}.json", subject.as_str()))
}

/// # Errors
/// Rejects a body, epoch or revision id that is not a safe path component.
pub fn bundle_dir(
    body: &BodyId,
    epoch: u32,
    id: &MatchBundleRevisionId,
) -> Result<SitePath, PublishError> {
    SitePath::new(format!(
        "data/bundles/{}/{}/{}/{}",
        body.make(),
        body.model(),
        epoch,
        id.to_hex()
    ))
}

/// # Errors
/// Rejects a body, epoch or revision id that is not a safe path component.
pub fn bundle_manifest_path(
    body: &BodyId,
    epoch: u32,
    id: &MatchBundleRevisionId,
) -> Result<SitePath, PublishError> {
    Ok(bundle_dir(body, epoch, id)?.join("manifest.json"))
}

/// # Errors
/// Rejects a body, epoch or revision id that is not a safe path component.
pub fn bundle_artifacts_path(
    body: &BodyId,
    epoch: u32,
    id: &MatchBundleRevisionId,
) -> Result<SitePath, PublishError> {
    Ok(bundle_dir(body, epoch, id)?.join("artifacts.json"))
}

/// The public part of a report: no seeds, membership or dependency rows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PublishedReportSummary {
    pub format: ReportFormat,
    pub report: ModelReportId,
    pub subject: ModelSubject,
    pub candidate: ModelVersionId,
    pub capability: QualificationCapability,
    pub scope: ReportScope,
    pub rule_revision: RuleRevision,
    pub outcome: AssessmentOutcome,
    pub metrics: ReportMetrics,
    pub supported_mask: SupportedMask,
    pub limitations: Vec<Limitation>,
    pub evidence_requests: Vec<EvidenceRequest>,
    pub artifacts: Vec<ArtifactBinding>,
}

impl PublishedReportSummary {
    pub fn new(report: &ModelReport, id: ModelReportId, artifacts: Vec<ArtifactBinding>) -> Self {
        Self {
            format: ReportFormat::V1,
            report: id,
            subject: report.subject.clone(),
            candidate: report.candidate.clone(),
            capability: report.capability,
            scope: report.scope.clone(),
            rule_revision: report.provenance.rule_revision.clone(),
            outcome: report.outcome,
            metrics: report.metrics.clone(),
            supported_mask: report.supported_mask.clone(),
            limitations: report.limitations.clone(),
            evidence_requests: report.evidence_requests.clone(),
            artifacts,
        }
    }
}

/// A public proof summary of a target's eligibility; dependency rows stay private.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetEligibilitySnapshot {
    pub format: EligibilityFormat,
    pub id: EligibilitySnapshotId,
    pub target: PublishedTargetId,
    pub basis: TargetBasis,
    pub artifacts: Vec<ArtifactBinding>,
    pub base_pin: Option<BasePin>,
    pub support_uncertainty_source: Option<SourceRef>,
    pub status: TargetEligibilityStatus,
    pub reasons: Vec<TargetEligibilityReason>,
}

impl From<&TargetEligibility> for TargetEligibilitySnapshot {
    fn from(eligibility: &TargetEligibility) -> Self {
        Self {
            format: EligibilityFormat::V1,
            id: eligibility.id,
            target: eligibility.target.clone(),
            basis: eligibility.basis,
            artifacts: eligibility.artifacts.clone(),
            base_pin: eligibility.base_pin.clone(),
            support_uncertainty_source: eligibility.support_uncertainty_source.clone(),
            status: eligibility.status,
            reasons: eligibility.reasons.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedCurrentProjection {
    #[serde(rename = "use")]
    pub use_: ProjectionUse,
    pub capability: QualificationCapability,
    pub scope_key: ScopeKey,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_report: Option<ModelReportId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_candidate: Option<ModelVersionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_eligibility: Option<EligibilitySnapshotId>,
    pub pending_generation: PendingGeneration,
    pub applicability: Applicability,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleSelection {
    pub body: BodyId,
    pub epoch: u32,
    #[serde(rename = "use")]
    pub use_: ProjectionUse,
    pub bundle: MatchBundleRevisionId,
    pub scope: SelectionScope,
    pub applicability: Applicability,
}

/// Every current entry of one model subject.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectCurrentPage {
    pub format: CurrentFormat,
    pub subject: ModelSubject,
    pub projections: Vec<PublishedCurrentProjection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bundles: Vec<BundleSelection>,
}

impl SubjectCurrentPage {
    pub fn new(
        subject: ModelSubject,
        projections: Vec<PublishedCurrentProjection>,
        bundles: Vec<BundleSelection>,
    ) -> Self {
        Self {
            format: CurrentFormat::V1,
            subject,
            projections,
            bundles,
        }
    }
}

/// A published report summary and its current applicability.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SiteReport {
    pub summary: PublishedReportSummary,
    pub applicability: Applicability,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportSummaryEntry {
    pub report: ModelReportId,
    pub subject: ModelSubject,
    pub candidate: ModelVersionId,
    pub capability: QualificationCapability,
    pub outcome: AssessmentOutcome,
    pub applicability: Applicability,
    pub path: SitePath,
    pub sha256: ArtifactHash,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EligibilityEntry {
    pub id: EligibilitySnapshotId,
    pub target: PublishedTargetId,
    pub basis: TargetBasis,
    pub status: TargetEligibilityStatus,
    pub path: SitePath,
    pub sha256: ArtifactHash,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentEntry {
    pub subject: ModelSubject,
    pub path: SitePath,
    pub sha256: ArtifactHash,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleEntry {
    pub bundle: MatchBundleRevisionId,
    pub body: BodyId,
    pub epoch: u32,
    pub path: SitePath,
    pub sha256: ArtifactHash,
    pub bytes: u64,
    pub artifacts_path: SitePath,
    pub artifacts_sha256: ArtifactHash,
    pub artifacts_bytes: u64,
}

/// The canonical target of a native family, independent of consumer mode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilyHeadPage {
    pub target: FamilyTargetId,
    pub version: LookVersion,
    pub source_limited: bool,
    pub safety: CandidateSafety,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<ModelReportId>,
    pub applicability: Applicability,
}

impl FamilyHeadPage {
    /// # Errors
    /// Rejects a target version that is not a valid Look version.
    pub fn new(target: FamilyTargetId, head: &FamilyHead) -> Result<Self, PublishError> {
        Ok(Self {
            version: LookVersion::new(format!("{}.0.0", target.version.get()))?,
            target,
            source_limited: head.source_limited,
            safety: head.safety,
            report: head.report,
            applicability: head.applicability.clone(),
        })
    }
}

/// The explicit current target a catalog selector may use; absent means history only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedTargetSummary {
    pub version: LookVersion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eligibility: Option<EligibilitySnapshotId>,
    pub applicability: Applicability,
}

/// A body's pinned calibration/parameter set, independent of any Look.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedBodyState {
    pub format: BodyStateFormat,
    pub body: BodyId,
    pub version: BodyStateVersion,
    pub stage: BodyStateStage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<ArtifactEntry>,
    pub input_transform: ArtifactEntry,
    pub body_params: ArtifactEntry,
    pub gauge: ArtifactEntry,
}

/// # Errors
/// Rejects a body or state version that is not a safe path component.
pub fn body_state_path(body: &BodyId, version: BodyStateVersion) -> Result<SitePath, PublishError> {
    SitePath::new(format!(
        "data/body-states/{}/{}/{}.json",
        body.make(),
        body.model(),
        version.get()
    ))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealizationCard {
    pub subject: BodyRealizationId,
    pub revision: RealizationRevisionId,
    pub version: RealizationVersion,
    pub target: FamilyTargetId,
    pub status: RealizationStatus,
    pub safety: CandidateSafety,
    pub scope: SelectionScope,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PublishedBundleRevisionPage {
    pub manifest: MatchBundleRevision,
    pub blobs: Vec<ArtifactBlob>,
}

impl PublishedBundleRevisionPage {
    /// # Errors
    /// Rejects a manifest whose id, artifact set or bytes disagree.
    pub fn new(
        manifest: MatchBundleRevision,
        blobs: Vec<ArtifactBlob>,
    ) -> Result<Self, PublishError> {
        if manifest.id != manifest.snapshot_id() {
            return Err(PublishError::MatchBundle(
                "bundle revision id does not match its content".to_owned(),
            ));
        }
        let mut digests = Vec::with_capacity(blobs.len());
        let mut named = std::collections::BTreeSet::new();
        for blob in &blobs {
            let digest = ArtifactHash::of(&blob.bytes);
            match blob.name.as_str() {
                "body.yaml" | "transform.yaml" | "profile.yaml" | "body_params.json"
                | "gauge.json" => {
                    named.insert(blob.name.as_str());
                }
                name => {
                    if name != digest.as_str() {
                        return Err(PublishError::MatchBundle(format!(
                            "artifact `{name}` is not content-addressed"
                        )));
                    }
                }
            }
            digests.push(digest);
        }
        for required in [
            "body.yaml",
            "transform.yaml",
            "body_params.json",
            "gauge.json",
        ] {
            if !named.contains(required) {
                return Err(PublishError::MatchBundle(format!(
                    "bundle has no `{required}`"
                )));
            }
        }
        let expected = 4
            + usize::from(named.contains("profile.yaml"))
            + manifest
                .sims
                .iter()
                .map(|sim| {
                    2 + usize::from(sim.covariance.is_some())
                        + usize::from(sim.realization_params.is_some())
                })
                .sum::<usize>();
        if blobs.len() != expected {
            return Err(PublishError::MatchBundle(
                "bundle revision bytes do not match its manifest".to_owned(),
            ));
        }
        let body_params = blobs
            .iter()
            .find(|blob| blob.name.as_str() == "body_params.json")
            .map(|blob| ArtifactHash::of(&blob.bytes))
            .ok_or_else(|| PublishError::MatchBundle("bundle has no body parameters".to_owned()))?;
        for sim in &manifest.sims {
            if sim.body_params.as_ref().map(|hash| hash.to_hex())
                != Some(body_params.as_str().to_owned())
            {
                return Err(PublishError::MatchBundle(format!(
                    "sim {} does not pin the bundle's body parameters",
                    sim.native_sim
                )));
            }
            for hash in [
                Some(&sim.base_artifact),
                Some(&sim.control_response),
                sim.covariance.as_ref(),
                sim.realization_params.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                if !digests
                    .iter()
                    .any(|digest| digest.as_str() == hash.to_hex())
                {
                    return Err(HashMismatch::NotListed {
                        name: crate::ArtifactName::new(hash.to_hex())?,
                    }
                    .into());
                }
            }
        }
        if manifest.manifest_hash != manifest.id {
            return Err(PublishError::MatchBundle(
                "bundle manifest hash does not equal its id".to_owned(),
            ));
        }
        Ok(Self { manifest, blobs })
    }

    pub fn to_json(&self) -> Result<Vec<u8>, PublishError> {
        Ok(self.manifest.manifest().into_bytes())
    }

    /// # Errors
    /// Propagates a serializer failure.
    pub fn artifacts_json(&self) -> Result<Vec<u8>, PublishError> {
        let mut entries = Vec::with_capacity(self.blobs.len());
        for blob in &self.blobs {
            entries.push(crate::manifest::ArtifactEntry {
                name: blob.name.clone(),
                sha256: ArtifactHash::of(&blob.bytes),
                bytes: blob.bytes.len() as u64,
            });
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(serde_json::to_vec_pretty(&entries)?)
    }
}
