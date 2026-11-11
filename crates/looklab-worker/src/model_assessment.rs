// SPDX-License-Identifier: AGPL-3.0-or-later
//! Durable execution of the versioned qualification protocols (docs/02 §5.1).

pub(crate) mod artifacts;
mod controls;
pub(crate) mod diagnostics;
mod engine;
pub(crate) mod evidence;
mod native;
mod pooling;
pub(crate) mod predict;

use std::collections::BTreeSet;

use looklab_fit::FitOptions;
use looklab_schema::{
    AssessmentOutcome, BodyRealizationVersionId, CohortRole, CombinationBasis,
    DependencyGeneration, DependencyKind, DependencyRef, DependencyTarget, DomainId,
    FamilyTargetId, InvalidationReason, LookId, ModelReportId, ModelSubject, ModelVersionId,
    PendingGeneration, ProbeSetId, ProjectionUse, QualificationCapability, RealizationRevisionId,
    ReportProvenance, ReportScope, RuleRevision, Sha256Hex, SourceRef, SubjectKey,
};
use looklab_server::model_store::{
    self, EvidenceBlob, EvidenceGroup, EvidenceMembership, EvidenceRole, EvidenceRoleAssignment,
    ExpectedSelection, JobLease, RealizationSelection, SelectionRef,
};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::assessment::{
    AssessmentBinding, CONTROLS_MATCHING_RULE, CROSS_BODY_RULE, NATIVE_PREDICTION_RULE,
    ProtocolAssessment, ReportParts, build_report, limitation,
};
use crate::clock::now_unix;
use crate::config::WorkerConfig;
use crate::error::{Result, WorkerError};
use crate::jobs::{AssessmentJob, AssessmentSources, JobOwnership, JobPayload};
use crate::model_assessment::artifacts::{
    is_native_revision, load_realization, private_membership,
};
use crate::model_assessment::evidence::Evidence;
use crate::source_identity;

const ASSESSMENT_SEED: u64 = 0x5eed_1a5e;
const NATIVE_DOMAIN: &str = "native-held-out";
const CONTROLS_DOMAIN: &str = "controls-held-out";
const CROSS_BODY_DOMAIN: &str = "cross-body-common-support";

pub async fn enqueue_candidate(
    conn: &mut sqlx::SqliteConnection,
    subject: &ModelSubject,
    candidate: &ModelVersionId,
    now: i64,
) -> Result<()> {
    let Some(capability) = candidate_capability(conn, candidate).await? else {
        return Ok(());
    };
    if assessment_current(conn, subject, candidate, capability).await? {
        return Ok(());
    }
    let domain = DomainId::new(domain_for(capability))
        .map_err(|error| WorkerError::Data(error.to_string()))?;
    let mut dependencies = candidate_dependencies(conn, candidate).await?;
    dependencies.extend(source_identity::sources().dependencies());
    let rule = RuleRevision::new(rule_for(capability))
        .map_err(|error| WorkerError::Data(error.to_string()))?;
    let rule_dependency =
        DependencyRef::required(DependencyKind::RuleVersion, DependencyTarget::rule(&rule));
    model_store::register_dependency(conn, &rule_dependency, now).await?;
    if !dependencies.contains(&rule_dependency) {
        dependencies.push(rule_dependency);
    }
    let dependencies = model_store::observe_dependency_snapshot(conn, &dependencies, now).await?;
    let pending = model_store::pending_generation(conn, &subject.key()).await?;
    let request = AssessmentJob::Camera {
        subject: subject.clone(),
        candidate: candidate.clone(),
        capability,
        domain,
        dependencies,
        pending,
        seeds: vec![ASSESSMENT_SEED],
        reassess_selected: false,
        sources: current_sources(),
    };
    enqueue_unique(conn, &request, now).await
}

pub(crate) async fn candidate_capability(
    conn: &mut sqlx::SqliteConnection,
    candidate: &ModelVersionId,
) -> Result<Option<QualificationCapability>> {
    match candidate {
        ModelVersionId::SharedFamily { .. } => {
            Ok(Some(QualificationCapability::CrossBodyAdmission))
        }
        ModelVersionId::BodyRealization { realization } => {
            let revision = model_store::realization_revision(
                conn,
                &RealizationRevisionId::for_version(realization),
            )
            .await?
            .ok_or_else(|| {
                WorkerError::Data(format!(
                    "candidate realization {} is not stored",
                    RealizationRevisionId::for_version(realization)
                ))
            })?;
            if revision.status == looklab_schema::RealizationStatus::Diagnostic {
                return Ok(None);
            }
            Ok(Some(
                if is_native_revision(conn, &revision)
                    .await
                    .map_err(WorkerError::Data)?
                {
                    QualificationCapability::NativePrediction
                } else {
                    QualificationCapability::ControlsMatching
                },
            ))
        }
    }
}

pub async fn enqueue_selected_reassessment(
    conn: &mut sqlx::SqliteConnection,
    subject: &ModelSubject,
    candidate: &ModelVersionId,
    capability: QualificationCapability,
    now: i64,
) -> Result<()> {
    let domain = DomainId::new(domain_for(capability))
        .map_err(|error| WorkerError::Data(error.to_string()))?;
    let mut dependencies = candidate_dependencies(conn, candidate).await?;
    dependencies.extend(source_identity::sources().dependencies());
    let rule = RuleRevision::new(rule_for(capability))
        .map_err(|error| WorkerError::Data(error.to_string()))?;
    let rule_dependency =
        DependencyRef::required(DependencyKind::RuleVersion, DependencyTarget::rule(&rule));
    model_store::register_dependency(conn, &rule_dependency, now).await?;
    if !dependencies.contains(&rule_dependency) {
        dependencies.push(rule_dependency);
    }
    let dependencies = model_store::observe_dependency_snapshot(conn, &dependencies, now).await?;
    let pending = model_store::pending_generation(conn, &subject.key()).await?;
    let request = AssessmentJob::Camera {
        subject: subject.clone(),
        candidate: candidate.clone(),
        capability,
        domain,
        dependencies,
        pending,
        seeds: vec![ASSESSMENT_SEED],
        reassess_selected: true,
        sources: current_sources(),
    };
    enqueue_unique(conn, &request, now).await
}

pub async fn enqueue_candidate_pool(
    pool: &SqlitePool,
    subject: &ModelSubject,
    candidate: &ModelVersionId,
    now: i64,
) -> Result<()> {
    let mut conn = pool.acquire().await?;
    enqueue_candidate(&mut conn, subject, candidate, now).await
}

/// Queues P4.3 verification for the active numerical sources.
pub async fn enqueue_engine_verification(
    pool: &SqlitePool,
    config: &WorkerConfig,
    now: i64,
) -> Result<()> {
    let sources = source_identity::sources();
    let rule = RuleRevision::new(crate::assessment::ENGINE_COVERAGE_RULE)
        .map_err(|error| WorkerError::Data(error.to_string()))?;
    let version = crate::assessment::EngineVersion::new(
        sources.fitter.clone(),
        sources.evaluator.clone(),
        sources.model.clone(),
        rule.clone(),
    )
    .map_err(WorkerError::Data)?;
    let mut reference = None;
    match std::fs::read_dir(config.data.join("engine")) {
        Ok(entries) => {
            let mut paths: Vec<_> = entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
                .collect();
            paths.sort();
            for path in paths {
                let bytes = std::fs::read(&path)?;
                let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                    continue;
                };
                if value.get("engine").and_then(|value| value.as_str())
                    != Some(version.source().as_str())
                    || value.get("rule_revision").and_then(|value| value.as_str())
                        != Some(rule.as_str())
                {
                    continue;
                }
                let hash = Sha256Hex::from_bytes(Sha256::digest(&bytes).into());
                let blob = config.blobs.join(hash.to_hex());
                if !blob.exists() {
                    std::fs::write(&blob, &bytes)?;
                }
                reference = Some(hash);
                break;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    enqueue_engine(pool, version.source(), &rule, reference, now).await
}

pub async fn enqueue_engine(
    pool: &SqlitePool,
    engine: &SourceRef,
    rule_revision: &RuleRevision,
    reference: Option<Sha256Hex>,
    now: i64,
) -> Result<()> {
    let sources = source_identity::sources();
    let mut conn = pool.acquire().await?;
    let dependencies = model_store::dependency_snapshot(&mut conn, &sources.dependencies()).await?;
    let request = AssessmentJob::Engine {
        engine: engine.clone(),
        rule_revision: rule_revision.clone(),
        reference,
        dependencies,
        sources: current_sources(),
    };
    enqueue_unique(&mut conn, &request, now).await
}

async fn assessment_current(
    conn: &mut sqlx::SqliteConnection,
    subject: &ModelSubject,
    candidate: &ModelVersionId,
    capability: QualificationCapability,
) -> Result<bool> {
    let (kind, id, target, version) = candidate_columns(candidate);
    let row: Option<(String, i64, Option<i64>)> = sqlx::query_as(
        "SELECT r.id, r.created_at, p.updated_at
         FROM model_reports r
         LEFT JOIN model_pending_work p ON p.subject_key = r.subject_key
         WHERE r.subject_key = ? AND r.candidate_kind = ? AND r.candidate_id = ?
           AND r.candidate_target = ? AND r.candidate_version = ? AND r.capability = ?
         ORDER BY r.created_at DESC, r.id DESC LIMIT 1",
    )
    .bind(subject.key().as_str())
    .bind(kind)
    .bind(id)
    .bind(target)
    .bind(version)
    .bind(capability.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    let Some((id, created_at, pending_at)) = row else {
        return Ok(false);
    };
    let Ok(id) = looklab_schema::ModelReportId::parse(&id) else {
        return Ok(false);
    };
    if pending_at.is_some_and(|pending_at| pending_at > created_at) {
        return Ok(false);
    }
    model_store::report_applicable(conn, &id)
        .await
        .map_err(WorkerError::ModelStore)
}

fn candidate_columns(candidate: &ModelVersionId) -> (&'static str, String, String, i64) {
    match candidate {
        ModelVersionId::SharedFamily { target } => (
            "shared_family",
            target.to_key(),
            target.to_key(),
            i64::from(target.version.get()),
        ),
        ModelVersionId::BodyRealization { realization } => (
            "body_realization",
            RealizationRevisionId::for_version(realization).to_string(),
            realization.target.to_key(),
            i64::from(realization.version.get()),
        ),
    }
}

async fn enqueue_unique(
    conn: &mut sqlx::SqliteConnection,
    request: &AssessmentJob,
    now: i64,
) -> Result<()> {
    let payload = JobPayload::ModelAssessment {
        request: Box::new(request.clone()),
    };
    let (kind, encoded) = payload.encode()?;
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE kind = ? AND payload = ? AND status IN ('queued', 'running')",
    )
    .bind(kind.as_str())
    .bind(&encoded)
    .fetch_one(&mut *conn)
    .await?;
    if pending == 0 {
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload, status, attempts, run_after, created_at)
             VALUES (?, ?, ?, 'queued', 0, ?, ?)",
        )
        .bind(crate::jobs::JobId::new().as_str())
        .bind(kind.as_str())
        .bind(&encoded)
        .bind(now)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Runs one durable assessment job. A fenced or stale input commits nothing.
pub async fn run(
    pool: &SqlitePool,
    config: &WorkerConfig,
    request: &AssessmentJob,
    ownership: &JobOwnership,
) -> Result<()> {
    if request.sources() != &current_sources() {
        return Err(WorkerError::Job(
            "assessment was queued for a superseded numerical implementation".to_owned(),
        ));
    }
    ownership.check(pool).await?;
    match request {
        AssessmentJob::Engine {
            engine,
            rule_revision,
            reference,
            dependencies,
            ..
        } => {
            engine::run(
                pool,
                config,
                engine,
                rule_revision,
                *reference,
                source_identity::sources(),
                dependencies,
                ownership,
            )
            .await
        }
        AssessmentJob::Camera {
            subject,
            candidate,
            capability,
            domain,
            dependencies,
            pending,
            seeds,
            reassess_selected,
            ..
        } => {
            run_camera(
                pool,
                config,
                subject,
                candidate,
                *capability,
                domain,
                dependencies,
                *pending,
                seeds,
                *reassess_selected,
                ownership,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_camera(
    pool: &SqlitePool,
    config: &WorkerConfig,
    subject: &ModelSubject,
    candidate: &ModelVersionId,
    capability: QualificationCapability,
    domain: &DomainId,
    dependencies: &[DependencyGeneration],
    pending: PendingGeneration,
    seeds: &[u64],
    reassess_selected: bool,
    ownership: &JobOwnership,
) -> Result<()> {
    let sources = source_identity::sources();
    let rule = RuleRevision::new(rule_for(capability))
        .map_err(|error| WorkerError::Data(error.to_string()))?;
    let options_digest = effective_options(config);
    let initial_policy = partition_policy(subject, capability, &[]);
    let mut provenance = ReportProvenance {
        rule_revision: rule.clone(),
        fitter: sources.fitter.clone(),
        evaluator: sources.evaluator.clone(),
        model: sources.model.clone(),
        probe_set: ProbeSetId::new("v1").map_err(|error| WorkerError::Data(error.to_string()))?,
        effective_options: options_digest,
        partition_policy: initial_policy,
        assessment_seeds: seeds.to_vec(),
    };
    let mut dependency_refs: Vec<DependencyRef> = dependencies
        .iter()
        .map(|entry| entry.dependency.clone())
        .collect();
    dependency_refs.extend(sources.dependencies());
    match (subject, candidate) {
        (
            ModelSubject::BodyRealization { realization },
            ModelVersionId::BodyRealization {
                realization: version,
            },
        ) if realization == &version.realization => {
            let revision = load_revision(pool, version).await?;
            let membership = private_membership(pool, config, &revision)
                .await
                .map_err(WorkerError::Data)?;
            let loaded = match load_realization(pool, config, version).await {
                Ok(loaded) => loaded,
                Err(error) => {
                    return persist_execution_failure(
                        pool,
                        subject,
                        candidate,
                        capability,
                        domain,
                        provenance,
                        dependency_refs,
                        membership.unwrap_or_default(),
                        error,
                        dependencies,
                        pending,
                        ownership,
                    )
                    .await;
                }
            };
            let loaded_evidence = match evidence::load(
                pool,
                config,
                version,
                membership.as_ref(),
                reassess_selected,
            )
            .await
            {
                Ok(evidence) => evidence,
                Err(error) => {
                    return persist_execution_failure(
                        pool,
                        subject,
                        candidate,
                        capability,
                        domain,
                        provenance,
                        dependency_refs,
                        membership.unwrap_or_default(),
                        error.to_string(),
                        dependencies,
                        pending,
                        ownership,
                    )
                    .await;
                }
            };
            let points = evidence::points(&loaded_evidence);
            let scope = ReportScope {
                points,
                combination_basis: CombinationBasis::Enumerated,
                defaults_assessed: loaded_evidence
                    .images
                    .iter()
                    .any(|image| image.combination.is_empty()),
                output_encoding: loaded.output_encoding,
                colour_cells: Vec::new(),
                exposure_cells: Vec::new(),
                assessed_bodies: vec![version.realization.body.clone()],
                assessed_domain: domain.clone(),
            };
            if scope.points.is_empty() {
                provenance.partition_policy = partition_policy(subject, capability, &[]);
            } else {
                provenance.partition_policy = partition_policy(
                    subject,
                    capability,
                    &loaded_evidence
                        .assessment_groups
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>(),
                );
            }
            dependency_refs.extend(revision.dependencies.iter().cloned());
            dependency_refs.sort();
            dependency_refs.dedup();
            let failure_provenance = provenance.clone();
            let failure_dependencies = dependency_refs.clone();
            let binding = AssessmentBinding {
                subject: subject.clone(),
                candidate: candidate.clone(),
                scope,
                provenance,
                dependencies: dependency_refs,
            };
            let execution = match capability {
                QualificationCapability::NativePrediction => native::run(
                    binding,
                    &loaded,
                    &loaded_evidence,
                    &fit_options(config),
                    config.max_samples_per_image,
                ),
                QualificationCapability::ControlsMatching => controls::run(
                    binding,
                    &loaded,
                    &loaded_evidence,
                    config.max_samples_per_image,
                ),
                QualificationCapability::CrossBodyAdmission => {
                    Err("a body realization cannot carry a cross-body report".to_owned())
                }
            };
            match execution {
                Ok((mut assessment, evaluated)) => {
                    let membership =
                        evidence_membership(&loaded_evidence, membership.as_ref(), &evaluated);
                    assessment.membership = membership;
                    crate::assessment::refresh_report_identity(&mut assessment);
                    let failed = assessment.report.outcome == AssessmentOutcome::Fail;
                    let result = persist_report(
                        pool,
                        subject,
                        candidate,
                        capability,
                        &assessment,
                        &revision.artifacts,
                        dependencies,
                        pending,
                        ownership,
                    )
                    .await;
                    if failed {
                        reassess_selected_after_failure(
                            pool,
                            subject,
                            candidate,
                            capability,
                            &assessment.report.scope,
                            now_unix(),
                        )
                        .await?;
                    }
                    result
                }
                Err(error) => {
                    persist_execution_failure(
                        pool,
                        subject,
                        candidate,
                        capability,
                        domain,
                        failure_provenance,
                        failure_dependencies,
                        membership.unwrap_or_default(),
                        error,
                        dependencies,
                        pending,
                        ownership,
                    )
                    .await
                }
            }
        }
        (ModelSubject::SharedFamily { family }, ModelVersionId::SharedFamily { target })
            if family == &target.family =>
        {
            let encoding = cross_body_encoding(pool, config, target).await?;
            let scope = ReportScope {
                points: Vec::new(),
                combination_basis: CombinationBasis::Enumerated,
                defaults_assessed: true,
                output_encoding: encoding,
                colour_cells: Vec::new(),
                exposure_cells: Vec::new(),
                assessed_bodies: Vec::new(),
                assessed_domain: domain.clone(),
            };
            let binding = AssessmentBinding {
                subject: subject.clone(),
                candidate: candidate.clone(),
                scope,
                provenance,
                dependencies: dependency_refs,
            };
            let mut assessment = pooling::run(pool, config, binding, target, sources).await?;
            crate::assessment::refresh_report_identity(&mut assessment);
            let failed = assessment.report.outcome == AssessmentOutcome::Fail;
            let result = persist_report(
                pool,
                subject,
                candidate,
                capability,
                &assessment,
                &[],
                dependencies,
                pending,
                ownership,
            )
            .await;
            if failed {
                reassess_selected_after_failure(
                    pool,
                    subject,
                    candidate,
                    capability,
                    &assessment.report.scope,
                    now_unix(),
                )
                .await?;
            }
            result
        }
        _ => Err(WorkerError::Data(
            "assessment candidate does not name the report subject".to_owned(),
        )),
    }
}

async fn load_revision(
    pool: &SqlitePool,
    candidate: &BodyRealizationVersionId,
) -> Result<looklab_server::model_store::StoredRealizationRevision> {
    let mut conn = pool.acquire().await?;
    model_store::realization_revision(&mut conn, &RealizationRevisionId::for_version(candidate))
        .await?
        .ok_or_else(|| {
            WorkerError::Data(format!(
                "candidate realization {} is not stored",
                RealizationRevisionId::for_version(candidate)
            ))
        })
}

async fn cross_body_encoding(
    pool: &SqlitePool,
    config: &WorkerConfig,
    target: &looklab_schema::FamilyTargetId,
) -> Result<looklab_schema::OutputEncoding> {
    let bytes =
        artifacts::read_target_artifact(config, pool, target, looklab_publish::RELEASE_ARTIFACT)
            .await
            .map_err(WorkerError::Data)?;
    let release: crate::refit::ReleaseRecord =
        serde_json::from_slice(&bytes).map_err(|error| WorkerError::Data(error.to_string()))?;
    Ok(artifacts::output_encoding(release.output_space))
}

async fn overlapping_projections(
    conn: &mut sqlx::SqliteConnection,
    subject: &ModelSubject,
    use_: ProjectionUse,
    capability: QualificationCapability,
    scope: &ReportScope,
) -> Result<Vec<looklab_schema::CurrentProjection>> {
    let mut overlapping = Vec::new();
    for projection in model_store::current_projections(conn, &subject.key(), use_).await? {
        if projection.capability != capability || !projection.applicability.is_current() {
            continue;
        }
        let Some(report) = projection.selected_report else {
            continue;
        };
        let Some(stored) = model_store::report(conn, &report).await? else {
            continue;
        };
        if stored.report.scope.overlaps(scope) {
            overlapping.push(projection);
        }
    }
    Ok(overlapping)
}

#[allow(clippy::too_many_arguments)]
async fn select_family_report(
    conn: &mut sqlx::SqliteConnection,
    family: &LookId,
    target: &FamilyTargetId,
    report: &ModelReportId,
    assessment: &ProtocolAssessment,
    pending: PendingGeneration,
    ownership: &JobOwnership,
    now: i64,
) -> Result<bool> {
    let subject = SubjectKey::for_family(family);
    let scope_key = assessment.report.scope.key();
    let current = model_store::current_projection(
        conn,
        &subject,
        ProjectionUse::QualifiedMatch,
        QualificationCapability::CrossBodyAdmission,
        &scope_key,
    )
    .await?;
    if current
        .as_ref()
        .is_some_and(|projection| projection.pending_generation > pending)
    {
        return Ok(false);
    }
    if current.as_ref().is_some_and(|projection| {
        projection.selected_report == Some(*report)
            && projection.selected_candidate.as_ref()
                == Some(&ModelVersionId::SharedFamily {
                    target: target.clone(),
                })
    }) {
        return Ok(false);
    }
    let expected = match current.as_ref() {
        Some(projection) => match &projection.selected_candidate {
            Some(ModelVersionId::SharedFamily { target }) => {
                model_store::ExpectedFamilySelection::Selected(model_store::FamilySelectionRef {
                    target,
                    report: projection.selected_report.as_ref(),
                })
            }
            _ => model_store::ExpectedFamilySelection::Absent,
        },
        None => model_store::ExpectedFamilySelection::Absent,
    };
    let snapshot = model_store::dependency_snapshot(conn, &dependency_refs(assessment)).await?;
    let storage = source_identity::sources().storage();
    let server_job = ownership.server_id();
    let expected_anchor = model_store::subject_state(conn, &subject)
        .await?
        .and_then(|state| state.anchor_revision);
    let outcome = model_store::select_family(
        conn,
        &model_store::FamilySelection {
            family,
            sources: &storage,
            expected_anchor,
            target,
            report,
            scope_key: &scope_key,
            expected,
            lease: JobLease {
                job: &server_job,
                attempts: ownership.attempts,
            },
            snapshot: &snapshot,
            observed_pending: pending,
        },
        now,
    )
    .await?;
    Ok(matches!(
        outcome,
        model_store::FamilyProjectionOutcome::Selected
    ))
}

async fn reassess_selected_after_failure(
    pool: &SqlitePool,
    subject: &ModelSubject,
    candidate: &ModelVersionId,
    capability: QualificationCapability,
    scope: &ReportScope,
    now: i64,
) -> Result<()> {
    let mut conn = pool.acquire().await?;
    let mut queued = BTreeSet::new();
    for projection in overlapping_projections(
        &mut conn,
        subject,
        ProjectionUse::QualifiedMatch,
        capability,
        scope,
    )
    .await?
    {
        let Some(selected) = projection.selected_candidate else {
            continue;
        };
        if &selected == candidate {
            continue;
        }
        if !queued.insert(selected.candidate_id()) {
            continue;
        }
        enqueue_selected_reassessment(&mut conn, subject, &selected, projection.capability, now)
            .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn persist_report(
    pool: &SqlitePool,
    subject: &ModelSubject,
    candidate: &ModelVersionId,
    capability: QualificationCapability,
    assessment: &ProtocolAssessment,
    artifacts: &[looklab_schema::ArtifactBinding],
    observed: &[DependencyGeneration],
    pending: PendingGeneration,
    ownership: &JobOwnership,
) -> Result<()> {
    let dependencies = dependency_refs(assessment);
    let now = now_unix();
    let mut tx = model_store::begin_immediate(pool).await?;
    if !lease_owned(&mut tx, ownership).await? {
        return Ok(());
    }
    if !snapshot_matches(&mut tx, &dependencies, observed).await? {
        return Ok(());
    }
    let existing = model_store::report(&mut tx, &assessment.report_id)
        .await?
        .is_some();
    model_store::insert_report(
        &mut tx,
        &assessment.report,
        &dependencies,
        artifacts,
        &assessment.membership,
        now,
    )
    .await?;
    for request in &assessment.report.evidence_requests {
        model_store::record_evidence_request(
            &mut tx,
            &assessment.report_id,
            &subject.key(),
            capability,
            request,
            now,
        )
        .await?;
    }
    let mut selected = false;
    let mut admitted = Vec::new();
    match (subject, candidate) {
        (
            ModelSubject::BodyRealization { realization },
            ModelVersionId::BodyRealization {
                realization: version,
            },
        ) if realization == &version.realization => {
            if assessment.report.outcome == AssessmentOutcome::Pass {
                selected = select(
                    &mut tx,
                    subject,
                    candidate,
                    capability,
                    ProjectionUse::QualifiedMatch,
                    Some(&assessment.report_id),
                    assessment,
                    pending,
                    ownership,
                    now,
                )
                .await?;
                if capability == QualificationCapability::ControlsMatching {
                    let _ = select(
                        &mut tx,
                        subject,
                        candidate,
                        capability,
                        ProjectionUse::ExperimentalMatch,
                        None,
                        assessment,
                        pending,
                        ownership,
                        now,
                    )
                    .await?;
                }
            } else if assessment.report.outcome == AssessmentOutcome::Fail {
                invalidate_contradicted(&mut tx, subject, candidate, capability, assessment, now)
                    .await?;
            }
        }
        (ModelSubject::SharedFamily { family }, ModelVersionId::SharedFamily { target })
            if family == &target.family =>
        {
            if assessment.report.outcome == AssessmentOutcome::Pass {
                selected = select_family_report(
                    &mut tx,
                    family,
                    target,
                    &assessment.report_id,
                    assessment,
                    pending,
                    ownership,
                    now,
                )
                .await?;
                if let Some(cohort) = &assessment.cohort
                    && !cohort_admitted(&mut tx, cohort).await?
                    && model_store::record_cohort_admission(&mut tx, cohort, now)
                        .await
                        .is_ok()
                {
                    admitted = cohort.members.iter().map(cohort_member_body).collect();
                    enqueue_refit_conn(&mut tx, &admitted, now).await?;
                }
            } else if assessment.report.outcome == AssessmentOutcome::Fail {
                invalidate_contradicted(&mut tx, subject, candidate, capability, assessment, now)
                    .await?;
            }
        }
        _ => {}
    }
    tx.commit().await?;
    if !existing || selected || !admitted.is_empty() {
        let _ = crate::jobs::request_publish(pool, now).await?;
    }
    Ok(())
}

async fn enqueue_refit_conn(
    conn: &mut sqlx::SqliteConnection,
    bodies: &[looklab_schema::BodyId],
    now: i64,
) -> Result<()> {
    let bodies = bodies
        .iter()
        .map(|body| {
            looklab_upload::BodyId::new(body.as_str())
                .map_err(|error| WorkerError::Data(error.to_string()))
        })
        .collect::<Result<Vec<_>>>()?;
    let payload = JobPayload::NightlyRefit { bodies };
    let (kind, encoded) = payload.encode()?;
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE kind = ? AND payload = ? AND status IN ('queued', 'running')",
    )
    .bind(kind.as_str())
    .bind(&encoded)
    .fetch_one(&mut *conn)
    .await?;
    if pending == 0 {
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload, status, attempts, run_after, created_at)
             VALUES (?, ?, ?, 'queued', 0, ?, ?)",
        )
        .bind(crate::jobs::JobId::new().as_str())
        .bind(kind.as_str())
        .bind(&encoded)
        .bind(now)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn select(
    conn: &mut sqlx::SqliteConnection,
    subject: &ModelSubject,
    candidate: &ModelVersionId,
    capability: QualificationCapability,
    use_: ProjectionUse,
    report: Option<&ModelReportId>,
    assessment: &ProtocolAssessment,
    pending: PendingGeneration,
    ownership: &JobOwnership,
    now: i64,
) -> Result<bool> {
    let ModelVersionId::BodyRealization { realization } = candidate else {
        return Ok(false);
    };
    let revision = RealizationRevisionId::for_version(realization);
    let scope_key = assessment.report.scope.key();
    let current =
        model_store::current_projection(conn, &subject.key(), use_, capability, &scope_key).await?;
    if current.as_ref().is_some_and(|projection| {
        projection.selected_report == report.copied()
            && projection.selected_candidate.as_ref() == Some(candidate)
    }) {
        return Ok(false);
    }
    let expected_revision =
        current
            .as_ref()
            .and_then(|projection| match &projection.selected_candidate {
                Some(ModelVersionId::BodyRealization { realization }) => {
                    Some(RealizationRevisionId::for_version(realization))
                }
                _ => None,
            });
    let expected_report = current
        .as_ref()
        .and_then(|projection| projection.selected_report);
    let expected = match (&expected_revision, expected_report.as_ref()) {
        (Some(revision), report) => ExpectedSelection::Selected(SelectionRef { revision, report }),
        _ => ExpectedSelection::Absent,
    };
    let snapshot = model_store::dependency_snapshot(conn, &dependency_refs(assessment)).await?;
    let storage = source_identity::sources().storage();
    let server_job = ownership.server_id();
    let expected_anchor = model_store::subject_state(
        conn,
        &looklab_schema::SubjectKey::for_family(&realization.target.family),
    )
    .await?
    .and_then(|state| state.anchor_revision);
    let outcome = model_store::select_realization(
        conn,
        &RealizationSelection {
            subject: &subject.key(),
            sources: &storage,
            expected_anchor,
            use_,
            capability,
            scope_key: &scope_key,
            revision: &revision,
            target: &realization.target,
            report,
            supersedes: &[],
            expected,
            lease: JobLease {
                job: &server_job,
                attempts: ownership.attempts,
            },
            snapshot: &snapshot,
            observed_pending: pending,
        },
        now,
    )
    .await?;
    Ok(matches!(outcome, model_store::SelectionOutcome::Selected))
}

async fn invalidate_contradicted(
    conn: &mut sqlx::SqliteConnection,
    subject: &ModelSubject,
    candidate: &ModelVersionId,
    capability: QualificationCapability,
    assessment: &ProtocolAssessment,
    now: i64,
) -> Result<()> {
    let mut invalidated = BTreeSet::new();
    for projection in overlapping_projections(
        conn,
        subject,
        ProjectionUse::QualifiedMatch,
        capability,
        &assessment.report.scope,
    )
    .await?
    {
        if projection.selected_candidate.as_ref() != Some(candidate) {
            continue;
        }
        let Some(report) = projection.selected_report else {
            continue;
        };
        if !invalidated.insert(report) {
            continue;
        }
        model_store::invalidate(
            conn,
            &DependencyRef::required(DependencyKind::Report, DependencyTarget::report(&report)),
            InvalidationReason::EvidenceLoss,
            now,
        )
        .await?;
    }
    Ok(())
}

async fn cohort_admitted(
    conn: &mut sqlx::SqliteConnection,
    request: &looklab_schema::CohortAdmissionRequest,
) -> Result<bool> {
    let existing = model_store::family_cohort(conn, &request.family, CohortRole::Admitted)
        .await?
        .members;
    Ok(existing.len() == request.members.len()
        && request
            .members
            .iter()
            .all(|member| existing.contains(member)))
}

fn cohort_member_body(member: &looklab_schema::CohortMember) -> looklab_schema::BodyId {
    match member {
        looklab_schema::CohortMember::Seed { body } => body.clone(),
        looklab_schema::CohortMember::Admitted { realization, .. } => {
            realization.realization.body.clone()
        }
    }
}

async fn lease_owned(conn: &mut sqlx::SqliteConnection, ownership: &JobOwnership) -> Result<bool> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE id = ? AND attempts = ? AND status = 'running'",
    )
    .bind(ownership.id.as_str())
    .bind(ownership.attempts)
    .fetch_one(&mut *conn)
    .await?;
    Ok(count == 1)
}

async fn snapshot_matches(
    conn: &mut sqlx::SqliteConnection,
    dependencies: &[DependencyRef],
    observed: &[DependencyGeneration],
) -> Result<bool> {
    let current = model_store::dependency_snapshot(conn, dependencies).await?;
    Ok(observed.iter().all(|expected| {
        current.iter().any(|actual| {
            actual.dependency == expected.dependency && actual.generation == expected.generation
        })
    }))
}

fn dependency_refs(assessment: &ProtocolAssessment) -> Vec<DependencyRef> {
    assessment.dependencies.clone()
}

#[allow(clippy::too_many_arguments)]
async fn persist_execution_failure(
    pool: &SqlitePool,
    subject: &ModelSubject,
    candidate: &ModelVersionId,
    capability: QualificationCapability,
    domain: &DomainId,
    mut provenance: ReportProvenance,
    dependencies: Vec<DependencyRef>,
    membership: EvidenceMembership,
    error: String,
    observed: &[DependencyGeneration],
    pending: PendingGeneration,
    ownership: &JobOwnership,
) -> Result<()> {
    let rule = RuleRevision::new(rule_for(capability))
        .map_err(|error| WorkerError::Data(error.to_string()))?;
    provenance.rule_revision = rule.clone();
    let scope = ReportScope {
        points: Vec::new(),
        combination_basis: CombinationBasis::Enumerated,
        defaults_assessed: false,
        output_encoding: looklab_schema::OutputEncoding::Srgb,
        colour_cells: Vec::new(),
        exposure_cells: Vec::new(),
        assessed_bodies: match subject {
            ModelSubject::BodyRealization { realization } => vec![realization.body.clone()],
            ModelSubject::SharedFamily { .. } => Vec::new(),
        },
        assessed_domain: domain.clone(),
    };
    let binding = AssessmentBinding {
        subject: subject.clone(),
        candidate: candidate.clone(),
        scope,
        provenance,
        dependencies,
    };
    let mut assessment = build_report(
        &binding,
        ReportParts {
            capability,
            rule,
            outcome: AssessmentOutcome::ExecutionFailure,
            metrics: looklab_schema::ReportMetrics::new(),
            supported_mask: looklab_schema::SupportedMask::new(),
            limitations: vec![limitation(error)],
            evidence_requests: Vec::new(),
            extra_dependencies: Vec::new(),
            membership,
            cohort: None,
        },
    );
    crate::assessment::refresh_report_identity(&mut assessment);
    persist_report(
        pool,
        subject,
        candidate,
        capability,
        &assessment,
        &[],
        observed,
        pending,
        ownership,
    )
    .await
}

fn evidence_membership(
    evidence: &Evidence,
    stored: Option<&EvidenceMembership>,
    evaluated: &[usize],
) -> EvidenceMembership {
    let mut membership = stored.cloned().unwrap_or_default();
    if membership.groups.is_empty() {
        for image in &evidence.images {
            membership.groups.push(EvidenceGroup {
                group: image.group.clone(),
                role: if evidence.assessment_groups.contains(&image.group) {
                    EvidenceRole::Assessment
                } else {
                    EvidenceRole::Training
                },
            });
        }
    }
    if membership.blobs.is_empty() {
        for image in &evidence.images {
            let pool_image = &evidence.pool.images[image.pool_index];
            let role = if evidence.assessment_groups.contains(&image.group) {
                EvidenceRole::Assessment
            } else {
                EvidenceRole::Training
            };
            if let Ok(image_ref) =
                looklab_schema::ImageRef::new(pool_image.image.id.as_str().to_owned())
            {
                membership.blobs.push(EvidenceBlob {
                    image: image_ref,
                    samples_hash: Sha256Hex::from_bytes(pool_image.image.samples_hash),
                    role,
                });
            }
            if let Ok(contribution) =
                looklab_schema::ContributionRef::new(pool_image.contribution.id.as_str())
                && !membership
                    .roles
                    .iter()
                    .any(|assignment| assignment.contribution == contribution)
            {
                membership
                    .roles
                    .push(EvidenceRoleAssignment { contribution, role });
            }
        }
    }
    let mut images = BTreeSet::new();
    let mut contributions = BTreeSet::new();
    for index in evaluated {
        let pool_image = &evidence.pool.images[*index];
        if let Ok(image) = looklab_schema::ImageRef::new(pool_image.image.id.as_str().to_owned()) {
            images.insert(image);
        }
        if let Ok(contribution) =
            looklab_schema::ContributionRef::new(pool_image.contribution.id.as_str())
        {
            contributions.insert(contribution);
        }
    }
    membership.evaluated_images = images.into_iter().collect();
    membership.evaluated_contributions = contributions.into_iter().collect();
    membership
        .groups
        .sort_by(|left, right| left.group.as_str().cmp(right.group.as_str()));
    membership.blobs.sort_by(|left, right| {
        left.image
            .as_str()
            .cmp(right.image.as_str())
            .then(left.samples_hash.to_hex().cmp(&right.samples_hash.to_hex()))
    });
    membership
        .roles
        .sort_by(|left, right| left.contribution.as_str().cmp(right.contribution.as_str()));
    membership
}

fn current_sources() -> AssessmentSources {
    let sources = source_identity::sources();
    AssessmentSources {
        fitter: sources.fitter.clone(),
        evaluator: sources.evaluator.clone(),
        model: sources.model.clone(),
    }
}

async fn candidate_dependencies(
    conn: &mut sqlx::SqliteConnection,
    candidate: &ModelVersionId,
) -> Result<Vec<DependencyRef>> {
    let mut dependencies = match candidate {
        ModelVersionId::SharedFamily { target } => {
            model_store::dependencies(conn, "family_head", &target.to_key()).await?
        }
        ModelVersionId::BodyRealization { realization } => {
            let revision = model_store::realization_revision(
                conn,
                &RealizationRevisionId::for_version(realization),
            )
            .await?
            .ok_or_else(|| {
                WorkerError::Data(format!(
                    "candidate realization {} is not stored",
                    RealizationRevisionId::for_version(realization)
                ))
            })?;
            let mut dependencies = revision.dependencies;
            dependencies.push(DependencyRef::required(
                DependencyKind::RealizationRevision,
                DependencyTarget::realization_revision(&RealizationRevisionId::for_version(
                    realization,
                )),
            ));
            dependencies
        }
    };
    let mut weightings = vec![DependencyTarget::root_weighting_global()];
    match candidate {
        ModelVersionId::SharedFamily { target } => {
            weightings.push(DependencyTarget::root_weighting_family(&target.family));
            let mut bodies = BTreeSet::new();
            for role in [CohortRole::Admitted, CohortRole::Seed] {
                for member in model_store::family_cohort(conn, &target.family, role)
                    .await?
                    .members
                {
                    match member {
                        looklab_schema::CohortMember::Seed { body } => {
                            bodies.insert(body);
                        }
                        looklab_schema::CohortMember::Admitted { realization, .. } => {
                            bodies.insert(realization.realization.body);
                        }
                    }
                }
            }
            for body in bodies {
                weightings.push(DependencyTarget::root_weighting_body(&body));
            }
        }
        ModelVersionId::BodyRealization { realization } => {
            weightings.push(DependencyTarget::root_weighting_family(
                &realization.target.family,
            ));
            weightings.push(DependencyTarget::root_weighting_body(
                &realization.realization.body,
            ));
        }
    }
    for weighting in weightings {
        dependencies.push(DependencyRef::required(
            DependencyKind::RootWeighting,
            weighting,
        ));
    }
    dependencies.sort();
    dependencies.dedup();
    Ok(dependencies)
}

fn rule_for(capability: QualificationCapability) -> &'static str {
    match capability {
        QualificationCapability::NativePrediction => NATIVE_PREDICTION_RULE,
        QualificationCapability::ControlsMatching => CONTROLS_MATCHING_RULE,
        QualificationCapability::CrossBodyAdmission => CROSS_BODY_RULE,
    }
}

fn domain_for(capability: QualificationCapability) -> &'static str {
    match capability {
        QualificationCapability::NativePrediction => NATIVE_DOMAIN,
        QualificationCapability::ControlsMatching => CONTROLS_DOMAIN,
        QualificationCapability::CrossBodyAdmission => CROSS_BODY_DOMAIN,
    }
}

fn fit_options(config: &WorkerConfig) -> FitOptions {
    FitOptions {
        seed: config.seed,
        max_samples_per_image: config.max_samples_per_image,
        sweep_budget: config.sweep_budget.unwrap_or(f64::MAX),
        ..FitOptions::default()
    }
}

fn effective_options(config: &WorkerConfig) -> Sha256Hex {
    let mut hasher = Sha256::new();
    hasher.update(config.seed.to_le_bytes());
    hasher.update(config.max_samples_per_image.to_le_bytes());
    hasher.update(
        config
            .sweep_budget
            .unwrap_or(f64::MAX)
            .to_bits()
            .to_le_bytes(),
    );
    Sha256Hex::from_bytes(hasher.finalize().into())
}

fn partition_policy(
    subject: &ModelSubject,
    capability: QualificationCapability,
    assessment: &[model_store::EvidenceGroupId],
) -> Sha256Hex {
    match (capability, subject) {
        (QualificationCapability::CrossBodyAdmission, _) => {
            let mut hasher = Sha256::new();
            hasher.update(b"looklab-cross-body-partition/1\0");
            hasher.update(subject.key().as_str().as_bytes());
            Sha256Hex::from_bytes(hasher.finalize().into())
        }
        (_, ModelSubject::BodyRealization { realization }) => {
            crate::model_fit::partition_policy_digest(
                &looklab_schema::SubjectKey::for_realization(realization),
                &assessment.iter().cloned().collect::<BTreeSet<_>>(),
            )
        }
        _ => {
            let mut hasher = Sha256::new();
            hasher.update(b"looklab-group-partition/1\0");
            hasher.update(subject.key().as_str().as_bytes());
            Sha256Hex::from_bytes(hasher.finalize().into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CROSS_BODY_DOMAIN;
    use super::*;
    use crate::assessment::{
        AssessmentBinding, CROSS_BODY_RULE, ProtocolAssessment, ReportParts, build_report,
    };
    use looklab_schema::{
        AssessedPoint, AssessmentOutcome, BodyId, CandidateSafety, CellId, CombinationBasis,
        DependencyKind, DependencyRef, DependencyTarget, DomainId, EligibilitySnapshotId,
        FamilyTargetId, LookId, MetricKey, ModelSubject, ModelVersionId, OutputEncoding,
        PendingGeneration, ProbeSetId, ProjectionUse, PublishedTargetId, QualificationCapability,
        ReportMetrics, ReportProvenance, ReportScope, RuleRevision, SafetyCheck,
        SettingCombination, Sha256Hex, SourceRef, SupportedMask, TargetBasis, TargetEligibility,
        TargetEligibilityStatus, TargetVersion,
    };
    use looklab_server::model_store::{EvidenceMembership, FamilySeed, NumericalSources};
    use looklab_server::{JobId, open_database};

    use crate::jobs::JobOwnership;

    fn family() -> LookId {
        LookId::new("classic-chrome").expect("family")
    }

    fn target() -> FamilyTargetId {
        FamilyTargetId {
            family: family(),
            version: TargetVersion::new(1).expect("version"),
        }
    }

    async fn family_version(pool: &SqlitePool, look: &str, version: u32) {
        let id = format!("{look}-{version}");
        sqlx::query(
            "INSERT INTO look_versions (id, look, version, status, created_at)
             VALUES (?, ?, ?, 'provisional', 0)",
        )
        .bind(&id)
        .bind(look)
        .bind(i64::from(version))
        .execute(pool)
        .await
        .expect("look version");
        sqlx::query(
            "INSERT INTO version_manifests (look_version_id, artifacts, created_at)
             VALUES (?, '[]', 0)",
        )
        .bind(&id)
        .execute(pool)
        .await
        .expect("manifest");
    }

    async fn running_job(pool: &SqlitePool, id: &JobId) {
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload, status, attempts, run_after, created_at)
             VALUES (?, 'refit', '{}', 'running', 1, 0, 0)",
        )
        .bind(id.as_str())
        .execute(pool)
        .await
        .expect("job");
    }

    fn scope_with(bodies: Vec<BodyId>) -> ReportScope {
        ReportScope {
            points: vec![AssessedPoint {
                settings: SettingCombination::new(),
                firmware: None,
                wb_mode: None,
            }],
            combination_basis: CombinationBasis::Enumerated,
            defaults_assessed: true,
            output_encoding: OutputEncoding::Srgb,
            colour_cells: Vec::new(),
            exposure_cells: Vec::new(),
            assessed_bodies: bodies,
            assessed_domain: DomainId::new(CROSS_BODY_DOMAIN).expect("domain"),
        }
    }

    fn scope() -> ReportScope {
        scope_with(vec![
            BodyId::new("fujifilm/x-t5").expect("body"),
            BodyId::new("sony/a7").expect("body"),
        ])
    }

    fn engine_prerequisite() -> looklab_schema::EnginePrerequisite {
        let mut metrics = ReportMetrics::new();
        metrics.insert(MetricKey::RecoveryRate, 1.0);
        metrics.insert(MetricKey::Coverage, 1.0);
        metrics.insert(MetricKey::AssessedCells, 1.0);
        metrics.insert(MetricKey::SupportedCells, 1.0);
        let mut supported_mask = SupportedMask::new();
        supported_mask.insert(CellId::new("cell-0").expect("cell"));
        looklab_schema::EnginePrerequisite {
            engine: SourceRef::new("looklab-engine@test").expect("engine"),
            rule_revision: RuleRevision::new(crate::assessment::ENGINE_COVERAGE_RULE)
                .expect("rule"),
            outcome: AssessmentOutcome::Pass,
            metrics,
            supported_mask,
            limitations: Vec::new(),
        }
    }

    fn report(
        family: &LookId,
        target: &FamilyTargetId,
        outcome: AssessmentOutcome,
    ) -> ProtocolAssessment {
        report_with(family, target, outcome, 1, scope())
    }

    fn report_with(
        family: &LookId,
        target: &FamilyTargetId,
        outcome: AssessmentOutcome,
        seed: u64,
        scope: ReportScope,
    ) -> ProtocolAssessment {
        let sources = source_identity::sources();
        let binding = AssessmentBinding {
            subject: ModelSubject::SharedFamily {
                family: family.clone(),
            },
            candidate: ModelVersionId::SharedFamily {
                target: target.clone(),
            },
            scope,
            provenance: ReportProvenance {
                rule_revision: RuleRevision::new(CROSS_BODY_RULE).expect("rule"),
                fitter: sources.fitter.clone(),
                evaluator: sources.evaluator.clone(),
                model: sources.model.clone(),
                probe_set: ProbeSetId::new("v1").expect("probes"),
                effective_options: Sha256Hex::from_bytes([0; 32]),
                partition_policy: Sha256Hex::from_bytes([1; 32]),
                assessment_seeds: vec![seed],
            },
            dependencies: Vec::new(),
        };
        let mut metrics = ReportMetrics::new();
        let mut supported_mask = SupportedMask::new();
        if outcome == AssessmentOutcome::Pass {
            metrics.insert(MetricKey::CrossBodyMedianDisagreementDe, 0.5);
            metrics.insert(MetricKey::Coverage, 1.0);
            metrics.insert(MetricKey::AssessedCells, 1.0);
            metrics.insert(MetricKey::SupportedCells, 1.0);
            supported_mask.insert(CellId::new("cell-0").expect("cell"));
        }
        build_report(
            &binding,
            ReportParts {
                capability: QualificationCapability::CrossBodyAdmission,
                rule: RuleRevision::new(CROSS_BODY_RULE).expect("rule"),
                outcome,
                metrics,
                supported_mask,
                limitations: Vec::new(),
                evidence_requests: Vec::new(),
                extra_dependencies: vec![crate::assessment::engine_dependency(
                    &engine_prerequisite(),
                )],
                membership: EvidenceMembership::default(),
                cohort: None,
            },
        )
    }

    async fn register_report_dependencies(pool: &SqlitePool, assessment: &ProtocolAssessment) {
        let mut conn = pool.acquire().await.expect("connection");
        model_store::observe_dependency_snapshot(&mut conn, &assessment.dependencies, 0)
            .await
            .expect("report dependencies");
    }

    async fn queued_reassessment(pool: &SqlitePool, target: &FamilyTargetId) -> bool {
        let payloads: Vec<String> = sqlx::query_scalar(
            "SELECT payload FROM jobs WHERE kind = 'model_assessment' AND status = 'queued'",
        )
        .fetch_all(pool)
        .await
        .expect("queued assessments");
        payloads.into_iter().any(|payload| {
            let Ok(JobPayload::ModelAssessment { request }) =
                JobPayload::decode(&crate::jobs::JobKind::ModelAssessment, &payload)
            else {
                return false;
            };
            matches!(
                *request,
                AssessmentJob::Camera {
                    candidate: ModelVersionId::SharedFamily { target: queued },
                    ..
                } if &queued == target
            )
        })
    }

    #[tokio::test]
    async fn a_passing_family_report_becomes_current_and_contrary_evidence_invalidates_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = dir.path().join("looklab.sqlite3");
        let pool = open_database(&db).await.expect("database");
        let family = family();
        family_version(&pool, family.as_str(), 1).await;
        let target = target();
        let ownership = JobOwnership {
            id: crate::jobs::JobId::new(),
            attempts: 1,
        };
        let server_job = ownership.server_id();
        running_job(&pool, &server_job).await;
        let storage: NumericalSources = source_identity::sources().storage();
        let seed_body = BodyId::new("fujifilm/x-t5").expect("body");
        {
            let mut conn = pool.acquire().await.expect("connection");
            let outcome = model_store::seed_family_target(
                &mut conn,
                &FamilySeed {
                    family: &family,
                    sources: &storage,
                    expected_anchor: None,
                    target: &target,
                    safety: CandidateSafety {
                        numerical: SafetyCheck::Passed,
                        release_gates: SafetyCheck::Passed,
                    },
                    source_limited: true,
                    seed_bodies: std::slice::from_ref(&seed_body),
                    dependencies: &[],
                    lease: JobLease {
                        job: &server_job,
                        attempts: 1,
                    },
                    snapshot: &[],
                    observed_pending: PendingGeneration::new(0),
                },
                0,
            )
            .await
            .expect("seed");
            assert!(matches!(
                outcome,
                model_store::FamilySelectionOutcome::Selected
            ));
            model_store::record_target_eligibility(
                &mut conn,
                &TargetEligibility {
                    id: EligibilitySnapshotId::from_digest([0; 32]),
                    target: PublishedTargetId {
                        look: family.clone(),
                        version: target.version,
                    },
                    basis: TargetBasis::NativeFamily,
                    artifacts: Vec::new(),
                    base_pin: None,
                    support_uncertainty_source: None,
                    status: TargetEligibilityStatus::Eligible,
                    reasons: Vec::new(),
                    dependencies: Vec::new(),
                },
                0,
            )
            .await
            .expect("eligibility");
            model_store::insert_engine_prerequisite(&mut conn, &engine_prerequisite(), 0)
                .await
                .expect("engine prerequisite");
        }

        let subject = ModelSubject::SharedFamily {
            family: family.clone(),
        };
        let candidate = ModelVersionId::SharedFamily {
            target: target.clone(),
        };
        let pass = report(&family, &target, AssessmentOutcome::Pass);
        let pass_id = pass.report_id;
        let scope_key = pass.report.scope.key();
        register_report_dependencies(&pool, &pass).await;
        persist_report(
            &pool,
            &subject,
            &candidate,
            QualificationCapability::CrossBodyAdmission,
            &pass,
            &[],
            &[],
            PendingGeneration::new(2),
            &ownership,
        )
        .await
        .expect("passing report persists");
        {
            let mut conn = pool.acquire().await.expect("connection");
            let projection = model_store::current_projection(
                &mut conn,
                &subject.key(),
                ProjectionUse::QualifiedMatch,
                QualificationCapability::CrossBodyAdmission,
                &scope_key,
            )
            .await
            .expect("projection")
            .expect("current projection");
            assert_eq!(projection.selected_report, Some(pass_id));
            assert_eq!(projection.selected_candidate, Some(candidate.clone()));
            assert_eq!(projection.pending_generation, PendingGeneration::new(2));
            assert!(projection.applicability.is_current());
            let head = model_store::family_head(&mut conn, &family)
                .await
                .expect("head")
                .expect("family head");
            assert_eq!(head.report, Some(pass_id));
        }

        let stale = report_with(&family, &target, AssessmentOutcome::Pass, 2, scope());
        register_report_dependencies(&pool, &stale).await;
        persist_report(
            &pool,
            &subject,
            &candidate,
            QualificationCapability::CrossBodyAdmission,
            &stale,
            &[],
            &[],
            PendingGeneration::new(1),
            &ownership,
        )
        .await
        .expect("stale report persists");
        {
            let mut conn = pool.acquire().await.expect("connection");
            let projection = model_store::current_projection(
                &mut conn,
                &subject.key(),
                ProjectionUse::QualifiedMatch,
                QualificationCapability::CrossBodyAdmission,
                &scope_key,
            )
            .await
            .expect("projection")
            .expect("current projection");
            assert_eq!(
                projection.selected_report,
                Some(pass_id),
                "an older generation must not replace a newer selection"
            );
            assert_eq!(projection.pending_generation, PendingGeneration::new(2));
        }

        let other = FamilyTargetId {
            family: family.clone(),
            version: TargetVersion::new(2).expect("version"),
        };
        let changed = scope_with(vec![
            BodyId::new("fujifilm/x-t5").expect("body"),
            BodyId::new("sony/a7").expect("body"),
            BodyId::new("nikon/z8").expect("body"),
        ]);
        {
            let mut conn = pool.acquire().await.expect("connection");
            for dependency in source_identity::sources().dependencies() {
                model_store::register_dependency(&mut conn, &dependency, 0)
                    .await
                    .expect("source dependency");
            }
            for target in [
                DependencyTarget::root_weighting_global(),
                DependencyTarget::root_weighting_family(&family),
                DependencyTarget::root_weighting_body(&seed_body),
            ] {
                model_store::register_dependency(
                    &mut conn,
                    &DependencyRef::required(DependencyKind::RootWeighting, target),
                    0,
                )
                .await
                .expect("root weighting");
            }
        }
        let other_fail = report_with(&family, &other, AssessmentOutcome::Fail, 3, changed.clone());
        register_report_dependencies(&pool, &other_fail).await;
        persist_report(
            &pool,
            &subject,
            &ModelVersionId::SharedFamily {
                target: other.clone(),
            },
            QualificationCapability::CrossBodyAdmission,
            &other_fail,
            &[],
            &[],
            PendingGeneration::new(3),
            &ownership,
        )
        .await
        .expect("replacement failure persists");
        {
            let mut conn = pool.acquire().await.expect("connection");
            let projection = model_store::current_projection(
                &mut conn,
                &subject.key(),
                ProjectionUse::QualifiedMatch,
                QualificationCapability::CrossBodyAdmission,
                &scope_key,
            )
            .await
            .expect("projection")
            .expect("current projection");
            assert!(
                projection.applicability.is_current(),
                "a failed replacement must not invalidate the selected predecessor"
            );
        }
        reassess_selected_after_failure(
            &pool,
            &subject,
            &ModelVersionId::SharedFamily { target: other },
            QualificationCapability::CrossBodyAdmission,
            &changed,
            0,
        )
        .await
        .expect("reassessment");
        assert!(
            queued_reassessment(&pool, &target).await,
            "a failed replacement must reassess the overlapping selected target"
        );

        let fail = report_with(&family, &target, AssessmentOutcome::Fail, 4, changed);
        register_report_dependencies(&pool, &fail).await;
        persist_report(
            &pool,
            &subject,
            &candidate,
            QualificationCapability::CrossBodyAdmission,
            &fail,
            &[],
            &[],
            PendingGeneration::new(4),
            &ownership,
        )
        .await
        .expect("contrary report persists");
        {
            let mut conn = pool.acquire().await.expect("connection");
            let projection = model_store::current_projection(
                &mut conn,
                &subject.key(),
                ProjectionUse::QualifiedMatch,
                QualificationCapability::CrossBodyAdmission,
                &scope_key,
            )
            .await
            .expect("projection")
            .expect("projection row");
            assert!(!projection.applicability.is_current());
            let head = model_store::family_head(&mut conn, &family)
                .await
                .expect("head")
                .expect("family head");
            assert!(!head.applicability.is_current());
        }
    }
}
