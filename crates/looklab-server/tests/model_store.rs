// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_schema::{
    Applicability, ArtifactBinding, ArtifactRole, AssessedPoint, AssessmentOutcome, BodyId,
    BodyRealizationId, BodyRealizationVersionId, BodyStatePins, BodyStateRecord, BodyStateStage,
    BodyStateVersion, BundleExclusion, BundleSimEntry, CandidateSafety, CellId,
    CohortAdmissionRequest, CohortMember, CohortRole, CombinationBasis, ControlEpochId,
    DependencyGeneration, DependencyKind, DependencyRef, DependencyTarget, DomainId,
    EligibilitySnapshotId, EnginePrerequisite, EvidenceRequest, EvidenceRequestKind,
    FamilyTargetId, FirmwareId, FirmwareVersion, MatchBundleRevision, MatchBundleRevisionId,
    MetricKey, ModelReport, ModelReportId, ModelVersionId, OutputEncoding, PendingGeneration,
    ProbeSetId, ProjectionUse, PublishedTargetId, QualificationCapability, RealizationRevisionId,
    RealizationStatus, RealizationVersion, ReportMetrics, ReportProvenance, ReportScope,
    RuleRevision, SafetyCheck, ScopeKey, SettingCombination, Sha256Hex, SimKey, SourceRef,
    SubjectKey, SupportedMask, TargetBasis, TargetEligibility, TargetEligibilityReason,
    TargetEligibilityStatus, TargetVersion,
};
use looklab_server::model_store::{
    self, BundleSelectionOutcome, DiagnosticImageStats, DiagnosticVerdict,
    DiagnosticWeightComponents, EvidenceGroup, EvidenceGroupId, EvidenceMembership,
    EvidenceRequestOrigin, EvidenceRole, ExpectedFamilySelection, ExpectedSelection, FamilyAdvance,
    FamilyCohortInapplicable, FamilyCohortState, FamilyProjectionOutcome, FamilySeed,
    FamilySelection, FamilySelectionOutcome, FamilySelectionRef, FamilySelectionRefOwned, JobLease,
    NewBundleRevision, NewRealizationRevision, NumericalSources, RealizationSelection,
    RealizationStage, SelectionOutcome, SelectionRef, StoreError,
};
use looklab_server::{JobId, open_database};
use sqlx::SqlitePool;
use tempfile::TempDir;

async fn fixture() -> (TempDir, SqlitePool) {
    let dir = TempDir::new().expect("temp dir");
    let pool = open_database(&dir.path().join("looklab.sqlite3"))
        .await
        .expect("database");
    (dir, pool)
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

fn body(name: &str) -> BodyId {
    BodyId::new(name).expect("body")
}

fn realization(epoch: u32, sim: &str) -> BodyRealizationId {
    BodyRealizationId {
        body: body("fujifilm/x-t5"),
        epoch: ControlEpochId::new(epoch),
        native_sim: SimKey::new(sim).expect("sim"),
    }
}

fn family_target(version: u32) -> FamilyTargetId {
    FamilyTargetId {
        family: looklab_schema::LookId::new("classic-chrome").expect("look"),
        version: TargetVersion::new(version).expect("version"),
    }
}

fn transform_dependency() -> DependencyRef {
    DependencyRef::required(
        DependencyKind::Transform,
        DependencyTarget::transform(&body("fujifilm/x-t5"), 1),
    )
}

async fn ensure_body_state(conn: &mut sqlx::SqliteConnection, body: &BodyId) -> BodyStateVersion {
    if let Some(state) = model_store::latest_body_state(conn, body, BodyStateStage::Native)
        .await
        .expect("body state")
    {
        return state.version;
    }
    let version = model_store::reserve_body_state(conn, body, BodyStateStage::Native, 0)
        .await
        .expect("body state reservation");
    model_store::insert_body_state(
        conn,
        &BodyStateRecord {
            body: body.clone(),
            version,
            stage: BodyStateStage::Native,
            pins: BodyStatePins {
                profile: None,
                input_transform: ArtifactBinding {
                    sha256: Sha256Hex::from_bytes([5; 32]),
                    role: ArtifactRole::InputTransform,
                },
                body_params: ArtifactBinding {
                    sha256: Sha256Hex::from_bytes([6; 32]),
                    role: ArtifactRole::BodyRealizationParams,
                },
                gauge: ArtifactBinding {
                    sha256: Sha256Hex::from_bytes([7; 32]),
                    role: ArtifactRole::Gauge,
                },
            },
            dependencies: Vec::new(),
        },
        &[],
        0,
    )
    .await
    .expect("body state");
    version
}

fn test_sources() -> NumericalSources {
    NumericalSources {
        fitter: SourceRef::new("fit@1").expect("fitter"),
        evaluator: SourceRef::new("evaluator@1").expect("evaluator"),
        model: SourceRef::new("model@1").expect("model"),
    }
}

async fn insert_revision(
    pool: &SqlitePool,
    subject: &BodyRealizationId,
    target: &FamilyTargetId,
    status: RealizationStatus,
    dependencies: &[DependencyRef],
) -> RealizationVersion {
    let mut tx = model_store::begin_immediate(pool)
        .await
        .expect("transaction");
    let body_state_version = ensure_body_state(&mut tx, &subject.body).await;
    let (_, version) = model_store::reserve_realization_version(&mut tx, subject, target, 0)
        .await
        .expect("reservation");
    let artifacts = [
        ArtifactBinding {
            sha256: Sha256Hex::from_bytes([2; 32]),
            role: ArtifactRole::Look,
        },
        ArtifactBinding {
            sha256: Sha256Hex::from_bytes([3; 32]),
            role: ArtifactRole::ControlResponse,
        },
    ];
    let version = model_store::insert_realization_revision(
        &mut tx,
        &NewRealizationRevision {
            subject,
            version,
            stage: RealizationStage::Native,
            target,
            body_state_version,
            input_transform_version: 1,
            realization_params_version: 1,
            control_response_version: 1,
            clip_k: 1.0,
            status,
            status_reason: None,
            safety: CandidateSafety {
                numerical: SafetyCheck::Passed,
                release_gates: SafetyCheck::Passed,
            },
            artifacts: &artifacts,
            dependencies,
        },
        0,
    )
    .await
    .expect("revision");
    tx.commit().await.expect("commit");
    version
}

async fn selection_snapshot(
    conn: &mut sqlx::SqliteConnection,
    revision: &RealizationRevisionId,
    report: Option<&ModelReportId>,
) -> Vec<DependencyGeneration> {
    let mut dependencies =
        model_store::dependencies(conn, "realization_revision", revision.as_str())
            .await
            .expect("revision dependencies");
    if let Some(report) = report {
        dependencies.extend(
            model_store::dependencies(conn, "report", &report.to_hex())
                .await
                .expect("report dependencies"),
        );
    }
    model_store::dependency_snapshot(conn, &dependencies)
        .await
        .expect("snapshot")
}

fn provenance() -> ReportProvenance {
    ReportProvenance {
        rule_revision: RuleRevision::new("rule/1").expect("rule"),
        fitter: SourceRef::new("fit@1").expect("fitter"),
        evaluator: SourceRef::new("evaluator@1").expect("evaluator"),
        model: SourceRef::new("model@1").expect("model"),
        probe_set: ProbeSetId::new("v1").expect("probes"),
        effective_options: Sha256Hex::from_bytes([0; 32]),
        partition_policy: Sha256Hex::from_bytes([1; 32]),
        assessment_seeds: vec![1],
    }
}

fn scope(domain: &str, bodies: Vec<BodyId>) -> ReportScope {
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
        assessed_bodies: bodies,
        assessed_domain: DomainId::new(domain).expect("domain"),
    }
}

fn pass_report(
    subject: looklab_schema::ModelSubject,
    candidate: ModelVersionId,
    capability: QualificationCapability,
    scope: ReportScope,
) -> ModelReport {
    let mut metrics = ReportMetrics::new();
    metrics.insert(MetricKey::SupportedCells, 1.0);
    metrics.insert(MetricKey::AssessedCells, 1.0);
    metrics.insert(MetricKey::Coverage, 1.0);
    match capability {
        QualificationCapability::NativePrediction => {
            metrics.insert(MetricKey::MedianDe, 1.0);
        }
        QualificationCapability::ControlsMatching => {
            metrics.insert(MetricKey::MedianDePostClip, 1.0);
        }
        QualificationCapability::CrossBodyAdmission => {
            metrics.insert(MetricKey::CrossBodyMedianDisagreementDe, 1.0);
        }
    }
    let mut supported_mask = SupportedMask::new();
    supported_mask.insert(CellId::new("neutral").expect("cell"));
    ModelReport {
        subject,
        candidate,
        capability,
        scope,
        provenance: provenance(),
        outcome: AssessmentOutcome::Pass,
        metrics,
        supported_mask,
        limitations: Vec::new(),
        evidence_requests: Vec::<EvidenceRequest>::new(),
    }
}

fn eligible_target(target: &FamilyTargetId) -> TargetEligibility {
    TargetEligibility {
        id: EligibilitySnapshotId::from_digest([0; 32]),
        target: PublishedTargetId::from(target),
        basis: TargetBasis::NativeFamily,
        artifacts: Vec::<ArtifactBinding>::new(),
        base_pin: None,
        support_uncertainty_source: None,
        status: TargetEligibilityStatus::Eligible,
        reasons: Vec::new(),
        dependencies: Vec::new(),
    }
}

async fn publishable_target(pool: &SqlitePool, target: &FamilyTargetId) {
    let job = JobId::new();
    running_job(pool, &job).await;
    let mut conn = pool.acquire().await.expect("connection");
    let outcome = model_store::seed_family_target(
        &mut conn,
        &FamilySeed {
            family: &target.family,
            sources: &test_sources(),
            expected_anchor: None,
            target,
            safety: CandidateSafety {
                numerical: SafetyCheck::Passed,
                release_gates: SafetyCheck::Passed,
            },
            source_limited: true,
            seed_bodies: &[body("fujifilm/x-t5")],
            dependencies: &[],
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &[],
            observed_pending: PendingGeneration::new(0),
        },
        0,
    )
    .await
    .expect("seed");
    assert_eq!(outcome, FamilySelectionOutcome::Selected);
    model_store::record_target_eligibility(&mut conn, &eligible_target(target), 0)
        .await
        .expect("eligibility");
}

struct CrossBodyPass {
    admission: ModelReportId,
    member: BodyRealizationId,
    member_version: RealizationVersion,
    member_report: ModelReportId,
    member_scope: ReportScope,
    member_mask: SupportedMask,
    domain: DomainId,
}

fn cross_body_scopes(domain: &str) -> (ReportScope, ReportScope) {
    let firmware_a = FirmwareId {
        body: body("fujifilm/x-t5"),
        epoch: ControlEpochId::new(1),
        version: "1.00".parse::<FirmwareVersion>().expect("firmware"),
    };
    let firmware_b = FirmwareId {
        body: body("sony/a7"),
        epoch: ControlEpochId::new(1),
        version: "1.00".parse::<FirmwareVersion>().expect("firmware"),
    };
    let global_scope = ReportScope {
        points: vec![
            AssessedPoint {
                settings: SettingCombination::new(),
                firmware: Some(firmware_a.clone()),
                wb_mode: None,
            },
            AssessedPoint {
                settings: SettingCombination::new(),
                firmware: Some(firmware_b),
                wb_mode: None,
            },
        ],
        combination_basis: CombinationBasis::Enumerated,
        defaults_assessed: true,
        output_encoding: OutputEncoding::Srgb,
        colour_cells: vec![CellId::new("neutral").expect("cell")],
        exposure_cells: vec![CellId::new("ev0").expect("cell")],
        assessed_bodies: vec![body("fujifilm/x-t5"), body("sony/a7")],
        assessed_domain: DomainId::new(domain).expect("domain"),
    };
    let member_scope = ReportScope {
        points: vec![AssessedPoint {
            settings: SettingCombination::new(),
            firmware: Some(firmware_a),
            wb_mode: None,
        }],
        combination_basis: CombinationBasis::Enumerated,
        defaults_assessed: true,
        output_encoding: OutputEncoding::Srgb,
        colour_cells: vec![CellId::new("neutral").expect("cell")],
        exposure_cells: vec![CellId::new("ev0").expect("cell")],
        assessed_bodies: vec![body("fujifilm/x-t5")],
        assessed_domain: DomainId::new(domain).expect("domain"),
    };
    (global_scope, member_scope)
}

async fn cross_body_pass(
    pool: &SqlitePool,
    target: &FamilyTargetId,
    assessment_seeds: Vec<u64>,
    domain: &str,
    created_at: i64,
) -> CrossBodyPass {
    let (global_scope, member_scope) = cross_body_scopes(domain);
    let mut conn = pool.acquire().await.expect("connection");
    let engine = EnginePrerequisite {
        engine: SourceRef::new("engine@1").expect("engine"),
        rule_revision: RuleRevision::new("p4.3/1").expect("rule"),
        outcome: AssessmentOutcome::Pass,
        metrics: {
            let mut metrics = ReportMetrics::new();
            metrics.insert(MetricKey::RecoveryRate, 0.9);
            metrics.insert(MetricKey::Coverage, 0.9);
            metrics.insert(MetricKey::AssessedCells, 1.0);
            metrics.insert(MetricKey::SupportedCells, 1.0);
            metrics
        },
        supported_mask: {
            let mut mask = SupportedMask::new();
            mask.insert(CellId::new("latent-1").expect("cell"));
            mask
        },
        limitations: Vec::new(),
    };
    let engine_id = model_store::insert_engine_prerequisite(&mut conn, &engine, 0)
        .await
        .expect("engine prerequisite");
    let mut report = pass_report(
        looklab_schema::ModelSubject::SharedFamily {
            family: target.family.clone(),
        },
        ModelVersionId::SharedFamily {
            target: target.clone(),
        },
        QualificationCapability::CrossBodyAdmission,
        global_scope,
    );
    report.provenance.assessment_seeds = assessment_seeds;
    let admission_dependencies = vec![
        DependencyRef::required(
            DependencyKind::FamilyVersion,
            DependencyTarget::family_version(target),
        ),
        DependencyRef::required(
            DependencyKind::EngineQualification,
            DependencyTarget::engine_qualification(&engine_id),
        ),
    ];
    let admission = model_store::insert_report(
        &mut conn,
        &report,
        &admission_dependencies,
        &[],
        &EvidenceMembership::default(),
        created_at,
    )
    .await
    .expect("admission report");
    let member = realization(1, "classic_chrome");
    let member_version =
        insert_revision(pool, &member, target, RealizationStatus::Candidate, &[]).await;
    let mut member_report = pass_report(
        looklab_schema::ModelSubject::BodyRealization {
            realization: member.clone(),
        },
        ModelVersionId::BodyRealization {
            realization: BodyRealizationVersionId {
                realization: member.clone(),
                version: member_version,
                target: target.clone(),
            },
        },
        QualificationCapability::NativePrediction,
        member_scope.clone(),
    );
    member_report
        .supported_mask
        .insert(CellId::new("ev0").expect("cell"));
    member_report.metrics.insert(MetricKey::SupportedCells, 2.0);
    member_report.metrics.insert(MetricKey::AssessedCells, 2.0);
    let member_mask = member_report.supported_mask.clone();
    let member_report = model_store::insert_report(
        &mut conn,
        &member_report,
        &[],
        &[],
        &EvidenceMembership::default(),
        created_at,
    )
    .await
    .expect("member report");
    CrossBodyPass {
        admission,
        member,
        member_version,
        member_report,
        member_scope,
        member_mask,
        domain: DomainId::new(domain).expect("domain"),
    }
}

async fn admit(pool: &SqlitePool, target: &FamilyTargetId, pass: &CrossBodyPass) {
    let mut conn = pool.acquire().await.expect("connection");
    model_store::record_cohort_admission(
        &mut conn,
        &CohortAdmissionRequest {
            family: target.family.clone(),
            target: target.clone(),
            report: pass.admission,
            members: vec![CohortMember::Admitted {
                realization: BodyRealizationVersionId {
                    realization: pass.member.clone(),
                    version: pass.member_version,
                    target: target.clone(),
                },
                report: pass.member_report,
                scope: Box::new(pass.member_scope.clone()),
                supported_mask: pass.member_mask.clone(),
            }],
            assessed_domain: pass.domain.clone(),
        },
        1,
    )
    .await
    .expect("admission");
}

struct QualifiedRealization {
    key: SubjectKey,
    revision: RealizationRevisionId,
    report: ModelReportId,
    scope_key: ScopeKey,
}

async fn qualified_realization(
    pool: &SqlitePool,
    target: &FamilyTargetId,
    domain: &str,
) -> QualifiedRealization {
    let subject = realization(1, "classic_chrome");
    let key = SubjectKey::for_realization(&subject);
    let version = insert_revision(
        pool,
        &subject,
        target,
        RealizationStatus::Candidate,
        &[transform_dependency()],
    )
    .await;
    let revision = RealizationRevisionId::for_version(&BodyRealizationVersionId {
        realization: subject.clone(),
        version,
        target: target.clone(),
    });
    let assessed = scope(domain, vec![body("fujifilm/x-t5")]);
    let report = pass_report(
        looklab_schema::ModelSubject::BodyRealization {
            realization: subject.clone(),
        },
        ModelVersionId::BodyRealization {
            realization: BodyRealizationVersionId {
                realization: subject,
                version,
                target: target.clone(),
            },
        },
        QualificationCapability::ControlsMatching,
        assessed.clone(),
    );
    let mut conn = pool.acquire().await.expect("connection");
    let report = model_store::insert_report(
        &mut conn,
        &report,
        &[transform_dependency()],
        &[],
        &EvidenceMembership::default(),
        0,
    )
    .await
    .expect("report");
    QualifiedRealization {
        key,
        revision,
        report,
        scope_key: assessed.key(),
    }
}

async fn select_qualified(
    conn: &mut sqlx::SqliteConnection,
    qualified: &QualifiedRealization,
    target: &FamilyTargetId,
    job: &JobId,
    expected: ExpectedSelection<'_>,
    now: i64,
) -> SelectionOutcome {
    let snapshot = selection_snapshot(conn, &qualified.revision, Some(&qualified.report)).await;
    model_store::select_realization(
        conn,
        &RealizationSelection {
            subject: &qualified.key,
            sources: &test_sources(),
            expected_anchor: None,
            use_: ProjectionUse::QualifiedMatch,
            capability: QualificationCapability::ControlsMatching,
            scope_key: &qualified.scope_key,
            revision: &qualified.revision,
            target,
            report: Some(&qualified.report),
            supersedes: &[],
            expected,
            lease: JobLease { job, attempts: 1 },
            snapshot: &snapshot,
            observed_pending: PendingGeneration::new(0),
        },
        now,
    )
    .await
    .expect("selection")
}

/// The family-selection case `select_family_report` drives through `model_store::select_family`.
struct FamilySelectionCase<'a> {
    target: &'a FamilyTargetId,
    report: &'a ModelReportId,
    scope_key: &'a ScopeKey,
    job: &'a JobId,
    sources: &'a NumericalSources,
    expected: ExpectedFamilySelection<'a>,
    observed: PendingGeneration,
    now: i64,
}

async fn select_family_report(
    conn: &mut sqlx::SqliteConnection,
    case: FamilySelectionCase<'_>,
) -> FamilyProjectionOutcome {
    let FamilySelectionCase {
        target,
        report,
        scope_key,
        job,
        sources,
        expected,
        observed,
        now,
    } = case;
    let dependencies = model_store::dependencies(conn, "report", &report.to_hex())
        .await
        .expect("dependencies");
    let snapshot = model_store::dependency_snapshot(conn, &dependencies)
        .await
        .expect("snapshot");
    model_store::select_family(
        conn,
        &FamilySelection {
            family: &target.family,
            sources,
            expected_anchor: None,
            target,
            report,
            scope_key,
            expected,
            lease: JobLease { job, attempts: 1 },
            snapshot: &snapshot,
            observed_pending: observed,
        },
        now,
    )
    .await
    .expect("family selection")
}

#[tokio::test]
async fn realization_versions_are_subject_scoped() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    let target = family_target(1);
    let epoch_one = realization(1, "classic_chrome");
    let epoch_two = realization(2, "provia");
    let dependencies = [transform_dependency()];
    let first = insert_revision(
        &pool,
        &epoch_one,
        &target,
        RealizationStatus::Candidate,
        &dependencies,
    )
    .await;
    let second = insert_revision(
        &pool,
        &epoch_two,
        &target,
        RealizationStatus::Candidate,
        &dependencies,
    )
    .await;
    assert_eq!(first.get(), 1);
    assert_eq!(second.get(), 1);

    let mut conn = pool.acquire().await.expect("connection");
    let only_epoch_one = model_store::realization_revisions(
        &mut conn,
        Some(&SubjectKey::for_realization(&epoch_one)),
    )
    .await
    .expect("revisions");
    assert_eq!(only_epoch_one.len(), 1);
    assert_eq!(only_epoch_one[0].subject, epoch_one);
    assert_eq!(only_epoch_one[0].target, target);
}

#[tokio::test]
async fn reports_are_immutable_and_evidence_scoped() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    let target = family_target(1);
    let realization = realization(1, "classic_chrome");
    let version = insert_revision(
        &pool,
        &realization,
        &target,
        RealizationStatus::Candidate,
        &[transform_dependency()],
    )
    .await;
    let candidate = ModelVersionId::BodyRealization {
        realization: BodyRealizationVersionId {
            realization: realization.clone(),
            version,
            target: target.clone(),
        },
    };
    let report = pass_report(
        looklab_schema::ModelSubject::BodyRealization { realization },
        candidate,
        QualificationCapability::ControlsMatching,
        scope("controls", vec![body("fujifilm/x-t5")]),
    );
    let dependencies = [transform_dependency()];
    let mut conn = pool.acquire().await.expect("connection");
    let first = model_store::insert_report(
        &mut conn,
        &report,
        &dependencies,
        &[],
        &EvidenceMembership::default(),
        0,
    )
    .await
    .expect("report");
    let second = model_store::insert_report(
        &mut conn,
        &report,
        &dependencies,
        &[],
        &EvidenceMembership::default(),
        1,
    )
    .await
    .expect("idempotent report");
    assert_eq!(first, second);
    assert_eq!(
        model_store::report_ids(&mut conn).await.expect("ids").len(),
        1
    );

    let mut evidence = EvidenceMembership::default();
    evidence.groups.push(EvidenceGroup {
        group: EvidenceGroupId::new("rerender-1").expect("group"),
        role: EvidenceRole::Training,
    });
    let third = model_store::insert_report(&mut conn, &report, &dependencies, &[], &evidence, 2)
        .await
        .expect("evidence-scoped report");
    assert_ne!(first, third);
    assert_eq!(
        model_store::report_ids(&mut conn).await.expect("ids").len(),
        2
    );

    let rewritten = sqlx::query("UPDATE model_reports SET outcome = 'fail' WHERE id = ?")
        .bind(first.to_hex())
        .execute(&mut *conn)
        .await;
    assert!(rewritten.is_err());
}

#[tokio::test]
async fn failed_replacements_keep_the_valid_predecessor() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    let target = family_target(1);
    publishable_target(&pool, &target).await;
    let subject = realization(1, "classic_chrome");
    let key = SubjectKey::for_realization(&subject);
    let dependencies = [transform_dependency()];
    let good = insert_revision(
        &pool,
        &subject,
        &target,
        RealizationStatus::Candidate,
        &dependencies,
    )
    .await;
    let good_id = RealizationRevisionId::for_version(&BodyRealizationVersionId {
        realization: subject.clone(),
        version: good,
        target: target.clone(),
    });
    let job = JobId::new();
    running_job(&pool, &job).await;
    let mut conn = pool.acquire().await.expect("connection");
    let snapshot = selection_snapshot(&mut conn, &good_id, None).await;
    let selected = model_store::select_realization(
        &mut conn,
        &RealizationSelection {
            subject: &key,
            sources: &test_sources(),
            expected_anchor: None,
            use_: ProjectionUse::ExperimentalMatch,
            capability: QualificationCapability::NativePrediction,
            scope_key: &ScopeKey::all(),
            revision: &good_id,
            target: &target,
            report: None,
            supersedes: &[],
            expected: ExpectedSelection::Absent,
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &snapshot,
            observed_pending: PendingGeneration::new(0),
        },
        0,
    )
    .await
    .expect("selection");
    assert_eq!(selected, SelectionOutcome::Selected);

    let diagnostic = insert_revision(
        &pool,
        &subject,
        &target,
        RealizationStatus::Diagnostic,
        &dependencies,
    )
    .await;
    let diagnostic_id = RealizationRevisionId::for_version(&BodyRealizationVersionId {
        realization: subject.clone(),
        version: diagnostic,
        target: target.clone(),
    });
    let rejected = model_store::select_realization(
        &mut conn,
        &RealizationSelection {
            subject: &key,
            sources: &test_sources(),
            expected_anchor: None,
            use_: ProjectionUse::ExperimentalMatch,
            capability: QualificationCapability::NativePrediction,
            scope_key: &ScopeKey::all(),
            revision: &diagnostic_id,
            target: &target,
            report: None,
            supersedes: &[],
            expected: ExpectedSelection::Selected(SelectionRef {
                revision: &good_id,
                report: None,
            }),
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &snapshot,
            observed_pending: PendingGeneration::new(0),
        },
        0,
    )
    .await
    .expect("rejection");
    assert!(matches!(
        rejected,
        SelectionOutcome::Rejected(
            looklab_server::model_store::SelectionRejection::NotAccepted(
                RealizationStatus::Diagnostic
            )
        )
    ));
    let projection = model_store::current_projection(
        &mut conn,
        &key,
        ProjectionUse::ExperimentalMatch,
        QualificationCapability::NativePrediction,
        &ScopeKey::all(),
    )
    .await
    .expect("projection")
    .expect("selected predecessor");
    assert_eq!(
        projection.selected_candidate,
        Some(ModelVersionId::BodyRealization {
            realization: BodyRealizationVersionId {
                realization: subject.clone(),
                version: good,
                target: target.clone(),
            },
        })
    );
}

#[tokio::test]
async fn stale_runs_cannot_roll_back_a_newer_selection() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    let target = family_target(1);
    publishable_target(&pool, &target).await;
    let subject = realization(1, "classic_chrome");
    let key = SubjectKey::for_realization(&subject);
    let dependencies = [transform_dependency()];
    let first = insert_revision(
        &pool,
        &subject,
        &target,
        RealizationStatus::Candidate,
        &dependencies,
    )
    .await;
    let second = insert_revision(
        &pool,
        &subject,
        &target,
        RealizationStatus::Candidate,
        &dependencies,
    )
    .await;
    let first_id = RealizationRevisionId::for_version(&BodyRealizationVersionId {
        realization: subject.clone(),
        version: first,
        target: target.clone(),
    });
    let second_id = RealizationRevisionId::for_version(&BodyRealizationVersionId {
        realization: subject.clone(),
        version: second,
        target: target.clone(),
    });
    let job = JobId::new();
    running_job(&pool, &job).await;
    let mut conn = pool.acquire().await.expect("connection");
    let snapshot = selection_snapshot(&mut conn, &first_id, None).await;

    let first_outcome = model_store::select_realization(
        &mut conn,
        &RealizationSelection {
            subject: &key,
            sources: &test_sources(),
            expected_anchor: None,
            use_: ProjectionUse::ExperimentalMatch,
            capability: QualificationCapability::NativePrediction,
            scope_key: &ScopeKey::all(),
            revision: &first_id,
            target: &target,
            report: None,
            supersedes: &[],
            expected: ExpectedSelection::Absent,
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &snapshot,
            observed_pending: PendingGeneration::new(0),
        },
        0,
    )
    .await
    .expect("first selection");
    assert_eq!(first_outcome, SelectionOutcome::Selected);
    let second_outcome = model_store::select_realization(
        &mut conn,
        &RealizationSelection {
            subject: &key,
            sources: &test_sources(),
            expected_anchor: None,
            use_: ProjectionUse::ExperimentalMatch,
            capability: QualificationCapability::NativePrediction,
            scope_key: &ScopeKey::all(),
            revision: &second_id,
            target: &target,
            report: None,
            supersedes: &[],
            expected: ExpectedSelection::Selected(SelectionRef {
                revision: &first_id,
                report: None,
            }),
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &snapshot,
            observed_pending: PendingGeneration::new(0),
        },
        0,
    )
    .await
    .expect("second selection");
    assert_eq!(second_outcome, SelectionOutcome::Selected);
    let stale = model_store::select_realization(
        &mut conn,
        &RealizationSelection {
            subject: &key,
            sources: &test_sources(),
            expected_anchor: None,
            use_: ProjectionUse::ExperimentalMatch,
            capability: QualificationCapability::NativePrediction,
            scope_key: &ScopeKey::all(),
            revision: &first_id,
            target: &target,
            report: None,
            supersedes: &[],
            expected: ExpectedSelection::Absent,
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &snapshot,
            observed_pending: PendingGeneration::new(0),
        },
        0,
    )
    .await
    .expect("stale selection");
    assert!(matches!(stale, SelectionOutcome::Stale { actual: Some(_) }));
}

#[tokio::test]
async fn invalidated_dependencies_fence_commits() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    let target = family_target(1);
    publishable_target(&pool, &target).await;
    let subject = realization(1, "classic_chrome");
    let key = SubjectKey::for_realization(&subject);
    let dependencies = [transform_dependency()];
    let version = insert_revision(
        &pool,
        &subject,
        &target,
        RealizationStatus::Candidate,
        &dependencies,
    )
    .await;
    let revision = RealizationRevisionId::for_version(&BodyRealizationVersionId {
        realization: subject.clone(),
        version,
        target: target.clone(),
    });
    let job = JobId::new();
    running_job(&pool, &job).await;
    let mut conn = pool.acquire().await.expect("connection");
    let snapshot = selection_snapshot(&mut conn, &revision, None).await;
    let baseline = model_store::select_realization(
        &mut conn,
        &RealizationSelection {
            subject: &key,
            sources: &test_sources(),
            expected_anchor: None,
            use_: ProjectionUse::ExperimentalMatch,
            capability: QualificationCapability::NativePrediction,
            scope_key: &ScopeKey::all(),
            revision: &revision,
            target: &target,
            report: None,
            supersedes: &[],
            expected: ExpectedSelection::Absent,
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &snapshot,
            observed_pending: PendingGeneration::new(0),
        },
        0,
    )
    .await
    .expect("baseline selection");
    assert_eq!(baseline, SelectionOutcome::Selected);
    let summary = model_store::invalidate(
        &mut conn,
        &transform_dependency(),
        looklab_schema::InvalidationReason::TransformReinterpretation,
        0,
    )
    .await
    .expect("invalidation");
    assert_eq!(summary.reports, 0);
    let outcome = model_store::select_realization(
        &mut conn,
        &RealizationSelection {
            subject: &key,
            sources: &test_sources(),
            expected_anchor: None,
            use_: ProjectionUse::ExperimentalMatch,
            capability: QualificationCapability::NativePrediction,
            scope_key: &ScopeKey::all(),
            revision: &revision,
            target: &target,
            report: None,
            supersedes: &[],
            expected: ExpectedSelection::Selected(SelectionRef {
                revision: &revision,
                report: None,
            }),
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &snapshot,
            observed_pending: PendingGeneration::new(0),
        },
        0,
    )
    .await
    .expect("fenced selection");
    assert!(
        matches!(
            outcome,
            SelectionOutcome::Rejected(
                looklab_server::model_store::SelectionRejection::DependencyAdvanced(_)
                    | looklab_server::model_store::SelectionRejection::RevokedDependency(_)
            )
        ),
        "expected a changed-source rejection, got {outcome:?}"
    );
    let projection = model_store::current_projection(
        &mut conn,
        &key,
        ProjectionUse::ExperimentalMatch,
        QualificationCapability::NativePrediction,
        &ScopeKey::all(),
    )
    .await
    .expect("projection")
    .expect("invalidated projection");
    assert_eq!(
        projection.applicability,
        Applicability::Invalidated {
            reason: looklab_schema::InvalidationReason::TransformReinterpretation
        }
    );
}

#[tokio::test]
async fn append_only_evidence_queues_without_invalidating() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    let target = family_target(1);
    publishable_target(&pool, &target).await;
    let subject = realization(1, "classic_chrome");
    let key = SubjectKey::for_realization(&subject);
    let dependencies = [transform_dependency()];
    let version = insert_revision(
        &pool,
        &subject,
        &target,
        RealizationStatus::Candidate,
        &dependencies,
    )
    .await;
    let revision = RealizationRevisionId::for_version(&BodyRealizationVersionId {
        realization: subject.clone(),
        version,
        target: target.clone(),
    });
    let job = JobId::new();
    running_job(&pool, &job).await;
    let mut conn = pool.acquire().await.expect("connection");
    let snapshot = selection_snapshot(&mut conn, &revision, None).await;
    let selected = model_store::select_realization(
        &mut conn,
        &RealizationSelection {
            subject: &key,
            sources: &test_sources(),
            expected_anchor: None,
            use_: ProjectionUse::ExperimentalMatch,
            capability: QualificationCapability::NativePrediction,
            scope_key: &ScopeKey::all(),
            revision: &revision,
            target: &target,
            report: None,
            supersedes: &[],
            expected: ExpectedSelection::Absent,
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &snapshot,
            observed_pending: PendingGeneration::new(0),
        },
        0,
    )
    .await
    .expect("selection");
    assert_eq!(selected, SelectionOutcome::Selected);

    let pending = model_store::note_evidence_arrival(&mut conn, &key, 1)
        .await
        .expect("pending");
    assert_eq!(pending.get(), 1);
    assert_eq!(
        model_store::applicability(
            &mut conn,
            &key,
            ProjectionUse::ExperimentalMatch,
            QualificationCapability::NativePrediction,
            &ScopeKey::all(),
        )
        .await
        .expect("applicability"),
        Applicability::Current
    );
    let reselected = model_store::select_realization(
        &mut conn,
        &RealizationSelection {
            subject: &key,
            sources: &test_sources(),
            expected_anchor: None,
            use_: ProjectionUse::ExperimentalMatch,
            capability: QualificationCapability::NativePrediction,
            scope_key: &ScopeKey::all(),
            revision: &revision,
            target: &target,
            report: None,
            supersedes: &[],
            expected: ExpectedSelection::Selected(SelectionRef {
                revision: &revision,
                report: None,
            }),
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &snapshot,
            observed_pending: pending,
        },
        1,
    )
    .await
    .expect("successor selection");
    assert_eq!(reselected, SelectionOutcome::Selected);
}

#[tokio::test]
async fn missing_dependency_generation_cannot_select() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    let target = family_target(1);
    publishable_target(&pool, &target).await;
    let subject = realization(1, "classic_chrome");
    let key = SubjectKey::for_realization(&subject);
    let version = insert_revision(
        &pool,
        &subject,
        &target,
        RealizationStatus::Candidate,
        &[transform_dependency()],
    )
    .await;
    let revision = RealizationRevisionId::for_version(&BodyRealizationVersionId {
        realization: subject,
        version,
        target: target.clone(),
    });
    let job = JobId::new();
    running_job(&pool, &job).await;
    let mut conn = pool.acquire().await.expect("connection");
    let outcome = model_store::select_realization(
        &mut conn,
        &RealizationSelection {
            subject: &key,
            sources: &test_sources(),
            expected_anchor: None,
            use_: ProjectionUse::ExperimentalMatch,
            capability: QualificationCapability::NativePrediction,
            scope_key: &ScopeKey::all(),
            revision: &revision,
            target: &target,
            report: None,
            supersedes: &[],
            expected: ExpectedSelection::Absent,
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &[],
            observed_pending: PendingGeneration::new(0),
        },
        0,
    )
    .await
    .expect("selection");
    assert!(matches!(
        outcome,
        SelectionOutcome::Rejected(
            looklab_server::model_store::SelectionRejection::DependencyUnregistered(_)
        )
    ));
}

#[tokio::test]
async fn family_seeding_is_fenced_and_source_limited() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    let target = family_target(1);
    let family = target.family.clone();
    let seed_body = body("fujifilm/x-t5");
    let first_job = JobId::new();
    let second_job = JobId::new();
    running_job(&pool, &first_job).await;
    running_job(&pool, &second_job).await;
    let safety = CandidateSafety {
        numerical: SafetyCheck::Passed,
        release_gates: SafetyCheck::Passed,
    };
    let first = async {
        let mut tx = model_store::begin_immediate(&pool).await.expect("tx");
        let outcome = model_store::seed_family_target(
            &mut tx,
            &FamilySeed {
                family: &family,
                sources: &test_sources(),
                expected_anchor: None,
                target: &target,
                safety,
                source_limited: true,
                seed_bodies: std::slice::from_ref(&seed_body),
                dependencies: &[],
                lease: JobLease {
                    job: &first_job,
                    attempts: 1,
                },
                snapshot: &[],
                observed_pending: PendingGeneration::new(0),
            },
            0,
        )
        .await
        .expect("seed");
        tx.commit().await.expect("commit");
        outcome
    };
    let second = async {
        let mut tx = model_store::begin_immediate(&pool).await.expect("tx");
        let outcome = model_store::seed_family_target(
            &mut tx,
            &FamilySeed {
                family: &family,
                sources: &test_sources(),
                expected_anchor: None,
                target: &target,
                safety,
                source_limited: true,
                seed_bodies: std::slice::from_ref(&seed_body),
                dependencies: &[],
                lease: JobLease {
                    job: &second_job,
                    attempts: 1,
                },
                snapshot: &[],
                observed_pending: PendingGeneration::new(0),
            },
            0,
        )
        .await
        .expect("seed");
        tx.commit().await.expect("commit");
        outcome
    };
    let (first, second) = tokio::join!(first, second);
    assert_eq!(
        [first, second]
            .iter()
            .filter(|outcome| matches!(outcome, FamilySelectionOutcome::Selected))
            .count(),
        1
    );
    let mut conn = pool.acquire().await.expect("connection");
    let head = model_store::family_head(&mut conn, &family)
        .await
        .expect("head")
        .expect("seeded");
    assert!(head.source_limited);
    assert!(head.report.is_none());
    assert_eq!(head.target, target);
    let cohort = model_store::family_cohort(&mut conn, &family, CohortRole::Seed)
        .await
        .expect("cohort");
    assert_eq!(cohort.members.len(), 1);
    assert!(matches!(
        &cohort.members[0],
        CohortMember::Seed { body } if body == &seed_body
    ));
    let rewrite = sqlx::query("UPDATE model_family_cohort SET body = 'sony/a7'")
        .execute(&mut *conn)
        .await;
    assert!(rewrite.is_err());
}

#[tokio::test]
async fn qualified_selection_requires_the_exact_scope_and_pass() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    let target = family_target(1);
    publishable_target(&pool, &target).await;
    let subject = realization(1, "classic_chrome");
    let key = SubjectKey::for_realization(&subject);
    let version = insert_revision(
        &pool,
        &subject,
        &target,
        RealizationStatus::Candidate,
        &[transform_dependency()],
    )
    .await;
    let revision = RealizationRevisionId::for_version(&BodyRealizationVersionId {
        realization: subject.clone(),
        version,
        target: target.clone(),
    });
    let assessed = scope("controls", vec![body("fujifilm/x-t5")]);
    let report = pass_report(
        looklab_schema::ModelSubject::BodyRealization {
            realization: subject.clone(),
        },
        ModelVersionId::BodyRealization {
            realization: BodyRealizationVersionId {
                realization: subject,
                version,
                target: target.clone(),
            },
        },
        QualificationCapability::ControlsMatching,
        assessed.clone(),
    );
    let mut conn = pool.acquire().await.expect("connection");
    let report_id = model_store::insert_report(
        &mut conn,
        &report,
        &[transform_dependency()],
        &[],
        &EvidenceMembership::default(),
        0,
    )
    .await
    .expect("report");
    let job = JobId::new();
    running_job(&pool, &job).await;
    let snapshot = selection_snapshot(&mut conn, &revision, Some(&report_id)).await;
    let scope_key = assessed.key();
    let selected = model_store::select_realization(
        &mut conn,
        &RealizationSelection {
            subject: &key,
            sources: &test_sources(),
            expected_anchor: None,
            use_: ProjectionUse::QualifiedMatch,
            capability: QualificationCapability::ControlsMatching,
            scope_key: &scope_key,
            revision: &revision,
            target: &target,
            report: Some(&report_id),
            supersedes: &[],
            expected: ExpectedSelection::Absent,
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &snapshot,
            observed_pending: PendingGeneration::new(0),
        },
        0,
    )
    .await
    .expect("selection");
    assert_eq!(selected, SelectionOutcome::Selected);
    assert!(
        model_store::qualified_selection(
            &mut conn,
            &key,
            QualificationCapability::ControlsMatching,
            &scope_key,
        )
        .await
        .expect("qualified")
        .is_some()
    );
    assert!(
        model_store::qualified_selection(
            &mut conn,
            &key,
            QualificationCapability::ControlsMatching,
            &scope("other-domain", vec![body("fujifilm/x-t5")]).key(),
        )
        .await
        .expect("other scope")
        .is_none()
    );

    let mut product = report;
    product.scope.combination_basis = CombinationBasis::Product;
    let rejected = model_store::insert_report(
        &mut conn,
        &product,
        &[transform_dependency()],
        &[],
        &EvidenceMembership::default(),
        0,
    )
    .await;
    assert!(matches!(
        rejected,
        Err(StoreError::Report(
            looklab_schema::ReportValidationError::ImplicitCombinationProduct
        ))
    ));
}

#[tokio::test]
async fn ineligible_targets_cannot_qualify_or_select() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    let target = family_target(1);
    publishable_target(&pool, &target).await;
    let subject = realization(1, "classic_chrome");
    let key = SubjectKey::for_realization(&subject);
    let version = insert_revision(
        &pool,
        &subject,
        &target,
        RealizationStatus::Candidate,
        &[transform_dependency()],
    )
    .await;
    let revision = RealizationRevisionId::for_version(&BodyRealizationVersionId {
        realization: subject.clone(),
        version,
        target: target.clone(),
    });
    let assessed = scope("controls", vec![body("fujifilm/x-t5")]);
    let report = pass_report(
        looklab_schema::ModelSubject::BodyRealization {
            realization: subject.clone(),
        },
        ModelVersionId::BodyRealization {
            realization: BodyRealizationVersionId {
                realization: subject.clone(),
                version,
                target: target.clone(),
            },
        },
        QualificationCapability::ControlsMatching,
        assessed.clone(),
    );
    let mut conn = pool.acquire().await.expect("connection");
    let report_id = model_store::insert_report(
        &mut conn,
        &report,
        &[transform_dependency()],
        &[],
        &EvidenceMembership::default(),
        0,
    )
    .await
    .expect("report");
    let job = JobId::new();
    running_job(&pool, &job).await;
    let snapshot = selection_snapshot(&mut conn, &revision, Some(&report_id)).await;
    let scope_key = assessed.key();
    let selected = model_store::select_realization(
        &mut conn,
        &RealizationSelection {
            subject: &key,
            sources: &test_sources(),
            expected_anchor: None,
            use_: ProjectionUse::QualifiedMatch,
            capability: QualificationCapability::ControlsMatching,
            scope_key: &scope_key,
            revision: &revision,
            target: &target,
            report: Some(&report_id),
            supersedes: &[],
            expected: ExpectedSelection::Absent,
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &snapshot,
            observed_pending: PendingGeneration::new(0),
        },
        0,
    )
    .await
    .expect("selection");
    assert_eq!(selected, SelectionOutcome::Selected);
    assert!(
        model_store::qualified_selection(
            &mut conn,
            &key,
            QualificationCapability::ControlsMatching,
            &scope_key,
        )
        .await
        .expect("qualified")
        .is_some()
    );
    let experimental_control = model_store::select_realization(
        &mut conn,
        &RealizationSelection {
            subject: &key,
            sources: &test_sources(),
            expected_anchor: None,
            use_: ProjectionUse::ExperimentalMatch,
            capability: QualificationCapability::ControlsMatching,
            scope_key: &ScopeKey::all(),
            revision: &revision,
            target: &target,
            report: None,
            supersedes: &[],
            expected: ExpectedSelection::Absent,
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &snapshot,
            observed_pending: PendingGeneration::new(0),
        },
        1,
    )
    .await
    .expect("experimental control");
    assert_eq!(experimental_control, SelectionOutcome::Selected);

    let mut invalid = eligible_target(&target);
    invalid.status = TargetEligibilityStatus::Invalidated;
    invalid
        .reasons
        .push(TargetEligibilityReason::DependencyInvalidated);
    model_store::record_target_eligibility(&mut conn, &invalid, 1)
        .await
        .expect("invalidated eligibility");
    assert!(
        model_store::qualified_selection(
            &mut conn,
            &key,
            QualificationCapability::ControlsMatching,
            &scope_key,
        )
        .await
        .expect("qualified")
        .is_none()
    );
    let rejected = model_store::select_realization(
        &mut conn,
        &RealizationSelection {
            subject: &key,
            sources: &test_sources(),
            expected_anchor: None,
            use_: ProjectionUse::QualifiedMatch,
            capability: QualificationCapability::ControlsMatching,
            scope_key: &scope_key,
            revision: &revision,
            target: &target,
            report: Some(&report_id),
            supersedes: &[],
            expected: ExpectedSelection::Selected(SelectionRef {
                revision: &revision,
                report: Some(&report_id),
            }),
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &snapshot,
            observed_pending: PendingGeneration::new(0),
        },
        1,
    )
    .await
    .expect("rejected");
    assert_eq!(
        rejected,
        SelectionOutcome::Rejected(
            looklab_server::model_store::SelectionRejection::TargetIneligible(target.clone())
        )
    );

    let experimental = model_store::select_realization(
        &mut conn,
        &RealizationSelection {
            subject: &key,
            sources: &test_sources(),
            expected_anchor: None,
            use_: ProjectionUse::ExperimentalMatch,
            capability: QualificationCapability::ControlsMatching,
            scope_key: &ScopeKey::all(),
            revision: &revision,
            target: &target,
            report: None,
            supersedes: &[],
            expected: ExpectedSelection::Selected(SelectionRef {
                revision: &revision,
                report: None,
            }),
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &snapshot,
            observed_pending: PendingGeneration::new(0),
        },
        2,
    )
    .await
    .expect("experimental selection");
    assert_eq!(
        experimental,
        SelectionOutcome::Rejected(
            looklab_server::model_store::SelectionRejection::TargetIneligible(target.clone())
        )
    );

    sqlx::query("UPDATE look_versions SET status = 'deprecated' WHERE look = ? AND version = ?")
        .bind("classic-chrome")
        .bind(1_i64)
        .execute(&mut *conn)
        .await
        .expect("deprecate");
    let unpublished = model_store::select_realization(
        &mut conn,
        &RealizationSelection {
            subject: &key,
            sources: &test_sources(),
            expected_anchor: None,
            use_: ProjectionUse::ExperimentalMatch,
            capability: QualificationCapability::ControlsMatching,
            scope_key: &ScopeKey::all(),
            revision: &revision,
            target: &target,
            report: None,
            supersedes: &[],
            expected: ExpectedSelection::Selected(SelectionRef {
                revision: &revision,
                report: None,
            }),
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &snapshot,
            observed_pending: PendingGeneration::new(0),
        },
        2,
    )
    .await
    .expect("unpublished");
    assert_eq!(
        unpublished,
        SelectionOutcome::Rejected(
            looklab_server::model_store::SelectionRejection::TargetUnpublished(target.clone())
        )
    );
}

#[tokio::test]
async fn eligibility_snapshots_are_immutable_and_invalidation_preserves_history() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    let target = PublishedTargetId {
        look: looklab_schema::LookId::new("classic-chrome").expect("look"),
        version: TargetVersion::new(1).expect("version"),
    };
    let cohort_dependency = DependencyRef::required(
        DependencyKind::Cohort,
        DependencyTarget::cohort(&looklab_schema::CohortId::new("cohort/1").expect("cohort")),
    );
    let eligibility = TargetEligibility {
        id: EligibilitySnapshotId::from_digest([0; 32]),
        target: target.clone(),
        basis: TargetBasis::NativeFamily,
        artifacts: Vec::<ArtifactBinding>::new(),
        base_pin: None,
        support_uncertainty_source: None,
        status: TargetEligibilityStatus::Eligible,
        reasons: Vec::new(),
        dependencies: vec![cohort_dependency.clone()],
    };
    let mut conn = pool.acquire().await.expect("connection");
    let first = model_store::record_target_eligibility(&mut conn, &eligibility, 0)
        .await
        .expect("eligibility");
    let current = model_store::target_eligibility(&mut conn, &target)
        .await
        .expect("current")
        .expect("eligible");
    assert_eq!(current.id, first);
    assert_eq!(current.status, TargetEligibilityStatus::Eligible);
    let summary = model_store::invalidate(
        &mut conn,
        &cohort_dependency,
        looklab_schema::InvalidationReason::CohortReinterpretation,
        1,
    )
    .await
    .expect("invalidation");
    assert!(summary.target_eligibilities >= 1);
    let invalidated = model_store::target_eligibility(&mut conn, &target)
        .await
        .expect("current")
        .expect("invalidated");
    assert_ne!(invalidated.id, first);
    assert_eq!(invalidated.status, TargetEligibilityStatus::Invalidated);
    assert!(
        invalidated
            .reasons
            .contains(&TargetEligibilityReason::DependencyInvalidated)
    );
    let historical = model_store::target_eligibility_snapshot(&mut conn, &first)
        .await
        .expect("historical")
        .expect("preserved");
    assert_eq!(historical.id, first);
    assert_eq!(historical.target, target);
    assert_eq!(historical.status, TargetEligibilityStatus::Eligible);
    assert!(historical.reasons.is_empty());
    assert_eq!(historical.dependencies, vec![cohort_dependency.clone()]);
}

#[tokio::test]
async fn legacy_scope_stays_unassessed_and_creates_no_subject() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    let mut conn = pool.acquire().await.expect("connection");
    assert!(
        model_store::subjects(&mut conn)
            .await
            .expect("subjects")
            .is_empty()
    );
    assert!(
        model_store::family_heads(&mut conn)
            .await
            .expect("heads")
            .is_empty()
    );
    assert!(
        model_store::legacy_scopes(&mut conn)
            .await
            .expect("legacy")
            .is_empty()
    );
    let subject = SubjectKey::for_realization(&realization(1, "classic_chrome"));
    assert_eq!(
        model_store::applicability(
            &mut conn,
            &subject,
            ProjectionUse::QualifiedMatch,
            QualificationCapability::NativePrediction,
            &ScopeKey::all(),
        )
        .await
        .expect("applicability"),
        Applicability::Unassessed
    );
}

#[tokio::test]
async fn cohort_admission_requires_exact_member_bindings() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    let target = family_target(1);
    let pass = cross_body_pass(&pool, &target, vec![1], "cross-body", 0).await;
    let mut conn = pool.acquire().await.expect("connection");
    let incomplete = CohortAdmissionRequest {
        family: target.family.clone(),
        target: target.clone(),
        report: pass.admission,
        members: vec![CohortMember::Seed {
            body: body("fujifilm/x-t5"),
        }],
        assessed_domain: DomainId::new("cross-body").expect("domain"),
    };
    assert!(
        model_store::record_cohort_admission(&mut conn, &incomplete, 0)
            .await
            .is_err()
    );
    drop(conn);
    admit(&pool, &target, &pass).await;
    let mut conn = pool.acquire().await.expect("connection");
    let admitted = model_store::family_cohort(&mut conn, &target.family, CohortRole::Admitted)
        .await
        .expect("cohort");
    assert_eq!(admitted.state, FamilyCohortState::Current);
    assert_eq!(admitted.members.len(), 1);
    assert!(matches!(
        &admitted.members[0],
        CohortMember::Admitted { realization, .. }
            if realization.realization == pass.member
    ));
    assert_eq!(admitted.report, Some(pass.admission));
}

#[tokio::test]
async fn invalidated_admission_keeps_cohort_queries_usable() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    let target = family_target(1);
    publishable_target(&pool, &target).await;
    let pass = cross_body_pass(&pool, &target, vec![1], "cross-body", 0).await;
    admit(&pool, &target, &pass).await;
    let mut conn = pool.acquire().await.expect("connection");
    let admitted = model_store::family_cohort(&mut conn, &target.family, CohortRole::Admitted)
        .await
        .expect("admitted cohort");
    assert_eq!(admitted.state, FamilyCohortState::Current);
    assert_eq!(admitted.report, Some(pass.admission));

    model_store::invalidate(
        &mut conn,
        &DependencyRef::required(
            DependencyKind::Report,
            DependencyTarget::report(&pass.member_report),
        ),
        looklab_schema::InvalidationReason::Withdrawal,
        2,
    )
    .await
    .expect("withdrawal");
    let admitted = model_store::family_cohort(&mut conn, &target.family, CohortRole::Admitted)
        .await
        .expect("usable cohort");
    assert_eq!(
        admitted.state,
        FamilyCohortState::Inapplicable {
            report: Some(pass.admission),
            reason: FamilyCohortInapplicable::NotApplicable,
        }
    );
    assert!(admitted.members.is_empty());
    assert!(admitted.report.is_none());
    let seed = model_store::family_cohort(&mut conn, &target.family, CohortRole::Seed)
        .await
        .expect("seed cohort");
    assert_eq!(seed.state, FamilyCohortState::Current);
    assert_eq!(seed.members.len(), 1);
    let head = model_store::family_head(&mut conn, &target.family)
        .await
        .expect("head")
        .expect("head");
    assert_eq!(head.target, target);
    assert_eq!(head.applicability, Applicability::Current);

    let replacement = cross_body_pass(&pool, &target, vec![2], "cross-body", 0).await;
    admit(&pool, &target, &replacement).await;
    let admitted = model_store::family_cohort(&mut conn, &target.family, CohortRole::Admitted)
        .await
        .expect("replacement cohort");
    assert_eq!(admitted.state, FamilyCohortState::Current);
    assert_eq!(admitted.report, Some(replacement.admission));
    assert_eq!(admitted.members.len(), 1);
}

#[tokio::test]
async fn passing_family_assessment_becomes_the_current_projection() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    let target = family_target(1);
    publishable_target(&pool, &target).await;
    let pass = cross_body_pass(&pool, &target, vec![1], "cross-body", 0).await;
    let job = JobId::new();
    running_job(&pool, &job).await;
    let mut conn = pool.acquire().await.expect("connection");
    let sources = test_sources();
    let family_subject = SubjectKey::for_family(&target.family);
    let scope_key = model_store::report(&mut conn, &pass.admission)
        .await
        .expect("report")
        .expect("stored")
        .report
        .scope
        .key();
    let dependencies = model_store::dependencies(&mut conn, "report", &pass.admission.to_hex())
        .await
        .expect("dependencies");
    let snapshot = model_store::dependency_snapshot(&mut conn, &dependencies)
        .await
        .expect("snapshot");
    let selection = FamilySelection {
        family: &target.family,
        sources: &sources,
        expected_anchor: None,
        target: &target,
        report: &pass.admission,
        scope_key: &scope_key,
        expected: ExpectedFamilySelection::Absent,
        lease: JobLease {
            job: &job,
            attempts: 1,
        },
        snapshot: &snapshot,
        observed_pending: PendingGeneration::new(0),
    };
    assert_eq!(
        model_store::select_family(&mut conn, &selection, 0)
            .await
            .expect("selection"),
        FamilyProjectionOutcome::Selected
    );
    let projection = model_store::current_projection(
        &mut conn,
        &family_subject,
        ProjectionUse::QualifiedMatch,
        QualificationCapability::CrossBodyAdmission,
        &scope_key,
    )
    .await
    .expect("projection")
    .expect("selected");
    assert_eq!(projection.applicability, Applicability::Current);
    assert_eq!(projection.selected_report, Some(pass.admission));
    assert_eq!(
        projection.selected_candidate,
        Some(ModelVersionId::SharedFamily {
            target: target.clone()
        })
    );
    let head = model_store::family_head(&mut conn, &target.family)
        .await
        .expect("head")
        .expect("head");
    assert_eq!(head.report, Some(pass.admission));
    assert!(
        model_store::family_cross_body_qualified(&mut conn, &target.family)
            .await
            .expect("qualified")
    );
    assert_eq!(
        model_store::select_family(&mut conn, &selection, 1)
            .await
            .expect("stale selection"),
        FamilyProjectionOutcome::Stale {
            actual: Some(FamilySelectionRefOwned {
                target: target.clone(),
                report: Some(pass.admission),
            })
        }
    );
    let successor_selection = FamilySelection {
        family: &target.family,
        sources: &sources,
        expected_anchor: None,
        target: &target,
        report: &pass.admission,
        scope_key: &scope_key,
        expected: ExpectedFamilySelection::Selected(FamilySelectionRef {
            target: &target,
            report: Some(&pass.admission),
        }),
        lease: JobLease {
            job: &job,
            attempts: 1,
        },
        snapshot: &snapshot,
        observed_pending: PendingGeneration::new(2),
    };
    assert_eq!(
        model_store::select_family(&mut conn, &successor_selection, 2)
            .await
            .expect("successor selection"),
        FamilyProjectionOutcome::Selected
    );
    let regressed_selection = FamilySelection {
        family: &target.family,
        sources: &sources,
        expected_anchor: None,
        target: &target,
        report: &pass.admission,
        scope_key: &scope_key,
        expected: ExpectedFamilySelection::Selected(FamilySelectionRef {
            target: &target,
            report: Some(&pass.admission),
        }),
        lease: JobLease {
            job: &job,
            attempts: 1,
        },
        snapshot: &snapshot,
        observed_pending: PendingGeneration::new(1),
    };
    assert_eq!(
        model_store::select_family(&mut conn, &regressed_selection, 2)
            .await
            .expect("regressed selection"),
        FamilyProjectionOutcome::Rejected(
            looklab_server::model_store::SelectionRejection::PendingRegressed {
                selected: PendingGeneration::new(2),
                observed: PendingGeneration::new(1),
            }
        )
    );

    model_store::invalidate(
        &mut conn,
        &DependencyRef::required(
            DependencyKind::Report,
            DependencyTarget::report(&pass.admission),
        ),
        looklab_schema::InvalidationReason::EvidenceLoss,
        2,
    )
    .await
    .expect("invalidate");
    let invalidated = model_store::current_projection(
        &mut conn,
        &family_subject,
        ProjectionUse::QualifiedMatch,
        QualificationCapability::CrossBodyAdmission,
        &scope_key,
    )
    .await
    .expect("projection")
    .expect("invalidated");
    assert_eq!(
        invalidated.applicability,
        Applicability::Invalidated {
            reason: looklab_schema::InvalidationReason::EvidenceLoss
        }
    );
    assert!(
        !model_store::family_cross_body_qualified(&mut conn, &target.family)
            .await
            .expect("qualified")
    );
    let head = model_store::family_head(&mut conn, &target.family)
        .await
        .expect("head")
        .expect("head");
    assert_eq!(
        head.applicability,
        Applicability::Invalidated {
            reason: looklab_schema::InvalidationReason::EvidenceLoss
        }
    );

    let replacement = cross_body_pass(&pool, &target, vec![2], "cross-body", 0).await;
    let dependencies =
        model_store::dependencies(&mut conn, "report", &replacement.admission.to_hex())
            .await
            .expect("dependencies");
    let snapshot = model_store::dependency_snapshot(&mut conn, &dependencies)
        .await
        .expect("snapshot");
    let replacement_selection = FamilySelection {
        family: &target.family,
        sources: &sources,
        expected_anchor: None,
        target: &target,
        report: &replacement.admission,
        scope_key: &scope_key,
        expected: ExpectedFamilySelection::Selected(FamilySelectionRef {
            target: &target,
            report: Some(&pass.admission),
        }),
        lease: JobLease {
            job: &job,
            attempts: 1,
        },
        snapshot: &snapshot,
        observed_pending: PendingGeneration::new(3),
    };
    assert_eq!(
        model_store::select_family(&mut conn, &replacement_selection, 3)
            .await
            .expect("replacement selection"),
        FamilyProjectionOutcome::Selected
    );
    let projection = model_store::current_projection(
        &mut conn,
        &family_subject,
        ProjectionUse::QualifiedMatch,
        QualificationCapability::CrossBodyAdmission,
        &scope_key,
    )
    .await
    .expect("projection")
    .expect("restored");
    assert_eq!(projection.applicability, Applicability::Current);
    assert_eq!(projection.selected_report, Some(replacement.admission));
    let head = model_store::family_head(&mut conn, &target.family)
        .await
        .expect("head")
        .expect("head");
    assert_eq!(head.report, Some(replacement.admission));
    assert_eq!(head.applicability, Applicability::Current);
}

#[tokio::test]
async fn newer_head_invalidation_keeps_older_target_selection() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    family_version(&pool, "classic-chrome", 2).await;
    let v1 = family_target(1);
    let v2 = family_target(2);
    publishable_target(&pool, &v1).await;
    let qualified = qualified_realization(&pool, &v1, "controls-v1").await;
    let job = JobId::new();
    running_job(&pool, &job).await;
    let mut conn = pool.acquire().await.expect("connection");
    assert_eq!(
        select_qualified(
            &mut conn,
            &qualified,
            &v1,
            &job,
            ExpectedSelection::Absent,
            0
        )
        .await,
        SelectionOutcome::Selected
    );
    assert!(
        model_store::qualified_selection(
            &mut conn,
            &qualified.key,
            QualificationCapability::ControlsMatching,
            &qualified.scope_key,
        )
        .await
        .expect("v1 qualified")
        .is_some()
    );

    let advanced = model_store::advance_family_head(
        &mut conn,
        &FamilyAdvance {
            family: &v1.family,
            sources: &test_sources(),
            expected_anchor: None,
            expected: v1.version,
            target: &v2,
            safety: CandidateSafety {
                numerical: SafetyCheck::Passed,
                release_gates: SafetyCheck::Passed,
            },
            source_limited: false,
            report: None,
            dependencies: &[],
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &[],
            observed_pending: PendingGeneration::new(0),
        },
        1,
    )
    .await
    .expect("advance");
    assert_eq!(advanced, FamilySelectionOutcome::Selected);
    model_store::record_target_eligibility(&mut conn, &eligible_target(&v2), 1)
        .await
        .expect("v2 eligibility");

    let pass = cross_body_pass(&pool, &v2, vec![1], "cross-body", 0).await;
    let scope_key = model_store::report(&mut conn, &pass.admission)
        .await
        .expect("report")
        .expect("stored")
        .report
        .scope
        .key();
    let sources = test_sources();
    assert_eq!(
        select_family_report(
            &mut conn,
            FamilySelectionCase {
                target: &v2,
                report: &pass.admission,
                scope_key: &scope_key,
                job: &job,
                sources: &sources,
                expected: ExpectedFamilySelection::Absent,
                observed: PendingGeneration::new(0),
                now: 2,
            },
        )
        .await,
        FamilyProjectionOutcome::Selected
    );
    model_store::invalidate(
        &mut conn,
        &DependencyRef::required(
            DependencyKind::Report,
            DependencyTarget::report(&pass.admission),
        ),
        looklab_schema::InvalidationReason::EvidenceLoss,
        3,
    )
    .await
    .expect("invalidate v2 report");
    let head = model_store::family_head(&mut conn, &v1.family)
        .await
        .expect("head")
        .expect("head");
    assert_eq!(
        head.applicability,
        Applicability::Invalidated {
            reason: looklab_schema::InvalidationReason::EvidenceLoss
        }
    );

    assert!(
        model_store::qualified_selection(
            &mut conn,
            &qualified.key,
            QualificationCapability::ControlsMatching,
            &qualified.scope_key,
        )
        .await
        .expect("v1 qualified")
        .is_some()
    );
    assert_eq!(
        select_qualified(
            &mut conn,
            &qualified,
            &v1,
            &job,
            ExpectedSelection::Selected(SelectionRef {
                revision: &qualified.revision,
                report: Some(&qualified.report),
            }),
            4,
        )
        .await,
        SelectionOutcome::Selected
    );

    let mut invalid = eligible_target(&v1);
    invalid.status = TargetEligibilityStatus::Invalidated;
    invalid
        .reasons
        .push(TargetEligibilityReason::DependencyInvalidated);
    model_store::record_target_eligibility(&mut conn, &invalid, 5)
        .await
        .expect("v1 ineligible");
    assert!(
        model_store::qualified_selection(
            &mut conn,
            &qualified.key,
            QualificationCapability::ControlsMatching,
            &qualified.scope_key,
        )
        .await
        .expect("v1 qualified")
        .is_none()
    );
    assert_eq!(
        select_qualified(
            &mut conn,
            &qualified,
            &v1,
            &job,
            ExpectedSelection::Selected(SelectionRef {
                revision: &qualified.revision,
                report: Some(&qualified.report),
            }),
            6,
        )
        .await,
        SelectionOutcome::Rejected(
            looklab_server::model_store::SelectionRejection::TargetIneligible(v1.clone())
        )
    );
}

#[tokio::test]
async fn family_selection_requires_a_passing_engine_prerequisite() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    let target = family_target(1);
    publishable_target(&pool, &target).await;
    let job = JobId::new();
    running_job(&pool, &job).await;
    let mut conn = pool.acquire().await.expect("connection");
    let sources = test_sources();
    let (global_scope, _) = cross_body_scopes("cross-body");
    let scope_key = global_scope.key();
    let family_dependency = DependencyRef::required(
        DependencyKind::FamilyVersion,
        DependencyTarget::family_version(&target),
    );
    let family_report = |scope: &ReportScope| {
        pass_report(
            looklab_schema::ModelSubject::SharedFamily {
                family: target.family.clone(),
            },
            ModelVersionId::SharedFamily {
                target: target.clone(),
            },
            QualificationCapability::CrossBodyAdmission,
            scope.clone(),
        )
    };

    let missing = family_report(&global_scope);
    let missing_id = model_store::insert_report(
        &mut conn,
        &missing,
        std::slice::from_ref(&family_dependency),
        &[],
        &EvidenceMembership::default(),
        0,
    )
    .await
    .expect("missing engine report");
    assert_eq!(
        select_family_report(
            &mut conn,
            FamilySelectionCase {
                target: &target,
                report: &missing_id,
                scope_key: &scope_key,
                job: &job,
                sources: &sources,
                expected: ExpectedFamilySelection::Absent,
                observed: PendingGeneration::new(0),
                now: 0,
            },
        )
        .await,
        FamilyProjectionOutcome::Rejected(
            looklab_server::model_store::SelectionRejection::EnginePrerequisiteFailed(missing_id)
        )
    );

    let failed_engine = EnginePrerequisite {
        engine: SourceRef::new("engine@2").expect("engine"),
        rule_revision: RuleRevision::new("p4.3/1").expect("rule"),
        outcome: AssessmentOutcome::Fail,
        metrics: ReportMetrics::new(),
        supported_mask: SupportedMask::new(),
        limitations: Vec::new(),
    };
    let failed_engine_id = model_store::insert_engine_prerequisite(&mut conn, &failed_engine, 0)
        .await
        .expect("failed engine");
    let failed = family_report(&global_scope);
    let failed_id = model_store::insert_report(
        &mut conn,
        &failed,
        &[
            family_dependency.clone(),
            DependencyRef::required(
                DependencyKind::EngineQualification,
                DependencyTarget::engine_qualification(&failed_engine_id),
            ),
        ],
        &[],
        &EvidenceMembership::default(),
        0,
    )
    .await
    .expect("failed engine report");
    assert_eq!(
        select_family_report(
            &mut conn,
            FamilySelectionCase {
                target: &target,
                report: &failed_id,
                scope_key: &scope_key,
                job: &job,
                sources: &sources,
                expected: ExpectedFamilySelection::Absent,
                observed: PendingGeneration::new(0),
                now: 0,
            },
        )
        .await,
        FamilyProjectionOutcome::Rejected(
            looklab_server::model_store::SelectionRejection::EnginePrerequisiteFailed(failed_id)
        )
    );

    let passing_engine = EnginePrerequisite {
        engine: SourceRef::new("engine@3").expect("engine"),
        rule_revision: RuleRevision::new("p4.3/1").expect("rule"),
        outcome: AssessmentOutcome::Pass,
        metrics: {
            let mut metrics = ReportMetrics::new();
            metrics.insert(MetricKey::RecoveryRate, 0.9);
            metrics.insert(MetricKey::Coverage, 0.9);
            metrics.insert(MetricKey::AssessedCells, 1.0);
            metrics.insert(MetricKey::SupportedCells, 1.0);
            metrics
        },
        supported_mask: {
            let mut mask = SupportedMask::new();
            mask.insert(CellId::new("latent-1").expect("cell"));
            mask
        },
        limitations: Vec::new(),
    };
    let passing_engine_id = model_store::insert_engine_prerequisite(&mut conn, &passing_engine, 0)
        .await
        .expect("passing engine");
    let passing = family_report(&global_scope);
    let passing_id = model_store::insert_report(
        &mut conn,
        &passing,
        &[
            family_dependency,
            DependencyRef::required(
                DependencyKind::EngineQualification,
                DependencyTarget::engine_qualification(&passing_engine_id),
            ),
        ],
        &[],
        &EvidenceMembership::default(),
        0,
    )
    .await
    .expect("passing engine report");
    assert_eq!(
        select_family_report(
            &mut conn,
            FamilySelectionCase {
                target: &target,
                report: &passing_id,
                scope_key: &scope_key,
                job: &job,
                sources: &sources,
                expected: ExpectedFamilySelection::Absent,
                observed: PendingGeneration::new(0),
                now: 1,
            },
        )
        .await,
        FamilyProjectionOutcome::Selected
    );
}

#[tokio::test]
async fn older_family_report_cannot_overwrite_newer_head_evidence() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    let target = family_target(1);
    publishable_target(&pool, &target).await;
    let job = JobId::new();
    running_job(&pool, &job).await;
    let mut conn = pool.acquire().await.expect("connection");
    let sources = test_sources();
    let newer = cross_body_pass(&pool, &target, vec![1], "cross-body-new", 0).await;
    let older = cross_body_pass(&pool, &target, vec![2], "cross-body-old", 10).await;
    let newer_scope = model_store::report(&mut conn, &newer.admission)
        .await
        .expect("report")
        .expect("stored")
        .report
        .scope
        .key();
    let older_scope = model_store::report(&mut conn, &older.admission)
        .await
        .expect("report")
        .expect("stored")
        .report
        .scope
        .key();
    assert_ne!(newer_scope, older_scope);
    assert_eq!(
        select_family_report(
            &mut conn,
            FamilySelectionCase {
                target: &target,
                report: &newer.admission,
                scope_key: &newer_scope,
                job: &job,
                sources: &sources,
                expected: ExpectedFamilySelection::Absent,
                observed: PendingGeneration::new(2),
                now: 0,
            },
        )
        .await,
        FamilyProjectionOutcome::Selected
    );
    assert_eq!(
        select_family_report(
            &mut conn,
            FamilySelectionCase {
                target: &target,
                report: &older.admission,
                scope_key: &older_scope,
                job: &job,
                sources: &sources,
                expected: ExpectedFamilySelection::Absent,
                observed: PendingGeneration::new(1),
                now: 1,
            },
        )
        .await,
        FamilyProjectionOutcome::Selected
    );
    let head = model_store::family_head(&mut conn, &target.family)
        .await
        .expect("head")
        .expect("head");
    assert_eq!(head.report, Some(newer.admission));
    let projection = model_store::current_projection(
        &mut conn,
        &SubjectKey::for_family(&target.family),
        ProjectionUse::QualifiedMatch,
        QualificationCapability::CrossBodyAdmission,
        &older_scope,
    )
    .await
    .expect("projection")
    .expect("older scope projection");
    assert_eq!(projection.selected_report, Some(older.admission));
    assert_eq!(projection.applicability, Applicability::Current);
}

#[tokio::test]
async fn diagnostic_stats_are_subject_scoped_and_separate() {
    let (_dir, pool) = fixture().await;
    let subject = SubjectKey::for_realization(&realization(1, "classic_chrome"));
    let mut conn = pool.acquire().await.expect("connection");
    model_store::register_subject(
        &mut conn,
        &looklab_schema::ModelSubject::BodyRealization {
            realization: realization(1, "classic_chrome"),
        },
        0,
    )
    .await
    .expect("subject");
    let stats = DiagnosticImageStats {
        image: looklab_schema::ImageRef::new("image-1").expect("image"),
        rms_de: 0.5,
        z: 1.0,
        verdict: DiagnosticVerdict::DownWeighted,
        reason: Some("experimental".to_owned()),
        weight_components: DiagnosticWeightComponents {
            look: 1.0,
            body: 1.0,
            contributor: looklab_schema::ContributorRef::new("credential-1").expect("credential"),
        },
    };
    model_store::write_diagnostic_image_stats(&mut conn, &subject, &stats, 0)
        .await
        .expect("write");
    let stored = model_store::diagnostic_image_stats(&mut conn, &subject)
        .await
        .expect("read");
    assert_eq!(stored, vec![stats]);
    model_store::clear_diagnostic_image_stats(&mut conn, &subject)
        .await
        .expect("clear");
    assert!(
        model_store::diagnostic_image_stats(&mut conn, &subject)
            .await
            .expect("read")
            .is_empty()
    );
}

#[tokio::test]
async fn family_head_advancement_is_compare_and_swap() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    family_version(&pool, "classic-chrome", 2).await;
    let first_target = family_target(1);
    let second_target = family_target(2);
    let family = first_target.family.clone();
    let job = JobId::new();
    running_job(&pool, &job).await;
    let safety = CandidateSafety {
        numerical: SafetyCheck::Passed,
        release_gates: SafetyCheck::Passed,
    };
    let mut conn = pool.acquire().await.expect("connection");
    model_store::seed_family_target(
        &mut conn,
        &FamilySeed {
            family: &family,
            sources: &test_sources(),
            expected_anchor: None,
            target: &first_target,
            safety,
            source_limited: true,
            seed_bodies: &[body("fujifilm/x-t5")],
            dependencies: &[],
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &[],
            observed_pending: PendingGeneration::new(0),
        },
        0,
    )
    .await
    .expect("seed");
    let advanced = model_store::advance_family_head(
        &mut conn,
        &FamilyAdvance {
            family: &family,
            sources: &test_sources(),
            expected_anchor: None,
            expected: first_target.version,
            target: &second_target,
            safety,
            source_limited: false,
            report: None,
            dependencies: &[],
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &[],
            observed_pending: PendingGeneration::new(0),
        },
        1,
    )
    .await
    .expect("advance");
    assert_eq!(advanced, FamilySelectionOutcome::Selected);
    let stale = model_store::advance_family_head(
        &mut conn,
        &FamilyAdvance {
            family: &family,
            sources: &test_sources(),
            expected_anchor: None,
            expected: first_target.version,
            target: &first_target,
            safety,
            source_limited: true,
            report: None,
            dependencies: &[],
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            snapshot: &[],
            observed_pending: PendingGeneration::new(0),
        },
        2,
    )
    .await
    .expect("stale advance");
    assert_eq!(
        stale,
        FamilySelectionOutcome::Stale {
            actual: Some(second_target.version)
        }
    );
    let head = model_store::family_head(&mut conn, &family)
        .await
        .expect("head")
        .expect("advanced");
    assert_eq!(head.target, second_target);
    assert!(!head.source_limited);
}

#[tokio::test]
async fn bundle_revisions_have_their_own_identity_and_current_pointer() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    let target = family_target(1);
    let subject = realization(1, "classic_chrome");
    let dependencies = [transform_dependency()];
    let version = insert_revision(
        &pool,
        &subject,
        &target,
        RealizationStatus::Candidate,
        &dependencies,
    )
    .await;
    let realization = RealizationRevisionId::for_version(&BodyRealizationVersionId {
        realization: subject,
        version,
        target: target.clone(),
    });
    let bundle = MatchBundleRevision {
        id: MatchBundleRevisionId::from_digest([0; 32]),
        body: body("fujifilm/x-t5"),
        epoch: ControlEpochId::new(1),
        schema_snapshot_version: 1,
        input_transform_version: 1,
        body_state_version: BodyStateVersion::new(1).expect("body state"),
        body_realization_params_version: 1,
        sims: vec![BundleSimEntry {
            native_sim: SimKey::new("classic_chrome").expect("sim"),
            realization,
            family_version: target,
            base_artifact: Sha256Hex::from_bytes([2; 32]),
            control_response: Sha256Hex::from_bytes([3; 32]),
            covariance: None,
            control_response_version: 1,
            clip_k: 1.0,
            realization_params_version: 1,
            body_params: Some(Sha256Hex::from_bytes([6; 32])),
            realization_params: None,
        }],
        excluded: Vec::<BundleExclusion>::new(),
        manifest_hash: MatchBundleRevisionId::from_digest([0; 32]),
        producer: SourceRef::new("worker@1").expect("producer"),
        created_at: 0,
    }
    .sealed();
    let manifest = bundle.manifest();
    let mut conn = pool.acquire().await.expect("connection");
    let id = model_store::insert_bundle_revision(
        &mut conn,
        &NewBundleRevision {
            body: &body("fujifilm/x-t5"),
            epoch: 1,
            manifest: &manifest,
            artifacts: &[],
            dependencies: &dependencies,
        },
        0,
    )
    .await
    .expect("bundle");
    assert_eq!(id, bundle.id);
    let stored = model_store::bundle_revision(&mut conn, &id)
        .await
        .expect("stored")
        .expect("bundle");
    assert_eq!(stored.manifest, manifest);
    assert_eq!(stored.revision.sims.len(), 1);
    let bundle_dependencies = model_store::dependencies(&mut conn, "bundle_revision", &id.to_hex())
        .await
        .expect("bundle dependencies");
    let snapshot = model_store::dependency_snapshot(&mut conn, &bundle_dependencies)
        .await
        .expect("snapshot");
    let job = JobId::new();
    running_job(&pool, &job).await;
    let selected = model_store::select_bundle(
        &mut conn,
        &model_store::BundleSelection {
            body: &body("fujifilm/x-t5"),
            epoch: 1,
            use_: ProjectionUse::ExperimentalMatch,
            id: &id,
            expected: None,
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            sources: &test_sources(),
            snapshot: &snapshot,
        },
        0,
    )
    .await
    .expect("selection");
    assert_eq!(selected, BundleSelectionOutcome::Selected);
    let current = model_store::current_bundles(&mut conn)
        .await
        .expect("current");
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].bundle, id);
    let reselected = model_store::select_bundle(
        &mut conn,
        &model_store::BundleSelection {
            body: &body("fujifilm/x-t5"),
            epoch: 1,
            use_: ProjectionUse::ExperimentalMatch,
            id: &id,
            expected: Some(&id),
            lease: JobLease {
                job: &job,
                attempts: 1,
            },
            sources: &test_sources(),
            snapshot: &snapshot,
        },
        1,
    )
    .await
    .expect("reselection");
    assert_eq!(reselected, BundleSelectionOutcome::Selected);
    model_store::invalidate(
        &mut conn,
        &transform_dependency(),
        looklab_schema::InvalidationReason::TransformReinterpretation,
        2,
    )
    .await
    .expect("invalidation");
    let invalidated = model_store::current_bundles(&mut conn)
        .await
        .expect("current");
    assert_eq!(
        invalidated[0].applicability,
        Applicability::Invalidated {
            reason: looklab_schema::InvalidationReason::TransformReinterpretation
        }
    );
    let rewrite = sqlx::query("UPDATE model_bundle_revisions SET manifest = '{}'")
        .execute(&mut *conn)
        .await;
    assert!(rewrite.is_err());
}

#[tokio::test]
async fn evidence_requests_are_idempotent_and_prunable() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    let target = family_target(1);
    let subject = realization(1, "classic_chrome");
    let version =
        insert_revision(&pool, &subject, &target, RealizationStatus::Candidate, &[]).await;
    let request = EvidenceRequest {
        kind: EvidenceRequestKind::MissingIndependentCaptures,
        detail: "a second independent capture at EV 0".to_owned(),
    };
    let mut report = pass_report(
        looklab_schema::ModelSubject::BodyRealization {
            realization: subject.clone(),
        },
        ModelVersionId::BodyRealization {
            realization: BodyRealizationVersionId {
                realization: subject.clone(),
                version,
                target,
            },
        },
        QualificationCapability::NativePrediction,
        scope("native", vec![body("fujifilm/x-t5")]),
    );
    report.outcome = AssessmentOutcome::InsufficientEvidence;
    report.metrics = ReportMetrics::new();
    report.supported_mask = SupportedMask::new();
    report.evidence_requests = vec![request.clone()];
    let key = SubjectKey::for_realization(&subject);
    let mut conn = pool.acquire().await.expect("connection");
    let report_id = model_store::insert_report(
        &mut conn,
        &report,
        &[],
        &[],
        &EvidenceMembership::default(),
        0,
    )
    .await
    .expect("report");
    model_store::record_evidence_request(
        &mut conn,
        &report_id,
        &key,
        QualificationCapability::NativePrediction,
        &request,
        0,
    )
    .await
    .expect("request");
    model_store::record_evidence_request(
        &mut conn,
        &report_id,
        &key,
        QualificationCapability::NativePrediction,
        &request,
        1,
    )
    .await
    .expect("idempotent request");
    let stored = model_store::evidence_requests(&mut conn)
        .await
        .expect("requests");
    assert_eq!(stored.len(), 1);
    assert!(matches!(stored[0].origin, EvidenceRequestOrigin::Report(_)));
    assert_eq!(stored[0].subject.key(), stored[0].subject_key);
    assert_eq!(stored[0].request, request);
    model_store::prune_evidence_requests(&mut conn, &report_id)
        .await
        .expect("prune");
    assert!(
        model_store::evidence_requests(&mut conn)
            .await
            .expect("requests")
            .is_empty()
    );
}

#[tokio::test]
async fn invalidated_report_requests_leave_active_lists() {
    let (_dir, pool) = fixture().await;
    family_version(&pool, "classic-chrome", 1).await;
    let target = family_target(1);
    let subject = realization(1, "classic_chrome");
    let version =
        insert_revision(&pool, &subject, &target, RealizationStatus::Candidate, &[]).await;
    let key = SubjectKey::for_realization(&subject);
    let mut conn = pool.acquire().await.expect("connection");
    let request = EvidenceRequest {
        kind: EvidenceRequestKind::MissingIndependentCaptures,
        detail: "a second independent capture at EV 0".to_owned(),
    };
    let mut report = pass_report(
        looklab_schema::ModelSubject::BodyRealization {
            realization: subject.clone(),
        },
        ModelVersionId::BodyRealization {
            realization: BodyRealizationVersionId {
                realization: subject.clone(),
                version,
                target,
            },
        },
        QualificationCapability::NativePrediction,
        scope("native", vec![body("fujifilm/x-t5")]),
    );
    report.outcome = AssessmentOutcome::InsufficientEvidence;
    report.metrics = ReportMetrics::new();
    report.supported_mask = SupportedMask::new();
    report.evidence_requests = vec![request.clone()];
    let report_id = model_store::insert_report(
        &mut conn,
        &report,
        &[],
        &[],
        &EvidenceMembership::default(),
        0,
    )
    .await
    .expect("report");
    model_store::record_evidence_request(
        &mut conn,
        &report_id,
        &key,
        QualificationCapability::NativePrediction,
        &request,
        0,
    )
    .await
    .expect("report request");
    let prerequisite = EvidenceRequest {
        kind: EvidenceRequestKind::MissingEngineQualification,
        detail: "the engine prerequisite is missing".to_owned(),
    };
    model_store::record_active_evidence_request(
        &mut conn,
        &key,
        QualificationCapability::NativePrediction,
        &prerequisite,
        Some(1),
        0,
    )
    .await
    .expect("active request");
    let listed = model_store::evidence_requests(&mut conn)
        .await
        .expect("requests");
    assert_eq!(listed.len(), 2);

    model_store::invalidate(
        &mut conn,
        &DependencyRef::required(DependencyKind::Report, DependencyTarget::report(&report_id)),
        looklab_schema::InvalidationReason::EvidenceLoss,
        1,
    )
    .await
    .expect("invalidate");
    let listed = model_store::evidence_requests(&mut conn)
        .await
        .expect("requests");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].origin, EvidenceRequestOrigin::PreCandidate);
    assert_eq!(listed[0].request, prerequisite);
    assert!(
        model_store::report(&mut conn, &report_id)
            .await
            .expect("history")
            .is_some()
    );
    assert!(
        !model_store::report_applicable(&mut conn, &report_id)
            .await
            .expect("applicability")
    );
}

#[tokio::test]
async fn active_evidence_requests_are_idempotent_and_resolve() {
    let (_dir, pool) = fixture().await;
    let subject = SubjectKey::for_realization(&realization(1, "classic_chrome"));
    let mut conn = pool.acquire().await.expect("connection");
    model_store::register_subject(
        &mut conn,
        &looklab_schema::ModelSubject::BodyRealization {
            realization: realization(1, "classic_chrome"),
        },
        0,
    )
    .await
    .expect("subject");
    let request = EvidenceRequest {
        kind: EvidenceRequestKind::MissingIndependentCaptures,
        detail: "fujifilm/x-t5/1/classic_chrome: 1 independent RAW capture; at least 2 required"
            .to_owned(),
    };
    model_store::record_active_evidence_request(
        &mut conn,
        &subject,
        QualificationCapability::NativePrediction,
        &request,
        Some(1),
        0,
    )
    .await
    .expect("request");
    model_store::record_active_evidence_request(
        &mut conn,
        &subject,
        QualificationCapability::NativePrediction,
        &request,
        Some(1),
        1,
    )
    .await
    .expect("idempotent request");
    let stored = model_store::evidence_requests(&mut conn)
        .await
        .expect("requests");
    assert_eq!(stored.len(), 1);
    assert!(matches!(
        stored[0].origin,
        EvidenceRequestOrigin::PreCandidate
    ));
    assert_eq!(stored[0].subject.key(), stored[0].subject_key);
    assert_eq!(stored[0].observed_groups, Some(1));
    model_store::record_active_evidence_request(
        &mut conn,
        &subject,
        QualificationCapability::NativePrediction,
        &request,
        Some(2),
        2,
    )
    .await
    .expect("updated request");
    let stored = model_store::evidence_requests(&mut conn)
        .await
        .expect("requests");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].observed_groups, Some(2));
    assert!(
        model_store::resolve_active_evidence_request(
            &mut conn,
            &subject,
            EvidenceRequestKind::MissingIndependentCaptures,
        )
        .await
        .expect("resolve")
    );
    assert!(
        model_store::evidence_requests(&mut conn)
            .await
            .expect("requests")
            .is_empty()
    );
    assert!(
        !model_store::resolve_active_evidence_request(
            &mut conn,
            &subject,
            EvidenceRequestKind::MissingIndependentCaptures,
        )
        .await
        .expect("resolved")
    );
}

#[tokio::test]
async fn blob_references_include_model_artifacts_and_exclude_unreferenced_staging() {
    let (_dir, pool) = fixture().await;
    let shared = Sha256Hex::from_bytes([11; 32]);
    let transform = Sha256Hex::from_bytes([12; 32]);
    let params = Sha256Hex::from_bytes([13; 32]);
    let gauge = Sha256Hex::from_bytes([14; 32]);
    let native_artifact = Sha256Hex::from_bytes([15; 32]);
    let native_warm = Sha256Hex::from_bytes([16; 32]);
    let eligibility = Sha256Hex::from_bytes([17; 32]);
    let unreferenced = Sha256Hex::from_bytes([18; 32]);
    let mut conn = pool.acquire().await.expect("connection");
    for (attempt, hash) in [("shared", shared), ("unreferenced", unreferenced)] {
        sqlx::query(
            "INSERT INTO blob_staging (attempt_id, hash, staged_at, expires_at)
             VALUES (?, ?, 0, 0)",
        )
        .bind(attempt)
        .bind(hash.as_bytes().as_slice())
        .execute(&mut *conn)
        .await
        .expect("staging");
    }
    sqlx::query(
        "INSERT INTO model_artifact_bindings
         (owner_kind, owner_id, artifact_sha256, role, created_at)
         VALUES ('realization_revision', 'rev', ?, 'look', 0)",
    )
    .bind(shared.to_hex())
    .execute(&mut *conn)
    .await
    .expect("model binding");
    sqlx::query(
        "INSERT INTO model_body_states
         (body, version, stage, profile_sha256, input_transform_sha256,
          body_params_sha256, gauge_sha256, created_at)
         VALUES ('fujifilm/x-t5', 1, 'native', NULL, ?, ?, ?, 0)",
    )
    .bind(transform.to_hex())
    .bind(params.to_hex())
    .bind(gauge.to_hex())
    .execute(&mut *conn)
    .await
    .expect("body state");
    sqlx::query(
        "INSERT INTO model_subjects (key, kind, family, body, epoch, native_sim, created_at)
         VALUES ('body_realization:fujifilm/x-t5/1/classic_chrome',
                 'body_realization', NULL, 'fujifilm/x-t5', 1, 'classic_chrome', 0)",
    )
    .execute(&mut *conn)
    .await
    .expect("subject");
    sqlx::query(
        "INSERT INTO model_subject_revisions
         (subject_key, revision, role, artifact_sha256, native_warm_state_sha256, recorded_at)
         VALUES ('body_realization:fujifilm/x-t5/1/classic_chrome', 1, 'native', ?, ?, 0)",
    )
    .bind(native_artifact.to_hex())
    .bind(native_warm.to_hex())
    .execute(&mut *conn)
    .await
    .expect("subject revision");
    sqlx::query(
        "INSERT INTO model_target_eligibility_snapshots
         (id, target, basis, artifacts, base_pin, support_uncertainty_source, status, reasons,
          recorded_at)
         VALUES ('snapshot', 'classic-chrome@1', 'native_family', ?, NULL, NULL, 'eligible',
                 '[]', 0)",
    )
    .bind(serde_json::json!([{"sha256": eligibility.to_hex(), "role": "look"}]).to_string())
    .execute(&mut *conn)
    .await
    .expect("eligibility snapshot");
    for hash in [shared, unreferenced] {
        sqlx::query("DELETE FROM blob_staging WHERE hash = ?")
            .bind(hash.as_bytes().as_slice())
            .execute(&mut *conn)
            .await
            .expect("expire staging");
    }
    for hash in [
        shared,
        transform,
        params,
        gauge,
        native_artifact,
        native_warm,
        eligibility,
    ] {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blob_references WHERE hash = ?")
            .bind(hash.to_hex())
            .fetch_one(&mut *conn)
            .await
            .expect("view count");
        assert_eq!(count, 1, "{hash} must remain referenced");
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blob_references WHERE hash = ?")
        .bind(unreferenced.to_hex())
        .fetch_one(&mut *conn)
        .await
        .expect("view count");
    assert_eq!(count, 0, "unreferenced staging hash must be deletable");
}
