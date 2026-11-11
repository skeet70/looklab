// SPDX-License-Identifier: AGPL-3.0-or-later
//! Conditional native mean-response prediction (docs/02 §5.1, docs/05 §8.1).

use std::collections::{BTreeMap, BTreeSet};

use looklab_color::ucs::Ucs;
use looklab_fit::{ModelErrorField, ProbeUncertainty};
use looklab_schema::{
    AssessedPoint, CellId, EvidenceRequestKind, FirmwareId, MetricKey, ModelSubject,
    OutputEncoding, QualificationCapability, ReportMetrics, RuleRevision, SettingCombination,
    SupportedMask, WbModeKey,
};

use crate::assessment::{
    AssessmentBinding, EvidenceGroup, EvidenceSource, PinnedModes, ProtocolAssessment, ReportParts,
    build_report, evidence_membership, evidence_request, limitation, median, metric, outcome,
    point_matches, resolve_point, rgb_space, supported_mask,
};

#[derive(Clone, Debug)]
pub struct NativeProbeSupport {
    pub probe: looklab_model::ProbeId,
    pub cell: CellId,
    pub supported: bool,
    pub held_out_evidence: bool,
}

#[derive(Clone, Debug)]
pub struct NativeObservation {
    pub group: EvidenceGroup,
    pub settings: SettingCombination,
    pub firmware: Option<FirmwareId>,
    pub wb_mode: Option<WbModeKey>,
    pub cell: CellId,
    pub predicted: Ucs,
    pub observed: Ucs,
    pub source: EvidenceSource,
}

pub struct NativeAssessmentInput {
    pub binding: AssessmentBinding,
    pub encoding: OutputEncoding,
    pub pinned: PinnedModes,
    pub support: Vec<NativeProbeSupport>,
    pub training_groups: Vec<EvidenceGroup>,
    pub observations: Vec<NativeObservation>,
    pub support_sigma: f64,
    pub min_independent_groups: usize,
}

/// # Errors
/// A probe or cell in the fit's support metadata has no v1 mapping.
pub fn support_from_fit(
    probes: &ProbeUncertainty,
    field: &ModelErrorField,
    encoding: OutputEncoding,
) -> Result<Vec<NativeProbeSupport>, String> {
    let space = rgb_space(encoding);
    let probe_set = looklab_model::probes::v1();
    let mut support = Vec::with_capacity(probes.probes.len());
    for (index, estimate) in probes.probes.iter().enumerate() {
        let probe = probes
            .ids
            .get(index)
            .ok_or_else(|| format!("probe estimate {index} has no probe id"))?;
        let definition = probe_set
            .probes
            .get(index)
            .ok_or_else(|| format!("probe {} has no v1 definition", probe.as_str()))?;
        let input = looklab_model::LookInput::new(definition.xyz);
        let cell = CellId::new(format!("cell-{}", looklab_fit::model_error::cell(&input)))
            .map_err(|error| error.to_string())?;
        support.push(NativeProbeSupport {
            probe: probe.clone(),
            cell,
            supported: estimate.supported,
            held_out_evidence: field.evidence_at(&input, space),
        });
    }
    Ok(support)
}

pub fn assess_native(input: NativeAssessmentInput) -> ProtocolAssessment {
    let NativeAssessmentInput {
        mut binding,
        encoding,
        pinned,
        support,
        training_groups,
        observations,
        support_sigma,
        min_independent_groups,
    } = input;
    let rule =
        RuleRevision::new(crate::assessment::NATIVE_PREDICTION_RULE).expect("native rule revision");
    let mut metrics = ReportMetrics::new();
    let mut limitations = Vec::new();
    let mut requests = Vec::new();
    let mut execution = false;
    let mut failed = false;
    let mut insufficient = false;

    if let Err(detail) = binding.validate() {
        execution = true;
        limitations.push(limitation(detail));
    }
    if !matches!(&binding.subject, ModelSubject::BodyRealization { .. }) {
        execution = true;
        limitations.push(limitation(
            "native prediction is a claim about a named BodyRealization",
        ));
    }
    if let ModelSubject::BodyRealization { realization } = &binding.subject
        && !binding.scope.assessed_bodies.contains(&realization.body)
    {
        execution = true;
        limitations.push(limitation(
            "the report scope does not name the realization body",
        ));
    }
    if encoding != binding.scope.output_encoding {
        execution = true;
        limitations.push(limitation(
            "the supplied output encoding differs from the report scope",
        ));
    }
    if !support_sigma.is_finite() || support_sigma <= 0.0 {
        execution = true;
        limitations.push(limitation("support sigma is not a positive finite value"));
    }
    if min_independent_groups == 0 {
        execution = true;
        limitations.push(limitation(
            "the independent-group requirement must be positive",
        ));
    }

    let assessed_cells: BTreeSet<CellId> = support.iter().map(|entry| entry.cell.clone()).collect();
    let supported_cells: BTreeSet<CellId> = support
        .iter()
        .filter(|entry| entry.supported && entry.held_out_evidence)
        .map(|entry| entry.cell.clone())
        .collect();
    if supported_cells.is_empty() {
        insufficient = true;
        requests.push(evidence_request(
            EvidenceRequestKind::MissingIndependentCaptures,
            "no probe has supported mean-response evidence for the assessed encoding",
        ));
    }
    for point in &binding.scope.points {
        if resolve_point(point, &pinned).is_none() {
            insufficient = true;
            requests.push(evidence_request(
                EvidenceRequestKind::MissingControlValues,
                "an assessed point does not resolve to one pinned firmware and WB mode",
            ));
        }
    }

    let training: BTreeSet<EvidenceGroup> = training_groups.iter().cloned().collect();
    let mut leaked = false;
    let mut synthetic = false;
    let mut residuals: BTreeMap<(EvidenceGroup, CellId, AssessedPoint), Vec<[f64; 3]>> =
        BTreeMap::new();
    for observation in &observations {
        if training.contains(&observation.group) {
            leaked = true;
            continue;
        }
        if observation.source == EvidenceSource::Synthetic {
            synthetic = true;
            continue;
        }
        let Some(point) = binding.scope.points.iter().find(|point| {
            point_matches(
                point,
                &pinned,
                &observation.settings,
                observation.firmware.as_ref(),
                observation.wb_mode.as_ref(),
            )
        }) else {
            limitations.push(limitation(
                "a held-out observation lies outside the assessed physical points",
            ));
            continue;
        };
        if !assessed_cells.contains(&observation.cell) {
            limitations.push(limitation(format!(
                "held-out cell {} is outside the probe-backed cells",
                observation.cell
            )));
            continue;
        }
        if !supported_cells.contains(&observation.cell) {
            limitations.push(limitation(format!(
                "held-out cell {} is outside the supported subset",
                observation.cell
            )));
            continue;
        }
        if !finite_ucs(observation.predicted) || !finite_ucs(observation.observed) {
            execution = true;
            continue;
        }
        residuals
            .entry((
                observation.group.clone(),
                observation.cell.clone(),
                point.clone(),
            ))
            .or_default()
            .push([
                observation.predicted.j - observation.observed.j,
                observation.predicted.a - observation.observed.a,
                observation.predicted.b - observation.observed.b,
            ]);
    }
    if leaked {
        insufficient = true;
        requests.push(evidence_request(
            EvidenceRequestKind::MissingIndependentCaptures,
            "same-RAW re-render groups cross the training/assessment partition",
        ));
    }
    if synthetic {
        limitations.push(limitation(
            "synthetic held-out renders are engine validation, not observed camera evidence",
        ));
    }

    let mut group_means: BTreeMap<(AssessedPoint, CellId), Vec<[f64; 3]>> = BTreeMap::new();
    for ((_, cell, point), vectors) in &residuals {
        if let Some(mean) = vector_mean(vectors) {
            group_means
                .entry((point.clone(), cell.clone()))
                .or_default()
                .push(mean);
        }
    }
    let mut cell_discrepancies = Vec::new();
    for point in &binding.scope.points {
        let mut point_covered = false;
        for cell in &supported_cells {
            let key = (point.clone(), cell.clone());
            let Some(means) = group_means.get(&key) else {
                insufficient = true;
                requests.push(evidence_request(
                    EvidenceRequestKind::MissingIndependentCaptures,
                    format!("missing independent held-out evidence for point {point:?} in {cell}"),
                ));
                continue;
            };
            if means.len() < min_independent_groups {
                insufficient = true;
                requests.push(evidence_request(
                    EvidenceRequestKind::MissingIndependentCaptures,
                    format!(
                        "point {point:?} in {cell} has {} independent groups; {} required",
                        means.len(),
                        min_independent_groups
                    ),
                ));
                continue;
            }
            point_covered = true;
            let Some(cell_mean) = vector_mean(means) else {
                continue;
            };
            let discrepancy = norm(cell_mean);
            cell_discrepancies.push(discrepancy);
            let uncertainty = vector_covariance(means).map(|covariance| {
                (covariance[0][0] + covariance[1][1] + covariance[2][2])
                    .max(0.0)
                    .sqrt()
            });
            if discrepancy > support_sigma {
                failed = true;
                match uncertainty {
                    Some(uncertainty) => limitations.push(limitation(format!(
                        "conditional mean discrepancy {discrepancy:.3} ΔE for {point:?} in {cell} exceeds the {support_sigma:.3} ΔE bound (standard error {uncertainty:.3})"
                    ))),
                    None => limitations.push(limitation(format!(
                        "conditional mean discrepancy {discrepancy:.3} ΔE for {point:?} in {cell} exceeds the {support_sigma:.3} ΔE bound"
                    ))),
                }
            }
        }
        if !point_covered {
            insufficient = true;
            requests.push(evidence_request(
                EvidenceRequestKind::MissingIndependentCaptures,
                format!("claimed point {point:?} has no supported-cell evidence"),
            ));
        }
    }
    if let Some(value) = median(&cell_discrepancies) {
        metric(&mut metrics, MetricKey::MedianDe, value);
    }

    let mask: SupportedMask = supported_mask(supported_cells.iter().cloned());
    binding.scope.colour_cells = mask.iter().cloned().collect();
    let supported_count = mask.len();
    let assessed_count = assessed_cells.len();
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
        "conditional mean response on the supported subset, not an absolute scene-XYZ calibration",
    ));
    limitations.push(limitation(
        "future-JPEG observation noise is not part of the stored mean-response uncertainty",
    ));

    let membership = evidence_membership(
        training_groups,
        observations
            .iter()
            .map(|entry| entry.group.clone())
            .collect(),
    );
    build_report(
        &binding,
        ReportParts {
            capability: QualificationCapability::NativePrediction,
            rule,
            outcome: outcome(execution, failed, insufficient),
            metrics,
            supported_mask: mask,
            limitations,
            evidence_requests: requests,
            extra_dependencies: Vec::new(),
            membership,
            cohort: None,
        },
    )
}

fn finite_ucs(value: Ucs) -> bool {
    value.j.is_finite() && value.a.is_finite() && value.b.is_finite()
}

fn vector_mean(vectors: &[[f64; 3]]) -> Option<[f64; 3]> {
    if vectors.is_empty() {
        return None;
    }
    let count = vectors.len() as f64;
    let mut sum = [0.0_f64; 3];
    for vector in vectors {
        for (slot, value) in sum.iter_mut().zip(vector) {
            *slot += *value;
        }
    }
    Some([sum[0] / count, sum[1] / count, sum[2] / count])
}

fn norm(vector: [f64; 3]) -> f64 {
    (vector[0] * vector[0] + vector[1] * vector[1] + vector[2] * vector[2]).sqrt()
}

fn vector_covariance(vectors: &[[f64; 3]]) -> Option<[[f64; 3]; 3]> {
    if vectors.len() < 2 {
        return None;
    }
    let mean = vector_mean(vectors)?;
    let count = vectors.len() as f64;
    let mut covariance = [[0.0_f64; 3]; 3];
    for vector in vectors {
        let delta = [
            vector[0] - mean[0],
            vector[1] - mean[1],
            vector[2] - mean[2],
        ];
        for row in 0..3 {
            for column in 0..3 {
                covariance[row][column] += delta[row] * delta[column];
            }
        }
    }
    for row in &mut covariance {
        for value in row {
            *value /= (count - 1.0) * count;
        }
    }
    Some(covariance)
}
