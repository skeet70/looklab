// SPDX-License-Identifier: AGPL-3.0-or-later
//! Production common-family and body-realization fitting. A camera-local fit
//! never becomes a family target, and experimental evidence cannot enter a cohort.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use looklab_body::{Cct, Delta6, InputTransform, TransformVersion};
use looklab_export::card::{CardMeta, CardOptions, card_status, write_card};
use looklab_export::checked_appearance;
use looklab_fit::gauge::{ComponentGauge, WbModeClass};
use looklab_fit::hierarchical::{
    BodyData, BodyParameters, BodyPin, BodyResume, BodyWarmStart, FamilyData, FamilyFirmwareId,
    FamilyMode, FirmwareId, GaugeScope, HierImage, HierInput, HierOptions, HierState,
    HierStateGeneration, RealizationData, RealizationWarmStart, fit_hierarchical, fit_realizations,
};
use looklab_fit::{
    ExposureGauge, ExposureModeId, FitOptions, ImageNuisances, ImageProjection, PreviousSupport,
    ProbeUncertainty, firmware_images, post_clip,
};
use looklab_model::file::{BodyProvenance, Provenance, RootProvenance, identity};
use looklab_model::realization::{ControlResponseModel, ControlValue, Settings, encode_crm};
use looklab_model::{
    CameraRendering, Density, Look, LookDocument, ProbeCovariance, ProbeId, ProbeScalars,
    UncertaintyData,
};
use looklab_publish::{
    BodyUse, CARD_ARTIFACT, PREVIEW_LUT_ARTIFACT, ProvenanceSummary, RELEASE_ARTIFACT,
    bake_preview_lut,
};
use looklab_schema::{
    Affects, ArtifactBinding, ArtifactRole, AssessmentOutcome, BodyRealizationId, BodyStatePins,
    BodyStateRecord, BodyStateStage, BodyStateVersion, CandidateSafety, CohortBasis, CohortMember,
    CohortRole, ContributionRef, Control, ControlEpochId, ControlType, DependencyGeneration,
    DependencyKind, DependencyRef, DependencyTarget, Epoch, EvidenceRequest, EvidenceRequestKind,
    ExposureModeKey, ExposureModeOffset, FamilyHead, FamilyTargetId, FirmwareOffset, GaugeId,
    IdentityResolutionRef, ImageRef, LookId, MatchParams, MatchParamsFormat,
    MatchRealizationParams, MatchStage, MatchTransform, ModelReportId, ModelSubject,
    ModelVersionId, PendingGeneration, ProbeSetId, ProfileId, ProjectionUse,
    PublishedSchemaSnapshot, PublishedTargetId, QualificationCapability, RealizationStatus,
    RealizationVersion, SafetyCheck, ScopeKey, Sha256Hex, StandingRef, SubjectKey, TargetBasis,
    TargetEligibility, TargetEligibilityStatus, TargetVersion, WbModeKey, WbModeOffset,
};
use looklab_server::admin::{self, ManifestArtifact};
use looklab_server::model_store::{
    self, EvidenceBlob, EvidenceGroupId, EvidenceMembership, EvidenceRole as StoredEvidenceRole,
    ExpectedSelection, FamilyCohortState, FitCheckpoint, JobLease, NewRealizationRevision,
    RealizationSelection, RealizationStage, SelectionOutcome, SelectionRef,
};
use looklab_server::{BlobError, BlobHash, BlobStore, ContributionId, CredentialId, FsBlobStore};
use looklab_trust::{
    GateInput, GatedUnion, ProbeEstimate as TrustProbeEstimate, ProbeSupport as TrustProbeSupport,
    Reputation, ReputationUpdate, TrustParams, VersionId, VersionProbes, gate_release,
};
use looklab_upload::{BodyId as UploadBodyId, LookSlug};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};

use crate::assessment::PinnedModes;
use crate::body::TransformDir;
use crate::bundle;
use crate::clock::{now_unix, rfc3339};
use crate::config::WorkerConfig;
use crate::db::{self, ImageOutcome};
use crate::error::{Result, WorkerError};
use crate::jobs::{JobId, JobKind, JobOwnership, JobPayload};
use crate::model_assessment::diagnostics::{
    CameraLocalDiagnostics, DIAGNOSTIC_FORMAT, DiagnosticBudget, DiagnosticProbe, DiagnosticSource,
};
use crate::pool::{EvidenceRole, EvidenceScope, LookPool, PoolImage};
use crate::refit::{GaugeConvention, LOOK_ARTIFACT, RefitOutcome, ReleaseRecord, StoredProbe};
use crate::roots::{self, RootData};
use crate::source_identity;
use crate::weights::{self, ConsistencyScores, WeightedFamily};

pub const PARTITION_POLICY: &str = "looklab-group-partition/1";

const FAMILY_BLOCK: &str = "family";
const REALIZATION_BLOCK: &str = "realization";
const PROBE_SET: &str = "v1";

/// The fitter class for an observation whose WB mode was not observed. It is
/// never serialized into published match parameters.
const UNKNOWN_WB_CLASS: WbModeClass = WbModeClass(u32::MAX);

#[derive(Debug, Clone)]
struct AdmissionBinding {
    candidate: ModelVersionId,
}

#[derive(Debug, Clone)]
struct ScopeMember {
    body: UploadBodyId,
    realization: BodyRealizationId,
    scope: EvidenceScope,
    admission: Option<AdmissionBinding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanKind {
    Seed,
    SeedAdvance,
    Qualified,
}

struct FamilyPlan {
    family: LookSlug,
    family_id: LookId,
    head: Option<FamilyHead>,
    kind: PlanKind,
    members: Vec<ScopeMember>,
    nonmembers: Vec<ScopeMember>,
    missing: Vec<String>,
    cohort_report: Option<ModelReportId>,
}

struct TouchedEpoch {
    body: UploadBodyId,
    epoch: u32,
    snapshot_version: u64,
    snapshot_root: PathBuf,
    published_state_version: BodyStateVersion,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredResume {
    body: Option<BodyWarmStart>,
    realization: RealizationWarmStart,
    crm: Option<ControlResponseModel>,
    state: HierState,
    membership: EvidenceMembership,
    #[serde(default)]
    sigma: Vec<(looklab_trust::EffectiveContributorId, f64)>,
    #[serde(default)]
    inventory: Option<BodyModes>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct GaugeArtifact {
    designated: UploadBodyId,
    exposure_mode: u32,
    wb_mode: u32,
    #[serde(default)]
    inventory: Option<BodyModes>,
    anchored_delta: bool,
    gauge: f64,
    convention: GaugeConvention,
}

#[derive(Default)]
struct Lineage {
    body_warm: BTreeMap<UploadBodyId, SavedWarm>,
    published_body_warm: BTreeMap<UploadBodyId, SavedWarm>,
    realization_warm: BTreeMap<BodyRealizationId, SavedRealization>,
    published_realization_warm: BTreeMap<BodyRealizationId, SavedRealization>,
    crm: BTreeMap<BodyRealizationId, ControlResponseModel>,
    published_crm: BTreeMap<BodyRealizationId, ControlResponseModel>,
    gauge: Option<SavedGauge>,
    sigma: Vec<(looklab_trust::EffectiveContributorId, f64)>,
    firmware: Vec<(FamilyFirmwareId, [f64; 2])>,
    qualified: bool,
    body_state: BTreeMap<UploadBodyId, (BodyStateVersion, Sha256Hex, u32)>,
    body_stage: BTreeMap<UploadBodyId, BodyStateStage>,
    authoritative_missing: BTreeSet<BodyRealizationId>,
}

struct TargetSnapshot {
    look: Look,
    record: ReleaseRecord,
    document: Vec<u8>,
}

struct PersistedBody {
    native: Option<BodyStateVersion>,
    published: BodyStateVersion,
    gauge: Sha256Hex,
    transform_version: u32,
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
struct BodyModes {
    exposure: BTreeMap<ExposureModeId, ExposureModeKey>,
    wb: BTreeMap<WbModeClass, WbModeKey>,
}

#[derive(Clone, Debug)]
struct SavedWarm {
    inventory: BodyModes,
    warm: BodyWarmStart,
}

#[derive(Clone, Debug)]
struct SavedGauge {
    inventory: BodyModes,
    gauge: ComponentGauge,
}

#[derive(Clone, Debug)]
struct SavedRealization {
    inventory: BodyModes,
    warm: RealizationWarmStart,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredCheckpoint {
    state: HierState,
    inventories: BTreeMap<UploadBodyId, BodyModes>,
}

#[derive(Clone)]
struct Partition {
    training: LookPool,
    assessment: BTreeSet<EvidenceGroupId>,
    training_groups: BTreeSet<EvidenceGroupId>,
}

/// Fit every dirty or explicitly requested family and realization.
pub async fn run(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    bodies: &[UploadBodyId],
    ownership: &JobOwnership,
) -> Result<Vec<(UploadBodyId, LookSlug, RefitOutcome)>> {
    run_inner(db_pool, config, bodies, ownership, false, None).await
}

/// Bypasses the unchanged-fingerprint shortcut for moderator revert requests.
pub async fn run_forced(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    ownership: &JobOwnership,
) -> Result<Vec<(UploadBodyId, LookSlug, RefitOutcome)>> {
    let requests = db::pending_refit_requests(db_pool).await?;
    let looks: BTreeSet<LookSlug> = requests
        .iter()
        .map(|request| request.look.clone())
        .collect();
    let outcomes = run_inner(db_pool, config, &[], ownership, true, Some(looks)).await?;
    for request in requests {
        db::clear_refit_request(db_pool, &request.look, request.generation).await?;
    }
    Ok(outcomes)
}

/// Enqueues a refit for unconsumed per-subject generations; queued work only
/// coalesces when it covers every pending subject's body.
pub async fn enqueue_pending(
    db_pool: &SqlitePool,
    _config: &WorkerConfig,
    now: i64,
) -> Result<Option<JobId>> {
    let mut tx = db_pool.begin_with("BEGIN IMMEDIATE").await?;
    let pending: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT p.subject_key, s.body FROM model_pending_work p
         JOIN model_subjects s ON s.key = p.subject_key
         LEFT JOIN model_pending_consumption c ON c.subject_key = p.subject_key
         WHERE p.generation > COALESCE(c.generation, 0)",
    )
    .fetch_all(&mut *tx)
    .await?;
    if pending.is_empty() {
        tx.commit().await?;
        return Ok(None);
    }
    let pending_bodies: BTreeSet<UploadBodyId> = pending
        .iter()
        .filter_map(|(_, body)| body.as_ref())
        .filter_map(|body| UploadBodyId::new(body).ok())
        .collect();
    let requires_all = pending.iter().any(|(_, body)| body.is_none());
    let payloads: Vec<String> =
        sqlx::query_scalar("SELECT payload FROM jobs WHERE kind = ? AND status = 'queued'")
            .bind(JobKind::NightlyRefit.as_str())
            .fetch_all(&mut *tx)
            .await?;
    for payload in payloads {
        let Ok(JobPayload::NightlyRefit { bodies }) =
            JobPayload::decode(&JobKind::NightlyRefit, &payload)
        else {
            continue;
        };
        let covers = if bodies.is_empty() {
            true
        } else {
            !requires_all && pending_bodies.iter().all(|body| bodies.contains(body))
        };
        if covers {
            tx.commit().await?;
            return Ok(None);
        }
    }
    let (kind, payload) = JobPayload::NightlyRefit { bodies: Vec::new() }.encode()?;
    let id = crate::jobs::insert(&mut tx, &kind, &payload, now).await?;
    tx.commit().await?;
    Ok(Some(id))
}

fn native_group_count(pools: &[LookPool]) -> Result<usize> {
    let mut groups = BTreeSet::new();
    for pool in pools {
        for image in &pool.images {
            if image.role == EvidenceRole::Native && image.is_eligible() {
                groups.insert(evidence_group(image)?);
            }
        }
    }
    Ok(groups.len())
}

struct ObservedPlan {
    family: PendingGeneration,
    members: BTreeMap<BodyRealizationId, PendingGeneration>,
}

async fn observe_plan(db_pool: &SqlitePool, plan: &FamilyPlan) -> Result<ObservedPlan> {
    let mut conn = db_pool.acquire().await?;
    let family = SubjectKey::for_family(&plan.family_id);
    let family = model_store::pending_generation(&mut conn, &family).await?;
    let mut members = BTreeMap::new();
    for member in plan.members.iter().chain(plan.nonmembers.iter()) {
        let subject = ModelSubject::BodyRealization {
            realization: member.realization.clone(),
        }
        .key();
        members.insert(
            member.realization.clone(),
            model_store::pending_generation(&mut conn, &subject).await?,
        );
    }
    Ok(ObservedPlan { family, members })
}

async fn ack_observed_plans(
    db_pool: &SqlitePool,
    plans: &[&FamilyPlan],
    observed: &BTreeMap<LookId, ObservedPlan>,
    now: i64,
) -> Result<()> {
    for plan in plans {
        if let Some(entry) = observed.get(&plan.family_id) {
            ack_observed_plan(db_pool, plan, entry, now).await?;
        }
    }
    Ok(())
}

async fn ack_observed_plan(
    db_pool: &SqlitePool,
    plan: &FamilyPlan,
    observed: &ObservedPlan,
    now: i64,
) -> Result<()> {
    let mut conn = db_pool.acquire().await?;
    model_store::register_subject(
        &mut conn,
        &ModelSubject::SharedFamily {
            family: plan.family_id.clone(),
        },
        now,
    )
    .await?;
    ack_pending(
        &mut conn,
        &SubjectKey::for_family(&plan.family_id),
        observed.family,
        now,
    )
    .await?;
    for member in plan.members.iter().chain(plan.nonmembers.iter()) {
        if let Some(generation) = observed.members.get(&member.realization) {
            let subject = ModelSubject::BodyRealization {
                realization: member.realization.clone(),
            }
            .key();
            ack_pending(&mut conn, &subject, *generation, now).await?;
        }
    }
    Ok(())
}

async fn ack_subject_generation(
    db_pool: &SqlitePool,
    subject: &SubjectKey,
    generation: PendingGeneration,
    now: i64,
) -> Result<()> {
    let mut conn = db_pool.acquire().await?;
    ack_pending(&mut conn, subject, generation, now).await
}

async fn note_missing_capture_groups(
    conn: &mut sqlx::SqliteConnection,
    family: &LookId,
    members: &[ScopeMember],
    count: usize,
    now: i64,
) -> Result<()> {
    let observed = Some(u32::try_from(count).unwrap_or(u32::MAX));
    for member in members {
        let subject_model = ModelSubject::BodyRealization {
            realization: member.realization.clone(),
        };
        let subject = subject_model.key();
        model_store::register_subject(conn, &subject_model, now).await?;
        if count < 2 {
            let needed = if count == 0 {
                "An independent capture from a different scene is needed"
            } else {
                "A second independent capture from a different scene is needed"
            };
            let detail = format!(
                "{}/{}/{}: {count} independent RAW capture(s) exist. {needed} to check the look against photos it was not fitted to. One RAW re-rendered to several film simulations counts as one capture.",
                member.body.as_str(),
                member.realization.epoch.get(),
                member.realization.native_sim.as_str()
            );
            model_store::record_active_evidence_request(
                conn,
                &subject,
                QualificationCapability::NativePrediction,
                &EvidenceRequest {
                    kind: EvidenceRequestKind::MissingIndependentCaptures,
                    detail,
                },
                observed,
                now,
            )
            .await?;
        } else {
            model_store::resolve_active_evidence_request(
                conn,
                &subject,
                EvidenceRequestKind::MissingIndependentCaptures,
            )
            .await?;
        }
    }
    let family_model = ModelSubject::SharedFamily {
        family: family.clone(),
    };
    let family_subject = family_model.key();
    model_store::register_subject(conn, &family_model, now).await?;
    if count < 2 {
        let bodies: Vec<&str> = members
            .iter()
            .map(|member| member.body.as_str())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let bodies = if bodies.is_empty() {
            "the supported body".to_owned()
        } else {
            bodies.join(", ")
        };
        let captures = if count == 1 {
            "1 independent RAW capture exists".to_owned()
        } else {
            format!("{count} independent RAW captures exist")
        };
        let needed = if count == 0 {
            "An independent capture from a different scene is needed"
        } else {
            "A second independent capture from a different scene is needed"
        };
        let detail = format!(
            "{bodies}: {captures}. {needed} to check the look against photos it was not fitted to. One RAW re-rendered to several film simulations counts as one capture."
        );
        model_store::record_active_evidence_request(
            conn,
            &family_subject,
            QualificationCapability::NativePrediction,
            &EvidenceRequest {
                kind: EvidenceRequestKind::MissingIndependentCaptures,
                detail,
            },
            observed,
            now,
        )
        .await?;
    } else {
        model_store::resolve_active_evidence_request(
            conn,
            &family_subject,
            EvidenceRequestKind::MissingIndependentCaptures,
        )
        .await?;
    }
    Ok(())
}

async fn ack_pending(
    conn: &mut sqlx::SqliteConnection,
    subject: &SubjectKey,
    generation: PendingGeneration,
    now: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO model_pending_consumption (subject_key, generation, consumed_at)
         VALUES (?, ?, ?)
         ON CONFLICT (subject_key) DO UPDATE
         SET generation = MAX(model_pending_consumption.generation, excluded.generation),
             consumed_at = excluded.consumed_at",
    )
    .bind(subject.as_str())
    .bind(i64::try_from(generation.get()).unwrap_or(i64::MAX))
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

struct PendingObservation {
    subject: SubjectKey,
    body: Option<UploadBodyId>,
    family: Option<LookSlug>,
    generation: PendingGeneration,
}

async fn observe_pending(db_pool: &SqlitePool) -> Result<Vec<PendingObservation>> {
    Ok(sqlx::query(
        "SELECT p.subject_key, s.body, s.family, p.generation FROM model_pending_work p
         JOIN model_subjects s ON s.key = p.subject_key
         LEFT JOIN model_pending_consumption c ON c.subject_key = p.subject_key
         WHERE p.generation > COALESCE(c.generation, 0)",
    )
    .try_map(|row: sqlx::sqlite::SqliteRow| {
        let subject = SubjectKey::new(row.try_get::<String, _>("subject_key")?)
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
        let body = row
            .try_get::<Option<&str>, _>("body")?
            .map(UploadBodyId::new)
            .transpose()
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
        let family = row
            .try_get::<Option<&str>, _>("family")?
            .map(LookSlug::new)
            .transpose()
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
        let generation = PendingGeneration::new(
            u64::try_from(row.try_get::<i64, _>("generation")?)
                .map_err(|error| sqlx::Error::Decode(Box::new(error)))?,
        );
        Ok(PendingObservation {
            subject,
            body,
            family,
            generation,
        })
    })
    .fetch_all(db_pool)
    .await?)
}

async fn ack_unplanned_pending(
    db_pool: &SqlitePool,
    pending: &[PendingObservation],
    plans: &[FamilyPlan],
    bodies: &[UploadBodyId],
    looks: Option<&BTreeSet<LookSlug>>,
    now: i64,
) -> Result<()> {
    let mut represented: BTreeSet<String> = BTreeSet::new();
    for plan in plans {
        represented.insert(SubjectKey::for_family(&plan.family_id).as_str().to_owned());
        for member in plan.members.iter().chain(plan.nonmembers.iter()) {
            represented.insert(
                ModelSubject::BodyRealization {
                    realization: member.realization.clone(),
                }
                .key()
                .as_str()
                .to_owned(),
            );
        }
    }
    let scoped = !bodies.is_empty() || looks.is_some();
    let mut conn = db_pool.acquire().await?;
    for observation in pending {
        if represented.contains(observation.subject.as_str()) {
            continue;
        }
        if scoped {
            let body_examined = observation
                .body
                .as_ref()
                .is_some_and(|body| bodies.contains(body));
            let look_examined = observation
                .family
                .as_ref()
                .is_some_and(|family| looks.is_some_and(|looks| looks.contains(family)));
            if !body_examined && !look_examined {
                continue;
            }
        }
        ack_pending(&mut conn, &observation.subject, observation.generation, now).await?;
    }
    Ok(())
}

async fn run_inner(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    bodies: &[UploadBodyId],
    ownership: &JobOwnership,
    force: bool,
    only_looks: Option<BTreeSet<LookSlug>>,
) -> Result<Vec<(UploadBodyId, LookSlug, RefitOutcome)>> {
    let pending = observe_pending(db_pool).await?;
    let plans = discover(db_pool, config, bodies).await?;
    let (snapshot_version, snapshot_root) =
        crate::body::published_snapshot(db_pool, config).await?;
    let plans: Vec<FamilyPlan> = plans
        .into_iter()
        .filter(|plan| {
            only_looks
                .as_ref()
                .is_none_or(|looks| looks.contains(&plan.family))
        })
        .collect();
    let mut outcomes = Vec::new();
    let mut touched: Vec<TouchedEpoch> = Vec::new();
    for component in group_components(&plans) {
        // Observations bound the post-fit acknowledgement to the generations the
        // fit examined; arrivals during fitting stay pending for a successor.
        let mut observed: BTreeMap<LookId, ObservedPlan> = BTreeMap::new();
        for plan in &component {
            observed.insert(plan.family_id.clone(), observe_plan(db_pool, plan).await?);
        }
        let free: Vec<&FamilyPlan> = component
            .iter()
            .copied()
            .filter(|plan| !plan.members.is_empty())
            .collect();
        if free.len() > 1 {
            match fit_component(
                db_pool,
                config,
                ownership,
                &free,
                force,
                snapshot_version,
                &snapshot_root,
            )
            .await
            {
                Ok((mut result, mut epochs)) => {
                    outcomes.append(&mut result);
                    touched.append(&mut epochs);
                }
                Err(error) => {
                    for plan in &free {
                        tracing::warn!(family = %plan.family, %error, "component fit failed");
                        for member in &plan.members {
                            outcomes.push((
                                member.body.clone(),
                                plan.family.clone(),
                                RefitOutcome::Unavailable(error.to_string()),
                            ));
                        }
                    }
                }
            }
        } else {
            for plan in &free {
                match fit_free(
                    db_pool,
                    config,
                    ownership,
                    plan,
                    force,
                    snapshot_version,
                    &snapshot_root,
                )
                .await
                {
                    Ok((mut result, mut epochs)) => {
                        outcomes.append(&mut result);
                        touched.append(&mut epochs);
                    }
                    Err(error) => {
                        tracing::warn!(family = %plan.family, %error, "family fit failed");
                        for member in &plan.members {
                            outcomes.push((
                                member.body.clone(),
                                plan.family.clone(),
                                RefitOutcome::Unavailable(error.to_string()),
                            ));
                        }
                    }
                }
            }
        }
        discard_unselected_reservations(db_pool, &component).await?;
        for plan in &component {
            let Some(observed) = observed.get(&plan.family_id) else {
                continue;
            };
            if plan.members.is_empty() && plan.nonmembers.is_empty() {
                for reason in &plan.missing {
                    tracing::warn!(family = %plan.family, %reason, "model fit has no evidence");
                }
                continue;
            }
            for reason in &plan.missing {
                if let Some(member) = plan.members.first().or_else(|| plan.nonmembers.first()) {
                    outcomes.push((
                        member.body.clone(),
                        plan.family.clone(),
                        RefitOutcome::Unavailable(reason.clone()),
                    ));
                }
            }
            ack_observed_plan(db_pool, plan, observed, now_unix()).await?;
            for member in &plan.nonmembers {
                match fit_pinned(
                    db_pool,
                    config,
                    ownership,
                    plan,
                    member,
                    false,
                    snapshot_version,
                    &snapshot_root,
                )
                .await
                {
                    Ok((outcome, mut epochs)) => {
                        if outcome != RefitOutcome::Skipped {
                            outcomes.push((member.body.clone(), plan.family.clone(), outcome));
                        }
                        touched.append(&mut epochs);
                    }
                    Err(error) => {
                        tracing::warn!(
                            family = %plan.family,
                            body = %member.body,
                            %error,
                            "pinned realization fit failed"
                        );
                        outcomes.push((
                            member.body.clone(),
                            plan.family.clone(),
                            RefitOutcome::Unavailable(error.to_string()),
                        ));
                    }
                }
            }
        }
    }
    ack_unplanned_pending(
        db_pool,
        &pending,
        &plans,
        bodies,
        only_looks.as_ref(),
        now_unix(),
    )
    .await?;
    register_bundles(db_pool, config, ownership, &touched).await?;
    Ok(outcomes)
}

fn group_components(plans: &[FamilyPlan]) -> Vec<Vec<&FamilyPlan>> {
    let mut by_body: BTreeMap<UploadBodyId, Vec<usize>> = BTreeMap::new();
    for (index, plan) in plans.iter().enumerate() {
        for member in &plan.members {
            by_body.entry(member.body.clone()).or_default().push(index);
        }
    }
    let mut visited = vec![false; plans.len()];
    let mut components = Vec::new();
    for start in 0..plans.len() {
        if visited[start] {
            continue;
        }
        let mut queue = vec![start];
        let mut component = Vec::new();
        visited[start] = true;
        while let Some(index) = queue.pop() {
            component.push(&plans[index]);
            for member in &plans[index].members {
                for next in by_body.get(&member.body).into_iter().flatten() {
                    let same_qualification = match (plans[index].kind, plans[*next].kind) {
                        (PlanKind::Qualified, PlanKind::Qualified) => true,
                        (PlanKind::Qualified, _) | (_, PlanKind::Qualified) => false,
                        _ => true,
                    };
                    if same_qualification && !visited[*next] {
                        visited[*next] = true;
                        queue.push(*next);
                    }
                }
            }
        }
        components.push(component);
    }
    components
}

/// Recipe rows are published by the member's own realization revision.
fn has_out_of_scope_native(experimental: &LookPool, scope: &EvidenceScope) -> bool {
    experimental.images.iter().any(|image| {
        image.role == EvidenceRole::Native
            && image.is_eligible()
            && scope.excludes_samples(experimental, image)
    })
}

async fn discard_unselected_reservations(
    db_pool: &SqlitePool,
    plans: &[&FamilyPlan],
) -> Result<()> {
    let mut tx = model_store::begin_immediate(db_pool).await?;
    for plan in plans {
        let subject = SubjectKey::for_family(&plan.family_id);
        let Some(reserved) = model_store::reserved_version(&mut tx, &subject).await? else {
            continue;
        };
        if reserved.kind != model_store::ReservedVersionKind::Family {
            continue;
        }
        let Some(version) = TargetVersion::new(reserved.version) else {
            continue;
        };
        model_store::discard_reserved_family_version(&mut tx, &plan.family_id, version).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn discover(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    bodies: &[UploadBodyId],
) -> Result<Vec<FamilyPlan>> {
    let mut conn = db_pool.acquire().await?;
    let heads = model_store::family_heads(&mut conn).await?;
    let subjects = model_store::subjects(&mut conn).await?;
    drop(conn);

    let mut head_by_family: BTreeMap<LookId, FamilyHead> = BTreeMap::new();
    for head in heads {
        head_by_family.insert(head.target.family.clone(), head);
    }
    let mut families: BTreeSet<LookSlug> = head_by_family
        .keys()
        .filter_map(|family| LookSlug::new(family.as_str()).ok())
        .collect();

    let data = crate::body::published_data_dir(db_pool, config).await?;
    for contribution in db::all_contributions(db_pool).await? {
        let resources = match data.load(&contribution.body) {
            Ok(resources) => resources,
            Err(error) => {
                tracing::warn!(body = %contribution.body, %error, "skipping body resources");
                continue;
            }
        };
        if let Some((_, family)) = crate::pool::realization_scope(&resources, &contribution)? {
            families.insert(family);
        }
    }

    let mut registered: BTreeMap<LookSlug, BTreeSet<(UploadBodyId, BodyRealizationId)>> =
        BTreeMap::new();
    for subject in &subjects {
        let ModelSubject::BodyRealization { realization } = subject else {
            continue;
        };
        let resources = match data.load(&realization.body) {
            Ok(resources) => resources,
            Err(error) => {
                tracing::warn!(body = %realization.body, %error, "skipping registered scope");
                continue;
            }
        };
        let Some(epoch) = resources
            .schema
            .epochs
            .iter()
            .find(|epoch| epoch.id == realization.epoch.get())
        else {
            continue;
        };
        let Some(native) = epoch.native_sim(&realization.native_sim) else {
            continue;
        };
        let Ok(family) = LookSlug::new(native.look.as_str()) else {
            continue;
        };
        families.insert(family.clone());
        registered
            .entry(family)
            .or_default()
            .insert((realization.body.clone(), realization.clone()));
    }

    let mut plans = Vec::new();
    for family in families {
        let Ok(family_id) = LookId::new(family.as_str()) else {
            continue;
        };
        let head = head_by_family.get(&family_id).cloned();
        let mut evidence_scopes: BTreeSet<(UploadBodyId, BodyRealizationId)> = BTreeSet::new();
        for scope in crate::pool::family_realizations(db_pool, config, &family).await? {
            evidence_scopes.insert(scope);
        }
        if let Some(scopes) = registered.get(&family) {
            evidence_scopes.extend(scopes.iter().cloned());
        }
        let evidence: Vec<(UploadBodyId, BodyRealizationId)> =
            evidence_scopes.into_iter().collect();
        if evidence.is_empty() {
            continue;
        }
        if !bodies.is_empty() && !evidence.iter().any(|(body, _)| bodies.contains(body)) {
            continue;
        }

        let mut plan = FamilyPlan {
            family: family.clone(),
            family_id: family_id.clone(),
            head: head.clone(),
            kind: PlanKind::Seed,
            members: Vec::new(),
            nonmembers: Vec::new(),
            missing: Vec::new(),
            cohort_report: None,
        };
        match &head {
            None => {
                plan.kind = PlanKind::Seed;
                for (body, realization) in &evidence {
                    plan.members.push(ScopeMember {
                        body: body.clone(),
                        realization: realization.clone(),
                        scope: EvidenceScope::Seed,
                        admission: None,
                    });
                }
            }
            Some(head) => {
                let mut conn = db_pool.acquire().await?;
                let seed_cohort =
                    model_store::family_cohort(&mut conn, &family_id, CohortRole::Seed).await?;
                let admitted_cohort =
                    model_store::family_cohort(&mut conn, &family_id, CohortRole::Admitted).await?;
                drop(conn);
                let seed_bodies: BTreeSet<UploadBodyId> = seed_cohort
                    .members
                    .iter()
                    .filter_map(|member| match member {
                        CohortMember::Seed { body } => Some(body.clone()),
                        CohortMember::Admitted { .. } => None,
                    })
                    .collect();
                let admitted_usable = matches!(&admitted_cohort.state, FamilyCohortState::Current)
                    && admitted_cohort
                        .members
                        .iter()
                        .any(|member| matches!(member, CohortMember::Admitted { .. }));
                let mut member_keys: BTreeSet<(UploadBodyId, BodyRealizationId)> = BTreeSet::new();
                let mut admitted_bindings: BTreeMap<
                    (UploadBodyId, BodyRealizationId),
                    AdmissionBinding,
                > = BTreeMap::new();
                if admitted_usable {
                    plan.kind = PlanKind::Qualified;
                    plan.cohort_report = admitted_cohort.report;
                    let mut conn = db_pool.acquire().await?;
                    for member in &admitted_cohort.members {
                        let CohortMember::Admitted {
                            realization: version,
                            report: report_id,
                            scope,
                            supported_mask,
                        } = member
                        else {
                            continue;
                        };
                        let realization = &version.realization;
                        let body = realization.body.clone();
                        let Some(stored) = model_store::report(&mut conn, report_id).await? else {
                            plan.missing.push(format!(
                                "admitted report {report_id} for {} is missing",
                                family
                            ));
                            continue;
                        };
                        let candidate_matches = matches!(
                            &stored.report.candidate,
                            ModelVersionId::BodyRealization { realization: candidate }
                                if candidate == version
                        );
                        let scope_covered = scope
                            .points
                            .iter()
                            .all(|point| stored.report.scope.points.contains(point))
                            && stored.report.scope.output_encoding == scope.output_encoding
                            && scope
                                .assessed_bodies
                                .iter()
                                .all(|body| stored.report.scope.assessed_bodies.contains(body));
                        let mask_covered = supported_mask
                            .iter()
                            .all(|cell| stored.report.supported_mask.contains(cell));
                        if !scope_covered
                            || !mask_covered
                            || !candidate_matches
                            || !matches!(stored.report.outcome, AssessmentOutcome::Pass)
                            || !model_store::report_applicable(&mut conn, report_id).await?
                        {
                            plan.missing.push(format!(
                                "admitted report {report_id} for {} does not cover its admitted scope and mask",
                                family
                            ));
                            continue;
                        }
                        let Some(pinned) =
                            pinned_modes_for_candidate(&mut conn, config, &stored.report.candidate)
                                .await?
                        else {
                            plan.missing.push(format!(
                                "admitted report {report_id} for {} has no recoverable physical pins",
                                family
                            ));
                            continue;
                        };
                        member_keys.insert((body.clone(), realization.clone()));
                        let binding = AdmissionBinding {
                            candidate: ModelVersionId::BodyRealization {
                                realization: version.clone(),
                            },
                        };
                        admitted_bindings
                            .insert((body.clone(), realization.clone()), binding.clone());
                        let qualified_scope = EvidenceScope::Qualified {
                            report: scope.as_ref().clone(),
                            mask: supported_mask.clone(),
                            pinned: pinned.clone(),
                        };
                        let out_of_scope = match crate::pool::build(
                            db_pool,
                            config,
                            &body,
                            &plan.family,
                            realization,
                            &EvidenceScope::Experimental,
                        )
                        .await
                        {
                            Ok(experimental) => {
                                has_out_of_scope_native(&experimental, &qualified_scope)
                            }
                            Err(_) => false,
                        };
                        plan.members.push(ScopeMember {
                            body: body.clone(),
                            realization: realization.clone(),
                            scope: qualified_scope,
                            admission: Some(binding.clone()),
                        });
                        if out_of_scope {
                            plan.nonmembers.push(ScopeMember {
                                body: body.clone(),
                                realization: realization.clone(),
                                scope: EvidenceScope::Experimental,
                                admission: Some(binding),
                            });
                        }
                    }
                    drop(conn);
                }
                if admitted_usable && !plan.members.is_empty() {
                    if seed_bodies.is_empty() {
                        plan.missing.push(format!(
                            "the source-limited seed cohort of {} has no recorded members",
                            family
                        ));
                    }
                    for (body, realization) in &evidence {
                        if member_keys.contains(&(body.clone(), realization.clone()))
                            || !seed_bodies.contains(body)
                        {
                            continue;
                        }
                        plan.members.push(ScopeMember {
                            body: body.clone(),
                            realization: realization.clone(),
                            scope: EvidenceScope::Seed,
                            admission: None,
                        });
                        member_keys.insert((body.clone(), realization.clone()));
                    }
                    for (body, realization) in &evidence {
                        if !member_keys.contains(&(body.clone(), realization.clone())) {
                            plan.nonmembers.push(ScopeMember {
                                body: body.clone(),
                                realization: realization.clone(),
                                scope: EvidenceScope::Experimental,
                                admission: admitted_bindings
                                    .get(&(body.clone(), realization.clone()))
                                    .cloned(),
                            });
                        }
                    }
                } else {
                    plan.kind = PlanKind::SeedAdvance;
                    plan.cohort_report = None;
                    plan.members.clear();
                    plan.nonmembers.clear();
                    match &admitted_cohort.state {
                        FamilyCohortState::Empty if head.source_limited => {}
                        FamilyCohortState::Empty => plan.missing.push(format!(
                            "{} has no admitted cohort; refitting the source-limited seed cohort",
                            family
                        )),
                        FamilyCohortState::Inapplicable { reason, .. } => plan.missing.push(format!(
                            "the admitted cohort of {} is not applicable ({reason:?}); refitting the source-limited seed cohort",
                            family
                        )),
                        FamilyCohortState::Current => plan.missing.push(format!(
                            "the admitted cohort of {} has no applicable exact report domain; refitting the source-limited seed cohort",
                            family
                        )),
                    }
                    if seed_bodies.is_empty() {
                        plan.missing.push(format!(
                            "the source-limited seed cohort of {} has no recorded members; no body joins it without an explicit admission",
                            family
                        ));
                    }
                    for (body, realization) in &evidence {
                        if seed_bodies.contains(body) {
                            plan.members.push(ScopeMember {
                                body: body.clone(),
                                realization: realization.clone(),
                                scope: EvidenceScope::Seed,
                                admission: None,
                            });
                        } else {
                            plan.nonmembers.push(ScopeMember {
                                body: body.clone(),
                                realization: realization.clone(),
                                scope: EvidenceScope::Experimental,
                                admission: None,
                            });
                        }
                    }
                    if plan.members.is_empty() && !seed_bodies.is_empty() {
                        plan.missing.push(format!(
                            "the source-limited seed cohort of {} has no eligible evidence",
                            family
                        ));
                    }
                }
            }
        }
        if plan.members.is_empty() && plan.nonmembers.is_empty() && plan.missing.is_empty() {
            plan.missing
                .push(format!("no eligible evidence for family {family}"));
        }
        plans.push(plan);
    }
    let now = now_unix();
    let mut conn = db_pool.acquire().await?;
    for plan in &plans {
        model_store::register_subject(
            &mut conn,
            &ModelSubject::SharedFamily {
                family: plan.family_id.clone(),
            },
            now,
        )
        .await?;
        for member in plan.members.iter().chain(plan.nonmembers.iter()) {
            model_store::register_subject(
                &mut conn,
                &ModelSubject::BodyRealization {
                    realization: member.realization.clone(),
                },
                now,
            )
            .await?;
        }
    }
    Ok(plans)
}

async fn fit_free(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    ownership: &JobOwnership,
    plan: &FamilyPlan,
    force: bool,
    snapshot_version: u64,
    snapshot_root: &std::path::Path,
) -> Result<(
    Vec<(UploadBodyId, LookSlug, RefitOutcome)>,
    Vec<TouchedEpoch>,
)> {
    let server_job = ownership.server_id();
    let sources = source_identity::sources().storage();
    let subject = SubjectKey::for_family(&plan.family_id);
    let observed = observe_plan(db_pool, plan).await?;
    let observed_pending = observed.family;
    let observed_anchor = {
        let mut conn = db_pool.acquire().await?;
        model_store::subject_state(&mut conn, &subject)
            .await?
            .and_then(|state| state.anchor_revision)
    };

    let mut member_pools: Vec<(ScopeMember, LookPool)> = Vec::new();
    for member in &plan.members {
        let transforms = TransformDir::new(&config.transforms);
        if transforms.load_optional(&member.body)?.is_none() {
            ack_observed_plan(db_pool, plan, &observed, now_unix()).await?;
            return Ok((
                vec![(
                    member.body.clone(),
                    plan.family.clone(),
                    RefitOutcome::Unavailable(format!(
                        "no input transform is published for {}",
                        member.body
                    )),
                )],
                Vec::new(),
            ));
        }
        let pool = crate::pool::build(
            db_pool,
            config,
            &member.body,
            &plan.family,
            &member.realization,
            &member.scope,
        )
        .await?;
        if !pool.images.is_empty() {
            member_pools.push((member.clone(), pool));
        }
    }
    if member_pools.is_empty() {
        ack_observed_plan(db_pool, plan, &observed, now_unix()).await?;
        return Ok((
            plan.members
                .iter()
                .map(|member| {
                    (
                        member.body.clone(),
                        plan.family.clone(),
                        RefitOutcome::Unavailable(
                            "no eligible evidence enters the family fit".to_owned(),
                        ),
                    )
                })
                .collect(),
            Vec::new(),
        ));
    }

    let mut pools: Vec<LookPool> = member_pools.iter().map(|(_, pool)| pool.clone()).collect();
    crate::pool::resolve_contributors(&mut pools)?;
    for (index, (_, pool)) in member_pools.iter_mut().enumerate() {
        *pool = pools[index].clone();
    }
    let pools: Vec<LookPool> = member_pools.iter().map(|(_, pool)| pool.clone()).collect();
    let capture_groups = native_group_count(&pools)?;
    {
        let mut conn = db_pool.acquire().await?;
        note_missing_capture_groups(
            &mut conn,
            &plan.family_id,
            &plan.members,
            capture_groups,
            now_unix(),
        )
        .await?;
    }
    if capture_groups == 0 {
        ack_observed_plan(db_pool, plan, &observed, now_unix()).await?;
        return Ok((
            plan.members
                .iter()
                .map(|member| {
                    (
                        member.body.clone(),
                        plan.family.clone(),
                        RefitOutcome::Unavailable(
                            "no eligible native images remain after excluding unsupported settings"
                                .to_owned(),
                        ),
                    )
                })
                .collect(),
            Vec::new(),
        ));
    }
    if capture_groups < 2 && plan.kind == PlanKind::Qualified {
        ack_observed_plan(db_pool, plan, &observed, now_unix()).await?;
        return Ok((
            plan.members
                .iter()
                .map(|member| {
                    (
                        member.body.clone(),
                        plan.family.clone(),
                        RefitOutcome::Unavailable(
                            "missing independent capture groups for a held-out family fold"
                                .to_owned(),
                        ),
                    )
                })
                .collect(),
            Vec::new(),
        ));
    }

    let mut observed_evidence = crate::pool::EvidenceObservation::merge(
        member_pools.iter().map(|(_, pool)| &pool.observation),
    )?;
    let scores = load_consistency_scores(db_pool, &member_pools).await?;
    let mut training_pools: Vec<LookPool> = Vec::new();
    let mut partitions: BTreeMap<(UploadBodyId, BodyRealizationId), Partition> = BTreeMap::new();
    for (member, pool) in &member_pools {
        let partition = partition_pool(pool, &subject)?;
        partitions.insert(
            (member.body.clone(), member.realization.clone()),
            partition.clone(),
        );
        training_pools.push(partition.training.clone());
    }
    let params = TrustParams {
        support_sigma_exit: fit_options(config).support_sigma_exit,
        ..TrustParams::default()
    };
    let roots = roots::load_family(&training_pools, &scores)?;
    let weighted = weights::weigh_family(&training_pools, &params, &roots.weights, &scores)?;
    let contributors = weights::family_contributor_index(&training_pools, &scores);
    let projection_pools: Vec<LookPool> = member_pools
        .iter()
        .map(|(_, pool)| native_only(pool))
        .collect();
    let options = fit_options(config);
    let weighting = weighting_dependencies(plan, &member_pools);
    let prefit_native = prefit_body_dependencies(
        config,
        &[plan],
        &member_pools,
        snapshot_version,
        &options,
        BodyStateStage::Native,
    )?;
    let prefit_published = prefit_body_dependencies(
        config,
        &[plan],
        &member_pools,
        snapshot_version,
        &options,
        BodyStateStage::Published,
    )?;
    let target_deps = target_dependencies(
        config,
        plan,
        &member_pools,
        &roots,
        snapshot_version,
        &options,
        &partitions,
        BodyStateStage::Native,
    )?;
    let mut split_deps = {
        let mut dependencies = target_deps.clone();
        dependencies.extend(crate::pool::stage_dependencies(
            &projection_pools,
            crate::pool::EvidenceStage::Native,
        ));
        dependencies.sort();
        dependencies.dedup();
        dependencies
    };
    let mut all = weighting.clone();
    all.extend(prefit_native.iter().cloned());
    all.extend(prefit_published.iter().cloned());
    all.extend(target_deps.iter().cloned());
    all.extend(split_deps.iter().cloned());
    crate::pool::complete_observation(db_pool, &mut observed_evidence, &all).await?;
    let prefit_native_snapshot = observed_evidence.snapshot_for(&prefit_native)?;
    let prefit_published_snapshot = observed_evidence.snapshot_for(&prefit_published)?;
    let target_snapshot = observed_evidence.snapshot_for(&target_deps)?;
    let mut split_snapshot = observed_evidence.snapshot_for(&split_deps)?;
    let participating: BTreeSet<(UploadBodyId, BodyRealizationId)> = member_pools
        .iter()
        .filter(|(_, pool)| !weighted.body_entries_native(pool).is_empty())
        .map(|(member, _)| (member.body.clone(), member.realization.clone()))
        .collect();
    let mut lineage = load_lineage(db_pool, config, &member_pools, &participating).await?;
    if !lineage.authoritative_missing.is_empty() {
        ack_observed_plan(db_pool, plan, &observed, now_unix()).await?;
        return Ok((
            plan.members
                .iter()
                .map(|member| {
                    (
                        member.body.clone(),
                        plan.family.clone(),
                        RefitOutcome::Unavailable(
                            "admitted lineage state is not recoverable".to_owned(),
                        ),
                    )
                })
                .collect(),
            Vec::new(),
        ));
    }

    let cold_common_native = has_scoped_admission(&plan.members);
    if cold_common_native {
        fence_common_native(&mut lineage);
    }

    let previous = match &plan.head {
        Some(head) => {
            Some(load_target_snapshot(db_pool, config, &plan.family, head.target.version).await?)
        }
        None => None,
    };
    if let Some(snapshot) = &previous
        && relative_gauge(&snapshot.record).is_none()
    {
        let target = plan
            .head
            .as_ref()
            .expect("a previous release implies a head")
            .target
            .clone();
        return migration_outcome(db_pool, plan, &target, snapshot.record.version).await;
    }

    let target = {
        let mut tx = model_store::begin_immediate(db_pool).await?;
        let target =
            model_store::reserve_family_version(&mut tx, &plan.family_id, now_unix()).await?;
        tx.commit().await?;
        target
    };
    let options = fit_options(config);
    let family_data = FamilyData {
        target: target.clone(),
        mode: FamilyMode::Fit {
            initial: previous.as_ref().map(|snapshot| snapshot.look.clone()),
        },
        previous_support: previous
            .as_ref()
            .map(|snapshot| previous_support(&snapshot.record)),
    };
    let targets: BTreeMap<BodyRealizationId, FamilyTargetId> = member_pools
        .iter()
        .map(|(member, _)| (member.realization.clone(), target.clone()))
        .collect();
    let mut prepared_modes = body_modes_for_members(&member_pools)?;
    rebind_lineage_modes(&mut lineage, &mut prepared_modes)?;
    let (bodies_data, body_modes) = build_bodies(
        &member_pools,
        &weighted,
        &contributors,
        &lineage,
        &targets,
        &partitions,
        &prepared_modes,
        true,
    )?;
    if bodies_data.is_empty() {
        ack_observed_plan(db_pool, plan, &observed, now_unix()).await?;
        return Ok((
            plan.members
                .iter()
                .map(|member| {
                    (
                        member.body.clone(),
                        plan.family.clone(),
                        RefitOutcome::Unavailable(
                            "no native evidence with an observed WB mode enters the family fit"
                                .to_owned(),
                        ),
                    )
                })
                .collect(),
            Vec::new(),
        ));
    }
    let fingerprint =
        standalone_family_fingerprint(plan, &member_pools, &training_pools, &roots, &options)?;
    let prior_attempt = {
        let mut conn = db_pool.acquire().await?;
        model_store::fit_attempt(&mut conn, &subject).await?
    };
    if !force && prior_attempt.is_none() && {
        let mut conn = db_pool.acquire().await?;
        model_store::pool_fingerprint(&mut conn, &subject)
            .await?
            .as_deref()
            == Some(fingerprint.as_str())
    } {
        let mut outcomes = Vec::new();
        let mut touched = Vec::new();
        if let Some(head) = &plan.head {
            for (member, pool) in &member_pools {
                let subject = ModelSubject::BodyRealization {
                    realization: member.realization.clone(),
                }
                .key();
                let conditional = ScopeMember {
                    body: member.body.clone(),
                    realization: member.realization.clone(),
                    scope: EvidenceScope::Experimental,
                    admission: member.admission.clone(),
                };
                let fingerprint = subject_fingerprint(
                    &subject,
                    std::slice::from_ref(pool),
                    &roots,
                    &head.target,
                    &options,
                    &partitions,
                    &FitQualification::new(None, std::iter::once(conditional.clone())),
                )?;
                let stored = {
                    let mut conn = db_pool.acquire().await?;
                    model_store::pool_fingerprint(&mut conn, &subject).await?
                };
                if stored.as_deref() == Some(fingerprint.as_str()) {
                    continue;
                }
                let (outcome, mut epochs) = fit_pinned(
                    db_pool,
                    config,
                    ownership,
                    plan,
                    &conditional,
                    true,
                    snapshot_version,
                    snapshot_root,
                )
                .await?;
                if outcome != RefitOutcome::Skipped {
                    outcomes.push((member.body.clone(), plan.family.clone(), outcome));
                }
                touched.append(&mut epochs);
            }
        }
        return Ok((outcomes, touched));
    }
    let mut resume_inventories = BTreeMap::new();
    let resume = prior_attempt
        .as_ref()
        .and_then(|attempt| attempt.states.get(FAMILY_BLOCK))
        .and_then(|checkpoint| {
            serde_json::from_str::<StoredCheckpoint>(checkpoint.as_str())
                .ok()
                .map(|checkpoint| {
                    resume_inventories = checkpoint.inventories;
                    checkpoint.state
                })
        })
        .filter(|state| state.generation == HierStateGeneration::CURRENT);
    let mut resume = common_native_resume(cold_common_native, resume);
    if let Some(state) = &mut resume
        && rebind_hier_state(state, &resume_inventories, &mut prepared_modes).is_err()
    {
        resume = None;
    }
    let mut hier_options = hier_options(
        config,
        &lineage,
        &bodies_data,
        resume,
        rebound_sigma(&lineage, &contributors),
        BodyPin::Free,
    );
    let input = HierInput {
        families: vec![family_data],
        bodies: bodies_data,
    };
    let mut fit = fit_hierarchical(&input, &hier_options);
    if let Err(looklab_fit::FitError::ResumeMismatch) = &fit
        && hier_options.resume.is_some()
    {
        hier_options.resume = None;
        fit = fit_hierarchical(&input, &hier_options);
    }
    let mut fit = match fit {
        Ok(fit) => fit,
        Err(looklab_fit::FitError::NoImages) => {
            ack_observed_plan(db_pool, plan, &observed, now_unix()).await?;
            return Ok((
                plan.members
                    .iter()
                    .map(|member| {
                        (
                            member.body.clone(),
                            plan.family.clone(),
                            RefitOutcome::Unavailable(
                                "no native images remain after partition and eligibility"
                                    .to_owned(),
                            ),
                        )
                    })
                    .collect(),
                Vec::new(),
            ));
        }
        Err(looklab_fit::FitError::NoModelErrorEvidence) => {
            let mut conn = db_pool.acquire().await?;
            model_store::record_pool_fingerprint(&mut conn, &subject, &fingerprint, now_unix())
                .await?;
            drop(conn);
            ack_observed_plan(db_pool, plan, &observed, now_unix()).await?;
            return Ok((
                plan.members
                    .iter()
                    .map(|member| {
                        (
                            member.body.clone(),
                            plan.family.clone(),
                            RefitOutcome::Unavailable(
                                "missing independent capture groups for held-out model error"
                                    .to_owned(),
                            ),
                        )
                    })
                    .collect(),
                Vec::new(),
            ));
        }
        Err(error) => return Err(WorkerError::Fit(error)),
    };
    hier_options.resume = None;
    let now = now_unix();
    if !fit.numerically_publishable() {
        let state = serde_json::to_string(&StoredCheckpoint {
            state: fit.state.clone(),
            inventories: body_modes.clone(),
        })?;
        let diagnostics = serde_json::json!({
            "converged": fit.diagnostics.converged,
            "evaluations": fit.diagnostics.evaluations,
            "outer_iterations": fit.diagnostics.outer_iterations,
            "limitations": fit.diagnostics.limitations,
        })
        .to_string();
        let first_attempt = prior_attempt
            .as_ref()
            .map_or(now, |attempt| attempt.started_at);
        let mut tx = model_store::begin_immediate(db_pool).await?;
        let consecutive = model_store::record_fit_attempt(
            &mut tx,
            &subject,
            FAMILY_BLOCK,
            Some(&FitCheckpoint::new(state)),
            now,
        )
        .await?;
        if consecutive >= config.max_consecutive_nonconverged {
            model_store::open_fit_hold(
                &mut tx,
                &subject,
                consecutive,
                first_attempt,
                now,
                &diagnostics,
            )
            .await?;
        }
        tx.commit().await?;
        ack_observed_plan(db_pool, plan, &observed, now_unix()).await?;
        return Ok((
            plan.members
                .iter()
                .map(|member| {
                    (
                        member.body.clone(),
                        plan.family.clone(),
                        RefitOutcome::NotConverged(target.version.get()),
                    )
                })
                .collect(),
            Vec::new(),
        ));
    }
    {
        let mut conn = db_pool.acquire().await?;
        model_store::clear_fit_attempt(&mut conn, &subject).await?;
        model_store::resolve_fit_hold(&mut conn, &subject).await?;
    }

    if capture_groups < 2
        && let Some(family) = fit
            .families
            .iter_mut()
            .find(|family| family.target.family == plan.family_id)
    {
        clear_family_support(family);
    }
    let family = fit
        .family_of(&plan.family_id)
        .ok_or_else(|| WorkerError::Data("the hierarchical fit produced no family".to_owned()))?;
    let gauge = fit
        .components
        .iter()
        .find(|gauge| {
            plan.members
                .iter()
                .any(|member| member.body.as_str() == gauge.designated.as_str())
        })
        .cloned();
    let gauge_value = gauge
        .as_ref()
        .map_or(0.0, |gauge| gauge_value(&fit.bodies, gauge));
    let candidate = candidate_estimates(&family.look, &family.probes, family.output_space);
    let candidate_probes = stored_probes(&family.look, &family.probes);
    let anchor_version = select_anchor(db_pool, &plan.family, target.version).await?;
    let anchor = match anchor_version {
        Some(version) => Some(
            load_target_snapshot(
                db_pool,
                config,
                &plan.family,
                TargetVersion::new(version)
                    .ok_or_else(|| WorkerError::Data("anchor target version is zero".to_owned()))?,
            )
            .await?,
        ),
        None => None,
    };
    if let Some(snapshot) = &anchor
        && relative_gauge(&snapshot.record).is_none()
    {
        return migration_outcome(db_pool, plan, &target, snapshot.record.version).await;
    }
    let previous_probes = match &previous {
        Some(snapshot) => stored_estimates(&snapshot.look, &snapshot.record, gauge_value)?,
        None => candidate.clone(),
    };
    let anchor_probes = match &anchor {
        Some(snapshot) => stored_estimates(&snapshot.look, &snapshot.record, gauge_value)?,
        None => previous_probes.clone(),
    };
    let anchor_id = anchor_version.unwrap_or(target.version.get());
    let gated = gated_since_anchor(previous.as_ref(), anchor_id, &anchor_probes);
    let mut published_gated = if anchor_id == target.version.get() {
        GatedUnion::at_anchor(&candidate)
    } else {
        gated.clone()
    };
    published_gated.include(&candidate);
    let gate = gate_release(
        &params,
        &GateInput {
            candidate: candidate.clone(),
            previous: previous_probes,
            anchor: anchor_probes,
            root_observations: roots.observations.clone(),
            gated_since_anchor: gated,
        },
    );
    if !gate.passed {
        db::insert_gate_failure(
            db_pool,
            plan.family.as_str(),
            target.version.get(),
            &serde_json::to_string(&gate)?,
            now,
        )
        .await?;
        let mut conn = db_pool.acquire().await?;
        model_store::record_pool_fingerprint(&mut conn, &subject, &fingerprint, now).await?;
        drop(conn);
        ack_observed_plan(db_pool, plan, &observed, now_unix()).await?;
        return Ok((
            plan.members
                .iter()
                .map(|member| {
                    (
                        member.body.clone(),
                        plan.family.clone(),
                        RefitOutcome::Held(target.version.get()),
                    )
                })
                .collect(),
            Vec::new(),
        ));
    }

    let summary = provenance_summary(
        family,
        &member_pools,
        anchor_id,
        &roots,
        snapshot_version,
        now,
    )?;
    let document_provenance = embedded_provenance(family, &summary);
    let maker = member_pools.first().map_or_else(
        || plan.family.as_str().to_owned(),
        |(_, pool)| pool.body.make().to_owned(),
    );
    let (document, document_bytes) = look_document(
        &plan.family,
        target.version.get(),
        family,
        &maker,
        &document_provenance,
    )?;
    let release = ReleaseRecord {
        version: target.version.get(),
        gauge: gauge_value,
        gauge_convention: GaugeConvention::Relative,
        anchor: anchor_id,
        clip_k: family.clip_k,
        output_space: family.output_space,
        probes: candidate_probes,
        gated_since_anchor: published_gated,
        converged: fit.diagnostics.converged,
        gate: Some(gate.clone()),
        approximation_p95: None,
        provenance: summary.clone(),
        created_at: now,
    };
    let safety = CandidateSafety {
        numerical: SafetyCheck::Passed,
        release_gates: SafetyCheck::Passed,
    };
    let mut touched = Vec::new();
    let mut candidates: Vec<(ModelSubject, ModelVersionId)> = Vec::new();
    let mut tx = model_store::begin_immediate(db_pool).await?;
    let artifacts = publish_target(
        &mut tx,
        config,
        plan,
        &target,
        &document,
        &document_bytes,
        &release,
        &roots,
        &target_deps,
        &target_snapshot,
        observed_anchor,
        &lease(ownership, &server_job),
        observed_pending,
        safety,
        now,
    )
    .await?;
    let family_dependency = DependencyRef::required(
        DependencyKind::FamilyVersion,
        DependencyTarget::family_version(&target),
    );
    let accepted_target = model_store::observe_dependency_snapshot(
        &mut tx,
        std::slice::from_ref(&family_dependency),
        now,
    )
    .await?;
    if !split_deps.contains(&family_dependency) {
        split_deps.push(family_dependency.clone());
    }
    split_snapshot.retain(|entry| entry.dependency != family_dependency);
    split_snapshot.extend(accepted_target);
    let look_artifacts: Vec<ArtifactBinding> = artifacts
        .iter()
        .filter(|artifact| artifact.role == ArtifactRole::Look)
        .copied()
        .collect();
    let body_versions = persist_body_states(
        &mut tx,
        config,
        plan,
        &fit,
        &body_modes,
        gauge.as_ref(),
        gauge_value,
        snapshot_version,
        &prefit_native,
        &prefit_native_snapshot,
        &prefit_published,
        &prefit_published_snapshot,
        &options,
        now,
    )
    .await?;
    for (member, pool) in &member_pools {
        let partition = partitions
            .get(&(member.body.clone(), member.realization.clone()))
            .expect("every member has a partition");
        let membership = membership(pool, partition)?;
        let modes = body_modes
            .get(&member.body)
            .ok_or_else(|| WorkerError::Data("missing WB mode inventory".to_owned()))?;
        let (native_version, recipe_version) = persist_realization(
            &mut tx,
            config,
            &target,
            member,
            pool,
            &fit,
            &body_versions,
            &look_artifacts,
            &membership,
            &target_deps,
            &prefit_native,
            &prefit_published,
            modes,
            safety,
            &contributors,
            now,
        )
        .await?;
        if let Some(version) = recipe_version {
            if let Some(published) = body_versions.get(&member.body) {
                touched.push(TouchedEpoch {
                    body: member.body.clone(),
                    epoch: member.realization.epoch.get(),
                    snapshot_version,
                    snapshot_root: snapshot_root.to_owned(),
                    published_state_version: published.published,
                });
            }
            let candidate = ModelVersionId::BodyRealization {
                realization: looklab_schema::BodyRealizationVersionId {
                    realization: member.realization.clone(),
                    version,
                    target: target.clone(),
                },
            };
            let subject = ModelSubject::BodyRealization {
                realization: member.realization.clone(),
            };
            if safety.accepted() {
                schedule_candidate(&mut tx, &subject, &candidate, now).await?;
                candidates.push((subject, candidate));
            }
        }
        if let Some(version) = native_version {
            let candidate = ModelVersionId::BodyRealization {
                realization: looklab_schema::BodyRealizationVersionId {
                    realization: member.realization.clone(),
                    version,
                    target: target.clone(),
                },
            };
            let subject = ModelSubject::BodyRealization {
                realization: member.realization.clone(),
            };
            if safety.accepted() {
                schedule_candidate(&mut tx, &subject, &candidate, now).await?;
                candidates.push((subject, candidate));
            }
        }
    }
    let family_subject = ModelSubject::SharedFamily {
        family: plan.family_id.clone(),
    };
    let family_candidate = ModelVersionId::SharedFamily {
        target: target.clone(),
    };
    if safety.accepted() {
        schedule_candidate(&mut tx, &family_subject, &family_candidate, now).await?;
        candidates.push((family_subject, family_candidate));
    }
    model_store::record_pool_fingerprint(&mut tx, &subject, &fingerprint, now).await?;
    for (subject, _) in &candidates {
        model_store::note_evidence_arrival(&mut tx, &subject.key(), now).await?;
    }
    tx.commit().await?;
    for (subject, candidate) in &candidates {
        if matches!(candidate, ModelVersionId::BodyRealization { .. }) {
            let pending = {
                let mut conn = db_pool.acquire().await?;
                model_store::pending_generation(&mut conn, &subject.key()).await?
            };
            select_experimental(
                db_pool,
                ownership,
                &subject.key(),
                candidate,
                &target,
                pending,
                observed_anchor,
                now,
            )
            .await?;
        }
    }
    if let Ok(projection_weighted) =
        weights::weigh_family(&projection_pools, &params, &roots.weights, &scores)
    {
        let projection_contributors = weights::family_contributor_index(&projection_pools, &scores);
        for (member, pool) in &member_pools {
            let Some(body) = fit.state.bodies.iter().find(|body| body.id == member.body) else {
                continue;
            };
            let Some(realization) = fit.state.realizations.iter().find(|realization| {
                realization.id == member.realization && realization.target == target
            }) else {
                continue;
            };
            propose_firmware_split(
                db_pool,
                config,
                ownership,
                pool,
                &target,
                &family.look,
                family.output_space,
                &body.state,
                realization.state.clip_k,
                body_modes.get(&member.body),
                &projection_weighted,
                &projection_contributors,
                &split_deps,
                &split_snapshot,
                &options,
            )
            .await;
        }
    } else {
        tracing::warn!(family = %plan.family, "firmware split weighting failed");
    }
    for (member, _) in &member_pools {
        let fallback = body_versions
            .get(&member.body)
            .map(|body| (body.published, body.gauge, body.transform_version));
        fit_diagnostic(
            db_pool,
            config,
            ownership,
            plan,
            member,
            snapshot_version,
            fallback,
            now,
        )
        .await?;
    }
    ack_observed_plan(db_pool, plan, &observed, now).await?;

    if plan.kind == PlanKind::Qualified {
        update_reputation(
            db_pool,
            &plan.family,
            target.version.get(),
            &member_pools,
            previous.as_ref().map(|snapshot| snapshot.record.created_at),
            lease(ownership, &server_job),
            &sources,
            &target_deps,
            &target_snapshot,
        )
        .await?;
    }
    Ok((
        plan.members
            .iter()
            .map(|member| {
                (
                    member.body.clone(),
                    plan.family.clone(),
                    RefitOutcome::Published(target.version.get()),
                )
            })
            .collect(),
        touched,
    ))
}

fn component_unavailable(
    plans: &[&FamilyPlan],
    reason: &str,
) -> Vec<(UploadBodyId, LookSlug, RefitOutcome)> {
    plans
        .iter()
        .flat_map(|plan| {
            plan.members.iter().map(|member| {
                (
                    member.body.clone(),
                    plan.family.clone(),
                    RefitOutcome::Unavailable(reason.to_owned()),
                )
            })
        })
        .collect()
}

async fn fit_component(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    ownership: &JobOwnership,
    plans: &[&FamilyPlan],
    force: bool,
    snapshot_version: u64,
    snapshot_root: &std::path::Path,
) -> Result<(
    Vec<(UploadBodyId, LookSlug, RefitOutcome)>,
    Vec<TouchedEpoch>,
)> {
    let server_job = ownership.server_id();
    let sources = source_identity::sources().storage();
    let now = now_unix();
    let mut observed_plans: BTreeMap<LookId, ObservedPlan> = BTreeMap::new();
    let mut observed_anchors: BTreeMap<LookId, Option<u32>> = BTreeMap::new();
    {
        let mut conn = db_pool.acquire().await?;
        for plan in plans {
            observed_plans.insert(plan.family_id.clone(), observe_plan(db_pool, plan).await?);
            observed_anchors.insert(
                plan.family_id.clone(),
                model_store::subject_state(&mut conn, &SubjectKey::for_family(&plan.family_id))
                    .await?
                    .and_then(|state| state.anchor_revision),
            );
        }
    }
    let mut member_pools: Vec<(usize, ScopeMember, LookPool)> = Vec::new();
    for (index, plan) in plans.iter().enumerate() {
        for member in &plan.members {
            if TransformDir::new(&config.transforms)
                .load_optional(&member.body)?
                .is_none()
            {
                continue;
            }
            let pool = crate::pool::build(
                db_pool,
                config,
                &member.body,
                &plan.family,
                &member.realization,
                &member.scope,
            )
            .await?;
            if !pool.images.is_empty() {
                member_pools.push((index, member.clone(), pool));
            }
        }
    }
    if member_pools.is_empty() {
        ack_observed_plans(db_pool, plans, &observed_plans, now_unix()).await?;
        return Ok((
            plans
                .iter()
                .flat_map(|plan| {
                    plan.members.iter().map(|member| {
                        (
                            member.body.clone(),
                            plan.family.clone(),
                            RefitOutcome::Unavailable(
                                "no eligible evidence enters the component fit".to_owned(),
                            ),
                        )
                    })
                })
                .collect(),
            Vec::new(),
        ));
    }
    let mut pools: Vec<LookPool> = member_pools
        .iter()
        .map(|(_, _, pool)| pool.clone())
        .collect();
    crate::pool::resolve_contributors(&mut pools)?;
    for (index, (_, _, pool)) in member_pools.iter_mut().enumerate() {
        *pool = pools[index].clone();
    }
    let mut observed = crate::pool::EvidenceObservation::merge(
        member_pools.iter().map(|(_, _, pool)| &pool.observation),
    )?;
    crate::pool::complete_observation(
        db_pool,
        &mut observed,
        &source_identity::sources().dependencies(),
    )
    .await?;
    let score_input: Vec<(ScopeMember, LookPool)> = member_pools
        .iter()
        .map(|(_, member, pool)| (member.clone(), pool.clone()))
        .collect();
    for plan in plans {
        let pools: Vec<LookPool> = member_pools
            .iter()
            .filter(|(index, _, _)| plans[*index].family_id == plan.family_id)
            .map(|(_, _, pool)| pool.clone())
            .collect();
        let capture_groups = native_group_count(&pools)?;
        {
            let mut conn = db_pool.acquire().await?;
            let members: Vec<ScopeMember> = plans
                .iter()
                .filter(|candidate| candidate.family_id == plan.family_id)
                .flat_map(|candidate| candidate.members.iter().cloned())
                .collect();
            note_missing_capture_groups(
                &mut conn,
                &plan.family_id,
                &members,
                capture_groups,
                now_unix(),
            )
            .await?;
        }
        if capture_groups == 0 {
            ack_observed_plans(db_pool, plans, &observed_plans, now_unix()).await?;
            return Ok((
                plans
                    .iter()
                    .flat_map(|plan| {
                        plan.members.iter().map(|member| {
                            (
                                member.body.clone(),
                                plan.family.clone(),
                                RefitOutcome::Unavailable(
                                    "no eligible native images remain after excluding unsupported settings"
                                        .to_owned(),
                                ),
                            )
                        })
                    })
                    .collect(),
                Vec::new(),
            ));
        }
        if capture_groups < 2 && plan.kind == PlanKind::Qualified {
            ack_observed_plans(db_pool, plans, &observed_plans, now_unix()).await?;
            return Ok((
                plans
                    .iter()
                    .flat_map(|plan| {
                        plan.members.iter().map(|member| {
                            (
                                member.body.clone(),
                                plan.family.clone(),
                                RefitOutcome::Unavailable(
                                    "missing independent capture groups for a held-out family fold"
                                        .to_owned(),
                                ),
                            )
                        })
                    })
                    .collect(),
                Vec::new(),
            ));
        }
    }
    let scores = load_consistency_scores(db_pool, &score_input).await?;
    let mut training_pools: Vec<LookPool> = Vec::new();
    let mut partitions: BTreeMap<(UploadBodyId, BodyRealizationId), Partition> = BTreeMap::new();
    for (_, member, pool) in &member_pools {
        let subject = ModelSubject::BodyRealization {
            realization: member.realization.clone(),
        }
        .key();
        let partition = partition_pool(pool, &subject)?;
        partitions.insert(
            (member.body.clone(), member.realization.clone()),
            partition.clone(),
        );
        training_pools.push(partition.training.clone());
    }
    let params = TrustParams {
        support_sigma_exit: fit_options(config).support_sigma_exit,
        ..TrustParams::default()
    };
    let roots = roots::load_family(&training_pools, &scores)?;
    let weighted = weights::weigh_family(&training_pools, &params, &roots.weights, &scores)?;
    let contributors = weights::family_contributor_index(&training_pools, &scores);
    let participating: BTreeSet<(UploadBodyId, BodyRealizationId)> = score_input
        .iter()
        .filter(|(_, pool)| !weighted.body_entries_native(pool).is_empty())
        .map(|(member, _)| (member.body.clone(), member.realization.clone()))
        .collect();
    let mut lineage = load_lineage(db_pool, config, &score_input, &participating).await?;
    if !lineage.authoritative_missing.is_empty() {
        ack_observed_plans(db_pool, plans, &observed_plans, now_unix()).await?;
        return Ok((
            plans
                .iter()
                .flat_map(|plan| {
                    plan.members.iter().map(|member| {
                        (
                            member.body.clone(),
                            plan.family.clone(),
                            RefitOutcome::Unavailable(
                                "admitted lineage state is not recoverable".to_owned(),
                            ),
                        )
                    })
                })
                .collect(),
            Vec::new(),
        ));
    }
    let cold_common_native = plans.iter().any(|plan| has_scoped_admission(&plan.members));
    if cold_common_native {
        fence_common_native(&mut lineage);
    }
    let options = fit_options(config);
    let mut family_data = Vec::new();
    let mut previous: BTreeMap<LookId, TargetSnapshot> = BTreeMap::new();
    let mut dirty = force;
    for plan in plans {
        let snapshot = match &plan.head {
            Some(head) => Some(
                load_target_snapshot(db_pool, config, &plan.family, head.target.version).await?,
            ),
            None => None,
        };
        if let Some(snapshot) = snapshot {
            if relative_gauge(&snapshot.record).is_none() {
                let version = plan
                    .head
                    .as_ref()
                    .map_or(0, |head| head.target.version.get());
                return Ok((
                    plan.members
                        .iter()
                        .map(|member| {
                            (
                                member.body.clone(),
                                plan.family.clone(),
                                RefitOutcome::MigrationRequired(version),
                            )
                        })
                        .collect(),
                    Vec::new(),
                ));
            }
            previous.insert(plan.family_id.clone(), snapshot);
        }
        if !force {
            let fingerprint = family_fingerprint(plan, plans, &member_pools, &scores, &options)?;
            let subject = SubjectKey::for_family(&plan.family_id);
            let mut conn = db_pool.acquire().await?;
            let stored = model_store::pool_fingerprint(&mut conn, &subject).await?;
            if stored.as_deref() != Some(fingerprint.as_str()) {
                dirty = true;
            }
        }
    }
    if !dirty {
        let mut outcomes = Vec::new();
        let mut touched = Vec::new();
        for plan in plans {
            let Some(head) = &plan.head else {
                continue;
            };
            for (_, member, pool) in member_pools
                .iter()
                .filter(|(index, _, _)| plans[*index].family_id == plan.family_id)
            {
                let subject = ModelSubject::BodyRealization {
                    realization: member.realization.clone(),
                }
                .key();
                let conditional = ScopeMember {
                    body: member.body.clone(),
                    realization: member.realization.clone(),
                    scope: EvidenceScope::Experimental,
                    admission: member.admission.clone(),
                };
                let fingerprint = subject_fingerprint(
                    &subject,
                    std::slice::from_ref(pool),
                    &roots,
                    &head.target,
                    &options,
                    &partitions,
                    &FitQualification::new(None, std::iter::once(conditional.clone())),
                )?;
                let stored = {
                    let mut conn = db_pool.acquire().await?;
                    model_store::pool_fingerprint(&mut conn, &subject).await?
                };
                if stored.as_deref() == Some(fingerprint.as_str()) {
                    continue;
                }
                let (outcome, mut epochs) = fit_pinned(
                    db_pool,
                    config,
                    ownership,
                    plan,
                    &conditional,
                    true,
                    snapshot_version,
                    snapshot_root,
                )
                .await?;
                if outcome != RefitOutcome::Skipped {
                    outcomes.push((member.body.clone(), plan.family.clone(), outcome));
                }
                touched.append(&mut epochs);
            }
        }
        ack_observed_plans(db_pool, plans, &observed_plans, now_unix()).await?;
        return Ok((outcomes, touched));
    }
    let mut targets: BTreeMap<LookId, FamilyTargetId> = BTreeMap::new();
    for plan in plans {
        let mut tx = model_store::begin_immediate(db_pool).await?;
        let target = model_store::reserve_family_version(&mut tx, &plan.family_id, now).await?;
        tx.commit().await?;
        targets.insert(plan.family_id.clone(), target);
    }
    for plan in plans {
        let target = targets
            .get(&plan.family_id)
            .cloned()
            .ok_or_else(|| WorkerError::Data("missing component target".to_owned()))?;
        let snapshot = previous.get(&plan.family_id);
        family_data.push(FamilyData {
            target,
            mode: FamilyMode::Fit {
                initial: snapshot.map(|snapshot| snapshot.look.clone()),
            },
            previous_support: snapshot.map(|snapshot| previous_support(&snapshot.record)),
        });
    }
    let realization_targets: BTreeMap<BodyRealizationId, FamilyTargetId> = member_pools
        .iter()
        .map(|(index, member, _)| {
            (
                member.realization.clone(),
                targets
                    .get(&plans[*index].family_id)
                    .cloned()
                    .expect("every plan has a target"),
            )
        })
        .collect();
    let mut prepared_modes = body_modes_for_members(&score_input)?;
    rebind_lineage_modes(&mut lineage, &mut prepared_modes)?;
    let (bodies_data, body_modes) = build_bodies(
        &score_input,
        &weighted,
        &contributors,
        &lineage,
        &realization_targets,
        &partitions,
        &prepared_modes,
        true,
    )?;
    if bodies_data.is_empty() {
        ack_observed_plans(db_pool, plans, &observed_plans, now_unix()).await?;
        return Ok((
            component_unavailable(
                plans,
                "no native evidence with an observed WB mode enters the component fit",
            ),
            Vec::new(),
        ));
    }
    let prior_attempts: BTreeMap<LookId, Option<model_store::FitAttempt>> = {
        let mut conn = db_pool.acquire().await?;
        let mut attempts = BTreeMap::new();
        for plan in plans {
            let subject = SubjectKey::for_family(&plan.family_id);
            attempts.insert(
                plan.family_id.clone(),
                model_store::fit_attempt(&mut conn, &subject).await?,
            );
        }
        attempts
    };
    let mut hier_options = hier_options(
        config,
        &lineage,
        &bodies_data,
        None,
        rebound_sigma(&lineage, &contributors),
        BodyPin::Free,
    );
    let input = HierInput {
        families: family_data,
        bodies: bodies_data,
    };
    let mut fit = match fit_hierarchical(&input, &hier_options) {
        Ok(fit) => fit,
        Err(looklab_fit::FitError::NoImages) => {
            ack_observed_plans(db_pool, plans, &observed_plans, now_unix()).await?;
            return Ok((
                component_unavailable(
                    plans,
                    "no native images remain after partition and eligibility",
                ),
                Vec::new(),
            ));
        }
        Err(looklab_fit::FitError::NoModelErrorEvidence) => {
            let mut conn = db_pool.acquire().await?;
            for plan in plans {
                let subject = SubjectKey::for_family(&plan.family_id);
                let fingerprint =
                    family_fingerprint(plan, plans, &member_pools, &scores, &options)?;
                model_store::record_pool_fingerprint(&mut conn, &subject, &fingerprint, now)
                    .await?;
            }
            drop(conn);
            ack_observed_plans(db_pool, plans, &observed_plans, now_unix()).await?;
            return Ok((
                plans
                    .iter()
                    .flat_map(|plan| {
                        plan.members.iter().map(|member| {
                            (
                                member.body.clone(),
                                plan.family.clone(),
                                RefitOutcome::Unavailable(
                                    "missing independent capture groups for held-out model error"
                                        .to_owned(),
                                ),
                            )
                        })
                    })
                    .collect(),
                Vec::new(),
            ));
        }
        Err(error) => return Err(WorkerError::Fit(error)),
    };
    hier_options.resume = None;
    if !fit.numerically_publishable() {
        let state = serde_json::to_string(&StoredCheckpoint {
            state: fit.state.clone(),
            inventories: body_modes.clone(),
        })?;
        let diagnostics = serde_json::json!({
            "converged": fit.diagnostics.converged,
            "evaluations": fit.diagnostics.evaluations,
            "outer_iterations": fit.diagnostics.outer_iterations,
            "limitations": fit.diagnostics.limitations,
        })
        .to_string();
        let mut tx = model_store::begin_immediate(db_pool).await?;
        for plan in plans {
            let subject = SubjectKey::for_family(&plan.family_id);
            let first_attempt = prior_attempts
                .get(&plan.family_id)
                .and_then(|attempt| attempt.as_ref())
                .map_or(now, |attempt| attempt.started_at);
            let consecutive = model_store::record_fit_attempt(
                &mut tx,
                &subject,
                FAMILY_BLOCK,
                Some(&FitCheckpoint::new(state.clone())),
                now,
            )
            .await?;
            if consecutive >= config.max_consecutive_nonconverged {
                model_store::open_fit_hold(
                    &mut tx,
                    &subject,
                    consecutive,
                    first_attempt,
                    now,
                    &diagnostics,
                )
                .await?;
            }
        }
        tx.commit().await?;
        ack_observed_plans(db_pool, plans, &observed_plans, now_unix()).await?;
        return Ok((
            plans
                .iter()
                .flat_map(|plan| {
                    plan.members.iter().map(|member| {
                        (
                            member.body.clone(),
                            plan.family.clone(),
                            RefitOutcome::NotConverged(targets[&plan.family_id].version.get()),
                        )
                    })
                })
                .collect(),
            Vec::new(),
        ));
    }
    {
        let mut tx = model_store::begin_immediate(db_pool).await?;
        for plan in plans {
            let subject = SubjectKey::for_family(&plan.family_id);
            model_store::clear_fit_attempt(&mut tx, &subject).await?;
            model_store::resolve_fit_hold(&mut tx, &subject).await?;
        }
        tx.commit().await?;
    }
    let one_group: BTreeSet<LookId> = {
        let mut families = BTreeSet::new();
        for plan in plans {
            let pools: Vec<LookPool> = member_pools
                .iter()
                .filter(|(index, _, _)| plans[*index].family_id == plan.family_id)
                .map(|(_, _, pool)| pool.clone())
                .collect();
            if native_group_count(&pools)? < 2 {
                families.insert(plan.family_id.clone());
            }
        }
        families
    };
    for family in &mut fit.families {
        if one_group.contains(&family.target.family) {
            clear_family_support(family);
        }
    }
    let gauge = fit
        .components
        .iter()
        .find(|gauge| {
            member_pools
                .iter()
                .any(|(_, member, _)| member.body.as_str() == gauge.designated.as_str())
        })
        .cloned();
    let gauge_value = gauge
        .as_ref()
        .map_or(0.0, |gauge| gauge_value(&fit.bodies, gauge));
    let mut all_gates_pass = true;
    for plan in plans {
        let target = targets
            .get(&plan.family_id)
            .cloned()
            .ok_or_else(|| WorkerError::Data("missing component target".to_owned()))?;
        let Some(family) = fit.family_of(&plan.family_id) else {
            all_gates_pass = false;
            continue;
        };
        let plan_training: Vec<LookPool> = member_pools
            .iter()
            .filter(|(index, _, _)| plans[*index].family_id == plan.family_id)
            .map(|(_, _, pool)| pool.clone())
            .collect();
        let plan_roots = roots::load_family(&plan_training, &scores)?;
        let candidate = candidate_estimates(&family.look, &family.probes, family.output_space);
        let previous_snapshot = previous.get(&plan.family_id);
        let previous_probes = match previous_snapshot {
            Some(snapshot) => stored_estimates(&snapshot.look, &snapshot.record, gauge_value)?,
            None => candidate.clone(),
        };
        let anchor_version = select_anchor(db_pool, &plan.family, target.version).await?;
        let anchor_snapshot = match anchor_version {
            Some(version) => Some(
                load_target_snapshot(
                    db_pool,
                    config,
                    &plan.family,
                    TargetVersion::new(version).ok_or_else(|| {
                        WorkerError::Data("anchor target version is zero".to_owned())
                    })?,
                )
                .await?,
            ),
            None => None,
        };
        let anchor_probes = match &anchor_snapshot {
            Some(snapshot) => stored_estimates(&snapshot.look, &snapshot.record, gauge_value)?,
            None => previous_probes.clone(),
        };
        let anchor_id = anchor_snapshot
            .as_ref()
            .map_or(target.version.get(), |snapshot| snapshot.record.version);
        let gated = gated_since_anchor(previous_snapshot, anchor_id, &anchor_probes);
        let gate = gate_release(
            &params,
            &GateInput {
                candidate,
                previous: previous_probes,
                anchor: anchor_probes,
                root_observations: plan_roots.observations.clone(),
                gated_since_anchor: gated,
            },
        );
        if !gate.passed {
            db::insert_gate_failure(
                db_pool,
                plan.family.as_str(),
                target.version.get(),
                &serde_json::to_string(&gate)?,
                now,
            )
            .await?;
            all_gates_pass = false;
        }
    }
    if !all_gates_pass {
        ack_observed_plans(db_pool, plans, &observed_plans, now_unix()).await?;
        return Ok((
            plans
                .iter()
                .flat_map(|plan| {
                    plan.members.iter().map(|member| {
                        (
                            member.body.clone(),
                            plan.family.clone(),
                            RefitOutcome::Held(targets[&plan.family_id].version.get()),
                        )
                    })
                })
                .collect(),
            Vec::new(),
        ));
    }
    let mut shared_deps = source_identity::sources().dependencies();
    shared_deps.push(DependencyRef::required(
        DependencyKind::RootWeighting,
        DependencyTarget::root_weighting_global(),
    ));
    for plan in plans {
        shared_deps.push(DependencyRef::required(
            DependencyKind::RootWeighting,
            DependencyTarget::root_weighting_family(&plan.family_id),
        ));
    }
    let mut shared_bodies: Vec<&UploadBodyId> =
        score_input.iter().map(|(member, _)| &member.body).collect();
    shared_bodies.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    shared_bodies.dedup();
    for body in shared_bodies {
        shared_deps.push(DependencyRef::required(
            DependencyKind::RootWeighting,
            DependencyTarget::root_weighting_body(body),
        ));
    }
    for (member, pool) in &score_input {
        shared_deps.push(DependencyRef::required(
            DependencyKind::Schema,
            DependencyTarget::published_schema(&PublishedSchemaSnapshot::new(
                member.body.clone(),
                snapshot_version,
            )),
        ));
        shared_deps.push(DependencyRef::required(
            DependencyKind::Transform,
            DependencyTarget::transform(&member.body, pool.transforms.version()),
        ));
        if let Ok(bytes) = TransformDir::new(&config.transforms).snapshot(&member.body) {
            let hash = Sha256Hex::from_bytes(Sha256::digest(&bytes).into());
            if let Ok(id) = ProfileId::new(hash.to_hex()) {
                shared_deps.push(DependencyRef::required(
                    DependencyKind::Profile,
                    DependencyTarget::profile(&id, pool.transforms.version()),
                ));
            }
        }
        for image in &pool.images {
            if image.role != EvidenceRole::Native || !image.is_eligible() {
                continue;
            }
            if let Ok(contribution) = ContributionRef::new(image.contribution.id.as_str()) {
                shared_deps.push(DependencyRef::required(
                    DependencyKind::Contribution,
                    DependencyTarget::contribution(&contribution),
                ));
            }
            if let Ok(image_ref) = ImageRef::new(image.image.id.as_str()) {
                shared_deps.push(DependencyRef::required(
                    DependencyKind::Image,
                    DependencyTarget::image(&image_ref),
                ));
            }
            if let Ok(standing) = StandingRef::new(image.credential.id.as_str()) {
                shared_deps.push(DependencyRef::required(
                    DependencyKind::Standing,
                    DependencyTarget::standing(&standing),
                ));
            }
            if let Ok(identity) = IdentityResolutionRef::new(image.credential.id.as_str()) {
                shared_deps.push(DependencyRef::required(
                    DependencyKind::IdentityResolution,
                    DependencyTarget::identity_resolution(&identity),
                ));
            }
        }
    }
    if let Ok(probe_set) = ProbeSetId::new(PROBE_SET) {
        shared_deps.push(DependencyRef::required(
            DependencyKind::ProbeSet,
            DependencyTarget::probe_set(&probe_set),
        ));
    }
    if let Ok(digest) = effective_options_digest(&options) {
        shared_deps.push(DependencyRef::required(
            DependencyKind::EffectiveOptions,
            DependencyTarget::effective_options(digest),
        ));
    }
    shared_deps.sort();
    shared_deps.dedup();
    let prefit_native = prefit_body_dependencies(
        config,
        plans,
        &score_input,
        snapshot_version,
        &options,
        BodyStateStage::Native,
    )?;
    let prefit_published = prefit_body_dependencies(
        config,
        plans,
        &score_input,
        snapshot_version,
        &options,
        BodyStateStage::Published,
    )?;
    let mut all = prefit_native.clone();
    all.extend(prefit_published.iter().cloned());
    crate::pool::complete_observation(db_pool, &mut observed, &all).await?;
    let prefit_native_snapshot = observed.snapshot_for(&prefit_native)?;
    let prefit_published_snapshot = observed.snapshot_for(&prefit_published)?;
    let body_versions = {
        let mut tx = model_store::begin_immediate(db_pool).await?;
        let versions = persist_body_states(
            &mut tx,
            config,
            plans[0],
            &fit,
            &body_modes,
            gauge.as_ref(),
            gauge_value,
            snapshot_version,
            &prefit_native,
            &prefit_native_snapshot,
            &prefit_published,
            &prefit_published_snapshot,
            &options,
            now,
        )
        .await?;
        tx.commit().await?;
        versions
    };
    let mut outcomes = Vec::new();
    let mut touched = Vec::new();
    for plan in plans {
        let target = targets
            .get(&plan.family_id)
            .cloned()
            .ok_or_else(|| WorkerError::Data("missing component target".to_owned()))?;
        let Some(family) = fit.family_of(&plan.family_id) else {
            continue;
        };
        let plan_training: Vec<LookPool> = member_pools
            .iter()
            .filter(|(index, _, _)| plans[*index].family_id == plan.family_id)
            .map(|(_, _, pool)| pool.clone())
            .collect();
        let plan_roots = roots::load_family(&plan_training, &scores)?;
        let candidate = candidate_estimates(&family.look, &family.probes, family.output_space);
        let candidate_probes = stored_probes(&family.look, &family.probes);
        let previous_snapshot = previous.get(&plan.family_id);
        let previous_probes = match previous_snapshot {
            Some(snapshot) => stored_estimates(&snapshot.look, &snapshot.record, gauge_value)?,
            None => candidate.clone(),
        };
        let anchor_version = select_anchor(db_pool, &plan.family, target.version).await?;
        let anchor_snapshot = match anchor_version {
            Some(version) => Some(
                load_target_snapshot(
                    db_pool,
                    config,
                    &plan.family,
                    TargetVersion::new(version).ok_or_else(|| {
                        WorkerError::Data("anchor target version is zero".to_owned())
                    })?,
                )
                .await?,
            ),
            None => None,
        };
        let anchor_probes = match &anchor_snapshot {
            Some(snapshot) => stored_estimates(&snapshot.look, &snapshot.record, gauge_value)?,
            None => previous_probes.clone(),
        };
        let anchor_id = anchor_snapshot
            .as_ref()
            .map_or(target.version.get(), |snapshot| snapshot.record.version);
        let gated = gated_since_anchor(previous_snapshot, anchor_id, &anchor_probes);
        let mut published_gated = if anchor_id == target.version.get() {
            GatedUnion::at_anchor(&candidate)
        } else {
            gated.clone()
        };
        published_gated.include(&candidate);
        let gate = gate_release(
            &params,
            &GateInput {
                candidate,
                previous: previous_probes,
                anchor: anchor_probes,
                root_observations: plan_roots.observations.clone(),
                gated_since_anchor: gated,
            },
        );
        if !gate.passed {
            db::insert_gate_failure(
                db_pool,
                plan.family.as_str(),
                target.version.get(),
                &serde_json::to_string(&gate)?,
                now,
            )
            .await?;
            for member in &plan.members {
                outcomes.push((
                    member.body.clone(),
                    plan.family.clone(),
                    RefitOutcome::Held(target.version.get()),
                ));
            }
            if let Some(entry) = observed_plans.get(&plan.family_id) {
                ack_observed_plan(db_pool, plan, entry, now_unix()).await?;
            }
            continue;
        }
        let summary = provenance_summary(
            family,
            &score_input
                .iter()
                .filter(|(member, _)| {
                    plan.members
                        .iter()
                        .any(|entry| entry.realization == member.realization)
                })
                .cloned()
                .collect::<Vec<_>>(),
            anchor_id,
            &plan_roots,
            snapshot_version,
            now,
        )?;
        let document_provenance = embedded_provenance(family, &summary);
        let maker = score_input
            .iter()
            .find(|(member, _)| {
                plan.members
                    .iter()
                    .any(|entry| entry.realization == member.realization)
            })
            .map_or_else(
                || plan.family.as_str().to_owned(),
                |(_, pool)| pool.body.make().to_owned(),
            );
        let (document, document_bytes) = look_document(
            &plan.family,
            target.version.get(),
            family,
            &maker,
            &document_provenance,
        )?;
        let release = ReleaseRecord {
            version: target.version.get(),
            gauge: gauge_value,
            gauge_convention: GaugeConvention::Relative,
            anchor: anchor_id,
            clip_k: family.clip_k,
            output_space: family.output_space,
            probes: candidate_probes,
            gated_since_anchor: published_gated,
            converged: fit.diagnostics.converged,
            gate: Some(gate.clone()),
            approximation_p95: None,
            provenance: summary,
            created_at: now,
        };
        let plan_pools: Vec<(ScopeMember, LookPool)> = score_input
            .iter()
            .filter(|(member, _)| {
                plan.members
                    .iter()
                    .any(|entry| entry.realization == member.realization)
            })
            .cloned()
            .collect();
        let projection_pools: Vec<LookPool> = plan_pools
            .iter()
            .map(|(_, pool)| native_only(pool))
            .collect();
        let target_deps = target_dependencies(
            config,
            plan,
            &plan_pools,
            &plan_roots,
            snapshot_version,
            &options,
            &partitions,
            BodyStateStage::Native,
        )?;
        let mut split_deps = {
            let mut dependencies = target_deps.clone();
            dependencies.extend(crate::pool::stage_dependencies(
                &projection_pools,
                crate::pool::EvidenceStage::Native,
            ));
            dependencies.sort();
            dependencies.dedup();
            dependencies
        };
        let safety = CandidateSafety {
            numerical: SafetyCheck::Passed,
            release_gates: SafetyCheck::Passed,
        };
        let observed_pending = observed_plans
            .get(&plan.family_id)
            .map_or(PendingGeneration::new(0), |observed| observed.family);
        let weighting = weighting_dependencies(plan, &plan_pools);
        let mut all = weighting.clone();
        all.extend(prefit_native.iter().cloned());
        all.extend(prefit_published.iter().cloned());
        all.extend(target_deps.iter().cloned());
        all.extend(split_deps.iter().cloned());
        crate::pool::complete_observation(db_pool, &mut observed, &all).await?;
        let target_snapshot = observed.snapshot_for(&target_deps)?;
        let mut split_snapshot = observed.snapshot_for(&split_deps)?;
        let mut tx = model_store::begin_immediate(db_pool).await?;
        let artifacts = publish_target(
            &mut tx,
            config,
            plan,
            &target,
            &document,
            &document_bytes,
            &release,
            &plan_roots,
            &target_deps,
            &target_snapshot,
            observed_anchors.get(&plan.family_id).copied().flatten(),
            &lease(ownership, &server_job),
            observed_pending,
            safety,
            now,
        )
        .await?;
        let family_dependency = DependencyRef::required(
            DependencyKind::FamilyVersion,
            DependencyTarget::family_version(&target),
        );
        let accepted_target = model_store::observe_dependency_snapshot(
            &mut tx,
            std::slice::from_ref(&family_dependency),
            now,
        )
        .await?;
        if !split_deps.contains(&family_dependency) {
            split_deps.push(family_dependency.clone());
        }
        split_snapshot.retain(|entry| entry.dependency != family_dependency);
        split_snapshot.extend(accepted_target);
        let look_artifacts: Vec<ArtifactBinding> = artifacts
            .iter()
            .filter(|artifact| artifact.role == ArtifactRole::Look)
            .copied()
            .collect();
        let mut candidates: Vec<(ModelSubject, ModelVersionId)> = Vec::new();
        for (member, pool) in &plan_pools {
            let partition = partitions
                .get(&(member.body.clone(), member.realization.clone()))
                .expect("every member has a partition");
            let membership = membership(pool, partition)?;
            let modes = body_modes
                .get(&member.body)
                .ok_or_else(|| WorkerError::Data("missing WB mode inventory".to_owned()))?;
            let (native_version, recipe_version) = persist_realization(
                &mut tx,
                config,
                &target,
                member,
                pool,
                &fit,
                &body_versions,
                &look_artifacts,
                &membership,
                &target_deps,
                &prefit_native,
                &prefit_published,
                modes,
                safety,
                &contributors,
                now,
            )
            .await?;
            for version in [native_version, recipe_version].into_iter().flatten() {
                let subject = ModelSubject::BodyRealization {
                    realization: member.realization.clone(),
                };
                let candidate = ModelVersionId::BodyRealization {
                    realization: looklab_schema::BodyRealizationVersionId {
                        realization: member.realization.clone(),
                        version,
                        target: target.clone(),
                    },
                };
                if safety.accepted() {
                    schedule_candidate(&mut tx, &subject, &candidate, now).await?;
                    candidates.push((subject, candidate));
                }
                if let Some(persisted) = body_versions.get(&member.body) {
                    touched.push(TouchedEpoch {
                        body: member.body.clone(),
                        epoch: member.realization.epoch.get(),
                        snapshot_version,
                        snapshot_root: snapshot_root.to_owned(),
                        published_state_version: persisted.published,
                    });
                }
            }
            outcomes.push((
                member.body.clone(),
                plan.family.clone(),
                RefitOutcome::Published(target.version.get()),
            ));
        }
        let family_subject = ModelSubject::SharedFamily {
            family: plan.family_id.clone(),
        };
        if safety.accepted() {
            schedule_candidate(
                &mut tx,
                &family_subject,
                &ModelVersionId::SharedFamily {
                    target: target.clone(),
                },
                now,
            )
            .await?;
        }
        let fingerprint = family_fingerprint(plan, plans, &member_pools, &scores, &options)?;
        model_store::record_pool_fingerprint(
            &mut tx,
            &SubjectKey::for_family(&plan.family_id),
            &fingerprint,
            now,
        )
        .await?;
        for (subject, _) in &candidates {
            model_store::note_evidence_arrival(&mut tx, &subject.key(), now).await?;
        }
        tx.commit().await?;
        for (subject, candidate) in &candidates {
            if matches!(candidate, ModelVersionId::BodyRealization { .. }) {
                let pending = {
                    let mut conn = db_pool.acquire().await?;
                    model_store::pending_generation(&mut conn, &subject.key()).await?
                };
                select_experimental(
                    db_pool,
                    ownership,
                    &subject.key(),
                    candidate,
                    &target,
                    pending,
                    observed_anchors.get(&plan.family_id).copied().flatten(),
                    now,
                )
                .await?;
            }
        }
        if let Ok(projection_weighted) =
            weights::weigh_family(&projection_pools, &params, &roots.weights, &scores)
        {
            let projection_contributors =
                weights::family_contributor_index(&projection_pools, &scores);
            for (member, pool) in &plan_pools {
                let Some(body) = fit.state.bodies.iter().find(|body| body.id == member.body) else {
                    continue;
                };
                let Some(realization) = fit.state.realizations.iter().find(|realization| {
                    realization.id == member.realization && realization.target == target
                }) else {
                    continue;
                };
                propose_firmware_split(
                    db_pool,
                    config,
                    ownership,
                    pool,
                    &target,
                    &family.look,
                    family.output_space,
                    &body.state,
                    realization.state.clip_k,
                    body_modes.get(&member.body),
                    &projection_weighted,
                    &projection_contributors,
                    &split_deps,
                    &split_snapshot,
                    &options,
                )
                .await;
            }
        } else {
            tracing::warn!(family = %plan.family, "firmware split weighting failed");
        }
        for (member, _) in &plan_pools {
            let fallback = body_versions
                .get(&member.body)
                .map(|body| (body.published, body.gauge, body.transform_version));
            fit_diagnostic(
                db_pool,
                config,
                ownership,
                plan,
                member,
                snapshot_version,
                fallback,
                now,
            )
            .await?;
        }
        if let Some(entry) = observed_plans.get(&plan.family_id) {
            ack_observed_plan(db_pool, plan, entry, now).await?;
        }
        if plan.kind == PlanKind::Qualified {
            update_reputation(
                db_pool,
                &plan.family,
                target.version.get(),
                &plan_pools,
                previous
                    .get(&plan.family_id)
                    .map(|snapshot| snapshot.record.created_at),
                lease(ownership, &server_job),
                &sources,
                &target_deps,
                &target_snapshot,
            )
            .await?;
        }
    }
    Ok((outcomes, touched))
}

#[allow(clippy::too_many_arguments)]
async fn fit_diagnostic(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    ownership: &JobOwnership,
    plan: &FamilyPlan,
    member: &ScopeMember,
    snapshot_version: u64,
    fallback_body_state: Option<(BodyStateVersion, Sha256Hex, u32)>,
    now: i64,
) -> Result<Option<RealizationVersion>> {
    let Some(head) = &plan.head else {
        return Ok(None);
    };
    let target = head.target.clone();
    let mut pool = crate::pool::build(
        db_pool,
        config,
        &member.body,
        &plan.family,
        &member.realization,
        &EvidenceScope::Experimental,
    )
    .await?;
    crate::pool::resolve_contributors(std::slice::from_mut(&mut pool))?;
    if pool.images.is_empty() {
        return Ok(None);
    }
    let subject = ModelSubject::BodyRealization {
        realization: member.realization.clone(),
    }
    .key();
    let lineage = load_lineage(
        db_pool,
        config,
        std::slice::from_ref(&(member.clone(), pool.clone())),
        &BTreeSet::new(),
    )
    .await?;
    let state = lineage
        .body_state
        .get(&member.body)
        .cloned()
        .or(fallback_body_state);
    let Some((body_state_version, _, transform_version)) = state.as_ref() else {
        return Ok(None);
    };
    let params = TrustParams::default();
    let scores = ConsistencyScores::new();
    let weighted = weights::weigh(
        &pool,
        &params,
        &crate::roots::RootWeights::default(),
        &scores,
    )?;
    let contributors = weights::contributor_index(&pool, &scores);
    let mut images = Vec::new();
    let epoch = pool
        .resources
        .schema
        .epochs
        .iter()
        .find(|epoch| epoch.id == member.realization.epoch.get())
        .ok_or_else(|| WorkerError::Data("diagnostic epoch is absent".to_owned()))?;
    let modes = mode_maps(&[epoch], &physical_wb_modes(&pool));
    let entries = weighted.fit_entries(&pool);
    let fits = weights::fit_images(&pool, &weighted, &contributors);
    for (entry, fit) in entries.iter().zip(fits) {
        let image = &pool.images[entry.0];
        images.push(HierImage {
            image: fit,
            firmware: FirmwareId::new(
                member.body.clone(),
                ControlEpochId::new(member.realization.epoch.get()),
                &image.contribution.firmware,
            )?,
            wb_mode: wb_class(
                epoch,
                &image.settings,
                &modes,
                image.physical.as_ref().map(|physical| &physical.wb_mode),
            )
            .unwrap_or(UNKNOWN_WB_CLASS),
        });
    }
    if images.is_empty() {
        return Ok(None);
    }
    let body = BodyData {
        id: member.body.clone(),
        transform: pool
            .transforms
            .input_transform(&member.body, TransformVersion::new(*transform_version))?,
        default_mode: images[0].image.exposure_mode,
        default_wb_mode: UNKNOWN_WB_CLASS,
        charts: Vec::new(),
        warm: lineage
            .body_warm
            .get(&member.body)
            .map(|saved| saved.warm.clone()),
        realizations: vec![RealizationData {
            id: member.realization.clone(),
            target: target.clone(),
            model: None,
            images,
            warm: None,
        }],
    };
    let input = HierInput {
        families: vec![FamilyData {
            target: target.clone(),
            mode: FamilyMode::Fit { initial: None },
            previous_support: None,
        }],
        bodies: vec![body],
    };
    let options = hier_options(
        config,
        &Lineage::default(),
        &input.bodies,
        None,
        Vec::new(),
        BodyPin::Free,
    );
    let fit = match fit_hierarchical(&input, &options) {
        Ok(fit) => fit,
        Err(looklab_fit::FitError::NoImages | looklab_fit::FitError::NoModelErrorEvidence) => {
            return Ok(None);
        }
        Err(error) => return Err(WorkerError::Fit(error)),
    };
    let Some(family) = fit.family_of(&plan.family_id) else {
        return Ok(None);
    };
    let gauge = fit
        .components
        .iter()
        .find(|gauge| gauge.designated.as_str() == member.body.as_str())
        .ok_or_else(|| WorkerError::Data("camera fit has no component gauge".to_owned()))?;
    let gauge_value = gauge_value(&fit.bodies, gauge);
    let gauge_artifact = GaugeArtifact {
        designated: member.body.clone(),
        exposure_mode: gauge.exposure_mode.0,
        wb_mode: gauge.wb_mode.0,
        inventory: Some(modes.clone()),
        anchored_delta: gauge.anchored_delta,
        gauge: gauge_value,
        convention: GaugeConvention::Relative,
    };
    let gauge_binding = store_binding(
        config,
        &serde_json::to_vec(&gauge_artifact)?,
        ArtifactRole::Gauge,
    )?;
    let summary = provenance_summary(
        family,
        std::slice::from_ref(&(member.clone(), pool.clone())),
        target.version.get(),
        &RootData::default(),
        snapshot_version,
        now,
    )?;
    let provenance = embedded_provenance(family, &summary);
    let maker = pool.body.make().to_owned();
    let (_document, document_bytes) = look_document(
        &plan.family,
        target.version.get(),
        family,
        &maker,
        &provenance,
    )?;
    let look_binding = store_binding(config, &document_bytes, ArtifactRole::Look)?;
    let warm = fit
        .native_realization_warm(&member.realization, &target)
        .unwrap_or(RealizationWarmStart {
            clip_k: family.clip_k,
            nuisances: Vec::new(),
            damping: 1e-3,
            response: None,
            coefficients: Vec::new(),
        });
    let partition = partition_pool(&pool, &subject)?;
    let membership = membership(&pool, &partition)?;
    let local_body = fit
        .bodies
        .iter()
        .find(|body| body.id == member.body)
        .ok_or_else(|| WorkerError::Data("diagnostic fit produced no body".to_owned()))?;
    let transform_binding = store_binding(
        config,
        &serde_json::to_vec(&local_body.transform)?,
        ArtifactRole::InputTransform,
    )?;
    let native_warm = fit
        .native_body_warm(&member.body)
        .unwrap_or_else(|| BodyWarmStart {
            delta: local_body.transform.delta(),
            exposure: local_body.exposure.clone(),
            wb: local_body.wb.clone(),
        });
    let local_params = match_params(
        &member.body,
        MatchStage::Native,
        &local_body.transform,
        &native_warm.exposure,
        &native_warm.wb,
        &modes,
    )?;
    let params_binding = store_binding(
        config,
        &serde_json::to_vec(&local_params)?,
        ArtifactRole::BodyRealizationParams,
    )?;
    let zero_crm = ControlResponseModel::zeroed(
        &pool.resources.schema,
        member.realization.epoch.get(),
        &member.realization.native_sim,
        1,
    )?;
    let crm_binding = store_binding(
        config,
        &encode_crm(&zero_crm)?,
        ArtifactRole::ControlResponse,
    )?;
    let groups: Vec<EvidenceGroupId> = {
        let mut groups = BTreeSet::new();
        for image in &pool.images {
            if image.role == EvidenceRole::Native && image.is_eligible() {
                groups.insert(evidence_group(image)?);
            }
        }
        groups.into_iter().collect()
    };
    let probes = family
        .probes
        .ids
        .iter()
        .zip(&family.probes.probes)
        .map(|(id, probe)| DiagnosticProbe {
            probe: id.clone(),
            post_clip: probe.post_ucs.to_array(),
            supported: probe.supported,
            sigma_de: probe.post_sigma_de,
        })
        .collect();
    let diagnostics = CameraLocalDiagnostics {
        format: DIAGNOSTIC_FORMAT.to_owned(),
        subject: member.realization.clone(),
        target: target.clone(),
        realization: ModelSubject::BodyRealization {
            realization: member.realization.clone(),
        }
        .key(),
        realization_version: 0,
        source: DiagnosticSource {
            fitter: source_identity::sources().fitter.clone(),
            evaluator: source_identity::sources().evaluator.clone(),
            model: source_identity::sources().model.clone(),
        },
        input_transform: transform_binding.sha256,
        realization_params: params_binding.sha256,
        gauge: gauge_binding.sha256,
        control_response: crm_binding.sha256,
        groups,
        probes,
        budget: DiagnosticBudget {
            evaluations: u64::try_from(fit.diagnostics.evaluations).unwrap_or(u64::MAX),
            sweeps: fit.diagnostics.outer_iterations,
            converged: fit.diagnostics.converged,
        },
    };
    let mut artifacts = vec![
        look_binding,
        gauge_binding,
        transform_binding,
        params_binding,
        crm_binding,
    ];
    artifacts.push(store_binding(
        config,
        &serde_json::to_vec(&warm)?,
        ArtifactRole::NativeWarmState,
    )?);
    let resume = StoredResume {
        body: fit.native_body_warm(&member.body),
        realization: warm.clone(),
        crm: None,
        state: fit.state.clone(),
        membership: membership.clone(),
        sigma: Vec::new(),
        inventory: Some(modes.clone()),
    };
    artifacts.push(store_binding(
        config,
        &serde_json::to_vec(&resume)?,
        ArtifactRole::FitResume,
    )?);
    artifacts.push(store_binding(
        config,
        &serde_json::to_vec(&membership)?,
        ArtifactRole::EvidenceMembership,
    )?);
    let mut tx = model_store::begin_immediate(db_pool).await?;
    let (_, version) =
        model_store::reserve_realization_version(&mut tx, &member.realization, &target, now)
            .await?;
    let mut diagnostics = diagnostics;
    diagnostics.realization_version = version.get();
    artifacts.push(store_binding(
        config,
        &serde_json::to_vec(&diagnostics)?,
        ArtifactRole::FitDiagnostic,
    )?);
    let mut edges = source_identity::sources().dependencies();
    if let Ok(gauge) = GaugeId::new(gauge_binding.sha256.to_hex()) {
        edges.push(DependencyRef::required(
            DependencyKind::Gauge,
            DependencyTarget::gauge(&gauge),
        ));
    }
    edges.push(DependencyRef::required(
        DependencyKind::FamilyVersion,
        DependencyTarget::family_version(&target),
    ));
    edges.push(DependencyRef::required(
        DependencyKind::Schema,
        DependencyTarget::published_schema(&PublishedSchemaSnapshot::new(
            member.body.clone(),
            snapshot_version,
        )),
    ));
    edges.push(DependencyRef::required(
        DependencyKind::Transform,
        DependencyTarget::transform(&member.body, *transform_version),
    ));
    if let Ok(bytes) = TransformDir::new(&config.transforms).snapshot(&member.body) {
        let hash = Sha256Hex::from_bytes(Sha256::digest(&bytes).into());
        if let Ok(id) = ProfileId::new(hash.to_hex()) {
            edges.push(DependencyRef::required(
                DependencyKind::Profile,
                DependencyTarget::profile(&id, *transform_version),
            ));
        }
    }
    for image in &pool.images {
        if image.role != EvidenceRole::Native {
            continue;
        }
        if let Ok(contribution) = ContributionRef::new(image.contribution.id.as_str()) {
            edges.push(DependencyRef::required(
                DependencyKind::Contribution,
                DependencyTarget::contribution(&contribution),
            ));
        }
        if let Ok(image_ref) = ImageRef::new(image.image.id.as_str()) {
            edges.push(DependencyRef::required(
                DependencyKind::Image,
                DependencyTarget::image(&image_ref),
            ));
        }
        if let Ok(standing) = StandingRef::new(image.credential.id.as_str()) {
            edges.push(DependencyRef::required(
                DependencyKind::Standing,
                DependencyTarget::standing(&standing),
            ));
        }
        if let Ok(identity) = IdentityResolutionRef::new(image.credential.id.as_str()) {
            edges.push(DependencyRef::required(
                DependencyKind::IdentityResolution,
                DependencyTarget::identity_resolution(&identity),
            ));
        }
    }
    if let Ok(probe_set) = ProbeSetId::new(PROBE_SET) {
        edges.push(DependencyRef::required(
            DependencyKind::ProbeSet,
            DependencyTarget::probe_set(&probe_set),
        ));
    }
    if let Ok(digest) = effective_options_digest(&fit_options(config)) {
        edges.push(DependencyRef::required(
            DependencyKind::EffectiveOptions,
            DependencyTarget::effective_options(digest),
        ));
    }
    edges.push(DependencyRef::required(
        DependencyKind::PartitionPolicy,
        DependencyTarget::partition_policy(partition_policy_digest(
            &subject,
            &partition.assessment,
        )),
    ));
    edges.sort();
    edges.dedup();
    let revision = NewRealizationRevision {
        subject: &member.realization,
        version,
        stage: RealizationStage::Native,
        target: &target,
        body_state_version: *body_state_version,
        input_transform_version: *transform_version,
        realization_params_version: body_state_version.get(),
        control_response_version: zero_crm.version,
        clip_k: if warm.clip_k.is_finite() && warm.clip_k > 0.0 {
            warm.clip_k
        } else {
            0.03
        },
        status: RealizationStatus::Diagnostic,
        status_reason: Some("independent camera-local native diagnostic"),
        safety: CandidateSafety {
            numerical: if fit.numerically_publishable() {
                SafetyCheck::Passed
            } else {
                SafetyCheck::Failed
            },
            release_gates: SafetyCheck::Failed,
        },
        artifacts: &artifacts,
        dependencies: &edges,
    };
    let version = model_store::insert_realization_revision(&mut tx, &revision, now).await?;
    tx.commit().await?;
    let _ = ownership;
    Ok(Some(version))
}

#[allow(clippy::too_many_arguments)]
fn firmware_projection_rows(
    pool: &LookPool,
    look: &Look,
    space: looklab_color::RgbSpace,
    body: &BodyWarmStart,
    clip: f64,
    modes: Option<&BodyModes>,
    weighted: &WeightedFamily,
    contributors: &std::collections::HashMap<looklab_trust::EffectiveContributorId, u32>,
    options: &FitOptions,
) -> Result<(Vec<ImageProjection>, Vec<FirmwareId>)> {
    let epoch = pool
        .resources
        .schema
        .epochs
        .iter()
        .find(|epoch| epoch.id == pool.realization.epoch.get())
        .ok_or_else(|| {
            WorkerError::Data(format!(
                "epoch {} is not in the schema of {}",
                pool.realization.epoch.get(),
                pool.body
            ))
        })?;
    let delta = body.delta.transform();
    let mut images = Vec::new();
    let mut nuisances = Vec::new();
    let mut firmware = Vec::new();
    let mut clips = Vec::new();
    for (index, image) in pool.images.iter().enumerate() {
        if image.role != EvidenceRole::Native || !image.is_eligible() {
            continue;
        }
        let mut fit = weights::stage_fit_image(pool, weighted, contributors, index);
        let (trust_weight, sample_trust) = match fit.stage_weights.look() {
            Some(weights) if weights.trust_weight > 0.0 => {
                (weights.trust_weight, weights.sample_trust.clone())
            }
            _ => continue,
        };
        fit.trust_weight = trust_weight;
        fit.sample_trust = sample_trust;
        fit.transform = fit.transform.mul(&delta);
        let exposure_mode = epoch
            .exposure_modes
            .get(image.exposure_mode.0 as usize)
            .and_then(|mode| {
                modes.and_then(|modes| {
                    modes
                        .exposure
                        .iter()
                        .find_map(|(id, key)| (*key == mode.key).then_some(*id))
                })
            })
            .unwrap_or(image.exposure_mode);
        fit.exposure_mode = exposure_mode;
        let exposure = body
            .exposure
            .iter()
            .find(|offset| offset.mode == exposure_mode)
            .map_or(0.0, |offset| offset.offset);
        fit.exposure_hint = exposure.clamp(-8.0, 8.0);
        let wb = modes
            .and_then(|modes| {
                wb_class(
                    epoch,
                    &image.settings,
                    modes,
                    image.physical.as_ref().map(|physical| &physical.wb_mode),
                )
            })
            .and_then(|class| {
                body.wb
                    .iter()
                    .find_map(|(known, value)| (*known == class).then_some(*value))
            })
            .unwrap_or([0.0; 2]);
        nuisances.push(ImageNuisances {
            exposure,
            wb_log_r: wb[0],
            wb_log_b: wb[1],
            exposure_mode,
            contributor: fit.contributor,
            weight: fit.trust_weight,
        });
        firmware.push(
            FirmwareId::new(
                pool.body.clone(),
                ControlEpochId::new(pool.realization.epoch.get()),
                &image.contribution.firmware,
            )
            .map_err(WorkerError::Fit)?,
        );
        images.push(fit);
        clips.push(clip);
    }
    let projections = firmware_images(look, &images, &nuisances, &firmware, space, &clips, options)
        .map_err(WorkerError::Fit)?;
    Ok((projections, firmware))
}

#[allow(clippy::too_many_arguments)]
async fn propose_firmware_split(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    ownership: &JobOwnership,
    pool: &LookPool,
    target: &FamilyTargetId,
    look: &Look,
    space: looklab_color::RgbSpace,
    body: &BodyWarmStart,
    clip: f64,
    modes: Option<&BodyModes>,
    weighted: &WeightedFamily,
    contributors: &std::collections::HashMap<looklab_trust::EffectiveContributorId, u32>,
    dependencies: &[DependencyRef],
    snapshot: &[DependencyGeneration],
    options: &FitOptions,
) {
    let mut versions: BTreeSet<&str> = BTreeSet::new();
    for image in &pool.images {
        if image.role == EvidenceRole::Native && image.is_eligible() {
            versions.insert(image.contribution.firmware.as_str());
        }
    }
    if versions.len() < 2 {
        return;
    }
    let (projections, firmware) = match firmware_projection_rows(
        pool,
        look,
        space,
        body,
        clip,
        modes,
        weighted,
        contributors,
        options,
    ) {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(body = %pool.body, %error, "firmware projection failed");
            return;
        }
    };
    if let Err(error) = crate::split::detect_and_propose(
        db_pool,
        config,
        ownership,
        crate::split::FirmwareProjection {
            pool,
            target,
            look,
            firmware: &firmware,
            projections: &projections,
        },
        dependencies,
        snapshot,
    )
    .await
    {
        tracing::warn!(body = %pool.body, %error, "firmware split proposal failed");
    }
}

fn native_only(pool: &LookPool) -> LookPool {
    LookPool {
        body: pool.body.clone(),
        look: pool.look.clone(),
        realization: pool.realization.clone(),
        resources: pool.resources.clone(),
        transforms: pool.transforms.clone(),
        images: pool
            .images
            .iter()
            .filter(|image| image.role == EvidenceRole::Native)
            .cloned()
            .collect(),
        snapshot_version: pool.snapshot_version,
        data: pool.data.clone(),
        observation: pool.observation.clone(),
    }
}

/// The native images a qualified fit may consume.
fn eligible_native_only(pool: &LookPool) -> LookPool {
    let mut native = native_only(pool);
    native.images.retain(|image| image.is_eligible());
    native
}

/// The standalone family fit identity over a plan's training pools.
fn standalone_family_fingerprint(
    plan: &FamilyPlan,
    member_pools: &[(ScopeMember, LookPool)],
    training_pools: &[LookPool],
    roots: &RootData,
    options: &FitOptions,
) -> Result<String> {
    let subject = SubjectKey::for_family(&plan.family_id);
    let target = FamilyTargetId {
        family: plan.family_id.clone(),
        version: TargetVersion::new(1).expect("one is a positive target version"),
    };
    let native_pools: Vec<LookPool> = training_pools.iter().map(eligible_native_only).collect();
    let mut native_partitions = BTreeMap::new();
    for (member, pool) in member_pools {
        native_partitions.insert(
            (member.body.clone(), member.realization.clone()),
            partition_pool(&native_only(pool), &subject)?,
        );
    }
    let qualification = FitQualification::new(plan.cohort_report, plan.members.iter().cloned());
    subject_fingerprint(
        &subject,
        &native_pools,
        roots,
        &target,
        options,
        &native_partitions,
        &qualification,
    )
}

struct FamilyFingerprintInputs {
    pools: Vec<LookPool>,
    partitions: BTreeMap<(UploadBodyId, BodyRealizationId), Partition>,
    roots: RootData,
    qualification: FitQualification,
}

/// The inputs a family fingerprint hashes, independent of the component fit.
fn family_fingerprint_inputs(
    plan: &FamilyPlan,
    plans: &[&FamilyPlan],
    member_pools: &[(usize, ScopeMember, LookPool)],
    scores: &ConsistencyScores,
) -> Result<FamilyFingerprintInputs> {
    let family = &plan.family_id;
    let subject = SubjectKey::for_family(family);
    let mut pools = Vec::new();
    let mut partitions = BTreeMap::new();
    for (index, member, pool) in member_pools {
        if plans[*index].family_id != *family {
            continue;
        }
        let native = eligible_native_only(pool);
        partitions.insert(
            (member.body.clone(), member.realization.clone()),
            partition_pool(&native, &subject)?,
        );
        pools.push(native);
    }
    let qualification = FitQualification::new(plan.cohort_report, plan.members.iter().cloned());
    let roots = roots::load_family(&pools, scores)?;
    Ok(FamilyFingerprintInputs {
        pools,
        partitions,
        roots,
        qualification,
    })
}

fn family_fingerprint(
    plan: &FamilyPlan,
    plans: &[&FamilyPlan],
    member_pools: &[(usize, ScopeMember, LookPool)],
    scores: &ConsistencyScores,
    options: &FitOptions,
) -> Result<String> {
    let inputs = family_fingerprint_inputs(plan, plans, member_pools, scores)?;
    let target = FamilyTargetId {
        family: plan.family_id.clone(),
        version: TargetVersion::new(1).expect("one is a positive target version"),
    };
    subject_fingerprint(
        &SubjectKey::for_family(&plan.family_id),
        &inputs.pools,
        &inputs.roots,
        &target,
        options,
        &inputs.partitions,
        &inputs.qualification,
    )
}

async fn migration_outcome(
    db_pool: &SqlitePool,
    plan: &FamilyPlan,
    target: &FamilyTargetId,
    required: u32,
) -> Result<(
    Vec<(UploadBodyId, LookSlug, RefitOutcome)>,
    Vec<TouchedEpoch>,
)> {
    let version = target.version.get();
    let now = now_unix();
    let mut conn = db_pool.acquire().await?;
    model_store::register_subject(
        &mut conn,
        &ModelSubject::SharedFamily {
            family: plan.family_id.clone(),
        },
        now,
    )
    .await?;
    model_store::open_fit_hold(
        &mut conn,
        &SubjectKey::for_family(&plan.family_id),
        1,
        now,
        now,
        &serde_json::json!({
            "reason": "legacy_release_exposure_frame_unknown",
            "message": format!(
                "Release {required} predates the relative exposure gauge; an audited re-anchor and refit is required before version {version}."
            ),
            "pending_version": version,
            "required_release": required,
            "action": "moderator-approved audited re-anchor and refit",
        })
        .to_string(),
    )
    .await?;
    Ok((
        plan.members
            .iter()
            .map(|member| {
                (
                    member.body.clone(),
                    plan.family.clone(),
                    RefitOutcome::MigrationRequired(version),
                )
            })
            .collect(),
        Vec::new(),
    ))
}

#[allow(clippy::too_many_arguments)]
async fn fit_pinned(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    ownership: &JobOwnership,
    plan: &FamilyPlan,
    member: &ScopeMember,
    force: bool,
    snapshot_version: u64,
    snapshot_root: &std::path::Path,
) -> Result<(RefitOutcome, Vec<TouchedEpoch>)> {
    let subject = ModelSubject::BodyRealization {
        realization: member.realization.clone(),
    }
    .key();
    let observed_pending = {
        let mut conn = db_pool.acquire().await?;
        model_store::pending_generation(&mut conn, &subject).await?
    };
    let Some(head) = &plan.head else {
        ack_subject_generation(db_pool, &subject, observed_pending, now_unix()).await?;
        return Ok((
            RefitOutcome::Unavailable(format!(
                "{} has no published family target to pin",
                plan.family
            )),
            Vec::new(),
        ));
    };
    let target = head.target.clone();
    let observed_anchor = {
        let mut conn = db_pool.acquire().await?;
        model_store::subject_state(&mut conn, &SubjectKey::for_family(&target.family))
            .await?
            .and_then(|state| state.anchor_revision)
    };
    let snapshot = load_target_snapshot(db_pool, config, &plan.family, target.version).await?;
    if relative_gauge(&snapshot.record).is_none() {
        ack_subject_generation(db_pool, &subject, observed_pending, now_unix()).await?;
        return Ok((
            RefitOutcome::MigrationRequired(target.version.get()),
            Vec::new(),
        ));
    }
    if !db::manifest_exists(db_pool, plan.family.as_str(), target.version.get()).await? {
        ack_subject_generation(db_pool, &subject, observed_pending, now_unix()).await?;
        return Ok((
            RefitOutcome::Unavailable(format!("target {} is not published", plan.family)),
            Vec::new(),
        ));
    }
    let transforms = TransformDir::new(&config.transforms);
    if transforms.load_optional(&member.body)?.is_none() {
        ack_subject_generation(db_pool, &subject, observed_pending, now_unix()).await?;
        return Ok((
            RefitOutcome::Unavailable(format!(
                "no input transform is published for {}",
                member.body
            )),
            Vec::new(),
        ));
    }
    let mut pool = crate::pool::build(
        db_pool,
        config,
        &member.body,
        &plan.family,
        &member.realization,
        &member.scope,
    )
    .await?;
    crate::pool::resolve_contributors(std::slice::from_mut(&mut pool))?;
    if pool.images.is_empty() {
        ack_subject_generation(db_pool, &subject, observed_pending, now_unix()).await?;
        return Ok((
            RefitOutcome::Unavailable(format!(
                "no eligible evidence for {} on {}",
                plan.family, member.body
            )),
            Vec::new(),
        ));
    }
    let scores = load_consistency_scores(
        db_pool,
        std::slice::from_ref(&(member.clone(), pool.clone())),
    )
    .await?;
    let partition = partition_pool(&pool, &subject)?;
    let mut partitions = BTreeMap::new();
    partitions.insert(
        (member.body.clone(), member.realization.clone()),
        partition.clone(),
    );
    let params = TrustParams {
        support_sigma_exit: fit_options(config).support_sigma_exit,
        ..TrustParams::default()
    };
    let roots = roots::load_family(std::slice::from_ref(&partition.training), &scores)?;
    let weighted = weights::weigh_family(
        std::slice::from_ref(&partition.training),
        &params,
        &roots.weights,
        &scores,
    )?;
    let contributors =
        weights::family_contributor_index(std::slice::from_ref(&partition.training), &scores);
    // The pinned conditional fit reads its own saved target-bound reference state;
    // this lineage is not written into the common family lineage.
    let participating = BTreeSet::from([(member.body.clone(), member.realization.clone())]);
    let mut lineage = load_lineage(
        db_pool,
        config,
        std::slice::from_ref(&(member.clone(), pool.clone())),
        &participating,
    )
    .await?;
    if !lineage.authoritative_missing.is_empty() {
        ack_subject_generation(db_pool, &subject, observed_pending, now_unix()).await?;
        return Ok((
            RefitOutcome::Unavailable("admitted lineage state is not recoverable".to_owned()),
            Vec::new(),
        ));
    }
    let options = fit_options(config);
    let targets = BTreeMap::from([(member.realization.clone(), target.clone())]);
    let mut prepared_modes =
        body_modes_for_members(std::slice::from_ref(&(member.clone(), pool.clone())))?;
    rebind_lineage_modes(&mut lineage, &mut prepared_modes)?;
    let (bodies_data, body_modes) = build_bodies(
        std::slice::from_ref(&(member.clone(), pool.clone())),
        &weighted,
        &contributors,
        &lineage,
        &targets,
        &partitions,
        &prepared_modes,
        !lineage.qualified,
    )?;
    if bodies_data.is_empty() {
        ack_subject_generation(db_pool, &subject, observed_pending, now_unix()).await?;
        return Ok((
            RefitOutcome::Unavailable(format!(
                "no native evidence for {} on {}",
                plan.family, member.body
            )),
            Vec::new(),
        ));
    }
    let fingerprint = subject_fingerprint(
        &subject,
        std::slice::from_ref(&partition.training),
        &roots,
        &target,
        &options,
        &partitions,
        &FitQualification::new(None, std::iter::once(member.clone())),
    )?;
    let prior_attempt = {
        let mut conn = db_pool.acquire().await?;
        model_store::fit_attempt(&mut conn, &subject).await?
    };
    if !force && prior_attempt.is_none() && {
        let mut conn = db_pool.acquire().await?;
        model_store::pool_fingerprint(&mut conn, &subject)
            .await?
            .as_deref()
            == Some(fingerprint.as_str())
    } {
        return Ok((RefitOutcome::Skipped, Vec::new()));
    }
    let mut resume_inventories = BTreeMap::new();
    let mut resume = prior_attempt
        .as_ref()
        .and_then(|attempt| attempt.states.get(REALIZATION_BLOCK))
        .and_then(|checkpoint| {
            serde_json::from_str::<StoredCheckpoint>(checkpoint.as_str())
                .ok()
                .map(|checkpoint| {
                    resume_inventories = checkpoint.inventories;
                    checkpoint.state
                })
        })
        .filter(|state| state.generation == HierStateGeneration::CURRENT);
    if let Some(state) = &mut resume
        && rebind_hier_state(state, &resume_inventories, &mut prepared_modes).is_err()
    {
        resume = None;
    }
    let mut observed = crate::pool::EvidenceObservation::merge([&pool.observation])?;
    let pin_stage = lineage
        .body_stage
        .get(&member.body)
        .copied()
        .unwrap_or(BodyStateStage::Published);
    let qualified_warm = if lineage.qualified {
        let warm = if pin_stage == BodyStateStage::Native {
            lineage
                .body_warm
                .get(&member.body)
                .map(|saved| saved.warm.clone())
        } else {
            lineage
                .published_body_warm
                .get(&member.body)
                .map(|saved| saved.warm.clone())
        };
        match warm {
            Some(warm) => Some(warm),
            None => match lineage.body_state.get(&member.body) {
                None => None,
                Some((version, _, _)) => {
                    let mut conn = db_pool.acquire().await?;
                    match model_store::body_state(&mut conn, &member.body, *version).await? {
                        None => None,
                        Some(state) => {
                            let bytes = read_blob(config, &state.pins.body_params.sha256)?;
                            let params: MatchParams = serde_json::from_slice(&bytes)?;
                            body_modes
                                .get(&member.body)
                                .and_then(|modes| body_warm_from_params(&params, modes))
                        }
                    }
                }
            },
        }
    } else {
        None
    };
    if lineage.qualified && qualified_warm.is_none() {
        ack_subject_generation(db_pool, &subject, observed_pending, now_unix()).await?;
        return Ok((
            RefitOutcome::Unavailable(
                "qualified body pin is not recoverable from its immutable body state".to_owned(),
            ),
            Vec::new(),
        ));
    }
    let projection_published = qualified_warm.is_some() && pin_stage == BodyStateStage::Published;
    let body_pin = qualified_warm
        .map(|state| {
            let resume = vec![BodyResume {
                id: member.body.clone(),
                state,
            }];
            match pin_stage {
                BodyStateStage::Native => BodyPin::Native(resume),
                BodyStateStage::Published => BodyPin::Published(resume),
            }
        })
        .unwrap_or(BodyPin::Free);
    let prefit_native = prefit_body_dependencies(
        config,
        &[plan],
        std::slice::from_ref(&(member.clone(), pool.clone())),
        snapshot_version,
        &fit_options(config),
        BodyStateStage::Native,
    )?;
    let prefit_published = prefit_body_dependencies(
        config,
        &[plan],
        std::slice::from_ref(&(member.clone(), pool.clone())),
        snapshot_version,
        &fit_options(config),
        BodyStateStage::Published,
    )?;
    let target_version_dependency = DependencyRef::required(
        DependencyKind::FamilyVersion,
        DependencyTarget::family_version(&target),
    );
    let mut all = prefit_native.clone();
    all.extend(prefit_published.iter().cloned());
    all.push(target_version_dependency.clone());
    crate::pool::complete_observation(db_pool, &mut observed, &all).await?;
    let prefit_native_snapshot = observed.snapshot_for(&prefit_native)?;
    let prefit_published_snapshot = observed.snapshot_for(&prefit_published)?;
    let mut hier_options = hier_options(
        config,
        &lineage,
        &bodies_data,
        resume,
        rebound_sigma(&lineage, &contributors),
        body_pin,
    );
    let input = HierInput {
        families: vec![FamilyData::pinned(target.clone(), snapshot.look.clone())],
        bodies: bodies_data,
    };
    let mut fit = fit_realizations(&input, &hier_options);
    if let Err(looklab_fit::FitError::ResumeMismatch) = &fit
        && hier_options.resume.is_some()
    {
        hier_options.resume = None;
        fit = fit_realizations(&input, &hier_options);
    }
    let fit = match fit {
        Ok(fit) => fit,
        Err(looklab_fit::FitError::NoImages) => {
            ack_subject_generation(db_pool, &subject, observed_pending, now_unix()).await?;
            return Ok((
                RefitOutcome::Unavailable(
                    "no native images remain after partition and eligibility".to_owned(),
                ),
                Vec::new(),
            ));
        }
        Err(looklab_fit::FitError::NoModelErrorEvidence) => {
            let mut conn = db_pool.acquire().await?;
            model_store::record_pool_fingerprint(&mut conn, &subject, &fingerprint, now_unix())
                .await?;
            drop(conn);
            ack_subject_generation(db_pool, &subject, observed_pending, now_unix()).await?;
            return Ok((
                RefitOutcome::Unavailable(
                    "missing independent capture groups for held-out model error".to_owned(),
                ),
                Vec::new(),
            ));
        }
        Err(error) => return Err(WorkerError::Fit(error)),
    };
    hier_options.resume = None;
    let now = now_unix();
    if !fit.numerically_publishable() {
        let state = serde_json::to_string(&StoredCheckpoint {
            state: fit.state.clone(),
            inventories: body_modes.clone(),
        })?;
        let mut conn = db_pool.acquire().await?;
        let consecutive = model_store::record_fit_attempt(
            &mut conn,
            &subject,
            REALIZATION_BLOCK,
            Some(&FitCheckpoint::new(state)),
            now,
        )
        .await?;
        if consecutive >= config.max_consecutive_nonconverged {
            model_store::open_fit_hold(
                &mut conn,
                &subject,
                consecutive,
                prior_attempt
                    .as_ref()
                    .map_or(now, |attempt| attempt.started_at),
                now,
                &serde_json::json!({
                    "converged": fit.diagnostics.converged,
                    "evaluations": fit.diagnostics.evaluations,
                    "outer_iterations": fit.diagnostics.outer_iterations,
                    "limitations": fit.diagnostics.limitations,
                })
                .to_string(),
            )
            .await?;
        }
        ack_subject_generation(db_pool, &subject, observed_pending, now_unix()).await?;
        return Ok((RefitOutcome::NotConverged(target.version.get()), Vec::new()));
    }
    {
        let mut conn = db_pool.acquire().await?;
        model_store::clear_fit_attempt(&mut conn, &subject).await?;
        model_store::resolve_fit_hold(&mut conn, &subject).await?;
    }
    let gauge = fit
        .gauges
        .iter()
        .map(|binding| binding.gauge.clone())
        .find(|gauge| gauge.designated.as_str() == member.body.as_str());
    let gauge_value = gauge
        .as_ref()
        .map_or(0.0, |gauge| gauge_value(&fit.bodies, gauge));
    let target_deps = target_dependencies(
        config,
        plan,
        std::slice::from_ref(&(member.clone(), pool.clone())),
        &roots,
        snapshot_version,
        &options,
        &partitions,
        BodyStateStage::Native,
    )?;
    let projection_pool = native_only(&pool);
    let split_deps = {
        let mut dependencies = target_deps.clone();
        dependencies.extend(crate::pool::stage_dependencies(
            std::slice::from_ref(&projection_pool),
            crate::pool::EvidenceStage::Native,
        ));
        dependencies.push(target_version_dependency.clone());
        dependencies.sort();
        dependencies.dedup();
        dependencies
    };
    let mut all = prefit_native.clone();
    all.extend(prefit_published.iter().cloned());
    all.extend(target_deps.iter().cloned());
    all.extend(split_deps.iter().cloned());
    crate::pool::complete_observation(db_pool, &mut observed, &all).await?;
    let split_snapshot = observed.snapshot_for(&split_deps)?;
    let safety = CandidateSafety {
        numerical: SafetyCheck::Passed,
        release_gates: SafetyCheck::Passed,
    };
    let look_artifacts = target_artifact_bindings(&snapshot.document);
    let mut tx = model_store::begin_immediate(db_pool).await?;
    let body_versions: BTreeMap<UploadBodyId, PersistedBody> =
        if lineage.qualified && lineage.body_state.contains_key(&member.body) {
            lineage
                .body_state
                .iter()
                .map(|(body, (version, gauge, transform_version))| {
                    (
                        body.clone(),
                        PersistedBody {
                            native: Some(*version),
                            published: *version,
                            gauge: *gauge,
                            transform_version: *transform_version,
                        },
                    )
                })
                .collect()
        } else {
            persist_body_states(
                &mut tx,
                config,
                plan,
                &fit,
                &body_modes,
                gauge.as_ref(),
                gauge_value,
                snapshot_version,
                &prefit_native,
                &prefit_native_snapshot,
                &prefit_published,
                &prefit_published_snapshot,
                &options,
                now,
            )
            .await?
        };
    let membership = membership(&pool, &partition)?;
    let modes = body_modes
        .get(&member.body)
        .ok_or_else(|| WorkerError::Data("missing WB mode inventory".to_owned()))?;
    let (native_version, recipe_version) = persist_realization(
        &mut tx,
        config,
        &target,
        member,
        &pool,
        &fit,
        &body_versions,
        &look_artifacts,
        &membership,
        &target_deps,
        &prefit_native,
        &prefit_published,
        modes,
        safety,
        &contributors,
        now,
    )
    .await?;
    let mut touched = Vec::new();
    let mut candidates: Vec<(ModelSubject, ModelVersionId)> = Vec::new();
    if let Some(version) = recipe_version {
        if let Some(published) = body_versions.get(&member.body) {
            touched.push(TouchedEpoch {
                body: member.body.clone(),
                epoch: member.realization.epoch.get(),
                snapshot_version,
                snapshot_root: snapshot_root.to_owned(),
                published_state_version: published.published,
            });
        }
        let candidate = ModelVersionId::BodyRealization {
            realization: looklab_schema::BodyRealizationVersionId {
                realization: member.realization.clone(),
                version,
                target: target.clone(),
            },
        };
        let subject = ModelSubject::BodyRealization {
            realization: member.realization.clone(),
        };
        if safety.accepted() {
            schedule_candidate(&mut tx, &subject, &candidate, now).await?;
            candidates.push((subject, candidate));
        }
    }
    if let Some(version) = native_version {
        let candidate = ModelVersionId::BodyRealization {
            realization: looklab_schema::BodyRealizationVersionId {
                realization: member.realization.clone(),
                version,
                target: target.clone(),
            },
        };
        let subject = ModelSubject::BodyRealization {
            realization: member.realization.clone(),
        };
        if safety.accepted() {
            schedule_candidate(&mut tx, &subject, &candidate, now).await?;
            candidates.push((subject, candidate));
        }
    }
    model_store::record_pool_fingerprint(&mut tx, &subject, &fingerprint, now).await?;
    for (subject, _) in &candidates {
        model_store::note_evidence_arrival(&mut tx, &subject.key(), now).await?;
    }
    tx.commit().await?;
    for (subject, candidate) in &candidates {
        if matches!(candidate, ModelVersionId::BodyRealization { .. }) {
            let pending = {
                let mut conn = db_pool.acquire().await?;
                model_store::pending_generation(&mut conn, &subject.key()).await?
            };
            select_experimental(
                db_pool,
                ownership,
                &subject.key(),
                candidate,
                &target,
                pending,
                observed_anchor,
                now,
            )
            .await?;
        }
    }
    if let Ok(projection_weighted) = weights::weigh_family(
        std::slice::from_ref(&projection_pool),
        &params,
        &roots.weights,
        &scores,
    ) {
        let projection_contributors =
            weights::family_contributor_index(std::slice::from_ref(&projection_pool), &scores);
        let (state_bodies, state_realizations) = if projection_published {
            (
                &fit.state.published_bodies,
                &fit.state.published_realizations,
            )
        } else {
            (&fit.state.bodies, &fit.state.realizations)
        };
        if let Some(body) = state_bodies.iter().find(|body| body.id == member.body)
            && let Some(realization) = state_realizations.iter().find(|realization| {
                realization.id == member.realization && realization.target == target
            })
        {
            propose_firmware_split(
                db_pool,
                config,
                ownership,
                &pool,
                &target,
                &snapshot.look,
                snapshot.record.output_space,
                &body.state,
                realization.state.clip_k,
                body_modes.get(&member.body),
                &projection_weighted,
                &projection_contributors,
                &split_deps,
                &split_snapshot,
                &options,
            )
            .await;
        }
    } else {
        tracing::warn!(family = %plan.family, "firmware split weighting failed");
    }
    let fallback_body_state = body_versions
        .get(&member.body)
        .map(|body| (body.published, body.gauge, body.transform_version));
    fit_diagnostic(
        db_pool,
        config,
        ownership,
        plan,
        member,
        snapshot_version,
        fallback_body_state,
        now,
    )
    .await?;
    {
        let mut conn = db_pool.acquire().await?;
        ack_pending(&mut conn, &subject, observed_pending, now).await?;
    }
    Ok((
        recipe_version.map_or(RefitOutcome::Held(target.version.get()), |version| {
            RefitOutcome::Published(version.get())
        }),
        touched,
    ))
}

/// The deterministic held-out groups for one subject.
#[must_use]
pub fn assessment_partition(
    subject: &SubjectKey,
    groups: &[EvidenceGroupId],
) -> BTreeSet<EvidenceGroupId> {
    if groups.len() < 2 {
        return BTreeSet::new();
    }
    let mut held_out = BTreeSet::new();
    for group in groups {
        let mut hasher = Sha256::new();
        hasher.update(PARTITION_POLICY.as_bytes());
        hasher.update([0]);
        hasher.update(subject.as_str().as_bytes());
        hasher.update([0]);
        hasher.update(group.as_str().as_bytes());
        if hasher.finalize()[0] % 3 == 0 {
            held_out.insert(group.clone());
        }
    }
    if held_out.len() == groups.len() {
        held_out.clear();
    }
    held_out
}

/// Digest binding a subject's partition and policy into report provenance.
#[must_use]
pub fn partition_policy_digest(
    subject: &SubjectKey,
    assessment: &BTreeSet<EvidenceGroupId>,
) -> Sha256Hex {
    let mut hasher = Sha256::new();
    hasher.update(PARTITION_POLICY.as_bytes());
    hasher.update([0]);
    hasher.update(subject.as_str().as_bytes());
    for group in assessment {
        hasher.update([0]);
        hasher.update(group.as_str().as_bytes());
    }
    Sha256Hex::from_bytes(hasher.finalize().into())
}

/// The re-render group when preparation recorded one, else the image identity.
pub fn evidence_group(image: &PoolImage) -> Result<EvidenceGroupId> {
    let token = match image.samples.header().rerender_group {
        Some(group) => format!("rerender-{}", db::hex(group.as_bytes())),
        None => image.image.id.as_str(),
    };
    Ok(EvidenceGroupId::new(token)?)
}

fn partition_pool(pool: &LookPool, subject: &SubjectKey) -> Result<Partition> {
    let mut groups = BTreeSet::new();
    for image in &pool.images {
        if image.is_eligible() {
            groups.insert(evidence_group(image)?);
        }
    }
    let group_list: Vec<EvidenceGroupId> = groups.iter().cloned().collect();
    let assessment = assessment_partition(subject, &group_list);
    let training_groups: BTreeSet<EvidenceGroupId> = groups
        .into_iter()
        .filter(|group| !assessment.contains(group))
        .collect();
    let mut images = Vec::new();
    for image in &pool.images {
        if training_groups.contains(&evidence_group(image)?) {
            images.push(image.clone());
        }
    }
    Ok(Partition {
        training: LookPool {
            body: pool.body.clone(),
            look: pool.look.clone(),
            realization: pool.realization.clone(),
            resources: pool.resources.clone(),
            transforms: pool.transforms.clone(),
            images,
            snapshot_version: pool.snapshot_version,
            data: pool.data.clone(),
            observation: pool.observation.clone(),
        },
        assessment,
        training_groups,
    })
}

fn membership(pool: &LookPool, partition: &Partition) -> Result<EvidenceMembership> {
    let mut blobs = Vec::new();
    let mut groups = Vec::new();
    let mut roles: BTreeMap<String, StoredEvidenceRole> = BTreeMap::new();
    for image in &pool.images {
        if !image.is_eligible() {
            continue;
        }
        let group = evidence_group(image)?;
        let role = if partition.assessment.contains(&group) {
            StoredEvidenceRole::Assessment
        } else {
            StoredEvidenceRole::Training
        };
        blobs.push(EvidenceBlob {
            image: ImageRef::new(image.image.id.as_str()).map_err(|error| {
                WorkerError::Data(format!("image id is not an evidence reference: {error}"))
            })?,
            samples_hash: Sha256Hex::from_bytes(image.image.samples_hash),
            role,
        });
        groups.push(model_store::EvidenceGroup { group, role });
        let contribution = image.contribution.id.as_str().to_owned();
        match roles.get(&contribution) {
            Some(StoredEvidenceRole::Training) => {}
            _ => {
                roles.insert(contribution, role);
            }
        }
    }
    groups.sort_by(|left, right| left.group.as_str().cmp(right.group.as_str()));
    groups.dedup_by(|left, right| left.group == right.group);
    blobs.sort_by(|left, right| left.image.as_str().cmp(right.image.as_str()));
    let mut role_rows = Vec::new();
    for (contribution, role) in roles {
        role_rows.push(model_store::EvidenceRoleAssignment {
            contribution: ContributionRef::new(contribution).map_err(|error| {
                WorkerError::Data(format!(
                    "contribution id is not an evidence reference: {error}"
                ))
            })?,
            role,
        });
    }
    Ok(EvidenceMembership {
        blobs,
        groups,
        roles: role_rows,
        evaluated_images: Vec::new(),
        evaluated_contributions: Vec::new(),
    })
}

fn body_modes_for_members(
    members: &[(ScopeMember, LookPool)],
) -> Result<BTreeMap<UploadBodyId, BodyModes>> {
    let mut by_body: BTreeMap<UploadBodyId, Vec<&(ScopeMember, LookPool)>> = BTreeMap::new();
    for entry in members {
        by_body.entry(entry.0.body.clone()).or_default().push(entry);
    }
    let mut modes = BTreeMap::new();
    for (body, entries) in by_body {
        modes.insert(body.clone(), body_modes(&body, &entries)?);
    }
    Ok(modes)
}

#[allow(clippy::too_many_arguments)]
fn build_bodies(
    members: &[(ScopeMember, LookPool)],
    weighted: &WeightedFamily,
    contributors: &std::collections::HashMap<looklab_trust::EffectiveContributorId, u32>,
    lineage: &Lineage,
    targets: &BTreeMap<BodyRealizationId, FamilyTargetId>,
    partitions: &BTreeMap<(UploadBodyId, BodyRealizationId), Partition>,
    body_modes_map: &BTreeMap<UploadBodyId, BodyModes>,
    require_observed_wb: bool,
) -> Result<(Vec<BodyData>, BTreeMap<UploadBodyId, BodyModes>)> {
    let mut by_body: BTreeMap<UploadBodyId, Vec<&(ScopeMember, LookPool)>> = BTreeMap::new();
    for entry in members {
        by_body.entry(entry.0.body.clone()).or_default().push(entry);
    }
    let mut bodies = Vec::new();
    let mut modes_by_body = BTreeMap::new();
    for (body, entries) in by_body {
        let first = entries
            .first()
            .ok_or_else(|| WorkerError::Data("empty body group".to_owned()))?;
        let transform = first
            .1
            .transforms
            .input_transform(&body, TransformVersion::new(first.1.transforms.version()))?;
        let modes = body_modes_map
            .get(&body)
            .ok_or_else(|| WorkerError::Data("missing WB mode inventory".to_owned()))?
            .clone();
        let mut realizations = Vec::new();
        let mut default_mode = None;
        let mut default_wb = None;
        for entry in &entries {
            let (member, pool) = (**entry).clone();
            let epoch = pool
                .resources
                .schema
                .epochs
                .iter()
                .find(|epoch| epoch.id == member.realization.epoch.get())
                .ok_or_else(|| {
                    WorkerError::Data(format!(
                        "epoch {} is not in the schema of {body}",
                        member.realization.epoch.get()
                    ))
                })?;
            let partition = partitions
                .get(&(member.body.clone(), member.realization.clone()))
                .ok_or_else(|| WorkerError::Data("missing partition".to_owned()))?;
            let mut entries: BTreeSet<usize> = weighted
                .body_entries(&pool)
                .into_iter()
                .map(|entry| entry.0)
                .collect();
            entries.extend(
                weighted
                    .body_entries_native(&pool)
                    .into_iter()
                    .map(|entry| entry.0),
            );
            let mut images = Vec::new();
            let held_out = pool
                .images
                .iter()
                .filter(|image| {
                    evidence_group(image)
                        .is_ok_and(|group| !partition.training_groups.contains(&group))
                })
                .count();
            let (mut unknown_exposure, mut unobserved_wb) = (0usize, 0usize);
            for index in &entries {
                let image = &pool.images[*index];
                if !partition.training_groups.contains(&evidence_group(image)?) {
                    continue;
                }
                let Some(exposure_key) = epoch
                    .exposure_modes
                    .get(image.exposure_mode.0 as usize)
                    .map(|mode| mode.key.clone())
                else {
                    unknown_exposure += 1;
                    continue;
                };
                let Some(exposure_mode) = modes
                    .exposure
                    .iter()
                    .find_map(|(id, key)| (*key == exposure_key).then_some(*id))
                else {
                    unknown_exposure += 1;
                    continue;
                };
                let mut fit = weights::stage_fit_image(&pool, weighted, contributors, *index);
                fit.exposure_mode = exposure_mode;
                let wb_mode = match (
                    wb_class(
                        epoch,
                        &image.settings,
                        &modes,
                        image.physical.as_ref().map(|physical| &physical.wb_mode),
                    ),
                    require_observed_wb,
                ) {
                    (Some(class), _) => class,
                    (None, true) => {
                        unobserved_wb += 1;
                        continue;
                    }
                    (None, false) => UNKNOWN_WB_CLASS,
                };
                default_mode.get_or_insert(exposure_mode);
                if default_wb.is_none() && wb_mode != UNKNOWN_WB_CLASS {
                    default_wb = Some(wb_mode);
                }
                images.push(HierImage {
                    image: fit,
                    firmware: FirmwareId::new(
                        body.clone(),
                        ControlEpochId::new(member.realization.epoch.get()),
                        &image.contribution.firmware,
                    )?,
                    wb_mode,
                });
            }
            if images.is_empty() {
                tracing::warn!(
                    body = %member.body,
                    realization = %member.realization.native_sim,
                    pool_images = pool.images.len(),
                    weighted = entries.len(),
                    held_out,
                    unknown_exposure,
                    unobserved_wb,
                    "no image of this realization enters the fit"
                );
                continue;
            }
            realizations.push(RealizationData {
                id: member.realization.clone(),
                target: targets
                    .get(&member.realization)
                    .cloned()
                    .ok_or_else(|| WorkerError::Data("missing family target".to_owned()))?,
                model: lineage.crm.get(&member.realization).cloned(),
                images,
                warm: lineage
                    .realization_warm
                    .get(&member.realization)
                    .map(|saved| saved.warm.clone()),
            });
        }
        if realizations.is_empty() {
            continue;
        }
        modes_by_body.insert(body.clone(), modes);
        bodies.push(BodyData {
            id: body.clone(),
            transform,
            default_mode: default_mode.unwrap_or_default(),
            default_wb_mode: default_wb.unwrap_or(UNKNOWN_WB_CLASS),
            charts: Vec::new(),
            warm: lineage.body_warm.get(&body).map(|saved| saved.warm.clone()),
            realizations,
        });
    }
    Ok((bodies, modes_by_body))
}

fn body_modes(body: &UploadBodyId, entries: &[&(ScopeMember, LookPool)]) -> Result<BodyModes> {
    let mut epochs = Vec::new();
    let mut physical = BTreeSet::new();
    for (member, pool) in entries {
        let epoch = pool
            .resources
            .schema
            .epochs
            .iter()
            .find(|epoch| epoch.id == member.realization.epoch.get())
            .ok_or_else(|| {
                WorkerError::Data(format!(
                    "epoch {} is not in the schema of {body}",
                    member.realization.epoch.get()
                ))
            })?;
        epochs.push(epoch);
        physical.extend(physical_wb_modes(pool));
    }
    Ok(mode_maps(&epochs, &physical))
}

fn physical_wb_modes(pool: &LookPool) -> BTreeSet<WbModeKey> {
    pool.images
        .iter()
        .filter_map(|image| {
            image
                .physical
                .as_ref()
                .map(|physical| physical.wb_mode.clone())
        })
        .collect()
}

fn mode_maps(epochs: &[&Epoch], physical: &BTreeSet<WbModeKey>) -> BodyModes {
    let mut exposure_keys = BTreeSet::new();
    let mut wb_key_names: Vec<WbModeKey> = Vec::new();
    for epoch in epochs {
        for mode in &epoch.exposure_modes {
            exposure_keys.insert(mode.key.clone());
        }
        let mut controls: Vec<&Control> = epoch
            .controls
            .iter()
            .filter(|control| {
                control.kind == ControlType::Enum && control.affects.contains(&Affects::Wb)
            })
            .collect();
        controls.sort_by(|left, right| left.key.as_str().cmp(right.key.as_str()));
        for control in controls {
            if let Some(values) = &control.values {
                for value in values {
                    if let Ok(key) = WbModeKey::new(value.as_str())
                        && !wb_key_names.contains(&key)
                    {
                        wb_key_names.push(key);
                    }
                }
            }
        }
    }
    let mut wb = BTreeMap::new();
    for (index, key) in wb_key_names.into_iter().enumerate() {
        if let Ok(index) = u32::try_from(index + 1) {
            wb.insert(WbModeClass(index), key);
        }
    }
    for key in physical {
        if wb.values().any(|known| known == key) {
            continue;
        }
        let mut class = physical_wb_class(key);
        while wb.contains_key(&class) {
            class = WbModeClass(class.0.wrapping_add(1));
            if class.0 == u32::MAX {
                class = WbModeClass(0x8000_0000);
            }
        }
        wb.insert(class, key.clone());
    }
    let mut exposure = BTreeMap::new();
    for (index, key) in exposure_keys.into_iter().enumerate() {
        if let Ok(index) = u32::try_from(index) {
            exposure.insert(ExposureModeId(index), key);
        }
    }
    BodyModes { exposure, wb }
}

fn physical_wb_class(key: &WbModeKey) -> WbModeClass {
    let digest = Sha256::digest(key.as_str().as_bytes());
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&digest[..4]);
    let value = u32::from_le_bytes(bytes) | 0x8000_0000;
    WbModeClass(if value == u32::MAX {
        u32::MAX - 1
    } else {
        value
    })
}

fn ensure_wb_key(modes: &mut BodyModes, key: &WbModeKey) -> WbModeClass {
    if let Some((class, _)) = modes.wb.iter().find(|(_, known)| *known == key) {
        return *class;
    }
    let mut class = physical_wb_class(key);
    while modes.wb.contains_key(&class) {
        class = WbModeClass(class.0.wrapping_add(1));
        if class.0 == u32::MAX {
            class = WbModeClass(0x8000_0000);
        }
    }
    modes.wb.insert(class, key.clone());
    class
}

fn ensure_exposure_key(modes: &mut BodyModes, key: &ExposureModeKey) -> ExposureModeId {
    if let Some((id, _)) = modes.exposure.iter().find(|(_, known)| *known == key) {
        return *id;
    }
    let digest = Sha256::digest(key.as_str().as_bytes());
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&digest[..4]);
    let mut id = ExposureModeId(u32::from_le_bytes(bytes) | 0x8000_0000);
    while modes.exposure.contains_key(&id) {
        id = ExposureModeId(id.0.wrapping_add(1));
    }
    modes.exposure.insert(id, key.clone());
    id
}

fn rebind_warm(warm: &mut BodyWarmStart, saved: &BodyModes, modes: &mut BodyModes) -> Result<()> {
    for offset in &mut warm.exposure {
        let Some(key) = saved.exposure.get(&offset.mode) else {
            return Err(WorkerError::Data(
                "saved exposure mode has no key in its inventory".to_owned(),
            ));
        };
        offset.mode = ensure_exposure_key(modes, key);
    }
    for (class, _) in &mut warm.wb {
        let Some(key) = saved.wb.get(class) else {
            return Err(WorkerError::Data(
                "saved WB class has no key in its inventory".to_owned(),
            ));
        };
        *class = ensure_wb_key(modes, key);
    }
    Ok(())
}

fn rebind_realization_warm(
    warm: &mut RealizationWarmStart,
    saved: &BodyModes,
    modes: &mut BodyModes,
) -> Result<()> {
    for nuisance in &mut warm.nuisances {
        let Some(key) = saved.exposure.get(&nuisance.exposure_mode) else {
            return Err(WorkerError::Data(
                "saved realization exposure mode has no key in its inventory".to_owned(),
            ));
        };
        nuisance.exposure_mode = ensure_exposure_key(modes, key);
    }
    Ok(())
}

fn rebind_hier_state(
    state: &mut HierState,
    inventories: &BTreeMap<UploadBodyId, BodyModes>,
    modes: &mut BTreeMap<UploadBodyId, BodyModes>,
) -> Result<()> {
    for body in state
        .bodies
        .iter_mut()
        .chain(state.published_bodies.iter_mut())
    {
        let (Some(inventory), Some(modes)) = (inventories.get(&body.id), modes.get_mut(&body.id))
        else {
            return Err(WorkerError::Data(
                "checkpoint body has no saved mode inventory".to_owned(),
            ));
        };
        for key in inventory.exposure.values() {
            ensure_exposure_key(modes, key);
        }
        for key in inventory.wb.values() {
            ensure_wb_key(modes, key);
        }
        rebind_warm(&mut body.state, inventory, modes)?;
    }
    for realization in state
        .realizations
        .iter_mut()
        .chain(state.published_realizations.iter_mut())
    {
        let (Some(inventory), Some(modes)) = (
            inventories.get(&realization.id.body),
            modes.get_mut(&realization.id.body),
        ) else {
            return Err(WorkerError::Data(
                "checkpoint realization has no saved mode inventory".to_owned(),
            ));
        };
        for key in inventory.exposure.values() {
            ensure_exposure_key(modes, key);
        }
        for key in inventory.wb.values() {
            ensure_wb_key(modes, key);
        }
        rebind_realization_warm(&mut realization.state, inventory, modes)?;
    }
    Ok(())
}

fn rebind_lineage_modes(
    lineage: &mut Lineage,
    modes: &mut BTreeMap<UploadBodyId, BodyModes>,
) -> Result<()> {
    for (body, saved) in lineage
        .body_warm
        .iter_mut()
        .chain(lineage.published_body_warm.iter_mut())
    {
        let Some(body_modes) = modes.get_mut(body) else {
            continue;
        };
        for key in saved.inventory.exposure.values() {
            ensure_exposure_key(body_modes, key);
        }
        for key in saved.inventory.wb.values() {
            ensure_wb_key(body_modes, key);
        }
        rebind_warm(&mut saved.warm, &saved.inventory, body_modes)?;
        saved.inventory = body_modes.clone();
    }
    for (id, saved) in lineage
        .realization_warm
        .iter_mut()
        .chain(lineage.published_realization_warm.iter_mut())
    {
        let Some(body_modes) = modes.get_mut(&id.body) else {
            continue;
        };
        for key in saved.inventory.exposure.values() {
            ensure_exposure_key(body_modes, key);
        }
        for key in saved.inventory.wb.values() {
            ensure_wb_key(body_modes, key);
        }
        rebind_realization_warm(&mut saved.warm, &saved.inventory, body_modes)?;
        saved.inventory = body_modes.clone();
    }
    if let Some(saved) = &mut lineage.gauge {
        let Some((_, body_modes)) = modes
            .iter_mut()
            .find(|(body, _)| body.as_str() == saved.gauge.designated.as_str())
        else {
            return Ok(());
        };
        for key in saved.inventory.exposure.values() {
            ensure_exposure_key(body_modes, key);
        }
        for key in saved.inventory.wb.values() {
            ensure_wb_key(body_modes, key);
        }
        let exposure = saved
            .inventory
            .exposure
            .get(&saved.gauge.exposure_mode)
            .ok_or_else(|| {
                WorkerError::Data("saved gauge exposure mode has no inventory key".to_owned())
            })?;
        let wb = saved
            .inventory
            .wb
            .get(&saved.gauge.wb_mode)
            .ok_or_else(|| {
                WorkerError::Data("saved gauge WB class has no inventory key".to_owned())
            })?;
        saved.gauge.exposure_mode = ensure_exposure_key(body_modes, exposure);
        saved.gauge.wb_mode = ensure_wb_key(body_modes, wb);
        saved.inventory = body_modes.clone();
    }
    Ok(())
}

fn wb_class(
    epoch: &Epoch,
    settings: &Settings,
    modes: &BodyModes,
    physical: Option<&WbModeKey>,
) -> Option<WbModeClass> {
    if let Some(key) = physical
        && let Some((class, _)) = modes.wb.iter().find(|(_, known)| *known == key)
    {
        return Some(*class);
    }
    let mut controls: Vec<&Control> = epoch
        .controls
        .iter()
        .filter(|control| {
            control.kind == ControlType::Enum && control.affects.contains(&Affects::Wb)
        })
        .collect();
    controls.sort_by(|left, right| left.key.as_str().cmp(right.key.as_str()));
    for control in controls {
        let Some(ControlValue::Enum(value)) = settings.get(&control.key) else {
            continue;
        };
        if let Ok(key) = WbModeKey::new(value.as_str())
            && let Some((class, _)) = modes.wb.iter().find(|(_, known)| **known == key)
        {
            return Some(*class);
        }
    }
    None
}

fn fit_options(config: &WorkerConfig) -> FitOptions {
    FitOptions {
        seed: config.seed,
        max_samples_per_image: config.max_samples_per_image,
        sweep_budget: config.sweep_budget.unwrap_or(f64::MAX),
        ..FitOptions::default()
    }
}

fn body_warm_from_params(params: &MatchParams, modes: &BodyModes) -> Option<BodyWarmStart> {
    let delta = Delta6::new(params.transform.delta).ok()?;
    let exposure = params
        .exposure
        .iter()
        .filter_map(|offset| {
            modes.exposure.iter().find_map(|(id, key)| {
                (*key == offset.mode).then_some(looklab_fit::ExposureOffset {
                    mode: *id,
                    offset: offset.offset,
                })
            })
        })
        .collect();
    let wb = params
        .wb
        .iter()
        .filter_map(|offset| {
            modes.wb.iter().find_map(|(class, key)| {
                (*key == offset.mode).then_some((*class, [offset.r, offset.b]))
            })
        })
        .collect();
    Some(BodyWarmStart {
        delta,
        exposure,
        wb,
    })
}

fn rebound_sigma(
    lineage: &Lineage,
    contributors: &std::collections::HashMap<looklab_trust::EffectiveContributorId, u32>,
) -> Vec<(looklab_fit::ContributorId, f64)> {
    lineage
        .sigma
        .iter()
        .filter_map(|(contributor, sigma)| {
            contributors
                .get(contributor)
                .map(|ordinal| (looklab_fit::ContributorId(*ordinal), *sigma))
        })
        .collect()
}

fn hier_options(
    config: &WorkerConfig,
    lineage: &Lineage,
    bodies: &[BodyData],
    resume: Option<HierState>,
    sigma: Vec<(looklab_fit::ContributorId, f64)>,
    body_pin: BodyPin,
) -> HierOptions {
    let body_ids: Vec<UploadBodyId> = bodies.iter().map(|body| body.id.clone()).collect();
    let (gauges, gauge_scopes) = match &lineage.gauge {
        Some(saved)
            if body_ids
                .iter()
                .any(|id| id.as_str() == saved.gauge.designated.as_str()) =>
        {
            (
                vec![saved.gauge.clone()],
                vec![GaugeScope {
                    gauge: saved.gauge.clone(),
                    bodies: body_ids,
                }],
            )
        }
        _ => (Vec::new(), Vec::new()),
    };
    HierOptions {
        fit: fit_options(config),
        gauges,
        gauge_scopes,
        shared_sigma: sigma,
        firmware: lineage.firmware.clone(),
        resume,
        body_pin,
        ..HierOptions::default()
    }
}

fn has_scoped_admission(members: &[ScopeMember]) -> bool {
    members.iter().any(|member| {
        member.admission.is_some() && matches!(&member.scope, EvidenceScope::Qualified { .. })
    })
}

/// Scoped qualified admissions authorize their claim, not saved native initialization:
/// the common solve starts cold for native warm, shared noise and checkpoint resume.
fn fence_common_native(lineage: &mut Lineage) {
    lineage.body_warm.clear();
    lineage.published_body_warm.clear();
    lineage.realization_warm.clear();
    lineage.published_realization_warm.clear();
    lineage.crm.clear();
    lineage.published_crm.clear();
    lineage.sigma.clear();
    lineage.firmware.clear();
}

fn common_native_resume(cold: bool, resume: Option<HierState>) -> Option<HierState> {
    if cold { None } else { resume }
}

/// Only members in `participating` supply the shared gauge, firmware offsets and contributor variances.
async fn load_lineage(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    members: &[(ScopeMember, LookPool)],
    participating: &BTreeSet<(UploadBodyId, BodyRealizationId)>,
) -> Result<Lineage> {
    let mut lineage = Lineage::default();
    let mut conn = db_pool.acquire().await?;
    for (member, _) in members {
        let participates =
            participating.contains(&(member.body.clone(), member.realization.clone()));
        let subject = ModelSubject::BodyRealization {
            realization: member.realization.clone(),
        }
        .key();
        let qualified_revision =
            model_store::current_projections(&mut conn, &subject, ProjectionUse::QualifiedMatch)
                .await?
                .into_iter()
                .filter(|projection| projection.applicability.is_current())
                .find_map(|projection| projection.selected_candidate)
                .and_then(|candidate| match candidate {
                    ModelVersionId::BodyRealization { realization } => Some(
                        looklab_schema::RealizationRevisionId::for_version(&realization),
                    ),
                    ModelVersionId::SharedFamily { .. } => None,
                });
        let (revision, qualified) = if let Some(admission) = &member.admission {
            let ModelVersionId::BodyRealization { realization } = &admission.candidate else {
                lineage
                    .authoritative_missing
                    .insert(member.realization.clone());
                continue;
            };
            (
                looklab_schema::RealizationRevisionId::for_version(realization),
                true,
            )
        } else {
            match qualified_revision {
                Some(revision) => (revision, true),
                None => match selected_revision(&mut conn, &subject).await? {
                    Some(revision) => (revision, false),
                    None => continue,
                },
            }
        };
        let Some(stored) = model_store::realization_revision(&mut conn, &revision).await? else {
            if member.admission.is_some() {
                lineage
                    .authoritative_missing
                    .insert(member.realization.clone());
            }
            continue;
        };
        if qualified {
            lineage.qualified = true;
            if let Some(state) =
                model_store::body_state(&mut conn, &member.body, stored.body_state_version).await?
            {
                lineage.body_stage.insert(member.body.clone(), state.stage);
            }
            lineage.body_state.insert(
                member.body.clone(),
                (
                    stored.body_state_version,
                    stored
                        .artifacts
                        .iter()
                        .find(|artifact| artifact.role == ArtifactRole::Gauge)
                        .map_or(Sha256Hex::from_bytes([0; 32]), |artifact| artifact.sha256),
                    stored.input_transform_version,
                ),
            );
        }
        let artifacts =
            model_store::artifacts(&mut conn, "realization_revision", revision.as_str()).await?;
        let mut native_body_found = false;
        let mut native_realization_found = false;
        for artifact in artifacts {
            if artifact.role != ArtifactRole::FitResume {
                continue;
            }
            let hash = artifact.sha256;
            let bytes = match read_blob(config, &hash) {
                Ok(bytes) => bytes,
                Err(error) => {
                    tracing::warn!(%error, hash = %hash.to_hex(), "missing private fit artifact");
                    continue;
                }
            };
            let Ok(resume) = serde_json::from_slice::<StoredResume>(&bytes) else {
                continue;
            };
            let inventory = resume.inventory.clone();
            if inventory.is_none() {
                lineage
                    .authoritative_missing
                    .insert(member.realization.clone());
            }
            let inventory = inventory.unwrap_or_default();
            let native_body = resume
                .state
                .bodies
                .iter()
                .find(|body| body.id == member.body)
                .map(|body| SavedWarm {
                    inventory: inventory.clone(),
                    warm: body.state.clone(),
                });
            let published_body = resume
                .state
                .published_bodies
                .iter()
                .find(|body| body.id == member.body)
                .map(|body| SavedWarm {
                    inventory: inventory.clone(),
                    warm: body.state.clone(),
                })
                .or_else(|| {
                    resume.body.clone().map(|warm| SavedWarm {
                        inventory: inventory.clone(),
                        warm,
                    })
                });
            let native_realization = resume
                .state
                .realizations
                .iter()
                .find(|realization| {
                    realization.id == member.realization && realization.target == stored.target
                })
                .map(|realization| SavedRealization {
                    inventory: inventory.clone(),
                    warm: realization.state.clone(),
                });
            let published_realization = resume
                .state
                .published_realizations
                .iter()
                .find(|realization| {
                    realization.id == member.realization && realization.target == stored.target
                })
                .map(|realization| SavedRealization {
                    inventory: inventory.clone(),
                    warm: realization.state.clone(),
                })
                .or_else(|| {
                    Some(SavedRealization {
                        inventory: inventory.clone(),
                        warm: resume.realization.clone(),
                    })
                });
            if let Some(body) = native_body {
                native_body_found = true;
                lineage.body_warm.entry(member.body.clone()).or_insert(body);
            }
            if let Some(body) = published_body {
                lineage
                    .published_body_warm
                    .entry(member.body.clone())
                    .or_insert(body);
            }
            if let Some(realization) = native_realization {
                native_realization_found = true;
                if let Some(crm) = realization.warm.response.clone() {
                    lineage.crm.entry(member.realization.clone()).or_insert(crm);
                }
                lineage
                    .realization_warm
                    .entry(member.realization.clone())
                    .or_insert(realization);
            }
            if let Some(realization) = published_realization {
                if let Some(crm) = resume
                    .crm
                    .clone()
                    .or_else(|| realization.warm.response.clone())
                {
                    lineage
                        .published_crm
                        .entry(member.realization.clone())
                        .or_insert(crm);
                }
                lineage
                    .published_realization_warm
                    .entry(member.realization.clone())
                    .or_insert(realization);
            }
            if participates {
                if lineage.sigma.is_empty() {
                    lineage.sigma = resume.sigma.clone();
                }
                if lineage.firmware.is_empty() {
                    lineage.firmware = resume.state.firmware.clone();
                }
            }
        }
        if member.admission.is_some() && (!native_body_found || !native_realization_found) {
            lineage
                .authoritative_missing
                .insert(member.realization.clone());
        }
        if lineage.gauge.is_none()
            && participates
            && let Some(state) =
                model_store::body_state(&mut conn, &member.body, stored.body_state_version).await?
        {
            let hash = state.pins.gauge.sha256;
            if let Ok(bytes) = read_blob(config, &hash)
                && let Ok(artifact) = serde_json::from_slice::<GaugeArtifact>(&bytes)
            {
                let inventory = artifact.inventory.clone();
                if inventory.is_none() {
                    lineage
                        .authoritative_missing
                        .insert(member.realization.clone());
                }
                lineage.gauge = Some(SavedGauge {
                    inventory: inventory.unwrap_or_default(),
                    gauge: ComponentGauge {
                        designated: looklab_body::BodyId::new(artifact.designated.as_str())
                            .map_err(|error| WorkerError::Data(format!("{error:?}")))?,
                        exposure_mode: looklab_fit::ExposureModeId(artifact.exposure_mode),
                        wb_mode: WbModeClass(artifact.wb_mode),
                        anchored_delta: artifact.anchored_delta,
                    },
                });
            }
        }
    }
    Ok(lineage)
}

async fn selected_revision(
    conn: &mut sqlx::SqliteConnection,
    subject: &SubjectKey,
) -> Result<Option<looklab_schema::RealizationRevisionId>> {
    for use_ in [
        ProjectionUse::QualifiedMatch,
        ProjectionUse::ExperimentalMatch,
    ] {
        for projection in model_store::current_projections(conn, subject, use_).await? {
            if !projection.applicability.is_current() {
                continue;
            }
            if let Some(ModelVersionId::BodyRealization { realization }) =
                projection.selected_candidate
            {
                return Ok(Some(looklab_schema::RealizationRevisionId::for_version(
                    &realization,
                )));
            }
        }
    }
    let mut revisions = model_store::realization_revisions(conn, Some(subject)).await?;
    revisions.sort_by_key(|revision| revision.version);
    for revision in revisions.iter().rev() {
        if revision.status == RealizationStatus::Candidate && revision.safety.accepted() {
            return Ok(Some(revision.id.clone()));
        }
    }
    Ok(None)
}

async fn load_consistency_scores(
    db_pool: &SqlitePool,
    members: &[(ScopeMember, LookPool)],
) -> Result<ConsistencyScores> {
    let mut conn = db_pool.acquire().await?;
    load_qualified_scores(&mut conn, members, None).await
}

async fn load_native_qualified_outcomes(
    conn: &mut sqlx::SqliteConnection,
    members: &[(ScopeMember, LookPool)],
) -> Result<ConsistencyScores> {
    load_qualified_scores(
        conn,
        members,
        Some(QualificationCapability::NativePrediction),
    )
    .await
}

async fn load_qualified_scores(
    conn: &mut sqlx::SqliteConnection,
    members: &[(ScopeMember, LookPool)],
    required_capability: Option<QualificationCapability>,
) -> Result<ConsistencyScores> {
    let mut scores = ConsistencyScores::new();
    for (member, _) in members {
        let subject = ModelSubject::BodyRealization {
            realization: member.realization.clone(),
        }
        .key();
        let projections =
            model_store::current_projections(&mut *conn, &subject, ProjectionUse::QualifiedMatch)
                .await?;
        for projection in projections {
            if !projection.applicability.is_current() {
                continue;
            }
            if let Some(required) = required_capability
                && projection.capability != required
            {
                continue;
            }
            if !matches!(
                projection.capability,
                QualificationCapability::NativePrediction
                    | QualificationCapability::ControlsMatching
            ) {
                continue;
            }
            let (Some(report), Some(candidate)) =
                (projection.selected_report, projection.selected_candidate)
            else {
                continue;
            };
            let Some(stored) = model_store::report(&mut *conn, &report).await? else {
                continue;
            };
            if !matches!(stored.report.outcome, AssessmentOutcome::Pass)
                || !model_store::report_applicable(&mut *conn, &report).await?
            {
                continue;
            }
            let stats = model_store::qualified_image_stats(
                &mut *conn,
                &subject,
                &candidate,
                &projection.scope_key,
            )
            .await?;
            for stat in stats {
                if let Some(image) = db::ImageId::parse(stat.image.as_str()) {
                    scores.insert(image, outcome_of(stat.verdict));
                }
            }
        }
    }
    Ok(scores)
}

fn outcome_of(verdict: model_store::DiagnosticVerdict) -> ImageOutcome {
    match verdict {
        model_store::DiagnosticVerdict::Accepted => ImageOutcome::Accepted,
        model_store::DiagnosticVerdict::DownWeighted => ImageOutcome::DownWeighted,
        model_store::DiagnosticVerdict::Quarantined => ImageOutcome::Quarantined,
    }
}

#[allow(clippy::too_many_arguments)]
async fn select_experimental(
    pool: &SqlitePool,
    ownership: &JobOwnership,
    subject: &SubjectKey,
    candidate: &ModelVersionId,
    target: &FamilyTargetId,
    observed_pending: PendingGeneration,
    expected_anchor: Option<u32>,
    now: i64,
) -> Result<()> {
    let server_job = ownership.server_id();
    let sources = source_identity::sources().storage();
    let ModelVersionId::BodyRealization { realization } = candidate else {
        return Ok(());
    };
    let revision = looklab_schema::RealizationRevisionId::for_version(realization);
    let mut conn = pool.acquire().await?;
    let capability = crate::model_assessment::candidate_capability(&mut conn, candidate)
        .await?
        .unwrap_or(QualificationCapability::NativePrediction);
    let Some(stored) = model_store::realization_revision(&mut conn, &revision).await? else {
        return Ok(());
    };
    if stored.status != RealizationStatus::Candidate {
        return Ok(());
    }
    if !stored
        .artifacts
        .iter()
        .any(|artifact| artifact.role == ArtifactRole::RealizationParams)
    {
        return Ok(());
    }
    let dependencies =
        model_store::dependencies(&mut conn, "realization_revision", revision.as_str()).await?;
    let snapshot = model_store::dependency_snapshot(&mut conn, &dependencies).await?;
    let current = model_store::current_projection(
        &mut conn,
        subject,
        ProjectionUse::ExperimentalMatch,
        capability,
        &ScopeKey::all(),
    )
    .await?;
    let current_revision = current.as_ref().and_then(|projection| {
        projection
            .selected_candidate
            .as_ref()
            .and_then(|candidate| match candidate {
                ModelVersionId::BodyRealization { realization } => Some(
                    looklab_schema::RealizationRevisionId::for_version(realization),
                ),
                ModelVersionId::SharedFamily { .. } => None,
            })
    });
    let expected = match &current_revision {
        Some(revision) => ExpectedSelection::Selected(SelectionRef {
            revision,
            report: None,
        }),
        None => ExpectedSelection::Absent,
    };
    let outcome = model_store::select_realization(
        &mut conn,
        &RealizationSelection {
            subject,
            sources: &sources,
            use_: ProjectionUse::ExperimentalMatch,
            capability,
            scope_key: &ScopeKey::all(),
            revision: &revision,
            target,
            report: None,
            supersedes: &[],
            expected,
            expected_anchor,
            lease: lease(ownership, &server_job),
            snapshot: &snapshot,
            observed_pending,
        },
        now,
    )
    .await?;
    match outcome {
        SelectionOutcome::Selected => {}
        SelectionOutcome::Stale { actual } => {
            tracing::warn!(?actual, "experimental selection is stale");
        }
        SelectionOutcome::Rejected(rejection) => {
            tracing::warn!(?rejection, "experimental selection was fenced");
        }
    }
    Ok(())
}

async fn schedule_candidate(
    conn: &mut sqlx::SqliteConnection,
    subject: &ModelSubject,
    candidate: &ModelVersionId,
    now: i64,
) -> Result<()> {
    if let Err(error) =
        crate::model_assessment::enqueue_candidate(conn, subject, candidate, now).await
    {
        tracing::warn!(%error, "assessment scheduling failed");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn publish_target(
    conn: &mut sqlx::SqliteConnection,
    config: &WorkerConfig,
    plan: &FamilyPlan,
    target: &FamilyTargetId,
    document: &LookDocument,
    document_bytes: &[u8],
    release: &ReleaseRecord,
    roots: &RootData,
    dependencies: &[DependencyRef],
    weighting_snapshot: &[DependencyGeneration],
    observed_anchor: Option<u32>,
    lease: &JobLease<'_>,
    observed_pending: PendingGeneration,
    safety: CandidateSafety,
    now: i64,
) -> Result<Vec<ArtifactBinding>> {
    let sources = source_identity::sources().storage();
    let look = store_artifact(config, LOOK_ARTIFACT, document_bytes)?;
    let preview = store_artifact(config, PREVIEW_LUT_ARTIFACT, &bake_preview_lut(document))?;
    let release_artifact = store_artifact(config, RELEASE_ARTIFACT, &serde_json::to_vec(release)?)?;
    let mut artifacts = vec![look.clone(), preview.clone(), release_artifact];
    let mut bindings = vec![
        ArtifactBinding {
            sha256: Sha256Hex::parse(&look.sha256)
                .map_err(|error| WorkerError::Data(error.to_string()))?,
            role: ArtifactRole::Look,
        },
        ArtifactBinding {
            sha256: Sha256Hex::parse(&preview.sha256)
                .map_err(|error| WorkerError::Data(error.to_string()))?,
            role: ArtifactRole::Preview,
        },
    ];
    if let Some(status) = card_status(document) {
        let card = store_artifact(
            config,
            CARD_ARTIFACT,
            &write_card(
                document,
                &card_meta(&plan.family).with_status(status),
                &CardOptions::default(),
            )?,
        )?;
        bindings.push(ArtifactBinding {
            sha256: Sha256Hex::parse(&card.sha256)
                .map_err(|error| WorkerError::Data(error.to_string()))?,
            role: ArtifactRole::Card,
        });
        artifacts.push(card);
    }
    let status = if roots.weights.rooted {
        admin::VersionStatus::Rooted
    } else {
        admin::VersionStatus::Published
    };
    admin::write_version_manifest_conn(
        conn,
        &plan.family,
        VersionId::new(target.version.get()),
        status,
        &artifacts,
        &roots.admissions,
    )
    .await?;
    sqlx::query(
        "UPDATE look_versions SET status = ?
         WHERE look = ? AND version = ? AND status = 'provisional'",
    )
    .bind(status.as_str())
    .bind(plan.family.as_str())
    .bind(i64::from(target.version.get()))
    .execute(&mut *conn)
    .await?;

    let snapshot = weighting_snapshot.to_vec();
    let eligibility = TargetEligibility {
        id: looklab_schema::EligibilitySnapshotId::from_digest([0; 32]),
        target: PublishedTargetId::from(target),
        basis: TargetBasis::NativeFamily,
        artifacts: bindings,
        base_pin: None,
        support_uncertainty_source: Some(source_identity::sources().evaluator.clone()),
        status: TargetEligibilityStatus::Eligible,
        reasons: Vec::new(),
        dependencies: dependencies.to_vec(),
    };
    model_store::record_target_eligibility(conn, &eligibility, now).await?;

    let expected_anchor = observed_anchor;
    let seed_bodies: Vec<UploadBodyId> = plan
        .members
        .iter()
        .map(|member| member.body.clone())
        .collect();
    let source_limited = plan.kind != PlanKind::Qualified;
    let outcome = match &plan.head {
        None => {
            model_store::seed_family_target(
                conn,
                &model_store::FamilySeed {
                    family: &plan.family_id,
                    sources: &sources,
                    target,
                    safety,
                    source_limited,
                    seed_bodies: &seed_bodies,
                    dependencies,
                    lease: *lease,
                    snapshot: &snapshot,
                    observed_pending,
                    expected_anchor,
                },
                now,
            )
            .await?
        }
        Some(head) => {
            if plan.kind != PlanKind::Qualified {
                record_seed_cohort(conn, &plan.family_id, &seed_bodies, now).await?;
            }
            model_store::advance_family_head(
                conn,
                &model_store::FamilyAdvance {
                    family: &plan.family_id,
                    sources: &sources,
                    expected: head.target.version,
                    target,
                    safety,
                    source_limited,
                    report: None,
                    dependencies,
                    lease: *lease,
                    snapshot: &snapshot,
                    observed_pending,
                    expected_anchor,
                },
                now,
            )
            .await?
        }
    };
    match outcome {
        model_store::FamilySelectionOutcome::Selected => {
            if plan.head.is_none() {
                model_store::set_subject_anchor(
                    conn,
                    &SubjectKey::for_family(&plan.family_id),
                    None,
                    target.version.get(),
                    now,
                )
                .await?;
            }
        }
        model_store::FamilySelectionOutcome::Stale { actual } => {
            tracing::warn!(
                family = %plan.family,
                ?actual,
                "family head changed during the fit; the candidate stays unselected"
            );
        }
        model_store::FamilySelectionOutcome::Rejected(rejection) => {
            tracing::warn!(
                family = %plan.family,
                ?rejection,
                "family head selection was fenced"
            );
        }
    }
    Ok(eligibility.artifacts)
}

async fn record_seed_cohort(
    conn: &mut sqlx::SqliteConnection,
    family: &LookId,
    bodies: &[UploadBodyId],
    now: i64,
) -> Result<()> {
    let members: Vec<CohortMember> = bodies
        .iter()
        .map(|body| CohortMember::Seed { body: body.clone() })
        .collect();
    let mut hasher = Sha256::new();
    for body in bodies {
        hasher.update(body.as_str().as_bytes());
        hasher.update([0]);
    }
    let digest = Sha256Hex::from_bytes(hasher.finalize().into());
    model_store::record_cohort_revision(
        conn,
        family,
        CohortRole::Seed,
        CohortBasis::SeedEvidence,
        digest,
        &members,
        now,
    )
    .await?;
    Ok(())
}

trait FitOutput {
    fn bodies(&self) -> &[BodyParameters];
    fn state(&self) -> &HierState;
    fn family_firmware(&self, target: &FamilyTargetId) -> Vec<(FirmwareId, [f64; 2])>;
    fn native_body_warm(&self, body: &UploadBodyId) -> Option<BodyWarmStart>;
    fn published_body_warm(&self, body: &UploadBodyId) -> Option<BodyWarmStart>;
    fn native_realization_warm(
        &self,
        id: &BodyRealizationId,
        target: &FamilyTargetId,
    ) -> Option<RealizationWarmStart>;
    fn published_realization_warm(
        &self,
        id: &BodyRealizationId,
        target: &FamilyTargetId,
    ) -> Option<RealizationWarmStart>;
    fn realization(
        &self,
        id: &BodyRealizationId,
        target: &FamilyTargetId,
    ) -> Option<RealizationParametersView>;
}

struct RealizationParametersView {
    clip_k: f64,
    model: Option<ControlResponseModel>,
    images: Vec<ImageNuisances>,
}

impl FitOutput for looklab_fit::hierarchical::HierarchicalFit {
    fn bodies(&self) -> &[BodyParameters] {
        &self.bodies
    }

    fn state(&self) -> &HierState {
        &self.state
    }

    fn family_firmware(&self, target: &FamilyTargetId) -> Vec<(FirmwareId, [f64; 2])> {
        self.family(target)
            .map(|family| family.firmware.clone())
            .unwrap_or_default()
    }

    fn native_body_warm(&self, body: &UploadBodyId) -> Option<BodyWarmStart> {
        self.body_warm(body)
    }

    fn published_body_warm(&self, body: &UploadBodyId) -> Option<BodyWarmStart> {
        self.published_body_warm(body)
    }

    fn native_realization_warm(
        &self,
        id: &BodyRealizationId,
        target: &FamilyTargetId,
    ) -> Option<RealizationWarmStart> {
        self.realization_warm(id, target)
    }

    fn published_realization_warm(
        &self,
        id: &BodyRealizationId,
        target: &FamilyTargetId,
    ) -> Option<RealizationWarmStart> {
        self.published_realization_warm(id, target)
    }

    fn realization(
        &self,
        id: &BodyRealizationId,
        target: &FamilyTargetId,
    ) -> Option<RealizationParametersView> {
        self.realization(id, target)
            .map(|realization| RealizationParametersView {
                clip_k: realization.clip_k,
                model: realization.model.clone(),
                images: realization.images.clone(),
            })
    }
}

impl FitOutput for looklab_fit::hierarchical::RealizationFit {
    fn bodies(&self) -> &[BodyParameters] {
        &self.bodies
    }

    fn state(&self) -> &HierState {
        &self.state
    }

    fn family_firmware(&self, target: &FamilyTargetId) -> Vec<(FirmwareId, [f64; 2])> {
        self.state
            .firmware
            .iter()
            .filter(|(key, _)| key.family.as_str() == target.family.as_str())
            .map(|(key, offset)| (key.firmware.clone(), *offset))
            .collect()
    }

    fn native_body_warm(&self, body: &UploadBodyId) -> Option<BodyWarmStart> {
        self.body_warm(body)
    }

    fn published_body_warm(&self, body: &UploadBodyId) -> Option<BodyWarmStart> {
        self.published_body_warm(body)
    }

    fn native_realization_warm(
        &self,
        id: &BodyRealizationId,
        target: &FamilyTargetId,
    ) -> Option<RealizationWarmStart> {
        self.realization_warm(id, target)
    }

    fn published_realization_warm(
        &self,
        id: &BodyRealizationId,
        target: &FamilyTargetId,
    ) -> Option<RealizationWarmStart> {
        self.published_realization_warm(id, target)
    }

    fn realization(
        &self,
        id: &BodyRealizationId,
        target: &FamilyTargetId,
    ) -> Option<RealizationParametersView> {
        self.realization(id, target)
            .map(|realization| RealizationParametersView {
                clip_k: realization.clip_k,
                model: realization.model.clone(),
                images: realization.images.clone(),
            })
    }
}

#[allow(clippy::too_many_arguments)]
async fn persist_body_states(
    conn: &mut sqlx::SqliteConnection,
    config: &WorkerConfig,
    _plan: &FamilyPlan,
    fit: &impl FitOutput,
    modes: &BTreeMap<UploadBodyId, BodyModes>,
    gauge: Option<&ComponentGauge>,
    gauge_value: f64,
    snapshot_version: u64,
    native_dependencies: &[DependencyRef],
    native_snapshot: &[DependencyGeneration],
    published_dependencies: &[DependencyRef],
    published_snapshot: &[DependencyGeneration],
    options: &FitOptions,
    now: i64,
) -> Result<BTreeMap<UploadBodyId, PersistedBody>> {
    let mut versions = BTreeMap::new();
    if fit.bodies().is_empty() {
        return Ok(versions);
    }
    let gauge = gauge
        .ok_or_else(|| WorkerError::Data("body calibration has no component gauge".to_owned()))?;
    let gauge_modes = modes
        .iter()
        .find(|(body, _)| body.as_str() == gauge.designated.as_str())
        .map(|(_, inventory)| inventory)
        .ok_or_else(|| {
            WorkerError::Data("component gauge has no designated-body inventory".to_owned())
        })?;
    let gauge_artifact = GaugeArtifact {
        designated: UploadBodyId::new(gauge.designated.as_str())
            .map_err(|error| WorkerError::Data(error.to_string()))?,
        exposure_mode: gauge.exposure_mode.0,
        wb_mode: gauge.wb_mode.0,
        inventory: Some(gauge_modes.clone()),
        anchored_delta: gauge.anchored_delta,
        gauge: gauge_value,
        convention: GaugeConvention::Relative,
    };
    let gauge_binding = store_binding(
        config,
        &serde_json::to_vec(&gauge_artifact)?,
        ArtifactRole::Gauge,
    )?;
    let transforms = TransformDir::new(&config.transforms);
    for body in fit.bodies() {
        let Some(modes) = modes.get(&body.id) else {
            continue;
        };
        let native = fit
            .native_body_warm(&body.id)
            .unwrap_or_else(|| BodyWarmStart {
                delta: body.transform.delta(),
                exposure: body.exposure.clone(),
                wb: body.wb.clone(),
            });
        let native_params = match_params(
            &body.id,
            MatchStage::Native,
            &body.transform,
            &native.exposure,
            &native.wb,
            modes,
        )?;
        let published_params = match_params(
            &body.id,
            MatchStage::Published,
            &body.transform,
            &body.exposure,
            &body.wb,
            modes,
        )?;
        let transform_binding = store_binding(
            config,
            &serde_json::to_vec(&body.transform)?,
            ArtifactRole::InputTransform,
        )?;
        let profile_bytes = transforms.snapshot(&body.id)?;
        let profile_hash = Sha256Hex::from_bytes(Sha256::digest(&profile_bytes).into());
        let profile_binding =
            store_binding(config, &profile_bytes, ArtifactRole::CalibrationProfile)?;
        let native_state_dependencies = body_state_dependencies(
            &body.id,
            snapshot_version,
            body.transform.version().get(),
            profile_hash,
            gauge_binding.sha256,
            options,
            native_dependencies,
        );
        let published_state_dependencies = body_state_dependencies(
            &body.id,
            snapshot_version,
            body.transform.version().get(),
            profile_hash,
            gauge_binding.sha256,
            options,
            published_dependencies,
        );
        let native_params = store_binding(
            config,
            &serde_json::to_vec(&native_params)?,
            ArtifactRole::BodyRealizationParams,
        )?;
        let published_params = store_binding(
            config,
            &serde_json::to_vec(&published_params)?,
            ArtifactRole::BodyRealizationParams,
        )?;
        let native_version = reserve_and_insert_body_state(
            conn,
            &body.id,
            BodyStateStage::Native,
            transform_binding,
            native_params,
            gauge_binding,
            profile_binding,
            &native_state_dependencies,
            native_snapshot,
            now,
        )
        .await?;
        let published_version = reserve_and_insert_body_state(
            conn,
            &body.id,
            BodyStateStage::Published,
            transform_binding,
            published_params,
            gauge_binding,
            profile_binding,
            &published_state_dependencies,
            published_snapshot,
            now,
        )
        .await?;
        versions.insert(
            body.id.clone(),
            PersistedBody {
                native: Some(native_version),
                published: published_version,
                gauge: gauge_binding.sha256,
                transform_version: body.transform.version().get(),
            },
        );
    }
    Ok(versions)
}

#[allow(clippy::too_many_arguments)]
async fn reserve_and_insert_body_state(
    conn: &mut sqlx::SqliteConnection,
    body: &UploadBodyId,
    stage: BodyStateStage,
    transform: ArtifactBinding,
    params: ArtifactBinding,
    gauge: ArtifactBinding,
    profile: ArtifactBinding,
    dependencies: &[DependencyRef],
    snapshot: &[DependencyGeneration],
    now: i64,
) -> Result<BodyStateVersion> {
    let version = model_store::reserve_body_state(conn, body, stage, now).await?;
    let record = BodyStateRecord {
        body: body.clone(),
        version,
        stage,
        pins: BodyStatePins {
            profile: Some(profile),
            input_transform: transform,
            body_params: params,
            gauge,
        },
        dependencies: dependencies.to_vec(),
    };
    Ok(model_store::insert_body_state(conn, &record, snapshot, now).await?)
}

fn body_state_dependencies(
    body: &UploadBodyId,
    snapshot_version: u64,
    transform_version: u32,
    profile: Sha256Hex,
    _gauge: Sha256Hex,
    options: &FitOptions,
    target_dependencies: &[DependencyRef],
) -> Vec<DependencyRef> {
    let mut dependencies = target_dependencies.to_vec();
    dependencies.push(DependencyRef::required(
        DependencyKind::Schema,
        DependencyTarget::published_schema(&PublishedSchemaSnapshot::new(
            body.clone(),
            snapshot_version,
        )),
    ));
    dependencies.push(DependencyRef::required(
        DependencyKind::Transform,
        DependencyTarget::transform(body, transform_version),
    ));
    if let Ok(id) = ProfileId::new(profile.to_hex()) {
        dependencies.push(DependencyRef::required(
            DependencyKind::Profile,
            DependencyTarget::profile(&id, transform_version),
        ));
    }
    if let Ok(digest) = effective_options_digest(options) {
        dependencies.push(DependencyRef::required(
            DependencyKind::EffectiveOptions,
            DependencyTarget::effective_options(digest),
        ));
    }
    if let Ok(probe_set) = ProbeSetId::new(PROBE_SET) {
        dependencies.push(DependencyRef::required(
            DependencyKind::ProbeSet,
            DependencyTarget::probe_set(&probe_set),
        ));
    }
    dependencies.push(DependencyRef::required(
        DependencyKind::RootWeighting,
        DependencyTarget::root_weighting_global(),
    ));
    dependencies.push(DependencyRef::required(
        DependencyKind::RootWeighting,
        DependencyTarget::root_weighting_body(body),
    ));
    dependencies.sort();
    dependencies.dedup();
    dependencies
}

#[allow(clippy::too_many_arguments)]
async fn persist_realization(
    conn: &mut sqlx::SqliteConnection,
    config: &WorkerConfig,
    target: &FamilyTargetId,
    member: &ScopeMember,
    pool: &LookPool,
    fit: &impl FitOutput,
    body_versions: &BTreeMap<UploadBodyId, PersistedBody>,
    target_artifacts: &[ArtifactBinding],
    membership: &EvidenceMembership,
    dependencies: &[DependencyRef],
    native_stage: &[DependencyRef],
    published_stage: &[DependencyRef],
    modes: &BodyModes,
    safety: CandidateSafety,
    contributors: &std::collections::HashMap<looklab_trust::EffectiveContributorId, u32>,
    now: i64,
) -> Result<(Option<RealizationVersion>, Option<RealizationVersion>)> {
    let realization = fit
        .realization(&member.realization, target)
        .ok_or_else(|| {
            WorkerError::Data("the fit produced no realization parameters".to_owned())
        })?;
    let persisted = body_versions.get(&member.body).ok_or_else(|| {
        WorkerError::Data("the fit produced no body state for the realization".to_owned())
    })?;
    let native_warm = fit
        .native_realization_warm(&member.realization, target)
        .unwrap_or(RealizationWarmStart {
            clip_k: realization.clip_k,
            nuisances: realization.images.clone(),
            damping: 1e-3,
            response: realization.model.clone(),
            coefficients: Vec::new(),
        });
    let published_warm = fit
        .published_realization_warm(&member.realization, target)
        .unwrap_or(native_warm.clone());
    let native_crm = native_warm.response.clone();
    let published_crm = realization.model.clone().or_else(|| native_crm.clone());
    let family_firmware = fit.family_firmware(target);
    let mut native_edges = dependencies.to_vec();
    native_edges.extend(native_stage.iter().cloned());
    native_edges.sort();
    native_edges.dedup();
    let mut published_edges = dependencies.to_vec();
    published_edges.extend(published_stage.iter().cloned());
    published_edges.sort();
    published_edges.dedup();
    let native_version = persist_one_revision(
        conn,
        config,
        member,
        target,
        RealizationStage::Native,
        if safety.accepted() {
            RealizationStatus::Candidate
        } else {
            RealizationStatus::Diagnostic
        },
        safety,
        persisted.native.unwrap_or(persisted.published),
        persisted.transform_version,
        persisted.gauge,
        target_artifacts,
        &native_warm,
        native_crm.as_ref(),
        fit.native_body_warm(&member.body),
        fit.state(),
        membership,
        modes,
        &native_edges,
        native_warm.clip_k,
        &family_firmware,
        contributors,
        now,
    )
    .await?;
    let recipe_version = persist_one_revision(
        conn,
        config,
        member,
        target,
        RealizationStage::Recipe,
        if safety.accepted() {
            RealizationStatus::Candidate
        } else {
            RealizationStatus::Diagnostic
        },
        safety,
        persisted.published,
        persisted.transform_version,
        persisted.gauge,
        target_artifacts,
        &published_warm,
        published_crm.as_ref(),
        fit.published_body_warm(&member.body),
        fit.state(),
        membership,
        modes,
        &published_edges,
        realization.clip_k,
        &family_firmware,
        contributors,
        now,
    )
    .await?;
    let _ = pool;
    Ok((Some(native_version), Some(recipe_version)))
}

#[allow(clippy::too_many_arguments)]
async fn persist_one_revision(
    conn: &mut sqlx::SqliteConnection,
    config: &WorkerConfig,
    member: &ScopeMember,
    target: &FamilyTargetId,
    stage: RealizationStage,
    status: RealizationStatus,
    safety: CandidateSafety,
    body_state_version: BodyStateVersion,
    input_transform_version: u32,
    gauge_hash: Sha256Hex,
    target_artifacts: &[ArtifactBinding],
    warm: &RealizationWarmStart,
    crm: Option<&ControlResponseModel>,
    body_warm: Option<BodyWarmStart>,
    state: &HierState,
    membership: &EvidenceMembership,
    modes: &BodyModes,
    dependencies: &[DependencyRef],
    clip_k: f64,
    family_firmware: &[(FirmwareId, [f64; 2])],
    contributors: &std::collections::HashMap<looklab_trust::EffectiveContributorId, u32>,
    now: i64,
) -> Result<RealizationVersion> {
    let (_, version) =
        model_store::reserve_realization_version(conn, &member.realization, target, now).await?;
    let mut artifacts = target_artifacts.to_vec();
    if let Some(crm) = crm {
        artifacts.push(store_binding(
            config,
            &encode_crm(crm)?,
            ArtifactRole::ControlResponse,
        )?);
    }
    if stage == RealizationStage::Native || crm.is_some_and(|crm| crm.version != 0) {
        let params = realization_params(target, member, version, crm, clip_k, family_firmware)?;
        if stage == RealizationStage::Native {
            params
                .validate_native()
                .map_err(|error| WorkerError::Data(error.to_owned()))?;
        } else {
            params
                .validate()
                .map_err(|error| WorkerError::Data(error.to_owned()))?;
        }
        artifacts.push(store_binding(
            config,
            &serde_json::to_vec(&params)?,
            ArtifactRole::RealizationParams,
        )?);
    }
    let stable: BTreeMap<u32, looklab_trust::EffectiveContributorId> = contributors
        .iter()
        .map(|(contributor, ordinal)| (*ordinal, *contributor))
        .collect();
    let sigma = state
        .sigma
        .iter()
        .filter_map(|(ordinal, value)| {
            stable
                .get(&ordinal.0)
                .map(|contributor| (*contributor, *value))
        })
        .collect();
    let resume = StoredResume {
        body: body_warm,
        realization: warm.clone(),
        crm: crm.cloned(),
        state: state.clone(),
        membership: membership.clone(),
        sigma,
        inventory: Some(modes.clone()),
    };
    if stage == RealizationStage::Native {
        artifacts.push(store_binding(
            config,
            &serde_json::to_vec(&warm)?,
            ArtifactRole::NativeWarmState,
        )?);
    }
    artifacts.push(store_binding(
        config,
        &serde_json::to_vec(&resume)?,
        ArtifactRole::FitResume,
    )?);
    artifacts.push(store_binding(
        config,
        &serde_json::to_vec(membership)?,
        ArtifactRole::EvidenceMembership,
    )?);
    artifacts.push(store_binding(
        config,
        &serde_json::to_vec(&warm.nuisances)?,
        ArtifactRole::ImageNuisance,
    )?);
    let mut edges = dependencies.to_vec();
    if let Ok(gauge) = GaugeId::new(gauge_hash.to_hex()) {
        edges.push(DependencyRef::required(
            DependencyKind::Gauge,
            DependencyTarget::gauge(&gauge),
        ));
    }
    edges.sort();
    edges.dedup();
    let revision = NewRealizationRevision {
        subject: &member.realization,
        version,
        stage,
        target,
        body_state_version,
        input_transform_version,
        realization_params_version: body_state_version.get(),
        control_response_version: crm.map_or(0, |crm| crm.version),
        clip_k: if clip_k.is_finite() && clip_k > 0.0 {
            clip_k
        } else {
            0.03
        },
        status,
        status_reason: None,
        safety,
        artifacts: &artifacts,
        dependencies: &edges,
    };
    Ok(model_store::insert_realization_revision(conn, &revision, now).await?)
}

fn match_params(
    body: &UploadBodyId,
    stage: MatchStage,
    transform: &InputTransform,
    exposure: &[looklab_fit::ExposureOffset],
    wb: &[(WbModeClass, [f64; 2])],
    modes: &BodyModes,
) -> Result<MatchParams> {
    let mut exposure_offsets = Vec::new();
    for offset in exposure {
        if let Some(mode) = modes.exposure.get(&offset.mode) {
            exposure_offsets.push(ExposureModeOffset {
                mode: mode.clone(),
                offset: offset.offset,
            });
        }
    }
    let mut wb_offsets = Vec::new();
    for (class, gains) in wb {
        if let Some(key) = modes.wb.get(class) {
            wb_offsets.push(WbModeOffset {
                mode: key.clone(),
                r: gains[0],
                b: gains[1],
            });
        }
    }
    let params = MatchParams {
        format: MatchParamsFormat::V1,
        stage,
        body: body.clone(),
        transform: MatchTransform {
            version: transform.version().get(),
            camera_to_xyz: transform.at_cct(Cct::D65).0,
            delta: transform.delta().params(),
        },
        exposure: exposure_offsets,
        wb: wb_offsets,
    };
    params
        .validate(body)
        .map_err(|error| WorkerError::Data(error.to_owned()))?;
    Ok(params)
}

fn realization_params(
    target: &FamilyTargetId,
    member: &ScopeMember,
    version: RealizationVersion,
    crm: Option<&ControlResponseModel>,
    clip_k: f64,
    firmware: &[(FirmwareId, [f64; 2])],
) -> Result<MatchRealizationParams> {
    let firmware = firmware
        .iter()
        .filter(|(id, _)| id.body == member.body && id.epoch == member.realization.epoch)
        .map(|(id, offset)| FirmwareOffset {
            firmware: looklab_schema::FirmwareId {
                body: id.body.clone(),
                epoch: id.epoch,
                version: id.version.clone(),
            },
            t_shift: offset[0],
            log_g_shift: offset[1],
        })
        .collect();
    let params = MatchRealizationParams {
        format: MatchParamsFormat::V1,
        realization: looklab_schema::BodyRealizationVersionId {
            realization: member.realization.clone(),
            version,
            target: target.clone(),
        },
        family: target.clone(),
        version: version.get(),
        control_response_version: crm.map_or(0, |crm| crm.version),
        clip_k: if clip_k.is_finite() && clip_k > 0.0 {
            clip_k
        } else {
            0.03
        },
        firmware,
    };
    Ok(params)
}

fn gauge_value(bodies: &[BodyParameters], gauge: &ComponentGauge) -> f64 {
    bodies
        .iter()
        .find(|body| body.id.as_str() == gauge.designated.as_str())
        .and_then(|body| {
            body.exposure
                .iter()
                .find(|offset| offset.mode == gauge.exposure_mode)
        })
        .map_or(0.0, |offset| offset.offset)
}

fn target_artifact_bindings(document: &[u8]) -> Vec<ArtifactBinding> {
    let Ok(hash) = Sha256Hex::parse(&db::hex(&Sha256::digest(document))) else {
        return Vec::new();
    };
    vec![ArtifactBinding {
        sha256: hash,
        role: ArtifactRole::Look,
    }]
}

#[allow(clippy::too_many_arguments)]
fn prefit_body_dependencies(
    _config: &WorkerConfig,
    _plans: &[&FamilyPlan],
    members: &[(ScopeMember, LookPool)],
    _snapshot_version: u64,
    options: &FitOptions,
    stage: BodyStateStage,
) -> Result<Vec<DependencyRef>> {
    let pools: Vec<LookPool> = members.iter().map(|(_, pool)| pool.clone()).collect();
    let evidence_stage = match stage {
        BodyStateStage::Native => crate::pool::EvidenceStage::Native,
        BodyStateStage::Published => crate::pool::EvidenceStage::Published,
    };
    let mut dependencies = crate::pool::stage_dependencies(&pools, evidence_stage);
    dependencies.extend(source_identity::sources().dependencies());
    if let Ok(probe_set) = ProbeSetId::new(PROBE_SET) {
        dependencies.push(DependencyRef::required(
            DependencyKind::ProbeSet,
            DependencyTarget::probe_set(&probe_set),
        ));
    }
    if let Ok(digest) = effective_options_digest(options) {
        dependencies.push(DependencyRef::required(
            DependencyKind::EffectiveOptions,
            DependencyTarget::effective_options(digest),
        ));
    }
    dependencies.sort();
    dependencies.dedup();
    Ok(dependencies)
}

fn weighting_dependencies(
    plan: &FamilyPlan,
    members: &[(ScopeMember, LookPool)],
) -> Vec<DependencyRef> {
    let mut dependencies = vec![
        DependencyRef::required(
            DependencyKind::RootWeighting,
            DependencyTarget::root_weighting_global(),
        ),
        DependencyRef::required(
            DependencyKind::RootWeighting,
            DependencyTarget::root_weighting_family(&plan.family_id),
        ),
    ];
    let mut bodies: Vec<&UploadBodyId> = members.iter().map(|(member, _)| &member.body).collect();
    bodies.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    bodies.dedup();
    for body in bodies {
        dependencies.push(DependencyRef::required(
            DependencyKind::RootWeighting,
            DependencyTarget::root_weighting_body(body),
        ));
    }
    dependencies
}

#[allow(clippy::too_many_arguments)]
fn target_dependencies(
    _config: &WorkerConfig,
    plan: &FamilyPlan,
    members: &[(ScopeMember, LookPool)],
    roots: &RootData,
    _snapshot_version: u64,
    options: &FitOptions,
    partitions: &BTreeMap<(UploadBodyId, BodyRealizationId), Partition>,
    stage: BodyStateStage,
) -> Result<Vec<DependencyRef>> {
    let mut dependencies = source_identity::sources().dependencies();
    for (member, pool) in members {
        dependencies.push(DependencyRef::required(
            DependencyKind::Schema,
            DependencyTarget::published_schema(&PublishedSchemaSnapshot::new(
                member.body.clone(),
                pool.observation.snapshot_version.get(),
            )),
        ));
        dependencies.push(DependencyRef::required(
            DependencyKind::Transform,
            DependencyTarget::transform(&member.body, pool.transforms.version()),
        ));
        dependencies.extend(
            pool.observation
                .dependencies
                .iter()
                .filter(|entry| entry.dependency.kind == DependencyKind::Profile)
                .map(|entry| entry.dependency.clone()),
        );
        for image in &pool.images {
            if !image.is_eligible() {
                continue;
            }
            if stage == BodyStateStage::Native && image.role != EvidenceRole::Native {
                continue;
            }
            let group = evidence_group(image)?;
            if partitions
                .get(&(member.body.clone(), member.realization.clone()))
                .is_some_and(|partition| partition.assessment.contains(&group))
            {
                continue;
            }
            let contribution = ContributionRef::new(image.contribution.id.as_str())
                .map_err(|error| WorkerError::Data(error.to_string()))?;
            dependencies.push(DependencyRef::required(
                DependencyKind::Contribution,
                DependencyTarget::contribution(&contribution),
            ));
            let image_ref = ImageRef::new(image.image.id.as_str())
                .map_err(|error| WorkerError::Data(error.to_string()))?;
            dependencies.push(DependencyRef::required(
                DependencyKind::Image,
                DependencyTarget::image(&image_ref),
            ));
            if let Ok(standing) = StandingRef::new(image.credential.id.as_str()) {
                dependencies.push(DependencyRef::required(
                    DependencyKind::Standing,
                    DependencyTarget::standing(&standing),
                ));
            }
            if let Ok(identity) = IdentityResolutionRef::new(image.credential.id.as_str()) {
                dependencies.push(DependencyRef::required(
                    DependencyKind::IdentityResolution,
                    DependencyTarget::identity_resolution(&identity),
                ));
            }
        }
    }
    for admission in &roots.admissions {
        if let Ok(root) = looklab_schema::RootAdmissionRef::new(admission.as_str()) {
            dependencies.push(DependencyRef::required(
                DependencyKind::RootAdmission,
                DependencyTarget::root_admission(&root),
            ));
        }
    }
    if let Ok(probe_set) = ProbeSetId::new(PROBE_SET) {
        dependencies.push(DependencyRef::required(
            DependencyKind::ProbeSet,
            DependencyTarget::probe_set(&probe_set),
        ));
    }
    if let Some(report) = plan.cohort_report {
        dependencies.push(DependencyRef::required(
            DependencyKind::Report,
            DependencyTarget::report(&report),
        ));
    }
    dependencies.push(DependencyRef::required(
        DependencyKind::RootWeighting,
        DependencyTarget::root_weighting_global(),
    ));
    dependencies.push(DependencyRef::required(
        DependencyKind::RootWeighting,
        DependencyTarget::root_weighting_family(&plan.family_id),
    ));
    let mut bodies: Vec<&UploadBodyId> = members.iter().map(|(member, _)| &member.body).collect();
    bodies.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    bodies.dedup();
    for body in bodies {
        dependencies.push(DependencyRef::required(
            DependencyKind::RootWeighting,
            DependencyTarget::root_weighting_body(body),
        ));
    }
    if let Ok(digest) = effective_options_digest(options) {
        dependencies.push(DependencyRef::required(
            DependencyKind::EffectiveOptions,
            DependencyTarget::effective_options(digest),
        ));
    }
    let subject = SubjectKey::for_family(&plan.family_id);
    let assessment: BTreeSet<EvidenceGroupId> = partitions
        .values()
        .flat_map(|partition| partition.assessment.iter().cloned())
        .collect();
    dependencies.push(DependencyRef::required(
        DependencyKind::PartitionPolicy,
        DependencyTarget::partition_policy(partition_policy_digest(&subject, &assessment)),
    ));
    dependencies.sort();
    dependencies.dedup();
    Ok(dependencies)
}

fn effective_options_digest(options: &FitOptions) -> Result<Sha256Hex> {
    let mut hasher = Sha256::new();
    hasher.update(format!("{options:?}").as_bytes());
    Ok(Sha256Hex::from_bytes(hasher.finalize().into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FitScopeKind {
    Seed,
    Qualified,
    Experimental,
}

impl FitScopeKind {
    fn of(scope: &EvidenceScope) -> Self {
        match scope {
            EvidenceScope::Seed => Self::Seed,
            EvidenceScope::Qualified { .. } => Self::Qualified,
            EvidenceScope::Experimental => Self::Experimental,
        }
    }

    const fn tag(self) -> u8 {
        match self {
            Self::Seed => 0,
            Self::Qualified => 1,
            Self::Experimental => 2,
        }
    }
}

#[derive(Debug, Clone)]
struct MemberQualification {
    body: UploadBodyId,
    realization: BodyRealizationId,
    scope: FitScopeKind,
    admission: Option<ModelVersionId>,
}

/// The qualification that authorizes a fit, independent of the images carrying it.
#[derive(Debug, Clone, Default)]
struct FitQualification {
    cohort: Option<ModelReportId>,
    members: Vec<MemberQualification>,
}

impl FitQualification {
    fn new(cohort: Option<ModelReportId>, members: impl IntoIterator<Item = ScopeMember>) -> Self {
        let mut members: Vec<MemberQualification> = members
            .into_iter()
            .map(|member| MemberQualification {
                body: member.body,
                realization: member.realization,
                scope: FitScopeKind::of(&member.scope),
                admission: member.admission.map(|binding| binding.candidate),
            })
            .collect();
        members.sort_by(|left, right| {
            (&left.body, &left.realization).cmp(&(&right.body, &right.realization))
        });
        Self { cohort, members }
    }

    fn hash_into(&self, hasher: &mut Sha256) {
        match &self.cohort {
            Some(report) => {
                hasher.update([1]);
                hasher.update(report.to_string().as_bytes());
            }
            None => hasher.update([0]),
        }
        for member in &self.members {
            hasher.update([0]);
            hasher.update(member.body.as_str().as_bytes());
            hasher.update([0]);
            hasher.update(member.realization.epoch.get().to_le_bytes());
            hasher.update(member.realization.native_sim.as_str().as_bytes());
            hasher.update([member.scope.tag()]);
            match &member.admission {
                Some(candidate) => {
                    hasher.update([1]);
                    hasher.update(candidate.candidate_id().as_bytes());
                }
                None => hasher.update([0]),
            }
        }
    }
}

fn subject_fingerprint(
    subject: &SubjectKey,
    pools: &[LookPool],
    roots: &RootData,
    target: &FamilyTargetId,
    options: &FitOptions,
    partitions: &BTreeMap<(UploadBodyId, BodyRealizationId), Partition>,
    qualification: &FitQualification,
) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(subject.as_str().as_bytes());
    hasher.update(target.to_key().as_bytes());
    qualification.hash_into(&mut hasher);
    for pool in pools {
        for image in &pool.images {
            hasher.update(image.image.id.as_str().as_bytes());
            hasher.update(db::hex(&image.image.samples_hash).as_bytes());
            hasher.update(image.contribution.id.as_str().as_bytes());
            hasher.update(image.contribution.status.as_str().as_bytes());
            hasher.update(image.contribution.intent.as_str().as_bytes());
            hasher.update(
                image
                    .image
                    .status
                    .map_or("pending", ImageOutcome::as_str)
                    .as_bytes(),
            );
            hasher.update(image.credential.id.as_str().as_bytes());
            hasher.update(format!("{:?}", image.credential.tier).as_bytes());
            hasher.update(image.contributor.to_string().as_bytes());
            hasher.update(serde_json::to_vec(&image.credential.scope)?);
            hasher.update(serde_json::to_vec(&image.settings)?);
            hasher.update(image.epoch.to_le_bytes());
            hasher.update(image.exposure_mode.0.to_le_bytes());
            hasher.update(image.realization.epoch.get().to_le_bytes());
            hasher.update(image.realization.native_sim.as_str().as_bytes());
            hasher.update(format!("{:?}", image.role).as_bytes());
            hasher.update(format!("{:?}", image.eligibility).as_bytes());
            if image.is_eligible() {
                for admitted in &image.admitted {
                    hasher.update([u8::from(*admitted)]);
                }
            }
        }
    }
    let mut admissions: Vec<&str> = roots
        .admissions
        .iter()
        .map(|admission| admission.as_str())
        .collect();
    admissions.sort_unstable();
    for admission in admissions {
        hasher.update(admission.as_bytes());
        hasher.update([0]);
    }
    hasher.update([u8::from(roots.weights.rooted)]);
    hasher.update(format!("{options:?}").as_bytes());
    hasher.update(source_identity::checkpoint_identity().to_hex().as_bytes());
    for dependency in source_identity::sources().dependencies() {
        hasher.update(dependency.kind.as_str().as_bytes());
        hasher.update(dependency.target.as_str().as_bytes());
    }
    let assessment: BTreeSet<EvidenceGroupId> = partitions
        .values()
        .flat_map(|partition| partition.assessment.iter().cloned())
        .collect();
    hasher.update(
        partition_policy_digest(subject, &assessment)
            .to_hex()
            .as_bytes(),
    );
    Ok(db::hex(&hasher.finalize()))
}

fn look_document(
    look: &LookSlug,
    version: u32,
    family: &looklab_fit::hierarchical::FamilyParameters,
    maker: &str,
    provenance: &Provenance,
) -> Result<(LookDocument, Vec<u8>)> {
    let identity = identity(look.as_str(), &format!("{version}.0.0"))?;
    let mut document = LookDocument::from_look(identity, family.look.clone())?;
    document.file.provenance = provenance.clone();
    document.file.rendering = Some(CameraRendering {
        maker: maker.to_owned(),
        name: look.as_str().to_owned(),
    });
    let count = family.probes.probes.len();
    let mut covariance = Vec::with_capacity(count * 9);
    let mut covariance_post = Vec::with_capacity(count * 9);
    let mut bias = Vec::with_capacity(count);
    let mut sigma = Vec::with_capacity(count);
    let mut sigma_post = Vec::with_capacity(count);
    let mut pre_reliable = Vec::with_capacity(count);
    let mut support = Vec::with_capacity(count);
    let mut density = Vec::with_capacity(count);
    for probe in &family.probes.probes {
        for row in &probe.covariance {
            covariance.extend_from_slice(row);
        }
        for row in &probe.post_covariance {
            covariance_post.extend_from_slice(row);
        }
        bias.push(probe.bias_de);
        sigma.push(probe.sigma_de);
        sigma_post.push(probe.post_sigma_de);
        pre_reliable.push(if probe.pre_clip_reliable { 1.0 } else { 0.0 });
        support.push(if probe.supported { 1.0 } else { 0.0 });
        density.push(probe.density);
    }
    document = document.with_uncertainty(UncertaintyData {
        probe_cov: Some(ProbeCovariance::new(count, covariance)?),
        probe_cov_post: Some(ProbeCovariance::new(count, covariance_post)?),
        probe_bias: Some(ProbeScalars::new(bias)?),
        probe_sigma: Some(ProbeScalars::new(sigma)?),
        probe_sigma_post: Some(ProbeScalars::new(sigma_post)?),
        probe_pre_reliable: Some(ProbeScalars::new(pre_reliable)?),
        support: Some(ProbeScalars::new(support)?),
        density: Some(Density::new(density)?),
    })?;
    let bytes = document.encode()?;
    Ok((document, bytes))
}

fn card_meta(look: &LookSlug) -> CardMeta {
    let short: String = look.as_str().chars().take(27).collect();
    CardMeta::new(look.as_str(), short)
}

fn probe_xyz() -> BTreeMap<ProbeId, looklab_color::Xyz> {
    looklab_model::probes::v1()
        .probes
        .into_iter()
        .map(|probe| (probe.id, probe.xyz))
        .collect()
}

fn clear_family_support(family: &mut looklab_fit::hierarchical::FamilyParameters) {
    for probe in &mut family.probes.probes {
        probe.supported = false;
        probe.supported_sigma = false;
        probe.supported_density = false;
        probe.supported_clusters = false;
        probe.supported_bias = false;
    }
}

fn stored_probes(look: &Look, probes: &ProbeUncertainty) -> Vec<StoredProbe> {
    let xyz = probe_xyz();
    probes
        .ids
        .iter()
        .zip(&probes.probes)
        .map(|(id, probe)| {
            let feasible = xyz
                .get(id)
                .is_some_and(|xyz| checked_appearance(look, *xyz).is_ok());
            StoredProbe {
                id: id.clone(),
                sigma: if feasible { probe.sigma_de } else { f64::MAX },
                sigma_post: if feasible {
                    probe.post_sigma_de
                } else {
                    f64::MAX
                },
                bias_de: probe.bias_de,
                pre_clip_identified: feasible && probe.pre_clip_reliable,
                support: if feasible {
                    TrustProbeSupport {
                        sigma: probe.supported_sigma,
                        density: probe.supported_density,
                        clusters: probe.supported_clusters,
                        bias: probe.supported_bias,
                    }
                } else {
                    TrustProbeSupport::default()
                },
            }
        })
        .collect()
}

fn candidate_estimates(
    look: &Look,
    probes: &ProbeUncertainty,
    _output_space: looklab_color::RgbSpace,
) -> VersionProbes {
    let xyz = probe_xyz();
    VersionProbes {
        probes: probes
            .ids
            .iter()
            .zip(&probes.probes)
            .map(|(id, probe)| {
                let feasible = xyz
                    .get(id)
                    .is_some_and(|xyz| checked_appearance(look, *xyz).is_ok());
                TrustProbeEstimate {
                    id: id.clone(),
                    pre_clip: if feasible {
                        probe.ucs.to_array()
                    } else {
                        [f64::NAN; 3]
                    },
                    sigma_pre_clip: if feasible { probe.sigma_de } else { f64::MAX },
                    post_clip: if feasible {
                        probe.post_ucs.to_array()
                    } else {
                        [f64::NAN; 3]
                    },
                    sigma_post_clip: if feasible {
                        probe.post_sigma_de
                    } else {
                        f64::MAX
                    },
                    bias_de: probe.bias_de,
                    pre_clip_identified: feasible && probe.pre_clip_reliable,
                    support: if feasible {
                        TrustProbeSupport {
                            sigma: probe.supported_sigma,
                            density: probe.supported_density,
                            clusters: probe.supported_clusters,
                            bias: probe.supported_bias,
                        }
                    } else {
                        TrustProbeSupport::default()
                    },
                }
            })
            .collect(),
    }
}

fn stored_estimates(look: &Look, record: &ReleaseRecord, gauge: f64) -> Result<VersionProbes> {
    let by_id: BTreeMap<&ProbeId, &StoredProbe> = record
        .probes
        .iter()
        .map(|probe| (&probe.id, probe))
        .collect();
    let reference = relative_gauge(record).ok_or(WorkerError::MigrationRequired {
        version: record.version,
    })?;
    let exposure = ExposureGauge::between(reference, gauge);
    let mut probes = Vec::with_capacity(by_id.len());
    for probe in looklab_model::probes::v1().probes {
        let stored = by_id.get(&probe.id).ok_or_else(|| {
            WorkerError::Data(format!(
                "release {} lacks probe {}",
                record.version,
                probe.id.as_str()
            ))
        })?;
        let input = exposure.apply(probe.xyz);
        let feasible = checked_appearance(look, input).is_ok();
        probes.push(TrustProbeEstimate {
            id: probe.id,
            pre_clip: if feasible {
                look.eval(input).to_array()
            } else {
                [f64::NAN; 3]
            },
            sigma_pre_clip: if feasible { stored.sigma } else { f64::MAX },
            post_clip: if feasible {
                post_clip(look, input, record.output_space, record.clip_k).to_array()
            } else {
                [f64::NAN; 3]
            },
            sigma_post_clip: if feasible {
                stored.sigma_post
            } else {
                f64::MAX
            },
            bias_de: stored.bias_de,
            pre_clip_identified: feasible && stored.pre_clip_identified,
            support: if feasible {
                stored.support
            } else {
                TrustProbeSupport::default()
            },
        });
    }
    Ok(VersionProbes { probes })
}

fn relative_gauge(record: &ReleaseRecord) -> Option<f64> {
    match record.gauge_convention {
        GaugeConvention::Relative => Some(record.gauge),
        GaugeConvention::LegacyAbsoluteHeader => None,
    }
}

fn gated_since_anchor(
    previous: Option<&TargetSnapshot>,
    anchor: u32,
    anchor_probes: &VersionProbes,
) -> GatedUnion {
    match previous {
        Some(snapshot) if snapshot.record.anchor == anchor => {
            snapshot.record.gated_since_anchor.clone()
        }
        _ => GatedUnion::at_anchor(anchor_probes),
    }
}

fn previous_support(record: &ReleaseRecord) -> PreviousSupport {
    PreviousSupport {
        sigma: record
            .probes
            .iter()
            .map(|probe| probe.support.sigma)
            .collect(),
        bias: record
            .probes
            .iter()
            .map(|probe| probe.support.bias)
            .collect(),
    }
}

fn provenance_summary(
    family: &looklab_fit::hierarchical::FamilyParameters,
    members: &[(ScopeMember, LookPool)],
    anchor_version: u32,
    roots: &RootData,
    snapshot_version: u64,
    now: i64,
) -> Result<ProvenanceSummary> {
    let mut prep_versions: Vec<u32> = members
        .iter()
        .flat_map(|(_, pool)| pool.images.iter())
        .map(|image| image.samples.header().prep_version.get())
        .collect();
    prep_versions.sort_unstable();
    prep_versions.dedup();
    let mut bodies = Vec::new();
    for provenance in &family.provenance {
        let pool = members
            .iter()
            .find(|(member, _)| member.realization == provenance.realization)
            .map(|(_, pool)| pool);
        let credentials: BTreeSet<String> = pool
            .map(|pool| {
                pool.images
                    .iter()
                    .map(|image| image.credential.id.as_str().to_owned())
                    .collect()
            })
            .unwrap_or_default();
        let images = pool.map_or(0, |pool| {
            pool.images
                .iter()
                .filter(|image| image.role == EvidenceRole::Native && image.is_eligible())
                .count()
        });
        bodies.push(BodyUse {
            body: provenance.body.as_str().to_owned(),
            epochs: vec![provenance.realization.epoch.get()],
            images: u32::try_from(images).unwrap_or(u32::MAX),
            credentials: u32::try_from(credentials.len()).unwrap_or(u32::MAX),
        });
    }
    bodies.sort_by(|left, right| left.body.cmp(&right.body));
    bodies.dedup_by(|left, right| left.body == right.body);
    Ok(ProvenanceSummary {
        bodies,
        root_admissions: roots
            .admissions
            .iter()
            .map(|admission| admission.as_str().to_owned())
            .collect(),
        anchor_version: format!("{anchor_version}.0.0"),
        community_snapshot_version: snapshot_version,
        prep_versions,
        created: rfc3339(now),
        software: format!("looklab-worker {}", env!("CARGO_PKG_VERSION")),
    })
}

fn embedded_provenance(
    family: &looklab_fit::hierarchical::FamilyParameters,
    summary: &ProvenanceSummary,
) -> Provenance {
    let mut bodies: BTreeMap<String, BodyProvenance> = BTreeMap::new();
    for provenance in &family.provenance {
        let body = provenance.body.as_str().to_owned();
        let entry = bodies.entry(body.clone()).or_insert_with(|| {
            let counts = summary.bodies.iter().find(|entry| entry.body == body);
            BodyProvenance {
                body,
                epoch_map_version: 0,
                epochs: Vec::new(),
                input_transform_version: provenance.transform_version.get(),
                realization_params_version: 0,
                control_response_version: provenance.model_version.unwrap_or(0),
                clip_k: provenance.clip_k,
                images: counts.map_or(0, |entry| entry.images),
                credentials: counts.map_or(0, |entry| entry.credentials),
            }
        });
        entry.epochs.push(provenance.realization.epoch.get());
    }
    let mut bodies: Vec<BodyProvenance> = bodies.into_values().collect();
    for body in &mut bodies {
        body.epochs.sort_unstable();
        body.epochs.dedup();
    }
    Provenance {
        bodies,
        root: RootProvenance {
            admissions: summary.root_admissions.clone(),
            anchor_version: summary.anchor_version.clone(),
        },
        base: None,
        needs_rebase: false,
        community_snapshot_version: summary.community_snapshot_version,
        prep_versions: summary.prep_versions.clone(),
        created: summary.created.clone(),
        software: summary.software.clone(),
    }
}

async fn pinned_modes_for_revision(
    conn: &mut sqlx::SqliteConnection,
    config: &WorkerConfig,
    stored: &model_store::StoredRealizationRevision,
) -> Result<Option<PinnedModes>> {
    let Some(params_binding) = stored
        .artifacts
        .iter()
        .find(|artifact| artifact.role == ArtifactRole::RealizationParams)
    else {
        return Ok(None);
    };
    let params: MatchRealizationParams =
        serde_json::from_slice(&read_blob(config, &params_binding.sha256)?)?;
    let Some(state) =
        model_store::body_state(conn, &stored.subject.body, stored.body_state_version).await?
    else {
        return Ok(None);
    };
    let body_params: MatchParams =
        serde_json::from_slice(&read_blob(config, &state.pins.body_params.sha256)?)?;
    Ok(Some(PinnedModes {
        firmware: params
            .firmware
            .iter()
            .map(|entry| entry.firmware.clone())
            .collect(),
        wb_modes: body_params
            .wb
            .iter()
            .map(|entry| entry.mode.clone())
            .collect(),
    }))
}

pub(crate) async fn pinned_modes_for_candidate(
    conn: &mut sqlx::SqliteConnection,
    config: &WorkerConfig,
    candidate: &ModelVersionId,
) -> Result<Option<PinnedModes>> {
    let ModelVersionId::BodyRealization { realization } = candidate else {
        return Ok(None);
    };
    let revision = looklab_schema::RealizationRevisionId::for_version(realization);
    let Some(stored) = model_store::realization_revision(conn, &revision).await? else {
        return Ok(None);
    };
    pinned_modes_for_revision(conn, config, &stored).await
}

async fn load_target_snapshot(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    look: &LookSlug,
    version: TargetVersion,
) -> Result<TargetSnapshot> {
    let version = version.get();
    let artifacts = db::manifest_artifacts(db_pool, look.as_str(), version)
        .await?
        .ok_or_else(|| WorkerError::Data(format!("{} {version} has no manifest", look)))?;
    let document = read_artifact(config, &artifacts, LOOK_ARTIFACT)?;
    let record = read_artifact(config, &artifacts, RELEASE_ARTIFACT)?;
    Ok(TargetSnapshot {
        look: LookDocument::decode(&document)?.look,
        record: serde_json::from_slice(&record)?,
        document,
    })
}

async fn select_anchor(
    db_pool: &SqlitePool,
    look: &LookSlug,
    current: TargetVersion,
) -> Result<Option<u32>> {
    let family =
        LookId::new(look.as_str()).map_err(|error| WorkerError::Data(error.to_string()))?;
    let subject = SubjectKey::for_family(&family);
    let mut conn = db_pool.acquire().await?;
    let anchor = model_store::subject_state(&mut conn, &subject)
        .await?
        .and_then(|state| state.anchor_revision)
        .filter(|anchor| *anchor < current.get());
    Ok(anchor)
}

fn read_artifact(
    config: &WorkerConfig,
    artifacts: &[ManifestArtifact],
    name: &str,
) -> Result<Vec<u8>> {
    let artifact = artifacts
        .iter()
        .find(|artifact| artifact.name == name)
        .ok_or_else(|| WorkerError::Data(format!("manifest has no `{name}` artifact")))?;
    Ok(std::fs::read(config.blobs.join(&artifact.sha256))?)
}

fn store_blob(config: &WorkerConfig, bytes: &[u8]) -> Result<BlobHash> {
    let store = FsBlobStore::new(config.blobs.as_path()).map_err(blob_error)?;
    let hash = BlobHash::of(bytes);
    store.put(hash, bytes).map_err(blob_error)?;
    Ok(hash)
}

fn blob_error(error: BlobError) -> WorkerError {
    match error {
        BlobError::Io(error) => WorkerError::Io(error),
    }
}

fn store_artifact(config: &WorkerConfig, name: &str, bytes: &[u8]) -> Result<ManifestArtifact> {
    let hash = store_blob(config, bytes)?;
    Ok(ManifestArtifact {
        name: name.to_owned(),
        sha256: hash.to_hex(),
    })
}

fn store_binding(
    config: &WorkerConfig,
    bytes: &[u8],
    role: ArtifactRole,
) -> Result<ArtifactBinding> {
    let hash = store_blob(config, bytes)?;
    Ok(ArtifactBinding {
        sha256: Sha256Hex::parse(&hash.to_hex())
            .map_err(|error| WorkerError::Data(error.to_string()))?,
        role,
    })
}

fn read_blob(config: &WorkerConfig, hash: &Sha256Hex) -> Result<Vec<u8>> {
    Ok(std::fs::read(config.blobs.join(hash.to_hex()))?)
}

fn lease<'a>(ownership: &JobOwnership, job: &'a looklab_server::JobId) -> JobLease<'a> {
    JobLease {
        job,
        attempts: ownership.attempts,
    }
}

async fn register_bundles(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    ownership: &JobOwnership,
    touched: &[TouchedEpoch],
) -> Result<()> {
    let server_job = ownership.server_id();
    let sources = source_identity::sources().storage();
    let mut seen = BTreeSet::new();
    for entry in touched {
        if !seen.insert((entry.body.clone(), entry.epoch)) {
            continue;
        }
        let recorded = bundle::record_epoch_revision(
            db_pool,
            config,
            entry.snapshot_version,
            &entry.snapshot_root,
            &entry.body,
            entry.epoch,
            entry.published_state_version,
        )
        .await?;
        let Some((id, scope)) = recorded else {
            continue;
        };
        let use_ = match scope {
            looklab_schema::SelectionScope::Qualified => ProjectionUse::QualifiedMatch,
            looklab_schema::SelectionScope::Experimental => ProjectionUse::ExperimentalMatch,
            looklab_schema::SelectionScope::Unassessed => continue,
        };
        let now = now_unix();
        let mut conn = db_pool.acquire().await?;
        let dependencies =
            model_store::dependencies(&mut conn, "bundle_revision", &id.to_hex()).await?;
        let snapshot = model_store::dependency_snapshot(&mut conn, &dependencies).await?;
        let current = model_store::current_bundles(&mut conn).await?;
        let expected = current
            .iter()
            .find(|current| {
                current.body == entry.body && current.epoch == entry.epoch && current.use_ == use_
            })
            .map(|current| current.bundle);
        let outcome = model_store::select_bundle(
            &mut conn,
            &model_store::BundleSelection {
                body: &entry.body,
                epoch: entry.epoch,
                use_,
                id: &id,
                expected: expected.as_ref(),
                lease: lease(ownership, &server_job),
                sources: &sources,
                snapshot: &snapshot,
            },
            now,
        )
        .await?;
        if let model_store::BundleSelectionOutcome::Rejected(rejection) = outcome {
            tracing::warn!(
                body = %entry.body,
                epoch = entry.epoch,
                ?rejection,
                "bundle selection was fenced"
            );
        }
    }
    Ok(())
}

#[derive(Default)]
struct ReputationCounts {
    total: u32,
    accepted: u32,
    quarantined: u32,
    passing_charts: u32,
}

struct ReputationImage<'a> {
    role: EvidenceRole,
    eligible: bool,
    chart: bool,
    contribution: &'a ContributionId,
    credential: &'a CredentialId,
    outcome: Option<ImageOutcome>,
}

fn reputation_counts<'a>(
    images: impl IntoIterator<Item = ReputationImage<'a>>,
) -> BTreeMap<CredentialId, ReputationCounts> {
    let mut counts: BTreeMap<CredentialId, ReputationCounts> = BTreeMap::new();
    let mut chart_pass: BTreeMap<&ContributionId, (&CredentialId, bool)> = BTreeMap::new();
    for image in images {
        if image.role != EvidenceRole::Native || !image.eligible {
            continue;
        }
        let entry = counts.entry(image.credential.clone()).or_default();
        if let Some(outcome) = image.outcome {
            entry.total += 1;
            match outcome {
                ImageOutcome::Accepted => entry.accepted += 1,
                ImageOutcome::DownWeighted => {}
                ImageOutcome::Quarantined => entry.quarantined += 1,
            }
        }
        if image.chart {
            let chart = chart_pass
                .entry(image.contribution)
                .or_insert((image.credential, true));
            if image.outcome != Some(ImageOutcome::Accepted) {
                chart.1 = false;
            }
        }
    }
    for (credential, passed) in chart_pass.into_values() {
        if passed && let Some(entry) = counts.get_mut(credential) {
            entry.passing_charts += 1;
        }
    }
    counts
}

#[allow(clippy::too_many_arguments)]
async fn update_reputation(
    db_pool: &SqlitePool,
    look: &LookSlug,
    version: u32,
    members: &[(ScopeMember, LookPool)],
    previous_created_at: Option<i64>,
    lease: JobLease<'_>,
    sources: &model_store::NumericalSources,
    dependencies: &[DependencyRef],
    snapshot: &[DependencyGeneration],
) -> Result<()> {
    let mut tx = model_store::begin_immediate(db_pool).await?;
    if !model_store::execution_current(&mut tx, lease, sources, dependencies, snapshot).await? {
        return Ok(());
    }
    let family =
        LookId::new(look.as_str()).map_err(|error| WorkerError::Data(error.to_string()))?;
    let Some(head) = model_store::family_head(&mut tx, &family).await? else {
        return Ok(());
    };
    if head.target.version.get() != version {
        return Ok(());
    }
    let params = TrustParams::default();
    let now = now_unix();
    let days = previous_created_at.map_or(0.0, |created| (now - created).max(0) as f64 / 86_400.0);
    let scores = load_native_qualified_outcomes(&mut tx, members).await?;
    let mut evidence = Vec::new();
    for (_, pool) in members {
        for image in &pool.images {
            evidence.push(ReputationImage {
                role: image.role,
                eligible: image.is_eligible(),
                chart: image.contribution.kind == looklab_upload::ContributionKind::Chart,
                contribution: &image.contribution.id,
                credential: &image.credential.id,
                outcome: scores.get(&image.image.id),
            });
        }
    }
    let counts = reputation_counts(evidence);
    for (credential, counts) in counts {
        if counts.total == 0 {
            continue;
        }
        let id = credential;
        let row: Option<f64> =
            sqlx::query_scalar("SELECT reputation FROM credentials WHERE id = ?")
                .bind(id.as_str())
                .fetch_optional(&mut *tx)
                .await?;
        let current = Reputation::saturating(row.unwrap_or(0.5));
        let updated = current.updated(
            &params,
            ReputationUpdate {
                accepted_fraction: f64::from(counts.accepted) / f64::from(counts.total),
                deliberate_quarantines: 0,
                automatic_quarantines: counts.quarantined,
                passing_chart_sessions: counts.passing_charts,
                days_elapsed: days,
            },
        );
        db::apply_reputation_once(&mut tx, look.as_str(), version, &id, updated.value(), now)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod cohort_routing_tests {
    use super::*;
    use looklab_schema::{
        AssessedPoint, BodyId as SchemaBodyId, BodyRealizationVersionId, CellId,
        CohortAdmissionRequest, CombinationBasis, DomainId, EnginePrerequisite, FirmwareId,
        MatchParams, MatchParamsFormat, MatchRealizationParams, MatchStage, MatchTransform,
        MetricKey, ModelReport, OutputEncoding, ReportMetrics, ReportProvenance, ReportScope,
        RuleRevision, SettingCombination, SimKey, SourceRef, SupportedMask,
    };

    const FAMILY: &str = "classic-chrome";
    const BODY: &str = "fujifilm/x-t5";
    const SECOND_BODY: &str = "fujifilm/x-t4";
    const SIM: &str = "classic_chrome";
    const FIRMWARE: &str = "1.50";
    const OUT_OF_SCOPE_FIRMWARE: &str = "1.60";
    const DOMAIN: &str = "cross-body-common-support";
    const TRANSFORM: &str = "format: looklab-transform/1
version: 1
camera_to_xyz:
  - [1.0, 0.0, 0.0]
  - [0.0, 1.0, 0.0]
  - [0.0, 0.0, 1.0]
";
    const IN_SCOPE_SETTINGS: &str = r#"[{"key":"color","step":0,"source":"reproduced"},
    {"key":"color_chrome_effect","step":0,"source":"reproduced"},
    {"key":"color_chrome_fx_blue","step":0,"source":"reproduced"},
    {"key":"dynamic_range","step":0,"source":"reproduced"},
    {"key":"highlight_tone","step":0,"source":"reproduced"},
    {"key":"shadow_tone","step":0,"source":"reproduced"},
    {"key":"wb_shift_b","step":0,"source":"reproduced"},
    {"key":"wb_shift_r","step":0,"source":"reproduced"}]"#;

    struct CohortFixture {
        _dir: tempfile::TempDir,
        pool: SqlitePool,
        config: WorkerConfig,
        realization: BodyRealizationId,
        target: FamilyTargetId,
    }

    fn body() -> SchemaBodyId {
        SchemaBodyId::new(BODY).expect("body")
    }

    fn second_body() -> SchemaBodyId {
        SchemaBodyId::new(SECOND_BODY).expect("body")
    }

    fn family() -> LookId {
        LookId::new(FAMILY).expect("family")
    }

    fn realization() -> BodyRealizationId {
        BodyRealizationId {
            body: body(),
            epoch: ControlEpochId::new(1),
            native_sim: SimKey::new(SIM).expect("sim"),
        }
    }

    fn second_realization() -> BodyRealizationId {
        BodyRealizationId {
            body: second_body(),
            epoch: ControlEpochId::new(1),
            native_sim: SimKey::new(SIM).expect("sim"),
        }
    }

    fn target() -> FamilyTargetId {
        FamilyTargetId {
            family: family(),
            version: TargetVersion::new(1).expect("version"),
        }
    }

    async fn fixture() -> CohortFixture {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = dir.path().join("looklab.sqlite3");
        let pool = looklab_server::open_database(&db).await.expect("database");
        let transforms = dir.path().join("transforms");
        std::fs::create_dir_all(&transforms).expect("transform dir");
        std::fs::write(transforms.join("fujifilm-x-t5.yaml"), TRANSFORM).expect("transform");
        std::fs::write(transforms.join("fujifilm-x-t4.yaml"), TRANSFORM).expect("transform");
        let mut config = WorkerConfig::new(&db, dir.path().join("blobs"));
        config.data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
        config.transforms = transforms;
        config.snapshot_dir = dir.path().join("community");
        std::fs::create_dir_all(&config.blobs).expect("blob dir");
        CohortFixture {
            _dir: dir,
            pool,
            config,
            realization: realization(),
            target: target(),
        }
    }

    async fn insert_head(pool: &SqlitePool, source_limited: bool) {
        sqlx::query(
            "INSERT INTO look_versions (id, look, version, status, created_at)
             VALUES ('classic-chrome-1', 'classic-chrome', 1, 'provisional', 0)",
        )
        .execute(pool)
        .await
        .expect("look version");
        sqlx::query(
            "INSERT INTO model_family_heads
             (family, head_version, source_limited, numerical, release_gates, report_id,
              assessed_generation, applicability, invalidated_reason, updated_at)
             VALUES ('classic-chrome', 1, ?, 'passed', 'passed', NULL, 0, 'current', NULL, 0)",
        )
        .bind(if source_limited { 1i64 } else { 0i64 })
        .execute(pool)
        .await
        .expect("family head");
    }

    async fn register_evidence(pool: &SqlitePool, realization: &BodyRealizationId) {
        let mut conn = pool.acquire().await.expect("connection");
        model_store::register_subject(
            &mut conn,
            &ModelSubject::BodyRealization {
                realization: realization.clone(),
            },
            0,
        )
        .await
        .expect("subject");
    }

    async fn record_seed_cohort(pool: &SqlitePool) {
        let mut conn = pool.acquire().await.expect("connection");
        model_store::record_cohort_revision(
            &mut conn,
            &family(),
            CohortRole::Seed,
            CohortBasis::SeedEvidence,
            Sha256Hex::from_bytes([1; 32]),
            &[CohortMember::Seed { body: body() }],
            0,
        )
        .await
        .expect("seed cohort");
    }

    async fn record_admitted_cohort_without_admission(pool: &SqlitePool) {
        let mut conn = pool.acquire().await.expect("connection");
        model_store::record_cohort_revision(
            &mut conn,
            &family(),
            CohortRole::Admitted,
            CohortBasis::CrossBodyAdmission,
            Sha256Hex::from_bytes([2; 32]),
            &[CohortMember::Seed { body: body() }],
            0,
        )
        .await
        .expect("admitted cohort");
    }

    fn store_blob(config: &WorkerConfig, bytes: &[u8]) -> Sha256Hex {
        let hash = Sha256Hex::from_bytes(Sha256::digest(bytes).into());
        std::fs::write(config.blobs.join(hash.to_hex()), bytes).expect("blob");
        hash
    }

    fn store_body_params(config: &WorkerConfig, body: &SchemaBodyId) -> Sha256Hex {
        let params = MatchParams {
            format: MatchParamsFormat::V1,
            stage: MatchStage::Native,
            body: body.clone(),
            transform: MatchTransform {
                version: 1,
                camera_to_xyz: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                delta: [0.0; 6],
            },
            exposure: Vec::new(),
            wb: Vec::new(),
        };
        store_blob(config, &serde_json::to_vec(&params).expect("body params"))
    }

    fn store_realization_params(
        config: &WorkerConfig,
        realization: &BodyRealizationId,
        version: RealizationVersion,
        target: &FamilyTargetId,
    ) -> Sha256Hex {
        let params = MatchRealizationParams {
            format: MatchParamsFormat::V1,
            realization: BodyRealizationVersionId {
                realization: realization.clone(),
                version,
                target: target.clone(),
            },
            family: target.clone(),
            version: 1,
            control_response_version: 1,
            clip_k: 1.0,
            firmware: Vec::new(),
        };
        store_blob(
            config,
            &serde_json::to_vec(&params).expect("realization params"),
        )
    }

    fn engine_prerequisite() -> EnginePrerequisite {
        let mut metrics = ReportMetrics::new();
        metrics.insert(MetricKey::RecoveryRate, 0.9);
        metrics.insert(MetricKey::Coverage, 0.9);
        metrics.insert(MetricKey::AssessedCells, 1.0);
        metrics.insert(MetricKey::SupportedCells, 1.0);
        let mut supported_mask = SupportedMask::new();
        supported_mask.insert(CellId::new("latent-1").expect("cell"));
        EnginePrerequisite {
            engine: SourceRef::new("engine@1").expect("engine"),
            rule_revision: RuleRevision::new("p4.3/1").expect("rule"),
            outcome: AssessmentOutcome::Pass,
            metrics,
            supported_mask,
            limitations: Vec::new(),
        }
    }

    fn neutral_mask() -> SupportedMask {
        let mut mask = SupportedMask::new();
        mask.insert(CellId::new("neutral").expect("cell"));
        mask
    }

    fn scopes_for(mask: &SupportedMask) -> (ReportScope, ReportScope, ReportScope) {
        let cells: Vec<CellId> = mask.iter().cloned().collect();
        let wb_mode = WbModeKey::new("Daylight").expect("wb mode");
        let point = |body: SchemaBodyId| AssessedPoint {
            settings: SettingCombination::new(),
            firmware: Some(FirmwareId {
                body: body.clone(),
                epoch: ControlEpochId::new(1),
                version: FIRMWARE.parse().expect("firmware"),
            }),
            wb_mode: Some(wb_mode.clone()),
        };
        let scope = |body: SchemaBodyId| ReportScope {
            points: vec![point(body.clone())],
            combination_basis: CombinationBasis::Enumerated,
            defaults_assessed: true,
            output_encoding: OutputEncoding::Srgb,
            colour_cells: cells.clone(),
            exposure_cells: Vec::new(),
            assessed_bodies: vec![body],
            assessed_domain: DomainId::new(DOMAIN).expect("domain"),
        };
        let global = ReportScope {
            points: vec![point(body()), point(second_body())],
            combination_basis: CombinationBasis::Enumerated,
            defaults_assessed: true,
            output_encoding: OutputEncoding::Srgb,
            colour_cells: cells.clone(),
            exposure_cells: Vec::new(),
            assessed_bodies: vec![body(), second_body()],
            assessed_domain: DomainId::new(DOMAIN).expect("domain"),
        };
        (global, scope(body()), scope(second_body()))
    }

    fn pass_report(
        subject: ModelSubject,
        candidate: ModelVersionId,
        capability: QualificationCapability,
        scope: ReportScope,
        supported_mask: SupportedMask,
    ) -> ModelReport {
        let mut metrics = ReportMetrics::new();
        metrics.insert(MetricKey::SupportedCells, supported_mask.len() as f64);
        metrics.insert(MetricKey::AssessedCells, supported_mask.len() as f64);
        metrics.insert(MetricKey::Coverage, 1.0);
        match capability {
            QualificationCapability::NativePrediction => {
                metrics.insert(MetricKey::MedianDe, 1.0);
            }
            QualificationCapability::CrossBodyAdmission => {
                metrics.insert(MetricKey::CrossBodyMedianDisagreementDe, 1.0);
            }
            QualificationCapability::ControlsMatching => {}
        }
        ModelReport {
            subject,
            candidate,
            capability,
            scope,
            provenance: ReportProvenance {
                rule_revision: RuleRevision::new("rule/1").expect("rule"),
                fitter: SourceRef::new("fit@1").expect("fitter"),
                evaluator: SourceRef::new("evaluator@1").expect("evaluator"),
                model: SourceRef::new("model@1").expect("model"),
                probe_set: ProbeSetId::new("v1").expect("probes"),
                effective_options: Sha256Hex::from_bytes([0; 32]),
                partition_policy: Sha256Hex::from_bytes([1; 32]),
                assessment_seeds: vec![1],
            },
            outcome: AssessmentOutcome::Pass,
            metrics,
            supported_mask,
            limitations: Vec::new(),
            evidence_requests: Vec::new(),
        }
    }

    fn saved_firmware(body: &SchemaBodyId) -> FamilyFirmwareId {
        FamilyFirmwareId {
            family: family(),
            firmware: looklab_fit::FirmwareId {
                body: body.clone(),
                epoch: ControlEpochId::new(1),
                version: FIRMWARE.parse().expect("firmware"),
            },
        }
    }

    fn saved_sigma() -> Vec<(looklab_trust::EffectiveContributorId, f64)> {
        vec![(
            looklab_trust::EffectiveContributorId::from(looklab_trust::CredentialId::from_u128(1)),
            0.1,
        )]
    }

    fn stored_resume(
        body: &SchemaBodyId,
        realization: &BodyRealizationId,
        target: &FamilyTargetId,
    ) -> StoredResume {
        let warm = RealizationWarmStart {
            clip_k: 0.03,
            nuisances: Vec::new(),
            damping: 1e-3,
            response: None,
            coefficients: Vec::new(),
        };
        StoredResume {
            body: None,
            realization: warm.clone(),
            crm: None,
            state: HierState {
                generation: HierStateGeneration::default(),
                fingerprint: looklab_fit::hierarchical::HierFingerprint(0),
                outer_iterations: 0,
                bodies: vec![BodyResume {
                    id: body.clone(),
                    state: BodyWarmStart {
                        delta: Delta6::ZERO,
                        exposure: Vec::new(),
                        wb: Vec::new(),
                    },
                }],
                realizations: vec![looklab_fit::hierarchical::RealizationResume {
                    id: realization.clone(),
                    target: target.clone(),
                    state: warm,
                    recipe_fitted: false,
                }],
                published_bodies: Vec::new(),
                published_realizations: Vec::new(),
                families: Vec::new(),
                firmware: vec![(saved_firmware(body), [0.1, 0.01])],
                sigma: Vec::new(),
                body_decrement: None,
                families_certified: false,
                converged: false,
            },
            membership: EvidenceMembership::default(),
            sigma: saved_sigma(),
            inventory: Some(BodyModes::default()),
        }
    }

    fn gauge_artifact(body: &SchemaBodyId) -> GaugeArtifact {
        let mut inventory = BodyModes::default();
        inventory.exposure.insert(
            ExposureModeId(0),
            ExposureModeKey::new("dr100").expect("exposure mode"),
        );
        inventory
            .wb
            .insert(WbModeClass(0), WbModeKey::new("Daylight").expect("wb mode"));
        GaugeArtifact {
            designated: body.clone(),
            exposure_mode: 0,
            wb_mode: 0,
            inventory: Some(inventory),
            anchored_delta: false,
            gauge: 1.0,
            convention: GaugeConvention::Relative,
        }
    }

    async fn record_admission(fixture: &CohortFixture) -> ModelReportId {
        record_admission_with_mask(fixture, neutral_mask()).await
    }

    async fn record_admission_with_mask(
        fixture: &CohortFixture,
        mask: SupportedMask,
    ) -> ModelReportId {
        let mut conn = fixture.pool.acquire().await.expect("connection");
        let (global, first, second) = scopes_for(&mask);
        let members = [
            (body(), fixture.realization.clone(), first),
            (second_body(), second_realization(), second),
        ];
        let mut admitted = Vec::new();
        for (body, realization, scope) in members {
            let state_version =
                model_store::reserve_body_state(&mut conn, &body, BodyStateStage::Native, 0)
                    .await
                    .expect("body state");
            let body_params = store_body_params(&fixture.config, &body);
            let gauge = store_binding(
                &fixture.config,
                &serde_json::to_vec(&gauge_artifact(&body)).expect("gauge artifact"),
                ArtifactRole::Gauge,
            )
            .expect("gauge binding");
            model_store::insert_body_state(
                &mut conn,
                &BodyStateRecord {
                    body: body.clone(),
                    version: state_version,
                    stage: BodyStateStage::Native,
                    pins: BodyStatePins {
                        profile: None,
                        input_transform: ArtifactBinding {
                            sha256: Sha256Hex::from_bytes([5; 32]),
                            role: ArtifactRole::InputTransform,
                        },
                        body_params: ArtifactBinding {
                            sha256: body_params,
                            role: ArtifactRole::BodyRealizationParams,
                        },
                        gauge,
                    },
                    dependencies: Vec::new(),
                },
                &[],
                0,
            )
            .await
            .expect("body state insert");
            let (_, version) = model_store::reserve_realization_version(
                &mut conn,
                &realization,
                &fixture.target,
                0,
            )
            .await
            .expect("reservation");
            let params =
                store_realization_params(&fixture.config, &realization, version, &fixture.target);
            let resume = store_binding(
                &fixture.config,
                &serde_json::to_vec(&stored_resume(&body, &realization, &fixture.target))
                    .expect("resume"),
                ArtifactRole::FitResume,
            )
            .expect("resume binding");
            model_store::insert_realization_revision(
                &mut conn,
                &NewRealizationRevision {
                    subject: &realization,
                    version,
                    stage: RealizationStage::Native,
                    target: &fixture.target,
                    body_state_version: state_version,
                    input_transform_version: 1,
                    realization_params_version: 1,
                    control_response_version: 1,
                    clip_k: 1.0,
                    status: RealizationStatus::Candidate,
                    status_reason: None,
                    safety: CandidateSafety {
                        numerical: SafetyCheck::Passed,
                        release_gates: SafetyCheck::Passed,
                    },
                    artifacts: &[
                        ArtifactBinding {
                            sha256: params,
                            role: ArtifactRole::RealizationParams,
                        },
                        resume,
                    ],
                    dependencies: &[],
                },
                0,
            )
            .await
            .expect("realization revision");
            let report = pass_report(
                ModelSubject::BodyRealization {
                    realization: realization.clone(),
                },
                ModelVersionId::BodyRealization {
                    realization: BodyRealizationVersionId {
                        realization: realization.clone(),
                        version,
                        target: fixture.target.clone(),
                    },
                },
                QualificationCapability::NativePrediction,
                scope.clone(),
                mask.clone(),
            );
            let report = model_store::insert_report(
                &mut conn,
                &report,
                &[],
                &[],
                &EvidenceMembership::default(),
                0,
            )
            .await
            .expect("member report");
            admitted.push(CohortMember::Admitted {
                realization: BodyRealizationVersionId {
                    realization,
                    version,
                    target: fixture.target.clone(),
                },
                report,
                scope: Box::new(scope),
                supported_mask: mask.clone(),
            });
        }
        let engine = model_store::insert_engine_prerequisite(&mut conn, &engine_prerequisite(), 0)
            .await
            .expect("engine prerequisite");
        let admission = pass_report(
            ModelSubject::SharedFamily { family: family() },
            ModelVersionId::SharedFamily {
                target: fixture.target.clone(),
            },
            QualificationCapability::CrossBodyAdmission,
            global,
            mask,
        );
        let dependencies = vec![
            DependencyRef::required(
                DependencyKind::FamilyVersion,
                DependencyTarget::family_version(&fixture.target),
            ),
            DependencyRef::required(
                DependencyKind::EngineQualification,
                DependencyTarget::engine_qualification(&engine),
            ),
        ];
        let admission = model_store::insert_report(
            &mut conn,
            &admission,
            &dependencies,
            &[],
            &EvidenceMembership::default(),
            0,
        )
        .await
        .expect("admission report");
        model_store::record_cohort_admission(
            &mut conn,
            &CohortAdmissionRequest {
                family: family(),
                target: fixture.target.clone(),
                report: admission,
                members: admitted,
                assessed_domain: DomainId::new(DOMAIN).expect("domain"),
            },
            1,
        )
        .await
        .expect("admission");
        admission
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn metadata() -> looklab_upload::ManifestMetadata {
        looklab_upload::ManifestMetadata {
            make: Some("Fujifilm".to_owned()),
            model: Some("X-T5".to_owned()),
            firmware: Some(FIRMWARE.to_owned()),
            lens_model: Some("XF 35mm F1.4 R".to_owned()),
            focal_length_mm: Some(35.0),
            iso: Some(200),
            shutter_seconds: Some(1.0),
            aperture_f_number: Some(2.0),
            exposure_compensation_ev: Some(0.0),
            as_shot_wb: None,
            capture_month: None,
            software: Some("looklab-test".to_owned()),
            render_tag: None,
            render_tag_malformed: false,
            look: Some(looklab_upload::LookSettings {
                white_balance: Some(looklab_upload::Tagged {
                    raw: 0x0002,
                    value: Some(looklab_upload::WhiteBalanceMode::Daylight),
                }),
                ..Default::default()
            }),
        }
    }

    fn metadata_with_firmware(firmware: &str) -> looklab_upload::ManifestMetadata {
        let mut metadata = metadata();
        metadata.firmware = Some(firmware.to_owned());
        metadata
    }

    fn metadata_at(index: usize) -> looklab_upload::ManifestMetadata {
        let mut metadata = metadata();
        metadata.shutter_seconds = Some(1.0 / (index as f64 + 1.0));
        metadata
    }

    async fn insert_out_of_scope_contribution(fixture: &CohortFixture) {
        insert_out_of_scope_for(fixture, &body()).await;
    }

    async fn insert_out_of_scope_for(fixture: &CohortFixture, body: &SchemaBodyId) {
        let dataset = looklab_fit::synth::dataset(&looklab_fit::synth::SynthConfig {
            seed: 21,
            images: 1,
            samples_per_image: 64,
            ..looklab_fit::synth::SynthConfig::default()
        });
        let credential = CredentialId::new();
        sqlx::query(
            "INSERT INTO credentials
             (id, credential_id, webauthn_credential, tier, roles, reputation, created_at, status)
             VALUES (?, ?, '{}', 'seeder', '[]', 1.2, 0, 'active')",
        )
        .bind(credential.as_str())
        .bind(credential.as_str().as_bytes())
        .execute(&fixture.pool)
        .await
        .expect("credential");
        let contribution = ContributionId::new();
        sqlx::query(
            "INSERT INTO contributions
             (id, credential_id, batch_manifest_hash, signature, pow, kind, serial_hmac, intent,
              target_look_id, card, body_id, firmware_version, sim_key, settings, status, created_at)
             VALUES (?, ?, x'00', x'00', 'pow', 'library', NULL, 'native', NULL, NULL, ?, ?, ?, ?,
                     'pending', 100)",
        )
        .bind(contribution.as_str())
        .bind(credential.as_str())
        .bind(body.as_str())
        .bind(OUT_OF_SCOPE_FIRMWARE)
        .bind(SIM)
        .bind(r#"[{"key":"highlight_tone","step":1,"source":"reproduced"}]"#)
        .execute(&fixture.pool)
        .await
        .expect("contribution");
        let samples = &dataset.images[0].samples;
        let bytes = looklab_samples::encode(samples).expect("encode samples");
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        std::fs::write(fixture.config.blobs.join(hex(&hash)), &bytes).expect("blob file");
        sqlx::query(
            "INSERT INTO blobs (hash, ref_count) VALUES (?, 1)
             ON CONFLICT(hash) DO UPDATE SET ref_count = ref_count + 1",
        )
        .bind(hash.as_slice())
        .execute(&fixture.pool)
        .await
        .expect("blob row");
        let image = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO contribution_images
             (id, contribution_id, samples_hash, n_samples, meta, rerender_group, status)
             VALUES (?, ?, ?, ?, ?, NULL, 'pending')",
        )
        .bind(&image)
        .bind(contribution.as_str())
        .bind(hash.as_slice())
        .bind(i64::try_from(samples.n()).expect("samples"))
        .bind(
            serde_json::to_string(&metadata_with_firmware(OUT_OF_SCOPE_FIRMWARE))
                .expect("metadata"),
        )
        .execute(&fixture.pool)
        .await
        .expect("image");
    }

    fn sample_cell(rgb: [f32; 3]) -> CellId {
        let xyz = looklab_color::Xyz::from_array(rgb.map(f64::from));
        let input = looklab_model::LookInput::new(xyz);
        CellId::new(format!("cell-{}", looklab_fit::model_error::cell(&input))).expect("cell")
    }

    /// Inserts in-scope images and returns the full and reduced supported masks.
    async fn insert_in_scope_contribution(
        fixture: &CohortFixture,
        images: &[usize],
    ) -> (SupportedMask, SupportedMask) {
        let dataset = looklab_fit::synth::dataset(&looklab_fit::synth::SynthConfig {
            seed: 21,
            images: images.iter().copied().max().unwrap_or(0) + 1,
            samples_per_image: 200,
            ..looklab_fit::synth::SynthConfig::default()
        });
        let credential = CredentialId::new();
        sqlx::query(
            "INSERT INTO credentials
             (id, credential_id, webauthn_credential, tier, roles, reputation, created_at, status)
             VALUES (?, ?, '{}', 'seeder', '[]', 1.2, 0, 'active')",
        )
        .bind(credential.as_str())
        .bind(credential.as_str().as_bytes())
        .execute(&fixture.pool)
        .await
        .expect("credential");
        let contribution = ContributionId::new();
        sqlx::query(
            "INSERT INTO contributions
             (id, credential_id, batch_manifest_hash, signature, pow, kind, serial_hmac, intent,
              target_look_id, card, body_id, firmware_version, sim_key, settings, status, created_at)
             VALUES (?, ?, x'00', x'00', 'pow', 'library', NULL, 'native', NULL, NULL, ?, ?, ?, ?,
                     'pending', 100)",
        )
        .bind(contribution.as_str())
        .bind(credential.as_str())
        .bind(BODY)
        .bind(FIRMWARE)
        .bind(SIM)
        .bind(IN_SCOPE_SETTINGS)
        .execute(&fixture.pool)
        .await
        .expect("contribution");
        let mut full = SupportedMask::new();
        let mut per_image = Vec::new();
        for index in images {
            let samples = &dataset.images[*index].samples;
            let bytes = looklab_samples::encode(samples).expect("encode samples");
            let hash: [u8; 32] = Sha256::digest(&bytes).into();
            std::fs::write(fixture.config.blobs.join(hex(&hash)), &bytes).expect("blob file");
            sqlx::query(
                "INSERT INTO blobs (hash, ref_count) VALUES (?, 1)
                 ON CONFLICT(hash) DO UPDATE SET ref_count = ref_count + 1",
            )
            .bind(hash.as_slice())
            .execute(&fixture.pool)
            .await
            .expect("blob row");
            let image = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO contribution_images
                 (id, contribution_id, samples_hash, n_samples, meta, rerender_group, status)
                 VALUES (?, ?, ?, ?, ?, NULL, 'pending')",
            )
            .bind(&image)
            .bind(contribution.as_str())
            .bind(hash.as_slice())
            .bind(i64::try_from(samples.n()).expect("samples"))
            .bind(serde_json::to_string(&metadata_at(*index)).expect("metadata"))
            .execute(&fixture.pool)
            .await
            .expect("image");
            let mut cells = BTreeSet::new();
            for rgb in samples.in_rgb() {
                let cell = sample_cell(*rgb);
                full.insert(cell.clone());
                cells.insert(cell);
            }
            per_image.push(cells);
        }
        let reduced = per_image
            .iter()
            .find(|cells| cells.len() > 1)
            .map(|cells| {
                let removed = cells.iter().next().expect("cell");
                let mut mask = SupportedMask::new();
                for cell in full.iter() {
                    if cell != removed {
                        mask.insert(cell.clone());
                    }
                }
                mask
            })
            .unwrap_or_else(|| full.clone());
        (full, reduced)
    }

    async fn family_plan(fixture: &CohortFixture) -> FamilyPlan {
        let plans = discover(&fixture.pool, &fixture.config, &[])
            .await
            .expect("discover");
        plans
            .into_iter()
            .find(|plan| plan.family.as_str() == FAMILY)
            .expect("family plan")
    }

    async fn family_attempt(fixture: &CohortFixture) -> bool {
        let mut conn = fixture.pool.acquire().await.expect("connection");
        model_store::fit_attempt(&mut conn, &SubjectKey::for_family(&family()))
            .await
            .expect("family attempt")
            .is_some()
    }

    async fn head_version(fixture: &CohortFixture) -> u32 {
        let mut conn = fixture.pool.acquire().await.expect("connection");
        model_store::family_head(&mut conn, &family())
            .await
            .expect("head")
            .expect("family head")
            .target
            .version
            .get()
    }

    #[tokio::test]
    async fn an_admitted_cohort_routes_the_body_into_the_common_family_fit() {
        let fixture = fixture().await;
        insert_head(&fixture.pool, true).await;
        let admission = record_admission(&fixture).await;
        let plan = family_plan(&fixture).await;
        assert_eq!(plan.kind, PlanKind::Qualified);
        assert_eq!(plan.cohort_report, Some(admission));
        let member = plan
            .members
            .iter()
            .find(|member| member.realization == fixture.realization)
            .expect("admitted member enters the family fit");
        assert!(member.admission.is_some());
        assert!(matches!(&member.scope, EvidenceScope::Qualified { .. }));
        assert!(
            plan.nonmembers
                .iter()
                .all(|member| member.realization != fixture.realization),
            "an in-scope admitted body is not also a pinned experimental candidate"
        );
        let head = {
            let mut conn = fixture.pool.acquire().await.expect("connection");
            model_store::family_head(&mut conn, &family())
                .await
                .expect("head")
        }
        .expect("family head");
        assert!(
            head.source_limited,
            "discovery alone does not clear the source limit"
        );
    }

    #[tokio::test]
    async fn an_unadmitted_body_stays_experimental_and_never_joins_the_seed_cohort() {
        let fixture = fixture().await;
        insert_head(&fixture.pool, true).await;
        register_evidence(&fixture.pool, &fixture.realization).await;
        let plan = family_plan(&fixture).await;
        assert_eq!(plan.kind, PlanKind::SeedAdvance);
        assert!(
            plan.members.is_empty(),
            "missing seed membership must not enroll all evidence"
        );
        let nonmember = plan
            .nonmembers
            .iter()
            .find(|member| member.realization == fixture.realization)
            .expect("unadmitted body");
        assert!(matches!(&nonmember.scope, EvidenceScope::Experimental));
        assert!(nonmember.admission.is_none());
        assert!(
            plan.missing
                .iter()
                .any(|reason| reason.contains("no recorded members")),
            "the missing seed cohort is reported: {:?}",
            plan.missing
        );
    }

    #[tokio::test]
    async fn an_inapplicable_admitted_cohort_falls_back_to_the_seed_cohort() {
        let fixture = fixture().await;
        insert_head(&fixture.pool, false).await;
        record_seed_cohort(&fixture.pool).await;
        record_admitted_cohort_without_admission(&fixture.pool).await;
        register_evidence(&fixture.pool, &fixture.realization).await;
        let plan = family_plan(&fixture).await;
        assert_eq!(plan.kind, PlanKind::SeedAdvance);
        let member = plan
            .members
            .iter()
            .find(|member| member.realization == fixture.realization)
            .expect("seed member");
        assert!(matches!(&member.scope, EvidenceScope::Seed));
        assert!(
            plan.missing
                .iter()
                .any(|reason| reason.contains("not applicable")),
            "the inapplicable admission is reported: {:?}",
            plan.missing
        );
    }

    #[tokio::test]
    async fn out_of_scope_admitted_evidence_gets_the_experimental_pinned_path() {
        let fixture = fixture().await;
        insert_head(&fixture.pool, true).await;
        record_admission(&fixture).await;
        insert_out_of_scope_contribution(&fixture).await;
        let experimental = crate::pool::build(
            &fixture.pool,
            &fixture.config,
            &body(),
            &LookSlug::new(FAMILY).expect("look"),
            &fixture.realization,
            &EvidenceScope::Experimental,
        )
        .await
        .expect("experimental pool");
        assert!(
            experimental
                .images
                .iter()
                .any(|image| image.role == EvidenceRole::Native && image.is_eligible()),
            "the out-of-scope observation must stay eligible experimentally: {:?}",
            experimental
                .images
                .iter()
                .map(|image| &image.eligibility)
                .collect::<Vec<_>>()
        );
        let plan = family_plan(&fixture).await;
        assert_eq!(plan.kind, PlanKind::Qualified);
        assert!(
            plan.members.iter().any(|member| {
                member.realization == fixture.realization
                    && matches!(&member.scope, EvidenceScope::Qualified { .. })
            }),
            "the admitted body still enters the common family fit"
        );
        let nonmember = plan
            .nonmembers
            .iter()
            .find(|member| member.realization == fixture.realization)
            .expect("out-of-scope evidence is a pinned candidate");
        assert!(matches!(&nonmember.scope, EvidenceScope::Experimental));
        assert!(
            nonmember.admission.is_some(),
            "the pinned candidate keeps the admitted authorization"
        );
    }

    #[tokio::test]
    async fn a_missing_seed_cohort_is_reported_as_a_refit_blocker() {
        let fixture = fixture().await;
        insert_head(&fixture.pool, true).await;
        register_evidence(&fixture.pool, &fixture.realization).await;
        std::fs::remove_file(fixture.config.transforms.join("fujifilm-x-t5.yaml"))
            .expect("remove transform");
        let ownership = JobOwnership {
            id: JobId::new(),
            attempts: 1,
        };
        let outcomes = run(&fixture.pool, &fixture.config, &[], &ownership)
            .await
            .expect("fit-free run");
        assert!(
            outcomes.iter().any(|(_, _, outcome)| matches!(
                outcome,
                RefitOutcome::Unavailable(reason) if reason.contains("no recorded members")
            )),
            "the refit reports the missing seed cohort: {outcomes:?}"
        );
    }

    async fn insert_target_manifest(fixture: &CohortFixture) {
        let document = LookDocument::from_look(
            identity(family().as_str(), "1.0.0").expect("identity"),
            Look::colorimetric(),
        )
        .expect("look document");
        let look_artifact = store_artifact(
            &fixture.config,
            LOOK_ARTIFACT,
            &document.encode().expect("look bytes"),
        )
        .expect("look artifact");
        let release = ReleaseRecord {
            version: 1,
            gauge: 1.0,
            gauge_convention: GaugeConvention::Relative,
            anchor: 1,
            clip_k: 0.03,
            output_space: looklab_fit::RgbSpace::Srgb,
            probes: Vec::new(),
            gated_since_anchor: GatedUnion::default(),
            converged: true,
            gate: Some(gate_release(&TrustParams::default(), &GateInput::default())),
            approximation_p95: None,
            provenance: ProvenanceSummary::default(),
            created_at: 0,
        };
        let release_artifact = store_artifact(
            &fixture.config,
            RELEASE_ARTIFACT,
            &serde_json::to_vec(&release).expect("release"),
        )
        .expect("release artifact");
        let manifest = serde_json::to_string(&[look_artifact, release_artifact]).expect("manifest");
        sqlx::query(
            "INSERT INTO version_manifests (look_version_id, artifacts, created_at)
             VALUES ('classic-chrome-1', ?, 0)",
        )
        .bind(&manifest)
        .execute(&fixture.pool)
        .await
        .expect("manifest");
    }

    /// Mirrors the standalone fit's pool, partition and root inputs for its cache.
    async fn standalone_cache(
        fixture: &CohortFixture,
        plan: &FamilyPlan,
    ) -> (Vec<(ScopeMember, LookPool)>, Vec<LookPool>, RootData) {
        let mut member_pools = Vec::new();
        for member in &plan.members {
            let pool = crate::pool::build(
                &fixture.pool,
                &fixture.config,
                &member.body,
                &plan.family,
                &member.realization,
                &member.scope,
            )
            .await
            .expect("member pool");
            if !pool.images.is_empty() {
                member_pools.push((member.clone(), pool));
            }
        }
        let mut pools: Vec<LookPool> = member_pools.iter().map(|(_, pool)| pool.clone()).collect();
        crate::pool::resolve_contributors(&mut pools).expect("contributors");
        for (index, (_, pool)) in member_pools.iter_mut().enumerate() {
            *pool = pools[index].clone();
        }
        let scores = load_consistency_scores(&fixture.pool, &member_pools)
            .await
            .expect("scores");
        let subject = SubjectKey::for_family(&plan.family_id);
        let mut training_pools = Vec::new();
        for (_, pool) in &member_pools {
            training_pools.push(partition_pool(pool, &subject).expect("partition").training);
        }
        let roots = roots::load_family(&training_pools, &scores).expect("roots");
        (member_pools, training_pools, roots)
    }

    /// Persists the standalone family fingerprint the worker compares before a fit.
    async fn persist_family_cache(fixture: &CohortFixture, plan: &FamilyPlan) {
        let (member_pools, training_pools, roots) = standalone_cache(fixture, plan).await;
        let fingerprint = standalone_family_fingerprint(
            plan,
            &member_pools,
            &training_pools,
            &roots,
            &fit_options(&fixture.config),
        )
        .expect("family fingerprint");
        let mut conn = fixture.pool.acquire().await.expect("connection");
        model_store::record_pool_fingerprint(
            &mut conn,
            &SubjectKey::for_family(&plan.family_id),
            &fingerprint,
            0,
        )
        .await
        .expect("family fingerprint");
    }

    fn member_for(plan: &FamilyPlan, realization: &BodyRealizationId) -> ScopeMember {
        plan.members
            .iter()
            .find(|member| member.realization == *realization)
            .expect("member for realization")
            .clone()
    }

    #[tokio::test]
    async fn admission_without_new_images_does_not_skip_the_qualified_solve() {
        let mut fixture = fixture().await;
        fixture.config.sweep_budget = Some(1.0);
        insert_head(&fixture.pool, true).await;
        insert_target_manifest(&fixture).await;
        record_seed_cohort(&fixture.pool).await;
        let (full, _) = insert_in_scope_contribution(&fixture, &[0, 1]).await;
        let seed_plan = family_plan(&fixture).await;
        assert_eq!(seed_plan.kind, PlanKind::SeedAdvance);
        persist_family_cache(&fixture, &seed_plan).await;

        record_admission_with_mask(&fixture, full).await;
        let qualified_plan = family_plan(&fixture).await;
        assert_eq!(qualified_plan.kind, PlanKind::Qualified);

        let ownership = JobOwnership {
            id: JobId::new(),
            attempts: 1,
        };
        assert!(!family_attempt(&fixture).await);
        let outcomes = run(&fixture.pool, &fixture.config, &[], &ownership)
            .await
            .expect("run");
        assert!(
            family_attempt(&fixture).await,
            "admission without new images must start a canonical family attempt: {outcomes:?}"
        );
        assert_eq!(head_version(&fixture).await, 1);
    }

    #[tokio::test]
    async fn an_effective_admitted_mask_change_does_not_skip_the_qualified_solve() {
        let mut fixture = fixture().await;
        fixture.config.sweep_budget = Some(1.0);
        insert_head(&fixture.pool, true).await;
        insert_target_manifest(&fixture).await;
        record_seed_cohort(&fixture.pool).await;
        let (full, reduced) = insert_in_scope_contribution(&fixture, &[0, 1]).await;
        record_admission_with_mask(&fixture, full).await;
        let qualified_plan = family_plan(&fixture).await;
        assert_eq!(qualified_plan.kind, PlanKind::Qualified);
        persist_family_cache(&fixture, &qualified_plan).await;

        record_admission_with_mask(&fixture, reduced).await;
        let changed_plan = family_plan(&fixture).await;
        assert_eq!(changed_plan.kind, PlanKind::Qualified);

        let ownership = JobOwnership {
            id: JobId::new(),
            attempts: 1,
        };
        assert!(!family_attempt(&fixture).await);
        let outcomes = run(&fixture.pool, &fixture.config, &[], &ownership)
            .await
            .expect("run");
        assert!(
            family_attempt(&fixture).await,
            "an effective admitted-mask change must start a canonical family attempt: {outcomes:?}"
        );
        assert_eq!(head_version(&fixture).await, 1);
    }

    #[tokio::test]
    async fn exclusively_experimental_evidence_preserves_the_qualified_skip() {
        let mut fixture = fixture().await;
        fixture.config.sweep_budget = Some(1.0);
        insert_head(&fixture.pool, true).await;
        insert_target_manifest(&fixture).await;
        record_seed_cohort(&fixture.pool).await;
        let (full, _) = insert_in_scope_contribution(&fixture, &[0, 1]).await;
        record_admission_with_mask(&fixture, full).await;
        let qualified_plan = family_plan(&fixture).await;
        assert_eq!(qualified_plan.kind, PlanKind::Qualified);
        persist_family_cache(&fixture, &qualified_plan).await;
        assert!(!family_attempt(&fixture).await);

        let ownership = JobOwnership {
            id: JobId::new(),
            attempts: 1,
        };
        let baseline = run(&fixture.pool, &fixture.config, &[], &ownership)
            .await
            .expect("baseline run");
        assert!(
            !family_attempt(&fixture).await,
            "the unchanged qualified cache must skip the family fit: {baseline:?}"
        );
        assert_eq!(head_version(&fixture).await, 1);

        insert_out_of_scope_for(&fixture, &second_body()).await;
        let changed_plan = family_plan(&fixture).await;
        assert_eq!(changed_plan.kind, PlanKind::Qualified);
        let second_ownership = JobOwnership {
            id: JobId::new(),
            attempts: 1,
        };
        let outcomes = run(&fixture.pool, &fixture.config, &[], &second_ownership)
            .await
            .expect("run");
        assert!(
            !family_attempt(&fixture).await,
            "exclusively experimental evidence must preserve the family skip: {outcomes:?}"
        );
        assert_eq!(head_version(&fixture).await, 1);
    }

    #[tokio::test]
    async fn a_noncontributing_member_does_not_supply_the_common_gauge() {
        let fixture = fixture().await;
        insert_head(&fixture.pool, true).await;
        let (full, _) = insert_in_scope_contribution(&fixture, &[0, 1]).await;
        record_admission_with_mask(&fixture, full).await;
        let plan = family_plan(&fixture).await;
        assert_eq!(plan.kind, PlanKind::Qualified);
        let contributing = member_for(&plan, &fixture.realization);
        let empty = member_for(&plan, &second_realization());

        let mut contributing_pool = crate::pool::build(
            &fixture.pool,
            &fixture.config,
            &contributing.body,
            &plan.family,
            &contributing.realization,
            &contributing.scope,
        )
        .await
        .expect("contributing pool");
        crate::pool::resolve_contributors(std::slice::from_mut(&mut contributing_pool))
            .expect("contributors");
        assert!(
            contributing_pool
                .images
                .iter()
                .any(|image| image.is_eligible()),
            "the contributing member must have eligible native images"
        );
        let empty_pool = crate::pool::build(
            &fixture.pool,
            &fixture.config,
            &empty.body,
            &plan.family,
            &empty.realization,
            &empty.scope,
        )
        .await
        .expect("empty pool");
        assert!(
            empty_pool.images.is_empty(),
            "the noncontributing member has no evidence rows"
        );

        let members = [
            (empty, empty_pool),
            (contributing.clone(), contributing_pool),
        ];
        let participating =
            BTreeSet::from([(contributing.body.clone(), contributing.realization.clone())]);
        let lineage = load_lineage(&fixture.pool, &fixture.config, &members, &participating)
            .await
            .expect("lineage");
        assert!(lineage.authoritative_missing.is_empty());
        assert_eq!(
            lineage
                .gauge
                .as_ref()
                .expect("gauge")
                .gauge
                .designated
                .as_str(),
            contributing.body.as_str(),
            "a noncontributing member must not supply the common gauge"
        );
    }

    #[tokio::test]
    async fn a_pinned_conditional_reads_its_saved_reference_state() {
        let fixture = fixture().await;
        insert_head(&fixture.pool, true).await;
        let (full, _) = insert_in_scope_contribution(&fixture, &[0, 1]).await;
        record_admission_with_mask(&fixture, full).await;
        let plan = family_plan(&fixture).await;
        let empty = member_for(&plan, &second_realization());
        let empty_pool = crate::pool::build(
            &fixture.pool,
            &fixture.config,
            &empty.body,
            &plan.family,
            &empty.realization,
            &empty.scope,
        )
        .await
        .expect("empty pool");
        assert!(
            empty_pool.images.is_empty(),
            "the pinned conditional has no evidence rows"
        );

        let members = [(empty.clone(), empty_pool.clone())];
        let pinned = BTreeSet::from([(empty.body.clone(), empty.realization.clone())]);
        let pinned_lineage = load_lineage(&fixture.pool, &fixture.config, &members, &pinned)
            .await
            .expect("pinned lineage");
        assert!(pinned_lineage.authoritative_missing.is_empty());
        assert_eq!(
            pinned_lineage.firmware,
            vec![(saved_firmware(&empty.body), [0.1, 0.01])],
            "a pinned conditional reads its saved firmware offsets"
        );
        assert_eq!(
            pinned_lineage.sigma,
            saved_sigma(),
            "a pinned conditional reads its saved contributor variances"
        );
        assert!(
            pinned_lineage
                .realization_warm
                .contains_key(&empty.realization)
        );
        assert!(pinned_lineage.body_state.contains_key(&empty.body));

        let options = hier_options(
            &fixture.config,
            &pinned_lineage,
            &[],
            None,
            Vec::new(),
            BodyPin::Free,
        );
        assert_eq!(
            options.firmware,
            vec![(saved_firmware(&empty.body), [0.1, 0.01])],
            "the saved reference state reaches the pinned fit options"
        );

        let common = load_lineage(&fixture.pool, &fixture.config, &members, &BTreeSet::new())
            .await
            .expect("common lineage");
        assert!(common.firmware.is_empty());
        assert!(common.sigma.is_empty());
        assert!(common.realization_warm.contains_key(&empty.realization));
        assert!(common.body_state.contains_key(&empty.body));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject() -> SubjectKey {
        SubjectKey::for_family(&LookId::new("classic-chrome").expect("family"))
    }

    #[tokio::test]
    async fn no_data_pending_is_drained_and_a_new_arrival_queues_a_successor() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = dir.path().join("looklab.sqlite3");
        let pool = looklab_server::open_database(&db).await.expect("database");
        let subject = subject();
        {
            let mut conn = pool.acquire().await.expect("connection");
            model_store::note_evidence_arrival(&mut conn, &subject, 0)
                .await
                .expect("pending arrival");
        }
        let config = WorkerConfig::new(&db, dir.path().join("blobs"));
        let ownership = JobOwnership {
            id: JobId::new(),
            attempts: 1,
        };
        run(&pool, &config, &[], &ownership)
            .await
            .expect("no-data run");
        assert!(
            enqueue_pending(&pool, &config, 1)
                .await
                .expect("enqueue")
                .is_none(),
            "a no-data generation must be consumed"
        );
        {
            let mut conn = pool.acquire().await.expect("connection");
            model_store::note_evidence_arrival(&mut conn, &subject, 2)
                .await
                .expect("successor arrival");
        }
        assert!(
            enqueue_pending(&pool, &config, 3)
                .await
                .expect("enqueue")
                .is_some(),
            "a later arrival must queue a successor"
        );
    }

    fn group(token: &str) -> EvidenceGroupId {
        EvidenceGroupId::new(token).expect("group")
    }

    async fn pending_fixture() -> (tempfile::TempDir, SqlitePool, WorkerConfig) {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = dir.path().join("looklab.sqlite3");
        let pool = looklab_server::open_database(&db).await.expect("database");
        let config = WorkerConfig::new(&db, dir.path().join("blobs"));
        (dir, pool, config)
    }

    async fn queued_refits(pool: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT payload FROM jobs WHERE kind = 'nightly_refit' AND status = 'queued'",
        )
        .fetch_all(pool)
        .await
        .expect("queued refits")
    }

    #[tokio::test]
    async fn concurrent_pending_enqueues_one_all_body_successor() {
        let (_dir, pool, config) = pending_fixture().await;
        {
            let mut conn = pool.acquire().await.expect("connection");
            model_store::note_evidence_arrival(&mut conn, &subject(), 0)
                .await
                .expect("pending");
        }
        let (left, right) = tokio::join!(
            enqueue_pending(&pool, &config, 1),
            enqueue_pending(&pool, &config, 1)
        );
        let returned = usize::from(left.expect("left").is_some())
            + usize::from(right.expect("right").is_some());
        assert_eq!(returned, 1);
        let queued = queued_refits(&pool).await;
        assert_eq!(queued.len(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&queued[0])
                .expect("payload")
                .get("bodies")
                .and_then(|bodies| bodies.as_array())
                .map(Vec::len),
            Some(0)
        );
    }

    #[tokio::test]
    async fn a_running_refit_permits_one_queued_successor() {
        let (_dir, pool, config) = pending_fixture().await;
        {
            let mut conn = pool.acquire().await.expect("connection");
            model_store::note_evidence_arrival(&mut conn, &subject(), 0)
                .await
                .expect("pending");
        }
        let job = crate::jobs::enqueue(&pool, &JobPayload::NightlyRefit { bodies: Vec::new() }, 0)
            .await
            .expect("running job");
        sqlx::query("UPDATE jobs SET status = 'running' WHERE id = ?")
            .bind(job.as_str())
            .execute(&pool)
            .await
            .expect("running");
        assert!(
            enqueue_pending(&pool, &config, 1)
                .await
                .expect("enqueue")
                .is_some()
        );
        assert_eq!(queued_refits(&pool).await.len(), 1);
    }

    #[tokio::test]
    async fn a_narrow_queued_refit_does_not_suppress_another_body() {
        let (_dir, pool, config) = pending_fixture().await;
        let body = UploadBodyId::new("fujifilm/x-t5").expect("body");
        let realization = BodyRealizationId {
            body: body.clone(),
            epoch: ControlEpochId::new(1),
            native_sim: looklab_schema::SimKey::new("classic_chrome").expect("sim"),
        };
        let subject = ModelSubject::BodyRealization { realization }.key();
        {
            let mut conn = pool.acquire().await.expect("connection");
            model_store::note_evidence_arrival(&mut conn, &subject, 0)
                .await
                .expect("pending");
        }
        crate::jobs::enqueue(
            &pool,
            &JobPayload::NightlyRefit {
                bodies: vec![UploadBodyId::new("sony/a7").expect("other")],
            },
            0,
        )
        .await
        .expect("narrow job");
        assert!(
            enqueue_pending(&pool, &config, 1)
                .await
                .expect("enqueue")
                .is_some()
        );
        let queued = queued_refits(&pool).await;
        assert_eq!(queued.len(), 2);
        assert!(queued.iter().any(|payload| {
            serde_json::from_str::<serde_json::Value>(payload)
                .ok()
                .and_then(|value| {
                    value
                        .get("bodies")
                        .and_then(|bodies| bodies.as_array())
                        .map(Vec::len)
                })
                == Some(0)
        }));
    }

    #[tokio::test]
    async fn a_body_scoped_refit_leaves_another_bodys_pending_evidence() {
        let (_dir, pool, config) = pending_fixture().await;
        let examined = UploadBodyId::new("fujifilm/x-t5").expect("body");
        let unvisited = BodyRealizationId {
            body: UploadBodyId::new("sony/a7").expect("body"),
            epoch: ControlEpochId::new(1),
            native_sim: looklab_schema::SimKey::new("classic_chrome").expect("sim"),
        };
        let visited_subject = ModelSubject::BodyRealization {
            realization: BodyRealizationId {
                body: examined.clone(),
                epoch: ControlEpochId::new(99),
                native_sim: looklab_schema::SimKey::new("classic_chrome").expect("sim"),
            },
        }
        .key();
        let family_subject =
            SubjectKey::for_family(&LookId::new("classic-chrome").expect("family"));
        let subject = ModelSubject::BodyRealization {
            realization: unvisited,
        }
        .key();
        {
            let mut conn = pool.acquire().await.expect("connection");
            for pending in [&subject, &visited_subject, &family_subject] {
                model_store::note_evidence_arrival(&mut conn, pending, 0)
                    .await
                    .expect("pending arrival");
            }
        }
        let ownership = JobOwnership {
            id: JobId::new(),
            attempts: 1,
        };
        run(&pool, &config, std::slice::from_ref(&examined), &ownership)
            .await
            .expect("scoped run");
        assert!(
            enqueue_pending(&pool, &config, 1)
                .await
                .expect("enqueue")
                .is_some(),
            "an unvisited body's evidence must still queue a successor"
        );
        for (subject, consumed) in [
            (&visited_subject, true),
            (&subject, false),
            (&family_subject, false),
        ] {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM model_pending_consumption WHERE subject_key = ?",
            )
            .bind(subject.as_str())
            .fetch_one(&pool)
            .await
            .expect("consumption");
            assert_eq!(
                count > 0,
                consumed,
                "scoped acknowledgment must only consume examined subjects"
            );
        }
    }

    #[tokio::test]
    async fn a_fitting_window_arrival_stays_pending_and_queues_a_successor() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = dir.path().join("looklab.sqlite3");
        let pool = looklab_server::open_database(&db).await.expect("database");
        let mut config = WorkerConfig::new(&db, dir.path().join("blobs"));
        config.data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
        let transforms = dir.path().join("transforms");
        std::fs::create_dir_all(&transforms).expect("transform dir");
        std::fs::write(
            transforms.join("fujifilm-x-t5.yaml"),
            "format: looklab-transform/1
version: 1
camera_to_xyz:
  - [1.0, 0.0, 0.0]
  - [0.0, 1.0, 0.0]
  - [0.0, 0.0, 1.0]
",
        )
        .expect("transform file");
        config.transforms = transforms;

        let body = UploadBodyId::new("fujifilm/x-t5").expect("body");
        let examined = ModelSubject::BodyRealization {
            realization: BodyRealizationId {
                body: body.clone(),
                epoch: ControlEpochId::new(1),
                native_sim: looklab_schema::SimKey::new("classic_chrome").expect("sim"),
            },
        }
        .key();
        let unvisited = ModelSubject::BodyRealization {
            realization: BodyRealizationId {
                body: UploadBodyId::new("sony/a7").expect("body"),
                epoch: ControlEpochId::new(1),
                native_sim: looklab_schema::SimKey::new("classic_chrome").expect("sim"),
            },
        }
        .key();
        let unplanned = ModelSubject::BodyRealization {
            realization: BodyRealizationId {
                body: body.clone(),
                epoch: ControlEpochId::new(99),
                native_sim: looklab_schema::SimKey::new("classic_chrome").expect("sim"),
            },
        }
        .key();
        {
            let mut conn = pool.acquire().await.expect("connection");
            for subject in [&examined, &unvisited, &unplanned] {
                model_store::note_evidence_arrival(&mut conn, subject, 0)
                    .await
                    .expect("pending arrival");
            }
        }
        // The fit registers its dependency snapshot after observing its inputs.
        let trigger = format!(
            "CREATE TRIGGER arrival_during_fit AFTER INSERT ON model_dependency_generations
             WHEN NEW.kind = 'schema'
             BEGIN
               INSERT INTO model_pending_work (subject_key, generation, updated_at)
               VALUES ('{}', 1, 0), ('{}', 1, 0)
               ON CONFLICT (subject_key) DO UPDATE
               SET generation = model_pending_work.generation + 1,
                   updated_at = excluded.updated_at;
             END",
            examined.as_str(),
            unplanned.as_str()
        );
        sqlx::query(&trigger)
            .execute(&pool)
            .await
            .expect("arrival trigger");

        let ownership = JobOwnership {
            id: JobId::new(),
            attempts: 1,
        };
        let outcomes = run(&pool, &config, std::slice::from_ref(&body), &ownership)
            .await
            .expect("fit-free run");
        assert_eq!(outcomes.len(), 1);
        assert!(
            matches!(&outcomes[0].2, RefitOutcome::Unavailable(_)),
            "the fixture must stop before a numerical fit"
        );

        for subject in [&examined, &unplanned] {
            let (pending, consumed): (i64, i64) = sqlx::query_as(
                "SELECT p.generation, COALESCE(c.generation, 0)
                 FROM model_pending_work p
                 LEFT JOIN model_pending_consumption c ON c.subject_key = p.subject_key
                 WHERE p.subject_key = ?",
            )
            .bind(subject.as_str())
            .fetch_one(&pool)
            .await
            .expect("examined pending");
            assert!(
                consumed >= 1,
                "the generation observed before fitting must be consumed"
            );
            assert!(
                pending > consumed,
                "an arrival during fitting must stay pending"
            );
        }
        assert!(
            enqueue_pending(&pool, &config, 1)
                .await
                .expect("enqueue")
                .is_some(),
            "an arrival during fitting must queue a successor"
        );
        let unvisited_consumed: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM model_pending_consumption WHERE subject_key = ?",
        )
        .bind(unvisited.as_str())
        .fetch_one(&pool)
        .await
        .expect("unvisited consumption");
        assert_eq!(
            unvisited_consumed, 0,
            "a body-scoped run must not consume an unvisited body's arrival"
        );
    }

    #[test]
    fn partition_is_deterministic_and_leaves_a_training_set() {
        let subject = subject();
        let groups: Vec<EvidenceGroupId> = (0..12)
            .map(|index| group(&format!("image-{index}")))
            .collect();
        let first = assessment_partition(&subject, &groups);
        let second = assessment_partition(&subject, &groups);
        assert_eq!(first, second);
        assert!(first.len() < groups.len());
        assert_eq!(
            partition_policy_digest(&subject, &first),
            partition_policy_digest(&subject, &second)
        );
    }

    #[test]
    fn a_single_group_is_never_held_out() {
        let subject = subject();
        let groups = vec![group("image-0")];
        assert!(assessment_partition(&subject, &groups).is_empty());
    }

    fn reputation_image<'a>(
        credential: &'a CredentialId,
        role: EvidenceRole,
        eligible: bool,
        chart: bool,
        contribution: &'a ContributionId,
        outcome: Option<ImageOutcome>,
    ) -> ReputationImage<'a> {
        ReputationImage {
            role,
            eligible,
            chart,
            contribution,
            credential,
            outcome,
        }
    }

    #[test]
    fn native_qualified_evidence_updates_reputation_counts() {
        let credential =
            CredentialId::try_from("00000000-0000-0000-0000-0000000000a1").expect("credential");
        let contribution =
            ContributionId::try_from("00000000-0000-0000-0000-0000000000b1").expect("contribution");
        let counts = reputation_counts([
            reputation_image(
                &credential,
                EvidenceRole::Native,
                true,
                false,
                &contribution,
                Some(ImageOutcome::Accepted),
            ),
            reputation_image(
                &credential,
                EvidenceRole::Native,
                true,
                false,
                &contribution,
                Some(ImageOutcome::Quarantined),
            ),
        ]);
        let counts = counts.get(&credential).expect("credential counts");
        assert_eq!(
            (counts.total, counts.accepted, counts.quarantined),
            (2, 1, 1)
        );
    }

    #[test]
    fn recipe_and_ineligible_evidence_leave_reputation_unchanged() {
        let credential =
            CredentialId::try_from("00000000-0000-0000-0000-0000000000a2").expect("credential");
        let contribution =
            ContributionId::try_from("00000000-0000-0000-0000-0000000000b2").expect("contribution");
        let counts = reputation_counts([
            reputation_image(
                &credential,
                EvidenceRole::Recipe,
                true,
                false,
                &contribution,
                Some(ImageOutcome::Accepted),
            ),
            reputation_image(
                &credential,
                EvidenceRole::Native,
                false,
                false,
                &contribution,
                Some(ImageOutcome::Accepted),
            ),
        ]);
        assert!(
            counts
                .get(&credential)
                .is_none_or(|counts| counts.total == 0)
        );
    }

    #[test]
    fn recipe_siblings_do_not_change_a_native_chart_pass() {
        let credential =
            CredentialId::try_from("00000000-0000-0000-0000-0000000000a3").expect("credential");
        let contribution =
            ContributionId::try_from("00000000-0000-0000-0000-0000000000b3").expect("contribution");
        let counts = reputation_counts([
            reputation_image(
                &credential,
                EvidenceRole::Native,
                true,
                true,
                &contribution,
                Some(ImageOutcome::Accepted),
            ),
            reputation_image(
                &credential,
                EvidenceRole::Recipe,
                true,
                true,
                &contribution,
                Some(ImageOutcome::Quarantined),
            ),
        ]);
        let counts = counts.get(&credential).expect("credential counts");
        assert_eq!(
            (counts.total, counts.accepted, counts.passing_charts),
            (1, 1, 1)
        );
    }

    #[test]
    fn wb_classes_are_keyed_by_the_enum_value() {
        let schema = looklab_schema::validate_control_schema(
            "body: fujifilm/x-t5
epochs:
  - id: 1
    firmware: { from: '1.00', to: '1.99' }
    native_sims:
      - key: classic_chrome
        look: classic-chrome
        makernote: { tag: FilmMode, value: 0x0600 }
    exposure_modes:
      - key: dr100
    controls:
      - key: white_balance
        type: enum
        values: [daylight, auto]
        default: auto
        look_affecting: true
        affects: [wb]
    solve_plan: []
",
        )
        .expect("schema");
        let epoch = &schema.epochs[0];
        let modes = mode_maps(&[epoch], &BTreeSet::new());
        assert_eq!(modes.wb.len(), 2);
        assert_eq!(
            modes.wb.get(&WbModeClass(1)).map(WbModeKey::as_str),
            Some("daylight")
        );
    }

    const NATIVE_BODY: &str = "fujifilm/x-t5";
    const NATIVE_FIRMWARE: &str = "1.00";
    const NATIVE_WB: &str = "Daylight";
    const NATIVE_PINNED_EXPOSURE_EV: f64 = 1.0;
    const NATIVE_CLIP_K: f64 = 0.03;

    struct NativeEvidence {
        membership: EvidenceMembership,
        default_combination: looklab_schema::SettingCombination,
        nondefault_combination: looklab_schema::SettingCombination,
        firmware: looklab_schema::FirmwareId,
        wb_mode: WbModeKey,
    }

    fn native_probe_indices(look: &Look) -> BTreeSet<usize> {
        use looklab_fit::model_error::{
            MODEL_C_BINS, MODEL_S_BINS, MODEL_S_MAX, MODEL_S_MIN, region,
        };
        use looklab_model::basis::C_MAX;
        let probes = looklab_model::probes::v1();
        let space = looklab_color::RgbSpace::Srgb;
        let color_width = C_MAX / MODEL_C_BINS as f64;
        let s_span = (MODEL_S_MAX - MODEL_S_MIN) / MODEL_S_BINS as f64;
        let mut best: Option<(usize, f64)> = None;
        for (index, probe) in probes.probes.iter().enumerate() {
            let input = looklab_model::LookInput::new(probe.xyz);
            if !(input.s.is_finite() && input.h_in.is_finite() && input.c_in.is_finite()) {
                continue;
            }
            if input.c_in <= 0.02 {
                continue;
            }
            // The fail case observes the same scene at zero offset; both exposures stay in one cell.
            let shifted = input.s + NATIVE_PINNED_EXPOSURE_EV;
            if region(input.s, input.h_in) != region(shifted, input.h_in) {
                continue;
            }
            let bin = ((input.s - MODEL_S_MIN) / s_span).floor();
            let lower = MODEL_S_MIN + bin * s_span;
            let upper = lower + s_span;
            let s_margin = [input.s, shifted]
                .into_iter()
                .map(|s| (s - lower).min(upper - s))
                .fold(f64::INFINITY, f64::min);
            let hue_within = input.h_in.rem_euclid(30.0);
            let hue_margin = hue_within.min(30.0 - hue_within);
            let color_within = input.c_in.rem_euclid(color_width);
            let color_margin = color_within.min(color_width - color_within);
            let clip_margin = native_preclip_rgb(look, probe.xyz, space)
                .into_iter()
                .map(|channel| (channel - NATIVE_CLIP_K).min((1.0 - NATIVE_CLIP_K) - channel))
                .fold(f64::INFINITY, f64::min);
            if s_margin < 0.5 || hue_margin < 5.0 || color_margin < 5.0 || clip_margin < 0.05 {
                continue;
            }
            let margin = hue_margin.min(color_margin);
            if best.is_none_or(|(_, best_margin)| margin > best_margin) {
                best = Some((index, margin));
            }
        }
        let (index, _) = best.expect(
            "the v1 probe set needs a chromatic probe inside one cell under the pinned exposure offset",
        );
        BTreeSet::from([index])
    }

    fn native_release_probes(selected: &BTreeSet<usize>) -> Vec<StoredProbe> {
        looklab_model::probes::v1()
            .probes
            .iter()
            .enumerate()
            .map(|(index, probe)| StoredProbe {
                id: probe.id.clone(),
                sigma: 0.5,
                sigma_post: 0.5,
                bias_de: 0.0,
                pre_clip_identified: true,
                support: if selected.contains(&index) {
                    TrustProbeSupport {
                        sigma: true,
                        density: true,
                        clusters: true,
                        bias: true,
                    }
                } else {
                    TrustProbeSupport::default()
                },
            })
            .collect()
    }

    fn native_modes() -> BodyModes {
        let mut modes = BodyModes::default();
        modes.exposure.insert(
            ExposureModeId(0),
            ExposureModeKey::new("dr100").expect("exposure mode"),
        );
        modes
    }

    fn native_default_settings(epoch: &Epoch) -> Settings {
        let mut settings = Settings::new();
        for control in &epoch.controls {
            settings.set(
                control.key.clone(),
                crate::assessment::default_value(control),
            );
        }
        settings
    }

    fn native_wire_settings(epoch: &Epoch, settings: &Settings) -> String {
        use looklab_upload::{ControlEntry, ControlSettings, SettingSource, StepIndex};
        let entries = epoch
            .controls
            .iter()
            .filter_map(|control| {
                let value = settings.get(&control.key)?;
                let step = match value {
                    ControlValue::Step(step) => *step,
                    ControlValue::Enum(name) => control
                        .values
                        .as_ref()?
                        .iter()
                        .position(|candidate| candidate == name)?
                        as i64,
                    ControlValue::Bool(value) => i64::from(*value),
                };
                Some(ControlEntry::new(
                    control.key.clone(),
                    StepIndex::new(step),
                    SettingSource::Reproduced,
                ))
            })
            .collect();
        serde_json::to_string(&ControlSettings::new(entries).expect("settings"))
            .expect("settings json")
    }

    fn native_metadata(n: usize) -> looklab_upload::ManifestMetadata {
        use looklab_upload::{LookSettings, ManifestMetadata, Tagged, WhiteBalanceMode};
        ManifestMetadata {
            make: Some("Fujifilm".to_owned()),
            model: Some("X-T5".to_owned()),
            firmware: Some(NATIVE_FIRMWARE.to_owned()),
            lens_model: None,
            focal_length_mm: None,
            iso: Some(200),
            shutter_seconds: Some(1.0 / (n as f64 + 1.0)),
            aperture_f_number: Some(2.0),
            exposure_compensation_ev: Some(0.0),
            as_shot_wb: None,
            capture_month: None,
            software: Some("looklab-test".to_owned()),
            render_tag: None,
            render_tag_malformed: false,
            look: Some(LookSettings {
                white_balance: Some(Tagged {
                    raw: 0x0002,
                    value: Some(WhiteBalanceMode::Daylight),
                }),
                ..Default::default()
            }),
        }
    }

    fn native_preclip_rgb(
        look: &Look,
        scene: looklab_color::Xyz,
        space: looklab_color::RgbSpace,
    ) -> [f64; 3] {
        let ucs = look.eval(scene);
        let jmh = looklab_color::ucs::ucs_to_jmh(ucs, looklab_color::ucs::UcsSpace::Ucs);
        let xyz = looklab_model::solve::reference_model().jmh_to_xyz(jmh);
        let rgb = space.xyz_to_rgb_linear(xyz);
        [rgb.r, rgb.g, rgb.b]
    }

    fn native_render(
        look: &Look,
        scene: looklab_color::Xyz,
        space: looklab_color::RgbSpace,
        clip_k: f64,
    ) -> [f32; 3] {
        let ucs = post_clip(look, scene, space, clip_k);
        let jmh = looklab_color::ucs::ucs_to_jmh(ucs, looklab_color::ucs::UcsSpace::Ucs);
        let xyz = looklab_model::solve::reference_model().jmh_to_xyz(jmh);
        let rgb = space.xyz_to_rgb_linear(xyz);
        [rgb.r as f32, rgb.g as f32, rgb.b as f32]
    }

    async fn native_evidence(
        pool: &SqlitePool,
        config: &WorkerConfig,
        epoch: &Epoch,
        look: &Look,
        observed_exposure_ev: f64,
    ) -> NativeEvidence {
        use looklab_samples::{
            InputSpace, OutputSpace, PrepParamsHash, PrepVersion, PriorTransformVersion,
            SampleFlags, SampleHeader, SampleSet,
        };
        let space = looklab_color::RgbSpace::Srgb;
        let selected = native_probe_indices(look);
        let probes = looklab_model::probes::v1();
        let gain = libm::pow(2.0, observed_exposure_ev);
        let mut in_rgb = Vec::new();
        let mut out_rgb = Vec::new();
        for index in &selected {
            let scene = probes.probes[*index].xyz;
            in_rgb.push([
                (scene.x / gain) as f32,
                (scene.y / gain) as f32,
                (scene.z / gain) as f32,
            ]);
            out_rgb.push(native_render(look, scene, space, NATIVE_CLIP_K));
        }
        while in_rgb.len() < 2 {
            let first = in_rgb[0];
            let first_out = out_rgb[0];
            in_rgb.push(first);
            out_rgb.push(first_out);
        }
        let n = in_rgb.len();
        let samples = SampleSet::new(
            SampleHeader {
                n: u32::try_from(n).expect("sample count"),
                input_space: InputSpace::CameraRgbWbLinear,
                output_space: OutputSpace::Srgb,
                wb_multipliers: [1.0, 1.0, 1.0],
                cct_hint_k: 0.0,
                exposure_hint_ev: 0.0,
                rerender_group: None,
                prep_version: PrepVersion::new(1),
                prior_transform_version: PriorTransformVersion::new(1),
                prep_params_hash: PrepParamsHash::new([0; 32]),
            },
            in_rgb,
            out_rgb,
            vec![1.0; n],
            vec![SampleFlags::NONE; n],
        )
        .expect("samples");

        let credential = CredentialId::new();
        sqlx::query(
            "INSERT INTO credentials
             (id, credential_id, webauthn_credential, tier, roles, reputation, created_at, status)
             VALUES (?, ?, '{}', 'new', '[]', 0.5, 0, 'active')",
        )
        .bind(credential.as_str())
        .bind(credential.as_str().as_bytes())
        .execute(pool)
        .await
        .expect("credential");

        let default_settings = native_default_settings(epoch);
        let mut nondefault_settings = default_settings.clone();
        nondefault_settings.set(
            looklab_schema::ControlKey::new("dynamic_range").expect("control"),
            ControlValue::Enum(looklab_schema::EnumValue::new("dr200").expect("dynamic range")),
        );
        nondefault_settings.set(
            looklab_schema::ControlKey::new("color").expect("control"),
            ControlValue::Step(1),
        );
        let default_json = native_wire_settings(epoch, &default_settings);
        let nondefault_json = native_wire_settings(epoch, &nondefault_settings);

        let mut groups = Vec::new();
        for _ in 0..5 {
            let image =
                native_insert_image(pool, config, &credential, &default_json, &samples).await;
            groups.push(EvidenceGroupId::new(image).expect("group"));
        }
        let image =
            native_insert_image(pool, config, &credential, &nondefault_json, &samples).await;
        groups.push(EvidenceGroupId::new(image).expect("group"));

        let membership = EvidenceMembership {
            groups: groups
                .into_iter()
                .map(|group| looklab_server::model_store::EvidenceGroup {
                    group,
                    role: StoredEvidenceRole::Assessment,
                })
                .collect(),
            ..EvidenceMembership::default()
        };
        NativeEvidence {
            membership,
            default_combination: crate::assessment::setting_combination(&default_settings, epoch),
            nondefault_combination: crate::assessment::setting_combination(
                &nondefault_settings,
                epoch,
            ),
            firmware: looklab_schema::FirmwareId {
                body: looklab_schema::BodyId::new(NATIVE_BODY).expect("body"),
                epoch: ControlEpochId::new(1),
                version: NATIVE_FIRMWARE.parse().expect("firmware"),
            },
            wb_mode: WbModeKey::new(NATIVE_WB).expect("wb mode"),
        }
    }

    async fn native_insert_image(
        pool: &SqlitePool,
        config: &WorkerConfig,
        credential: &CredentialId,
        settings: &str,
        samples: &looklab_samples::SampleSet,
    ) -> String {
        let bytes = looklab_samples::encode(samples).expect("encode samples");
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        std::fs::write(config.blobs.join(db::hex(&hash)), &bytes).expect("blob file");
        sqlx::query(
            "INSERT INTO blobs (hash, ref_count) VALUES (?, 1)
             ON CONFLICT(hash) DO UPDATE SET ref_count = ref_count + 1",
        )
        .bind(hash.as_slice())
        .execute(pool)
        .await
        .expect("blob row");
        let contribution = ContributionId::new();
        sqlx::query(
            "INSERT INTO contributions
             (id, credential_id, batch_manifest_hash, signature, pow, kind, serial_hmac, intent,
              target_look_id, body_id, firmware_version, sim_key, settings, status, created_at)
             VALUES (?, ?, x'00', x'00', 'pow', 'library', NULL, 'native', 'classic-chrome', ?, ?, 'classic_chrome', ?, 'pending', 0)",
        )
        .bind(contribution.as_str())
        .bind(credential.as_str())
        .bind(NATIVE_BODY)
        .bind(NATIVE_FIRMWARE)
        .bind(settings)
        .execute(pool)
        .await
        .expect("contribution");
        let image = uuid::Uuid::new_v4().to_string();
        let meta = serde_json::to_string(&native_metadata(samples.n())).expect("metadata");
        sqlx::query(
            "INSERT INTO contribution_images
             (id, contribution_id, samples_hash, n_samples, meta, rerender_group, status)
             VALUES (?, ?, ?, ?, ?, NULL, 'pending')",
        )
        .bind(&image)
        .bind(contribution.as_str())
        .bind(hash.as_slice())
        .bind(i64::try_from(samples.n()).expect("sample count"))
        .bind(meta)
        .execute(pool)
        .await
        .expect("image");
        image
    }

    async fn native_assessment(
        observed_exposure_ev: f64,
    ) -> (
        String,
        looklab_schema::ReportScope,
        String,
        String,
        NativeEvidence,
    ) {
        use looklab_body::{A0Source, BodyId as TransformBodyId, BodyPrior, TransformVersion};
        use looklab_color::Mat3;
        use looklab_fit::RgbSpace;
        use looklab_fit::hierarchical::HierFingerprint;
        use looklab_model::LookDocument;
        use looklab_model::file::identity;
        use looklab_schema::SimKey;
        use looklab_trust::{GateInput, TrustParams, gate_release};

        let dir = tempfile::tempdir().expect("temp dir");
        let db = dir.path().join("looklab.sqlite3");
        let pool = looklab_server::open_database(&db).await.expect("database");
        let mut config = WorkerConfig::new(&db, dir.path().join("blobs"));
        config.data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
        let transforms = dir.path().join("transforms");
        std::fs::create_dir_all(&transforms).expect("transform dir");
        std::fs::create_dir_all(&config.blobs).expect("blob dir");
        std::fs::write(
            transforms.join("fujifilm-x-t5.yaml"),
            "format: looklab-transform/1
version: 1
camera_to_xyz:
  - [1.0, 0.0, 0.0]
  - [0.0, 1.0, 0.0]
  - [0.0, 0.0, 1.0]
",
        )
        .expect("transform file");
        config.transforms = transforms;
        let family = LookId::new("classic-chrome").expect("family");
        let target = FamilyTargetId {
            family: family.clone(),
            version: TargetVersion::new(1).expect("version"),
        };
        let body = UploadBodyId::new(NATIVE_BODY).expect("body");
        let sim = SimKey::new("classic_chrome").expect("sim");
        let realization = BodyRealizationId {
            body: body.clone(),
            epoch: ControlEpochId::new(1),
            native_sim: sim.clone(),
        };
        let resources = crate::body::DataDir::new(config.data.clone())
            .load(&body)
            .expect("body resources");
        let epoch = resources
            .schema
            .epochs
            .iter()
            .find(|epoch| epoch.id == 1)
            .expect("epoch");
        let look = Look::colorimetric();

        let release = ReleaseRecord {
            version: 1,
            gauge: 1.0,
            gauge_convention: GaugeConvention::Relative,
            anchor: 1,
            clip_k: NATIVE_CLIP_K,
            output_space: RgbSpace::Srgb,
            probes: native_release_probes(&native_probe_indices(&look)),
            gated_since_anchor: GatedUnion::default(),
            converged: true,
            gate: Some(gate_release(&TrustParams::default(), &GateInput::default())),
            approximation_p95: None,
            provenance: ProvenanceSummary::default(),
            created_at: 0,
        };
        let release_artifact = store_artifact(
            &config,
            RELEASE_ARTIFACT,
            &serde_json::to_vec(&release).expect("release"),
        )
        .expect("release artifact");
        let manifest = serde_json::to_string(&vec![release_artifact]).expect("manifest");
        {
            let mut conn = pool.acquire().await.expect("connection");
            sqlx::query(
                "INSERT INTO look_versions (id, look, version, status, created_at)
                 VALUES ('classic-chrome-1', 'classic-chrome', 1, 'provisional', 0)",
            )
            .execute(&mut *conn)
            .await
            .expect("look version");
            sqlx::query(
                "INSERT INTO version_manifests (look_version_id, artifacts, created_at)
                 VALUES ('classic-chrome-1', ?, 0)",
            )
            .bind(&manifest)
            .execute(&mut *conn)
            .await
            .expect("manifest");
        }

        let look_bytes = LookDocument::from_look(
            identity(family.as_str(), "1.0.0").expect("identity"),
            look.clone(),
        )
        .expect("look document")
        .encode()
        .expect("look bytes");
        let look_binding =
            store_binding(&config, &look_bytes, ArtifactRole::Look).expect("look binding");

        let transform = InputTransform::prior(
            TransformBodyId::new(NATIVE_BODY).expect("transform body"),
            TransformVersion::new(1),
            BodyPrior {
                source: A0Source::Ssf,
                a0_a: Mat3::IDENTITY,
                a0_d65: Mat3::IDENTITY,
            },
        );
        let params = match_params(
            &body,
            MatchStage::Native,
            &transform,
            &[looklab_fit::ExposureOffset {
                mode: ExposureModeId(0),
                offset: NATIVE_PINNED_EXPOSURE_EV,
            }],
            &[],
            &native_modes(),
        )
        .expect("match params");
        let transform_binding = store_binding(
            &config,
            &serde_json::to_vec(&transform).expect("transform"),
            ArtifactRole::InputTransform,
        )
        .expect("transform binding");
        let params_binding = store_binding(
            &config,
            &serde_json::to_vec(&params).expect("params"),
            ArtifactRole::BodyRealizationParams,
        )
        .expect("params binding");
        let gauge_binding =
            store_binding(&config, b"{}", ArtifactRole::Gauge).expect("gauge binding");
        let gauge_hash = gauge_binding.sha256;

        let evidence = native_evidence(&pool, &config, epoch, &look, observed_exposure_ev).await;

        let mut conn = pool.acquire().await.expect("connection");
        let body_state_version =
            model_store::reserve_body_state(&mut conn, &body, BodyStateStage::Native, 0)
                .await
                .expect("body state reservation");
        model_store::insert_body_state(
            &mut conn,
            &BodyStateRecord {
                body: body.clone(),
                version: body_state_version,
                stage: BodyStateStage::Native,
                pins: BodyStatePins {
                    profile: None,
                    input_transform: transform_binding,
                    body_params: params_binding,
                    gauge: gauge_binding,
                },
                dependencies: Vec::new(),
            },
            &[],
            0,
        )
        .await
        .expect("body state");

        let warm = RealizationWarmStart {
            clip_k: NATIVE_CLIP_K,
            nuisances: Vec::new(),
            damping: 1e-3,
            response: None,
            coefficients: Vec::new(),
        };
        let state = HierState {
            generation: HierStateGeneration::default(),
            fingerprint: HierFingerprint(0),
            outer_iterations: 0,
            bodies: Vec::new(),
            realizations: Vec::new(),
            published_bodies: Vec::new(),
            published_realizations: Vec::new(),
            families: Vec::new(),
            firmware: Vec::new(),
            sigma: Vec::new(),
            body_decrement: None,
            families_certified: false,
            converged: false,
        };
        let member = ScopeMember {
            body: body.clone(),
            realization: realization.clone(),
            scope: EvidenceScope::Seed,
            admission: None,
        };
        let contributors: std::collections::HashMap<looklab_trust::EffectiveContributorId, u32> =
            std::collections::HashMap::new();
        let version = persist_one_revision(
            &mut conn,
            &config,
            &member,
            &target,
            RealizationStage::Native,
            RealizationStatus::Candidate,
            CandidateSafety {
                numerical: SafetyCheck::Passed,
                release_gates: SafetyCheck::Passed,
            },
            body_state_version,
            1,
            gauge_hash,
            std::slice::from_ref(&look_binding),
            &warm,
            None,
            None,
            &state,
            &evidence.membership,
            &native_modes(),
            &[],
            NATIVE_CLIP_K,
            &[],
            &contributors,
            0,
        )
        .await
        .expect("native revision");
        let candidate = looklab_schema::BodyRealizationVersionId {
            realization: realization.clone(),
            version,
            target: target.clone(),
        };
        drop(conn);

        let subject = ModelSubject::BodyRealization {
            realization: realization.clone(),
        };
        crate::model_assessment::enqueue_candidate_pool(
            &pool,
            &subject,
            &ModelVersionId::BodyRealization {
                realization: candidate.clone(),
            },
            crate::clock::now_unix(),
        )
        .await
        .expect("assessment enqueued");
        let claimed = crate::jobs::claim(&pool, 60, crate::clock::now_unix())
            .await
            .expect("claim")
            .expect("queued assessment");
        let JobPayload::ModelAssessment { request } =
            claimed.payload().expect("assessment payload")
        else {
            panic!("the queued job is not a model assessment");
        };
        let ownership = claimed.ownership();
        crate::model_assessment::run(&pool, &config, &request, &ownership)
            .await
            .expect("native assessment runs");
        let report: (String, String, String, String, String) = sqlx::query_as(
            "SELECT capability, outcome, scope, limitations, evidence_requests
             FROM model_reports
             WHERE subject_key = ? ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(subject.key().as_str())
        .fetch_one(&pool)
        .await
        .expect("native report");
        assert_eq!(report.0, "native_prediction");
        let controls: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM model_current_projections
             WHERE subject_key = ? AND capability = 'controls_matching'",
        )
        .bind(subject.key().as_str())
        .fetch_one(&pool)
        .await
        .expect("controls projections");
        assert_eq!(controls, 0);
        let scope: looklab_schema::ReportScope =
            serde_json::from_str(&report.2).expect("report scope");
        (report.1, scope, report.3, report.4, evidence)
    }

    #[tokio::test]
    async fn a_native_assessment_fails_when_the_pinned_exposure_offset_is_not_in_the_evidence() {
        let (outcome, scope, limitations, requests, evidence) = native_assessment(0.0).await;
        assert_eq!(
            outcome, "fail",
            "limitations: {limitations}; evidence requests: {requests}"
        );
        assert_eq!(scope.points.len(), 1);
        assert_eq!(scope.points[0].settings, evidence.default_combination);
        assert!(
            !scope
                .points
                .iter()
                .any(|point| point.settings == evidence.nondefault_combination)
        );
    }

    #[tokio::test]
    async fn a_native_assessment_passes_on_supported_default_evidence_without_a_crm() {
        let (outcome, scope, limitations, requests, evidence) =
            native_assessment(NATIVE_PINNED_EXPOSURE_EV).await;
        assert_eq!(
            outcome, "pass",
            "limitations: {limitations}; evidence requests: {requests}"
        );
        assert_eq!(scope.points.len(), 1);
        assert_eq!(scope.points[0].settings, evidence.default_combination);
        assert_eq!(scope.points[0].firmware, Some(evidence.firmware.clone()));
        assert_eq!(scope.points[0].wb_mode, Some(evidence.wb_mode.clone()));
        assert!(
            !scope
                .points
                .iter()
                .any(|point| point.settings == evidence.nondefault_combination)
        );
    }

    #[cfg(feature = "integration")]
    fn fence_body() -> UploadBodyId {
        UploadBodyId::new(NATIVE_BODY).expect("body")
    }

    #[cfg(feature = "integration")]
    fn fence_realization(body: &UploadBodyId) -> BodyRealizationId {
        BodyRealizationId {
            body: body.clone(),
            epoch: ControlEpochId::new(1),
            native_sim: looklab_schema::SimKey::new("classic_chrome").expect("sim"),
        }
    }

    #[cfg(feature = "integration")]
    fn fence_target() -> FamilyTargetId {
        FamilyTargetId {
            family: LookId::new("classic-chrome").expect("family"),
            version: TargetVersion::new(1).expect("version"),
        }
    }

    #[cfg(feature = "integration")]
    fn fence_crm() -> ControlResponseModel {
        let body = fence_body();
        let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
        let resources = crate::body::DataDir::new(data)
            .load(&body)
            .expect("body resources");
        ControlResponseModel::zeroed(
            &resources.schema,
            1,
            &fence_realization(&body).native_sim,
            1,
        )
        .expect("crm")
    }

    #[cfg(feature = "integration")]
    fn fence_lineage() -> Lineage {
        let body = fence_body();
        let realization = fence_realization(&body);
        let body_warm = BodyWarmStart {
            delta: Delta6::ZERO,
            exposure: Vec::new(),
            wb: Vec::new(),
        };
        let realization_warm = RealizationWarmStart {
            clip_k: 0.03,
            nuisances: Vec::new(),
            damping: 1e-3,
            response: None,
            coefficients: Vec::new(),
        };
        let mut lineage = Lineage::default();
        lineage.body_warm.insert(
            body.clone(),
            SavedWarm {
                inventory: BodyModes::default(),
                warm: body_warm.clone(),
            },
        );
        lineage.published_body_warm.insert(
            body.clone(),
            SavedWarm {
                inventory: BodyModes::default(),
                warm: body_warm,
            },
        );
        lineage.realization_warm.insert(
            realization.clone(),
            SavedRealization {
                inventory: BodyModes::default(),
                warm: realization_warm.clone(),
            },
        );
        lineage.published_realization_warm.insert(
            realization.clone(),
            SavedRealization {
                inventory: BodyModes::default(),
                warm: realization_warm,
            },
        );
        let crm = fence_crm();
        lineage.crm.insert(realization.clone(), crm.clone());
        lineage.published_crm.insert(realization, crm);
        lineage.sigma = vec![(
            looklab_trust::EffectiveContributorId::from(looklab_trust::CredentialId::from_u128(1)),
            0.1,
        )];
        lineage.firmware = vec![(
            FamilyFirmwareId {
                family: fence_target().family,
                firmware: FirmwareId {
                    body: looklab_schema::BodyId::new(body.as_str()).expect("body"),
                    epoch: ControlEpochId::new(1),
                    version: NATIVE_FIRMWARE.parse().expect("firmware"),
                },
            },
            [0.1, 0.01],
        )];
        lineage.gauge = Some(SavedGauge {
            inventory: BodyModes::default(),
            gauge: ComponentGauge {
                designated: looklab_body::BodyId::new(body.as_str()).expect("body"),
                exposure_mode: ExposureModeId(0),
                wb_mode: WbModeClass(0),
                anchored_delta: true,
            },
        });
        lineage.qualified = true;
        lineage.body_state.insert(
            body.clone(),
            (
                BodyStateVersion::new(1).expect("version"),
                Sha256Hex::from_bytes([0; 32]),
                1,
            ),
        );
        lineage.body_stage.insert(body, BodyStateStage::Native);
        lineage
    }

    #[cfg(feature = "integration")]
    fn qualified_fence_scope() -> EvidenceScope {
        EvidenceScope::Qualified {
            report: looklab_schema::ReportScope {
                points: Vec::new(),
                combination_basis: looklab_schema::CombinationBasis::Enumerated,
                defaults_assessed: false,
                output_encoding: looklab_schema::OutputEncoding::Srgb,
                colour_cells: Vec::new(),
                exposure_cells: Vec::new(),
                assessed_bodies: vec![looklab_schema::BodyId::new(NATIVE_BODY).expect("body")],
                assessed_domain: looklab_schema::DomainId::new("fence").expect("domain"),
            },
            mask: looklab_schema::SupportedMask::new(),
            pinned: PinnedModes::default(),
        }
    }

    #[cfg(feature = "integration")]
    fn fence_member(scope: EvidenceScope, admission: Option<AdmissionBinding>) -> ScopeMember {
        let body = fence_body();
        ScopeMember {
            body: body.clone(),
            realization: fence_realization(&body),
            scope,
            admission,
        }
    }

    #[cfg(feature = "integration")]
    fn admission_binding() -> AdmissionBinding {
        AdmissionBinding {
            candidate: ModelVersionId::BodyRealization {
                realization: looklab_schema::BodyRealizationVersionId {
                    realization: fence_realization(&fence_body()),
                    version: RealizationVersion::new(1).expect("version"),
                    target: fence_target(),
                },
            },
        }
    }

    #[cfg(feature = "integration")]
    fn checkpoint() -> HierState {
        HierState {
            generation: HierStateGeneration::CURRENT,
            fingerprint: looklab_fit::hierarchical::HierFingerprint(0),
            outer_iterations: 0,
            bodies: Vec::new(),
            realizations: Vec::new(),
            published_bodies: Vec::new(),
            published_realizations: Vec::new(),
            families: Vec::new(),
            firmware: Vec::new(),
            sigma: Vec::new(),
            body_decrement: None,
            families_certified: false,
            converged: false,
        }
    }

    #[cfg(feature = "integration")]
    #[test]
    fn common_native_fence_cold_starts_scoped_admissions() {
        let admitted = fence_member(qualified_fence_scope(), Some(admission_binding()));
        let seed = fence_member(EvidenceScope::Seed, None);
        let experimental = fence_member(EvidenceScope::Experimental, None);
        assert!(has_scoped_admission(&[admitted.clone(), seed.clone()]));
        assert!(!has_scoped_admission(&[seed, experimental]));
        assert!(admitted.admission.is_some());

        // The fence ignores saved masks and noise: any saved common-native state
        // is cleared, including a wider-mask predecessor.
        let mut lineage = fence_lineage();
        assert!(!lineage.body_warm.is_empty());
        assert!(!lineage.realization_warm.is_empty());
        assert!(!lineage.crm.is_empty());
        assert!(!lineage.sigma.is_empty());
        assert!(!lineage.firmware.is_empty());
        assert!(lineage.gauge.is_some());
        assert!(lineage.qualified);
        assert!(!lineage.body_state.is_empty());
        fence_common_native(&mut lineage);
        assert!(lineage.body_warm.is_empty());
        assert!(lineage.published_body_warm.is_empty());
        assert!(lineage.realization_warm.is_empty());
        assert!(lineage.published_realization_warm.is_empty());
        assert!(lineage.crm.is_empty());
        assert!(lineage.published_crm.is_empty());
        assert!(lineage.sigma.is_empty());
        assert!(lineage.firmware.is_empty());
        assert!(lineage.gauge.is_some());
        assert!(lineage.qualified);
        assert!(!lineage.body_state.is_empty());
        assert!(lineage.authoritative_missing.is_empty());

        let state = checkpoint();
        assert!(common_native_resume(true, Some(state.clone())).is_none());
        assert!(common_native_resume(false, Some(state)).is_some());
    }

    #[cfg(feature = "integration")]
    #[test]
    fn ordinary_seed_and_experimental_state_restoration_is_unchanged() {
        let seed = fence_member(EvidenceScope::Seed, None);
        let experimental = fence_member(EvidenceScope::Experimental, None);
        assert!(!has_scoped_admission(&[seed, experimental]));
        let lineage = fence_lineage();
        assert!(!lineage.body_warm.is_empty());
        assert!(!lineage.realization_warm.is_empty());
        assert!(!lineage.crm.is_empty());
        assert!(!lineage.sigma.is_empty());
        assert!(!lineage.firmware.is_empty());
        assert!(lineage.gauge.is_some());
        assert!(common_native_resume(false, Some(checkpoint())).is_some());
    }
}
