// SPDX-License-Identifier: AGPL-3.0-or-later
//! Chart-anchored scoped cross-body admission (docs/02 §5.1, docs/05 §8.1).

use std::collections::{BTreeMap, BTreeSet};

use looklab_color::ucs::{Ucs, delta_e_ucs};
use looklab_schema::{
    AssessmentOutcome, BodyId, BodyRealizationVersionId, CellId, CohortAdmissionRequest,
    CohortMember, ControlEpochId, DependencyRef, EvidenceRequestKind, FamilyTargetId, MetricKey,
    ModelReportId, ModelSubject, ModelVersionId, QualificationCapability, ReportMetrics,
    ReportScope, RuleRevision, SupportedMask,
};
use serde::{Deserialize, Serialize};

use crate::assessment::EngineVersion;
use crate::assessment::{
    AssessmentBinding, CROSS_BODY_RULE, EvidenceGroup, EvidenceSource, P19_DISAGREEMENT_MEDIAN_DE,
    ProtocolAssessment, ReportParts, build_report, cell_for_probe, engine_dependency,
    engine_prerequisite, evidence_membership, evidence_request, limitation, median, metric,
    outcome, supported_mask,
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GaugeBindingId(String);

impl GaugeBindingId {
    /// # Errors
    /// Empty or whitespace-bearing text.
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.is_empty() || value.chars().any(char::is_whitespace) {
            return Err("gauge binding id is empty or contains whitespace".to_owned());
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug)]
pub struct ProbePrediction {
    pub probe: looklab_model::ProbeId,
    pub ucs: Ucs,
    pub supported: bool,
}

pub fn predictions_from_fit(probes: &looklab_fit::ProbeUncertainty) -> Vec<ProbePrediction> {
    probes
        .probes
        .iter()
        .enumerate()
        .filter_map(|(index, estimate)| {
            Some(ProbePrediction {
                probe: probes.ids.get(index)?.clone(),
                ucs: estimate.post_ucs,
                supported: estimate.supported,
            })
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct CrossBodyEvidence {
    pub body: BodyId,
    pub epoch: ControlEpochId,
    pub native_sim: looklab_schema::SimKey,
    pub groups: Vec<EvidenceGroup>,
    pub chart_anchored: bool,
    pub gauge: GaugeBindingId,
    pub predictions: Vec<ProbePrediction>,
    pub source: EvidenceSource,
    pub calibration: DependencyRef,
    pub realization: BodyRealizationVersionId,
    pub member_scope: Option<ReportScope>,
    pub member_mask: Option<SupportedMask>,
    pub report: Option<ModelReportId>,
}

pub struct CrossBodyAssessmentInput {
    pub binding: AssessmentBinding,
    pub family_target: FamilyTargetId,
    pub engine: Option<looklab_schema::EnginePrerequisite>,
    pub engine_version: EngineVersion,
    pub bodies: Vec<CrossBodyEvidence>,
}

pub fn assess_cross_body(input: CrossBodyAssessmentInput) -> ProtocolAssessment {
    let CrossBodyAssessmentInput {
        binding,
        family_target,
        engine,
        engine_version,
        bodies,
    } = input;
    let rule = RuleRevision::new(CROSS_BODY_RULE).expect("cross-body rule revision");
    let mut metrics = ReportMetrics::new();
    let mut limitations = Vec::new();
    let mut requests = Vec::new();
    let mut extra_dependencies = Vec::new();
    let mut execution = false;
    let mut failed = false;
    let mut insufficient = false;

    if let Err(detail) = binding.validate() {
        execution = true;
        limitations.push(limitation(detail));
    }
    if !matches!(&binding.subject, ModelSubject::SharedFamily { .. }) {
        execution = true;
        limitations.push(limitation(
            "cross-body admission is a claim about a shared family",
        ));
    }
    if let ModelVersionId::SharedFamily { target } = &binding.candidate
        && target != &family_target
    {
        execution = true;
        limitations.push(limitation(
            "the family target does not name the report candidate",
        ));
    }
    if engine_version.fitter != binding.provenance.fitter
        || engine_version.evaluator != binding.provenance.evaluator
        || engine_version.model != binding.provenance.model
    {
        insufficient = true;
        requests.push(evidence_request(
            EvidenceRequestKind::MissingEngineQualification,
            "the engine version does not match the assessment's fitter/evaluator/model sources",
        ));
    }
    match engine_prerequisite(&engine_version, engine.as_ref()) {
        Ok(prerequisite) => {
            extra_dependencies.push(engine_dependency(&prerequisite));
        }
        Err((outcome, detail)) => {
            match outcome {
                AssessmentOutcome::InsufficientEvidence => insufficient = true,
                AssessmentOutcome::Fail => failed = true,
                AssessmentOutcome::ExecutionFailure => execution = true,
                AssessmentOutcome::Pass => {}
            }
            if outcome == AssessmentOutcome::InsufficientEvidence {
                requests.push(evidence_request(
                    EvidenceRequestKind::MissingEngineQualification,
                    detail.clone(),
                ));
            }
            limitations.push(limitation(detail));
        }
    }

    if bodies.len() < 2 {
        insufficient = true;
        requests.push(evidence_request(
            EvidenceRequestKind::MissingCrossBodyEvidence,
            "cross-body admission requires at least two independent chart-anchored bodies",
        ));
    }
    let mut body_ids: BTreeSet<&BodyId> = BTreeSet::new();
    let mut group_owner: BTreeMap<&EvidenceGroup, &BodyId> = BTreeMap::new();
    let mut gauges: BTreeSet<&GaugeBindingId> = BTreeSet::new();
    let mut sims: BTreeSet<&looklab_schema::SimKey> = BTreeSet::new();
    for body in &bodies {
        if !body_ids.insert(&body.body) {
            execution = true;
            limitations.push(limitation(format!(
                "body {} appears more than once",
                body.body
            )));
        }
        if !body.chart_anchored {
            insufficient = true;
            requests.push(evidence_request(
                EvidenceRequestKind::MissingCalibrationSource,
                format!(
                    "body {} needs a pinned chart-anchored calibration source",
                    body.body
                ),
            ));
        }
        if body.source != EvidenceSource::Observed {
            insufficient = true;
            requests.push(evidence_request(
                EvidenceRequestKind::MissingIndependentCaptures,
                format!(
                    "body {} evidence is synthetic; admission needs observed chart-anchored captures",
                    body.body
                ),
            ));
            limitations.push(limitation(format!(
                "body {} evidence is synthetic and cannot qualify admission",
                body.body
            )));
        }
        gauges.insert(&body.gauge);
        sims.insert(&body.native_sim);
        for group in &body.groups {
            if let Some(owner) = group_owner.insert(group, &body.body)
                && owner != &body.body
            {
                insufficient = true;
                requests.push(evidence_request(
                    EvidenceRequestKind::MissingIndependentCaptures,
                    "an evidence group is assigned to more than one body",
                ));
            }
        }
        extra_dependencies.push(body.calibration.clone());
        if let Some(report) = body.report {
            extra_dependencies.push(DependencyRef::required(
                looklab_schema::DependencyKind::Report,
                looklab_schema::DependencyTarget::report(&report),
            ));
        }
    }
    if gauges.len() > 1 {
        insufficient = true;
        requests.push(evidence_request(
            EvidenceRequestKind::MissingCrossBodyEvidence,
            "the assessed bodies do not share one pinned gauge binding",
        ));
    }
    if sims.len() > 1 {
        insufficient = true;
        requests.push(evidence_request(
            EvidenceRequestKind::MissingCrossBodyEvidence,
            "the assessed bodies do not share one common native sim",
        ));
    }
    let scope_bodies: BTreeSet<&BodyId> = binding.scope.assessed_bodies.iter().collect();
    if scope_bodies != body_ids {
        insufficient = true;
        requests.push(evidence_request(
            EvidenceRequestKind::MissingCrossBodyEvidence,
            "the report scope does not name exactly the assessed bodies",
        ));
    }

    let mut by_probe: BTreeMap<&looklab_model::ProbeId, BTreeMap<&BodyId, (Ucs, bool)>> =
        BTreeMap::new();
    for body in &bodies {
        for prediction in &body.predictions {
            if !finite_ucs(prediction.ucs) {
                execution = true;
                continue;
            }
            by_probe
                .entry(&prediction.probe)
                .or_default()
                .insert(&body.body, (prediction.ucs, prediction.supported));
        }
    }
    let mut assessed_cells: BTreeSet<CellId> = BTreeSet::new();
    let mut common_cells: BTreeSet<CellId> = BTreeSet::new();
    let mut disagreements = Vec::new();
    for (probe, predictions) in &by_probe {
        let Some(cell) = cell_for_probe(probe) else {
            execution = true;
            continue;
        };
        assessed_cells.insert(cell.clone());
        if predictions.len() != bodies.len()
            || !predictions.values().all(|(_, supported)| *supported)
        {
            continue;
        }
        common_cells.insert(cell);
        let values: Vec<Ucs> = predictions.values().map(|(ucs, _)| *ucs).collect();
        for left in 0..values.len() {
            for right in (left + 1)..values.len() {
                disagreements.push(delta_e_ucs(values[left], values[right]));
            }
        }
    }
    if common_cells.is_empty() {
        insufficient = true;
        requests.push(evidence_request(
            EvidenceRequestKind::MissingCrossBodyEvidence,
            "no probe is supported by every assessed body",
        ));
    }
    let disagreement = median(&disagreements);
    if let Some(value) = disagreement {
        metric(
            &mut metrics,
            MetricKey::CrossBodyMedianDisagreementDe,
            value,
        );
        if value > P19_DISAGREEMENT_MEDIAN_DE {
            failed = true;
            limitations.push(limitation(format!(
                "cross-body median disagreement {value:.3} ΔE exceeds the {P19_DISAGREEMENT_MEDIAN_DE:.3} ΔE bound"
            )));
        }
    }
    let mask: SupportedMask = supported_mask(common_cells.iter().cloned());
    let supported_count = mask.len();
    let assessed_count = assessed_cells.len().max(supported_count);
    let coverage = if assessed_count == 0 {
        0.0
    } else {
        supported_count as f64 / assessed_count as f64
    };
    metric(
        &mut metrics,
        MetricKey::SupportedCells,
        supported_count as f64,
    );
    metric(
        &mut metrics,
        MetricKey::AssessedCells,
        assessed_count as f64,
    );
    metric(&mut metrics, MetricKey::Coverage, coverage);

    limitations.push(limitation(
        "agreement is qualified only on the recorded common support and coverage",
    ));
    limitations.push(limitation(
        "chart-anchored predictions under pinned calibration and gauge conventions",
    ));

    let membership = evidence_membership(
        Vec::new(),
        bodies
            .iter()
            .flat_map(|body| body.groups.iter().cloned())
            .collect(),
    );
    let mut assessment = build_report(
        &binding,
        ReportParts {
            capability: QualificationCapability::CrossBodyAdmission,
            rule,
            outcome: outcome(execution, failed, insufficient),
            metrics,
            supported_mask: mask,
            limitations,
            evidence_requests: requests,
            extra_dependencies,
            membership,
            cohort: None,
        },
    );
    if assessment.report.outcome == AssessmentOutcome::Pass {
        let members: Option<Vec<CohortMember>> = bodies
            .into_iter()
            .map(|body| {
                Some(CohortMember::Admitted {
                    realization: body.realization,
                    report: body.report?,
                    scope: Box::new(body.member_scope?),
                    supported_mask: body.member_mask?,
                })
            })
            .collect();
        if let Some(members) = members {
            assessment.cohort = Some(CohortAdmissionRequest {
                family: family_target.family.clone(),
                target: family_target,
                report: assessment.report_id,
                members,
                assessed_domain: binding.scope.assessed_domain.clone(),
            });
        }
    }
    assessment
}

fn finite_ucs(value: Ucs) -> bool {
    value.j.is_finite() && value.a.is_finite() && value.b.is_finite()
}
