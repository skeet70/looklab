// SPDX-License-Identifier: AGPL-3.0-or-later
//! Executes chart-anchored cross-body admission on exact realization revisions.

use std::collections::{BTreeMap, BTreeSet};

use looklab_schema::{
    BodyId, BodyRealizationId, CohortRole, ControlEpochId, DependencyKind, DependencyRef,
    DependencyTarget, FamilyTargetId, ModelSubject, ModelVersionId, QualificationCapability,
    RealizationStatus, ReportScope, SafetyCheck, SimKey, SupportedMask,
};
use looklab_server::model_store::{self, StoredRealizationRevision};
use sqlx::SqlitePool;

use crate::assessment::{
    AssessmentBinding, CROSS_BODY_RULE, CrossBodyAssessmentInput, CrossBodyEvidence, EngineVersion,
    GaugeBindingId, ProbePrediction, ProtocolAssessment, ReportParts, assess_cross_body,
    build_report, evidence_request, limitation,
};
use crate::config::WorkerConfig;
use crate::error::{Result, WorkerError};
use crate::pool::EvidenceScope;

use super::artifacts::load_realization;
use super::diagnostics;

pub(crate) async fn run(
    pool: &SqlitePool,
    config: &WorkerConfig,
    mut binding: AssessmentBinding,
    target: &FamilyTargetId,
    sources: &crate::source_identity::NumericalSources,
) -> Result<ProtocolAssessment> {
    if !matches!(&binding.subject, ModelSubject::SharedFamily { .. }) {
        return Err(WorkerError::Data(
            "cross-body admission needs a shared-family subject".to_owned(),
        ));
    }
    let family = &target.family;
    let mut conn = pool.acquire().await?;
    let mut members = model_store::family_cohort(&mut conn, family, CohortRole::Admitted)
        .await?
        .members;
    if members.len() < 2 {
        members = model_store::family_cohort(&mut conn, family, CohortRole::Seed)
            .await?
            .members;
    }
    let revisions = model_store::realization_revisions(&mut conn, None).await?;
    let engine_version = EngineVersion::new(
        sources.fitter.clone(),
        sources.evaluator.clone(),
        sources.model.clone(),
        looklab_schema::RuleRevision::new(crate::assessment::ENGINE_COVERAGE_RULE)
            .map_err(|error| WorkerError::Data(error.to_string()))?,
    )
    .map_err(WorkerError::Data)?;
    let engine = model_store::engine_prerequisite(
        &mut conn,
        &looklab_schema::EnginePrerequisiteId {
            engine: engine_version.source().clone(),
            rule_revision: engine_version.rule_revision.clone(),
        },
    )
    .await?;
    drop(conn);

    let selected = select_revisions(target, &members, revisions);
    let mut bodies = Vec::new();
    for (body, revision, diagnostic) in selected {
        let candidate = looklab_schema::BodyRealizationVersionId {
            realization: revision.subject.clone(),
            version: revision.version,
            target: target.clone(),
        };
        let loaded = load_realization(pool, config, &candidate)
            .await
            .map_err(WorkerError::Data)?;
        let independent = match &diagnostic {
            Some(diagnostic) => match diagnostics::load(pool, config, diagnostic).await {
                Ok(Some(diagnostics)) => diagnostics::validate(&diagnostics, diagnostic, sources)
                    .ok()
                    .map(|()| diagnostics),
                _ => None,
            },
            None => None,
        };
        let groups = match &independent {
            Some(diagnostics) => diagnostics.groups.clone(),
            None => body_groups(pool, config, &revision).await?,
        };
        let predictions: Vec<ProbePrediction> = independent
            .as_ref()
            .map(|diagnostics| {
                diagnostics
                    .probes
                    .iter()
                    .map(|probe| ProbePrediction {
                        probe: probe.probe.clone(),
                        ucs: looklab_color::ucs::Ucs::new(
                            probe.post_clip[0],
                            probe.post_clip[1],
                            probe.post_clip[2],
                        ),
                        supported: probe.supported,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let gauge = independent
            .as_ref()
            .map(|diagnostics| diagnostics.gauge)
            .unwrap_or_else(|| looklab_schema::Sha256Hex::from_bytes([0; 32]));
        let subject = independent
            .as_ref()
            .map(|diagnostics| diagnostics.subject.clone())
            .unwrap_or_else(|| revision.subject.clone());
        bodies.push(CrossBodyEvidence {
            body: body.clone(),
            epoch: ControlEpochId::new(subject.epoch.get()),
            native_sim: subject.native_sim.clone(),
            groups,
            chart_anchored: matches!(
                loaded.transform.source(),
                looklab_body::A0Source::Ssf | looklab_body::A0Source::Chart
            ),
            gauge: GaugeBindingId::new(format!("gauge:{}", gauge.to_hex()))
                .map_err(WorkerError::Data)?,
            predictions,
            source: crate::assessment::EvidenceSource::Observed,
            calibration: DependencyRef::required(
                DependencyKind::Transform,
                DependencyTarget::transform(&body, revision.input_transform_version),
            ),
            realization: candidate.clone(),
            member_scope: None,
            member_mask: None,
            report: None,
        });
    }
    binding.scope.assessed_bodies = bodies.iter().map(|body| body.body.clone()).collect();
    binding.scope.exposure_cells = Vec::new();
    let mut conn = pool.acquire().await?;
    let reports = model_store::reports(&mut conn).await?;
    let mut proofs: Vec<Option<usize>> = Vec::with_capacity(bodies.len());
    for body in &bodies {
        let mut best: Option<usize> = None;
        for (index, stored) in reports.iter().enumerate() {
            let matches_subject = matches!(
                &stored.report.subject,
                ModelSubject::BodyRealization { realization: subject }
                    if subject == &body.realization.realization
            );
            let matches_candidate = matches!(
                &stored.report.candidate,
                ModelVersionId::BodyRealization { realization: candidate }
                    if candidate == &body.realization
            );
            if !matches_subject
                || !matches_candidate
                || !matches!(
                    stored.report.capability,
                    QualificationCapability::NativePrediction
                        | QualificationCapability::ControlsMatching
                )
                || !matches!(
                    stored.report.outcome,
                    looklab_schema::AssessmentOutcome::Pass
                )
                || !model_store::report_applicable(&mut conn, &stored.id).await?
            {
                continue;
            }
            if best.is_none_or(|best| {
                stored.report.scope.points.len() > reports[best].report.scope.points.len()
            }) {
                best = Some(index);
            }
        }
        proofs.push(best);
    }
    let scopes: Vec<&ReportScope> = proofs
        .iter()
        .flatten()
        .map(|index| &reports[*index].report.scope)
        .collect();
    let (points, colour_cells, exposure_cells, common) = common_scope(&scopes);
    binding.scope.points = points;
    binding.scope.colour_cells = colour_cells;
    binding.scope.exposure_cells = exposure_cells;
    if !common {
        let rule = looklab_schema::RuleRevision::new(CROSS_BODY_RULE)
            .map_err(|error| WorkerError::Data(error.to_string()))?;
        let assessment = build_report(
            &binding,
            ReportParts {
                capability: QualificationCapability::CrossBodyAdmission,
                rule,
                outcome: looklab_schema::AssessmentOutcome::InsufficientEvidence,
                metrics: looklab_schema::ReportMetrics::new(),
                supported_mask: SupportedMask::new(),
                limitations: vec![limitation(
                    "assessed bodies do not share an assessed settings combination",
                )],
                evidence_requests: vec![evidence_request(
                    looklab_schema::EvidenceRequestKind::MissingCrossBodyEvidence,
                    "capture the same assessed settings on every candidate body",
                )],
                extra_dependencies: Vec::new(),
                membership: model_store::EvidenceMembership::default(),
                cohort: None,
            },
        );
        return Ok(assessment);
    }
    binding.scope.defaults_assessed = binding
        .scope
        .points
        .iter()
        .any(|point| point.settings.is_empty());
    for (body, proof) in bodies.iter_mut().zip(&proofs) {
        let Some(index) = proof else {
            continue;
        };
        let stored = &reports[*index];
        let claim = project_member_claim(&binding.scope, &body.realization.realization);
        if claim.points.is_empty()
            || !scope_covers(&stored.report.scope, &claim)
            || !mask_covers(&stored.report.supported_mask, &claim)
        {
            continue;
        }
        body.report = Some(stored.id);
        body.member_scope = Some(stored.report.scope.clone());
        body.member_mask = Some(stored.report.supported_mask.clone());
    }
    drop(conn);
    let assessment = assess_cross_body(CrossBodyAssessmentInput {
        binding,
        family_target: target.clone(),
        engine,
        engine_version,
        bodies,
    });
    Ok(assessment)
}

type SelectedRevision = (
    BodyId,
    StoredRealizationRevision,
    Option<StoredRealizationRevision>,
);

type RevisionPair = (
    Option<StoredRealizationRevision>,
    Option<StoredRealizationRevision>,
);

type BodyRevisions = BTreeMap<SimKey, RevisionPair>;

fn select_revisions(
    target: &FamilyTargetId,
    _members: &[looklab_schema::CohortMember],
    revisions: Vec<StoredRealizationRevision>,
) -> Vec<SelectedRevision> {
    let mut by_body: BTreeMap<BodyId, BodyRevisions> = BTreeMap::new();
    for revision in revisions {
        if revision.target != *target {
            continue;
        }
        let body = revision.subject.body.clone();
        let sim = revision.subject.native_sim.clone();
        let (candidate, diagnostic) = by_body.entry(body).or_default().entry(sim).or_default();
        match revision.status {
            RealizationStatus::Candidate
                if revision.safety.numerical == SafetyCheck::Passed
                    && revision.safety.release_gates == SafetyCheck::Passed =>
            {
                keep_highest(candidate, revision);
            }
            RealizationStatus::Diagnostic => keep_highest(diagnostic, revision),
            RealizationStatus::Candidate => {}
        }
    }
    let mut candidates: Vec<SelectedRevision> = Vec::new();
    for (body, sims) in by_body {
        for (_, (candidate, diagnostic)) in sims {
            if let Some(candidate) = candidate {
                candidates.push((body.clone(), candidate, diagnostic));
            }
        }
    }
    let mut by_sim: BTreeMap<SimKey, Vec<SelectedRevision>> = BTreeMap::new();
    for (body, revision, diagnostic) in candidates {
        by_sim
            .entry(revision.subject.native_sim.clone())
            .or_default()
            .push((body, revision, diagnostic));
    }
    by_sim
        .into_values()
        .filter(|entries| {
            let bodies: BTreeSet<&BodyId> = entries.iter().map(|(body, _, _)| body).collect();
            bodies.len() >= 2
        })
        .max_by_key(|entries| entries.len())
        .unwrap_or_default()
}

fn keep_highest(slot: &mut Option<StoredRealizationRevision>, revision: StoredRealizationRevision) {
    if slot
        .as_ref()
        .is_none_or(|existing| revision.version > existing.version)
    {
        *slot = Some(revision);
    }
}

fn common_scope(
    scopes: &[&ReportScope],
) -> (
    Vec<looklab_schema::AssessedPoint>,
    Vec<looklab_schema::CellId>,
    Vec<looklab_schema::CellId>,
    bool,
) {
    let mut common_settings: Option<BTreeSet<looklab_schema::SettingCombination>> = None;
    let mut common_colour: Option<BTreeSet<looklab_schema::CellId>> = None;
    let mut common_exposure: Option<BTreeSet<looklab_schema::CellId>> = None;
    for scope in scopes {
        intersect(
            &mut common_settings,
            scope
                .points
                .iter()
                .map(|point| point.settings.clone())
                .collect::<BTreeSet<_>>()
                .into_iter(),
        );
        intersect(
            &mut common_colour,
            scope
                .colour_cells
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter(),
        );
        intersect(
            &mut common_exposure,
            scope
                .exposure_cells
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter(),
        );
    }
    let common_settings = common_settings.unwrap_or_default();
    let common_colour = common_colour.unwrap_or_default();
    let common_exposure = common_exposure.unwrap_or_default();
    let shared = !common_settings.is_empty();
    let mut points: BTreeSet<looklab_schema::AssessedPoint> = BTreeSet::new();
    for scope in scopes {
        for point in &scope.points {
            if !shared || common_settings.contains(&point.settings) {
                points.insert(point.clone());
            }
        }
    }
    (
        points.into_iter().collect(),
        common_colour.into_iter().collect(),
        common_exposure.into_iter().collect(),
        shared,
    )
}

fn intersect<T: Ord + Clone>(
    accumulator: &mut Option<BTreeSet<T>>,
    values: impl Iterator<Item = T>,
) {
    let values: BTreeSet<T> = values.collect();
    match accumulator {
        Some(current) => current.retain(|value| values.contains(value)),
        None => *accumulator = Some(values),
    }
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

async fn body_groups(
    pool: &SqlitePool,
    config: &WorkerConfig,
    revision: &StoredRealizationRevision,
) -> Result<Vec<looklab_server::model_store::EvidenceGroupId>> {
    let look = looklab_upload::LookSlug::new(revision.target.family.as_str())
        .map_err(|_| WorkerError::Data("family target is not a Look slug".to_owned()))?;
    let look_pool = crate::pool::build(
        pool,
        config,
        &revision.subject.body,
        &look,
        &revision.subject,
        &EvidenceScope::Experimental,
    )
    .await?;
    look_pool
        .images
        .iter()
        .filter(|image| image.is_eligible() && image.admitted.iter().any(|admitted| *admitted))
        .map(crate::model_fit::evidence_group)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(name: &str) -> BodyId {
        BodyId::new(name).expect("body")
    }

    fn scope(body: BodyId, cell: &str) -> ReportScope {
        ReportScope {
            points: vec![looklab_schema::AssessedPoint {
                settings: looklab_schema::SettingCombination::new(),
                firmware: Some(looklab_schema::FirmwareId {
                    body: body.clone(),
                    epoch: ControlEpochId::new(1),
                    version: "1.0".parse().expect("firmware"),
                }),
                wb_mode: Some(looklab_schema::WbModeKey::new("Auto").expect("wb")),
            }],
            combination_basis: looklab_schema::CombinationBasis::Enumerated,
            defaults_assessed: true,
            output_encoding: looklab_schema::OutputEncoding::Srgb,
            colour_cells: vec![looklab_schema::CellId::new(cell).expect("cell")],
            exposure_cells: Vec::new(),
            assessed_bodies: vec![body],
            assessed_domain: looklab_schema::DomainId::new("test").expect("domain"),
        }
    }

    #[test]
    fn different_member_firmware_points_survive_common_settings() {
        let first = scope(body("fujifilm/x-t5"), "cell-0");
        let second = scope(body("sony/a7"), "cell-0");
        let (points, colour, _, shared) = common_scope(&[&first, &second]);
        assert!(shared);
        assert_eq!(points.len(), 2);
        assert!(points.iter().any(|point| {
            point
                .firmware
                .as_ref()
                .is_some_and(|firmware| firmware.body == body("fujifilm/x-t5"))
        }));
        assert_eq!(
            colour,
            vec![looklab_schema::CellId::new("cell-0").expect("cell")]
        );
    }

    #[test]
    fn disjoint_settings_are_not_a_shared_domain() {
        let first = scope(body("fujifilm/x-t5"), "cell-0");
        let mut second = scope(body("sony/a7"), "cell-0");
        second.points[0].settings.insert(
            looklab_schema::ControlKey::new("color").expect("control"),
            looklab_schema::SettingValue::Step(1),
        );
        let (_, _, _, shared) = common_scope(&[&first, &second]);
        assert!(!shared);
    }

    #[test]
    fn uncovered_member_cell_is_excluded() {
        let first = scope(body("fujifilm/x-t5"), "cell-0");
        let second = scope(body("sony/a7"), "cell-0");
        let mut uncovered = scope(body("canon/eos-r5"), "cell-0");
        uncovered
            .colour_cells
            .push(looklab_schema::CellId::new("cell-9").expect("cell"));
        let (_, colour, _, shared) = common_scope(&[&first, &second, &uncovered]);
        assert!(shared);
        assert_eq!(
            colour,
            vec![looklab_schema::CellId::new("cell-0").expect("cell")]
        );
    }

    #[test]
    fn fully_disjoint_member_cell_has_no_common_support() {
        let first = scope(body("fujifilm/x-t5"), "cell-0");
        let second = scope(body("sony/a7"), "cell-0");
        let disjoint = scope(body("canon/eos-r5"), "cell-9");
        let (_, colour, _, shared) = common_scope(&[&first, &second, &disjoint]);
        assert!(shared);
        assert!(colour.is_empty());
    }
}
