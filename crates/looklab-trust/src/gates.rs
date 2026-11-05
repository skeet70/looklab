// SPDX-License-Identifier: AGPL-3.0-or-later
//! Release gates against the previous version and the anchor ([docs/05 §8]).
//!
//! Root non-regression compares each version's ΔE to the admitted root observations,
//! not to the anchor prediction. Every input validation failure is a violation and
//! fails the gate: an omitted probe cannot pass.

use std::collections::{BTreeMap, BTreeSet};

use looklab_model::ProbeId;
use serde::{Deserialize, Serialize};

use crate::params::TrustParams;
use crate::region::{EXPOSURE_MAX, EXPOSURE_MIN};

/// One version's prediction at one probe.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeEstimate {
    pub id: ProbeId,
    /// Predicted CAM16-UCS `J'a'b'`.
    pub jab: [f64; 3],
    /// `σ_ΔE` of this version at the probe.
    pub sigma: f64,
    /// `sigma <= support_sigma`.
    pub sigma_ok: bool,
    /// The probe's local sample density reaches the fit's density floor.
    pub density_ok: bool,
    /// The fit's support rule: `sigma_ok && density_ok` and outside the clip shoulder.
    pub supported: bool,
}

impl ProbeEstimate {
    /// Derives `supported` for a caller that has no clip-shoulder verdict.
    pub fn new(
        id: ProbeId,
        jab: [f64; 3],
        sigma: f64,
        density_ok: bool,
        support_sigma: f64,
    ) -> Self {
        let sigma_ok = sigma <= support_sigma;
        Self {
            id,
            jab,
            sigma,
            sigma_ok,
            density_ok,
            supported: sigma_ok && density_ok,
        }
    }

    /// Carries the fit's `supported` flag, which also covers the clip shoulder.
    pub fn from_fit(
        id: ProbeId,
        jab: [f64; 3],
        sigma: f64,
        density_ok: bool,
        supported: bool,
        support_sigma: f64,
    ) -> Self {
        Self {
            id,
            jab,
            sigma,
            sigma_ok: sigma <= support_sigma,
            density_ok,
            supported,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VersionProbes {
    pub probes: Vec<ProbeEstimate>,
}

/// An admitted root session's measured `J'a'b'` at one probe.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RootObservation {
    pub id: ProbeId,
    pub jab: [f64; 3],
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GateInput {
    pub candidate: VersionProbes,
    pub previous: VersionProbes,
    pub anchor: VersionProbes,
    pub root_observations: Vec<RootObservation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionRole {
    Candidate,
    Previous,
    Anchor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateReference {
    Previous,
    Anchor,
}

/// Root-probe ΔE of the candidate compared with a reference version's error.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RootGateReport {
    pub reference: GateReference,
    pub probes: usize,
    pub median: f64,
    pub median_reference: f64,
    pub median_allowance: f64,
    pub p95: f64,
    pub p95_reference: f64,
    pub p95_allowance: f64,
    pub pass: bool,
}

/// A gated probe and its computed drift bound.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GatedProbe {
    pub probe: ProbeId,
    pub shift: f64,
    pub bound: f64,
}

/// Bounded-drift comparison of the candidate against one reference version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DriftGateReport {
    pub reference: GateReference,
    pub floor: f64,
    pub ceiling: f64,
    pub gated: usize,
    pub allowed_over_bound: usize,
    pub over_ceiling: Vec<GatedProbe>,
    pub over_bound: Vec<GatedProbe>,
    pub p95_ratio: f64,
    pub pass: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "criterion", rename_all = "snake_case")]
pub enum Violation {
    DuplicateProbe {
        role: VersionRole,
        probe: ProbeId,
    },
    MissingProbe {
        role: VersionRole,
        probe: ProbeId,
    },
    RootMedian {
        reference: GateReference,
        value: f64,
        reference_value: f64,
        allowance: f64,
    },
    RootP95 {
        reference: GateReference,
        value: f64,
        reference_value: f64,
        allowance: f64,
    },
    OverCeiling {
        reference: GateReference,
        probe: ProbeId,
        shift: f64,
        ceiling: f64,
    },
    OverBound {
        reference: GateReference,
        probe: ProbeId,
        shift: f64,
        bound: f64,
    },
    TooManyOverBound {
        reference: GateReference,
        count: usize,
        allowed: usize,
    },
    P95Ratio {
        reference: GateReference,
        value: f64,
        limit: f64,
    },
    SupportedToUnsupported {
        probe: ProbeId,
        #[serde(default)]
        sigma: f64,
        #[serde(default)]
        threshold: f64,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GateReport {
    pub root_previous: RootGateReport,
    pub root_anchor: RootGateReport,
    pub drift_previous: DriftGateReport,
    pub drift_anchor: DriftGateReport,
    /// Probes that lost support without a σ violation.
    #[serde(default)]
    pub coverage_changes: Vec<ProbeId>,
    pub supported_regressions: Vec<ProbeId>,
    pub violations: Vec<Violation>,
    pub passed: bool,
}

pub fn gate_release(params: &TrustParams, input: &GateInput) -> GateReport {
    let mut violations = Vec::new();
    let candidate = index(&input.candidate, VersionRole::Candidate, &mut violations);
    let previous = index(&input.previous, VersionRole::Previous, &mut violations);
    let anchor = index(&input.anchor, VersionRole::Anchor, &mut violations);

    let all_ids: BTreeSet<ProbeId> = candidate
        .keys()
        .chain(previous.keys())
        .chain(anchor.keys())
        .chain(
            input
                .root_observations
                .iter()
                .map(|observation| &observation.id),
        )
        .cloned()
        .collect();
    for id in &all_ids {
        for (role, map) in [
            (VersionRole::Candidate, &candidate),
            (VersionRole::Previous, &previous),
            (VersionRole::Anchor, &anchor),
        ] {
            if !map.contains_key(id) {
                violations.push(Violation::MissingProbe {
                    role,
                    probe: id.clone(),
                });
            }
        }
    }
    let input_valid = violations.is_empty();

    let root_previous = root_gate(
        params,
        &candidate,
        &previous,
        &anchor,
        &input.root_observations,
        GateReference::Previous,
        input_valid,
    );
    let root_anchor = root_gate(
        params,
        &candidate,
        &previous,
        &anchor,
        &input.root_observations,
        GateReference::Anchor,
        input_valid,
    );
    let drift_previous = drift_gate(
        params,
        &candidate,
        &previous,
        GateReference::Previous,
        params.gate_previous_floor,
        params.gate_previous_ceiling,
        input_valid,
    );
    let drift_anchor = drift_gate(
        params,
        &candidate,
        &anchor,
        GateReference::Anchor,
        params.gate_anchor_floor,
        params.gate_anchor_ceiling,
        input_valid,
    );

    let mut coverage_changes: BTreeSet<ProbeId> = BTreeSet::new();
    let mut regressions: Vec<(ProbeId, f64, f64)> = Vec::new();
    let threshold_scale = 1.0 + params.gate_support_margin;
    for (id, probe) in &candidate {
        if probe.supported {
            continue;
        }
        let sigma_reference = [previous.get(id), anchor.get(id)]
            .into_iter()
            .flatten()
            .filter(|estimate| estimate.supported)
            .map(|estimate| estimate.sigma)
            .max_by(f64::total_cmp);
        let Some(sigma_reference) = sigma_reference else {
            continue;
        };
        if !probe.sigma_ok {
            let threshold = threshold_scale * sigma_reference;
            if probe.sigma > threshold {
                regressions.push((id.clone(), probe.sigma, threshold));
                continue;
            }
        }
        coverage_changes.insert(id.clone());
    }
    let supported_regressions: Vec<ProbeId> = regressions
        .iter()
        .map(|(probe, _, _)| probe.clone())
        .collect();
    let coverage_changes: Vec<ProbeId> = coverage_changes.into_iter().collect();

    collect_root_violations(&root_previous, &mut violations);
    collect_root_violations(&root_anchor, &mut violations);
    collect_drift_violations(&drift_previous, &mut violations);
    collect_drift_violations(&drift_anchor, &mut violations);
    violations.extend(regressions.into_iter().map(|(probe, sigma, threshold)| {
        Violation::SupportedToUnsupported {
            probe,
            sigma,
            threshold,
        }
    }));

    let passed = input_valid
        && root_previous.pass
        && root_anchor.pass
        && drift_previous.pass
        && drift_anchor.pass
        && supported_regressions.is_empty();

    GateReport {
        root_previous,
        root_anchor,
        drift_previous,
        drift_anchor,
        coverage_changes,
        supported_regressions,
        violations,
        passed,
    }
}

fn index<'a>(
    version: &'a VersionProbes,
    role: VersionRole,
    violations: &mut Vec<Violation>,
) -> BTreeMap<ProbeId, &'a ProbeEstimate> {
    let mut map = BTreeMap::new();
    for probe in &version.probes {
        if map.insert(probe.id.clone(), probe).is_some() {
            violations.push(Violation::DuplicateProbe {
                role,
                probe: probe.id.clone(),
            });
        }
    }
    map
}

fn distance(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    let d0 = a[0] - b[0];
    let d1 = a[1] - b[1];
    let d2 = a[2] - b[2];
    (d0 * d0 + d1 * d1 + d2 * d2).sqrt()
}

fn root_gate(
    params: &TrustParams,
    candidate: &BTreeMap<ProbeId, &ProbeEstimate>,
    previous: &BTreeMap<ProbeId, &ProbeEstimate>,
    anchor: &BTreeMap<ProbeId, &ProbeEstimate>,
    observations: &[RootObservation],
    reference: GateReference,
    input_valid: bool,
) -> RootGateReport {
    let (median_allowance, p95_allowance) = match reference {
        GateReference::Previous => (
            params.gate_root_median_previous,
            params.gate_root_p95_previous,
        ),
        GateReference::Anchor => (params.gate_root_median_anchor, params.gate_root_p95_anchor),
    };

    let mut candidate_errors = Vec::new();
    let mut reference_errors = Vec::new();
    for observation in observations {
        let (Some(candidate_probe), Some(reference_probe)) = (
            candidate.get(&observation.id),
            match reference {
                GateReference::Previous => previous.get(&observation.id),
                GateReference::Anchor => anchor.get(&observation.id),
            },
        ) else {
            continue;
        };
        candidate_errors.push(distance(&candidate_probe.jab, &observation.jab));
        reference_errors.push(distance(&reference_probe.jab, &observation.jab));
    }

    if observations.is_empty() {
        return RootGateReport {
            reference,
            probes: 0,
            median: 0.0,
            median_reference: 0.0,
            median_allowance,
            p95: 0.0,
            p95_reference: 0.0,
            p95_allowance,
            pass: input_valid,
        };
    }

    let median = quantile(&candidate_errors, 0.5);
    let median_reference = quantile(&reference_errors, 0.5);
    let p95 = quantile(&candidate_errors, 0.95);
    let p95_reference = quantile(&reference_errors, 0.95);
    let pass = input_valid
        && candidate_errors.len() == observations.len()
        && median - median_reference <= median_allowance
        && p95 - p95_reference <= p95_allowance;
    RootGateReport {
        reference,
        probes: candidate_errors.len(),
        median,
        median_reference,
        median_allowance,
        p95,
        p95_reference,
        p95_allowance,
        pass,
    }
}

fn drift_gate(
    params: &TrustParams,
    candidate: &BTreeMap<ProbeId, &ProbeEstimate>,
    reference_probes: &BTreeMap<ProbeId, &ProbeEstimate>,
    reference: GateReference,
    floor: f64,
    ceiling: f64,
    input_valid: bool,
) -> DriftGateReport {
    let ids: BTreeSet<&ProbeId> = candidate.keys().chain(reference_probes.keys()).collect();
    let mut gated = 0usize;
    let mut over_ceiling = Vec::new();
    let mut over_bound = Vec::new();
    let mut ratios = Vec::new();

    for probe_id in ids {
        let (Some(candidate_probe), Some(reference_probe)) =
            (candidate.get(probe_id), reference_probes.get(probe_id))
        else {
            continue;
        };
        if !candidate_probe.supported && !reference_probe.supported {
            continue;
        }
        gated += 1;
        let shift = distance(&candidate_probe.jab, &reference_probe.jab);
        let sigma_squared = candidate_probe.sigma * candidate_probe.sigma
            + reference_probe.sigma * reference_probe.sigma;
        let bound = (params.gate_bound_scale * sigma_squared.sqrt()).clamp(floor, ceiling);
        ratios.push(shift / bound);
        if shift > ceiling {
            over_ceiling.push(GatedProbe {
                probe: probe_id.clone(),
                shift,
                bound,
            });
        } else if shift > bound {
            over_bound.push(GatedProbe {
                probe: probe_id.clone(),
                shift,
                bound,
            });
        }
    }

    let allowed = (gated as f64 * params.gate_over_bound_fraction).floor() as usize;
    let p95_ratio = if ratios.is_empty() {
        0.0
    } else {
        quantile(&ratios, 0.95)
    };
    let pass = input_valid
        && over_ceiling.is_empty()
        && over_bound.len() <= allowed
        && p95_ratio <= params.gate_p95_ratio;

    DriftGateReport {
        reference,
        floor,
        ceiling,
        gated,
        allowed_over_bound: allowed,
        over_ceiling,
        over_bound,
        p95_ratio,
        pass,
    }
}

fn collect_root_violations(report: &RootGateReport, violations: &mut Vec<Violation>) {
    if report.median - report.median_reference > report.median_allowance {
        violations.push(Violation::RootMedian {
            reference: report.reference,
            value: report.median,
            reference_value: report.median_reference,
            allowance: report.median_allowance,
        });
    }
    if report.p95 - report.p95_reference > report.p95_allowance {
        violations.push(Violation::RootP95 {
            reference: report.reference,
            value: report.p95,
            reference_value: report.p95_reference,
            allowance: report.p95_allowance,
        });
    }
}

fn collect_drift_violations(report: &DriftGateReport, violations: &mut Vec<Violation>) {
    violations.extend(
        report
            .over_ceiling
            .iter()
            .map(|gated| Violation::OverCeiling {
                reference: report.reference,
                probe: gated.probe.clone(),
                shift: gated.shift,
                ceiling: report.ceiling,
            }),
    );
    violations.extend(report.over_bound.iter().map(|gated| Violation::OverBound {
        reference: report.reference,
        probe: gated.probe.clone(),
        shift: gated.shift,
        bound: gated.bound,
    }));
    if report.over_bound.len() > report.allowed_over_bound {
        violations.push(Violation::TooManyOverBound {
            reference: report.reference,
            count: report.over_bound.len(),
            allowed: report.allowed_over_bound,
        });
    }
    if report.p95_ratio > 1.0 {
        violations.push(Violation::P95Ratio {
            reference: report.reference,
            value: report.p95_ratio,
            limit: 1.0,
        });
    }
}

/// Nearest-rank for `q > 0.5`; an even-length median is the mean of the two middle values.
fn quantile(values: &[f64], q: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    if n == 0 {
        return 0.0;
    }
    if q <= 0.5 {
        if n.is_multiple_of(2) {
            0.5 * (sorted[n / 2 - 1] + sorted[n / 2])
        } else {
            sorted[n / 2]
        }
    } else {
        let rank = (q * n as f64).ceil() as usize;
        sorted[rank.clamp(1, n) - 1]
    }
}

/// Converts an evaluation of the old Look into the candidate's exposure gauge
/// ([docs/02 §2.1]): `L'(XYZ) = L(2^Δξ · XYZ)`.
///
/// [docs/02 §2.1]: ../../../docs/02-color-pipeline.md
pub fn convert_exposure_gauge<F>(evaluate: F, delta_xi: f64) -> impl Fn([f64; 3]) -> [f64; 3]
where
    F: Fn([f64; 3]) -> [f64; 3],
{
    let scale = libm::pow(2.0, delta_xi);
    move |xyz: [f64; 3]| evaluate([xyz[0] * scale, xyz[1] * scale, xyz[2] * scale])
}

/// True when the probe's rescaled exposure leaves the Look input clamp.
pub fn exposure_gauge_excluded(s: f64, delta_xi: f64) -> bool {
    let shifted = s + delta_xi;
    shifted > EXPOSURE_MAX || shifted < EXPOSURE_MIN
}
