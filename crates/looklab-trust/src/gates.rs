// SPDX-License-Identifier: AGPL-3.0-or-later
//! Release gates against the previous version and the anchor ([docs/05 §8]).
//!
//! Root non-regression compares each version's ΔE to the admitted root observations,
//! not to the anchor prediction. Drift is measured on post-clip predictions, with
//! pre-clip `J'` and hue drift bounded by the same ceiling. Every input validation
//! failure is a violation and fails the gate: an omitted probe cannot pass.

use std::collections::{BTreeMap, BTreeSet};

use looklab_model::ProbeId;
use serde::{Deserialize, Serialize};

use crate::params::TrustParams;
use crate::region::{EXPOSURE_MAX, EXPOSURE_MIN};

/// One probe's fit output before support is decided.
#[derive(Clone, Debug, PartialEq)]
pub struct ProbePrediction {
    pub id: ProbeId,
    /// Pre-clip `J'a'b'` the Look defines.
    pub pre_clip: [f64; 3],
    /// `σ_ΔE` of the pre-clip prediction.
    pub sigma_pre_clip: f64,
    /// Post-clip `J'a'b'` in the version's reference output encoding.
    pub post_clip: [f64; 3],
    /// `σ_ΔE` of the post-clip prediction.
    pub sigma_post_clip: f64,
    /// `|b_p|`: the identity-prior-attributable bias, ΔE.
    pub bias_de: f64,
    /// Absolute tempered effective weight per bin at the probe.
    pub density: f64,
    /// Distinct images with non-negligible weight near the probe.
    pub clusters: usize,
    /// The soft clip at the pre-clip prediction is not saturated: its smallest
    /// per-channel derivative is at least [`IDENTIFIED_CLIP_DERIVATIVE`].
    pub pre_clip_identified: bool,
}

/// Smallest soft-clip derivative at which the pre-clip value counts as identified
/// (docs/05 §8). The fit sets [`ProbePrediction::pre_clip_identified`].
pub const IDENTIFIED_CLIP_DERIVATIVE: f64 = 0.25;

/// Thresholds for the four support tests (docs/05 §8). Mirror the fit's
/// `FitOptions` support fields; a worker must configure both from the same values.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SupportThresholds {
    /// Post-clip `σ_ΔE` at which a probe enters support.
    pub sigma: f64,
    /// Post-clip `σ_ΔE` at which a supported probe leaves support.
    pub sigma_exit: f64,
    /// `|b_p|` at which a probe enters support, as a fraction of [`Self::sigma`].
    pub bias_fraction: f64,
    /// Factor on the bias threshold once a probe is supported.
    pub bias_exit_factor: f64,
    /// Absolute tempered effective weight per bin a probe's region needs.
    pub density_floor: f64,
    /// Distinct-image clusters a probe needs.
    pub min_clusters: usize,
}

impl Default for SupportThresholds {
    fn default() -> Self {
        Self {
            sigma: 1.5,
            sigma_exit: 1.65,
            bias_fraction: 0.33,
            bias_exit_factor: 1.1,
            density_floor: 0.5,
            min_clusters: 3,
        }
    }
}

/// The four support tests at one probe (docs/05 §8).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeSupport {
    /// The post-clip σ passed the enter or the exit threshold.
    pub sigma: bool,
    /// The region's density reached the absolute floor.
    pub density: bool,
    /// The probe has enough distinct-image clusters.
    pub clusters: bool,
    /// `|b_p|` passed the enter or the exit threshold.
    pub bias: bool,
}

impl ProbeSupport {
    /// A probe is supported only when every cause is.
    pub fn supported(&self) -> bool {
        self.sigma && self.density && self.clusters && self.bias
    }
}

/// One version's prediction at one probe.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeEstimate {
    pub id: ProbeId,
    /// Pre-clip `J'a'b'` the Look defines.
    pub pre_clip: [f64; 3],
    /// `σ_ΔE` of the pre-clip prediction.
    pub sigma_pre_clip: f64,
    /// Post-clip `J'a'b'` after the reference encoding and the version's soft clip.
    pub post_clip: [f64; 3],
    /// `σ_ΔE` of the post-clip prediction.
    pub sigma_post_clip: f64,
    /// `|b_p|`: the identity-prior-attributable bias, ΔE.
    pub bias_de: f64,
    pub support: ProbeSupport,
    /// The pre-clip value is identified in this version (see [`ProbePrediction`]).
    pub pre_clip_identified: bool,
}

impl ProbeEstimate {
    /// Decides support from a fit's raw quantities, using each cause's exit
    /// threshold where the previous published version supported it (docs/05 §8).
    pub fn new(
        prediction: ProbePrediction,
        previous: Option<&ProbeEstimate>,
        thresholds: &SupportThresholds,
    ) -> Self {
        let sigma_supported = previous.is_some_and(|probe| probe.support.sigma);
        let bias_supported = previous.is_some_and(|probe| probe.support.bias);
        let sigma_threshold = if sigma_supported {
            thresholds.sigma_exit
        } else {
            thresholds.sigma
        };
        let bias_threshold = thresholds.bias_fraction * thresholds.sigma;
        let bias_threshold = if bias_supported {
            thresholds.bias_exit_factor * bias_threshold
        } else {
            bias_threshold
        };
        Self {
            id: prediction.id,
            pre_clip: prediction.pre_clip,
            sigma_pre_clip: prediction.sigma_pre_clip,
            post_clip: prediction.post_clip,
            sigma_post_clip: prediction.sigma_post_clip,
            bias_de: prediction.bias_de,
            support: ProbeSupport {
                sigma: prediction.sigma_post_clip <= sigma_threshold,
                density: prediction.density >= thresholds.density_floor,
                clusters: prediction.clusters >= thresholds.min_clusters,
                bias: prediction.bias_de <= bias_threshold,
            },
            pre_clip_identified: prediction.pre_clip_identified,
        }
    }

    pub fn supported(&self) -> bool {
        self.support.supported()
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

/// Probes supported in any version published since the current anchor.
/// Re-anchoring restarts the set from the new anchor (docs/05 §8).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GatedUnion {
    probes: BTreeSet<ProbeId>,
}

impl GatedUnion {
    /// The union a re-anchor starts from: the new anchor's supported probes.
    pub fn at_anchor(anchor: &VersionProbes) -> Self {
        let mut union = Self::default();
        union.include(anchor);
        union
    }

    pub fn include(&mut self, version: &VersionProbes) {
        self.probes.extend(
            version
                .probes
                .iter()
                .filter(|probe| probe.supported())
                .map(|probe| probe.id.clone()),
        );
    }

    pub fn contains(&self, probe: &ProbeId) -> bool {
        self.probes.contains(probe)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GateInput {
    pub candidate: VersionProbes,
    pub previous: VersionProbes,
    pub anchor: VersionProbes,
    pub root_observations: Vec<RootObservation>,
    /// Probes supported in any published version since the current anchor.
    #[serde(default)]
    pub gated_since_anchor: GatedUnion,
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
    /// Gated probes that neither compared version supports, carried by the union.
    pub carried: usize,
    pub allowed_over_bound: usize,
    pub over_ceiling: Vec<GatedProbe>,
    pub over_bound: Vec<GatedProbe>,
    pub pre_clip_over_ceiling: Vec<GatedProbe>,
    pub hue_over_ceiling: Vec<GatedProbe>,
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
    PreClipOverCeiling {
        reference: GateReference,
        probe: ProbeId,
        shift: f64,
        ceiling: f64,
    },
    HueOverCeiling {
        reference: GateReference,
        probe: ProbeId,
        shift: f64,
        ceiling: f64,
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
        &input.gated_since_anchor,
        GateReference::Previous,
        input_valid,
    );
    let drift_anchor = drift_gate(
        params,
        &candidate,
        &anchor,
        &input.gated_since_anchor,
        GateReference::Anchor,
        input_valid,
    );

    let mut coverage_changes: BTreeSet<ProbeId> = BTreeSet::new();
    let mut regressions: Vec<(ProbeId, f64, f64)> = Vec::new();
    let threshold_scale = 1.0 + params.gate_support_margin;
    for (id, probe) in &candidate {
        if probe.supported() {
            continue;
        }
        let sigma_reference = [previous.get(id), anchor.get(id)]
            .into_iter()
            .flatten()
            .filter(|estimate| estimate.supported())
            .map(|estimate| estimate.sigma_post_clip)
            .max_by(f64::total_cmp);
        let Some(sigma_reference) = sigma_reference else {
            if input.gated_since_anchor.contains(id) {
                coverage_changes.insert(id.clone());
            }
            continue;
        };
        if !probe.support.sigma {
            let threshold = threshold_scale * sigma_reference;
            if probe.sigma_post_clip > params.support_sigma_exit
                && probe.sigma_post_clip > threshold
            {
                regressions.push((id.clone(), probe.sigma_post_clip, threshold));
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
        candidate_errors.push(distance(&candidate_probe.post_clip, &observation.jab));
        reference_errors.push(distance(&reference_probe.post_clip, &observation.jab));
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

fn drift_bounds(params: &TrustParams, reference: GateReference) -> (f64, f64) {
    match reference {
        GateReference::Previous => (params.gate_previous_floor, params.gate_previous_ceiling),
        GateReference::Anchor => (params.gate_anchor_floor, params.gate_anchor_ceiling),
    }
}

fn drift_gate(
    params: &TrustParams,
    candidate: &BTreeMap<ProbeId, &ProbeEstimate>,
    reference_probes: &BTreeMap<ProbeId, &ProbeEstimate>,
    sticky: &GatedUnion,
    reference: GateReference,
    input_valid: bool,
) -> DriftGateReport {
    let (floor, ceiling) = drift_bounds(params, reference);
    let ids: BTreeSet<&ProbeId> = candidate.keys().chain(reference_probes.keys()).collect();
    let mut gated = 0usize;
    let mut carried = 0usize;
    let mut over_ceiling = Vec::new();
    let mut over_bound = Vec::new();
    let mut pre_clip_over_ceiling = Vec::new();
    let mut hue_over_ceiling = Vec::new();
    let mut ratios = Vec::new();

    for probe_id in ids {
        let (Some(candidate_probe), Some(reference_probe)) =
            (candidate.get(probe_id), reference_probes.get(probe_id))
        else {
            continue;
        };
        let supported = candidate_probe.supported() || reference_probe.supported();
        let bounded = supported || sticky.contains(probe_id);
        if bounded {
            gated += 1;
            if !supported {
                carried += 1;
            }
        }
        let shift = distance(&candidate_probe.post_clip, &reference_probe.post_clip);
        let sigma_squared = candidate_probe.sigma_post_clip * candidate_probe.sigma_post_clip
            + reference_probe.sigma_post_clip * reference_probe.sigma_post_clip;
        let bound = (params.gate_bound_scale * sigma_squared.sqrt()).clamp(floor, ceiling);
        if shift > ceiling {
            over_ceiling.push(GatedProbe {
                probe: probe_id.clone(),
                shift,
                bound,
            });
        } else if bounded {
            ratios.push(shift / bound);
            if shift > bound {
                over_bound.push(GatedProbe {
                    probe: probe_id.clone(),
                    shift,
                    bound,
                });
            }
        }

        if !(candidate_probe.pre_clip_identified && reference_probe.pre_clip_identified) {
            continue;
        }
        let pre_clip_lightness = (candidate_probe.pre_clip[0] - reference_probe.pre_clip[0]).abs();
        if pre_clip_lightness > ceiling {
            pre_clip_over_ceiling.push(GatedProbe {
                probe: probe_id.clone(),
                shift: pre_clip_lightness,
                bound: ceiling,
            });
        }
        let hue = hue_drift_ucs(&candidate_probe.pre_clip, &reference_probe.pre_clip);
        if hue > ceiling {
            hue_over_ceiling.push(GatedProbe {
                probe: probe_id.clone(),
                shift: hue,
                bound: ceiling,
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
        && pre_clip_over_ceiling.is_empty()
        && hue_over_ceiling.is_empty()
        && p95_ratio <= params.gate_p95_ratio;

    DriftGateReport {
        reference,
        floor,
        ceiling,
        gated,
        carried,
        allowed_over_bound: allowed,
        over_ceiling,
        over_bound,
        pre_clip_over_ceiling,
        hue_over_ceiling,
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
    violations.extend(report.pre_clip_over_ceiling.iter().map(|gated| {
        Violation::PreClipOverCeiling {
            reference: report.reference,
            probe: gated.probe.clone(),
            shift: gated.shift,
            ceiling: report.ceiling,
        }
    }));
    violations.extend(
        report
            .hue_over_ceiling
            .iter()
            .map(|gated| Violation::HueOverCeiling {
                reference: report.reference,
                probe: gated.probe.clone(),
                shift: gated.shift,
                ceiling: report.ceiling,
            }),
    );
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

/// `ΔH_ab = 2·sqrt(C₁·C₂)·sin(Δh/2)`, the CIEDE2000 hue difference
/// (Sharma, Wu & Dalal 2005, eq. 10).
fn hue_drift_ucs(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    let chroma_a = (a[1] * a[1] + a[2] * a[2]).sqrt();
    let chroma_b = (b[1] * b[1] + b[2] * b[2]).sqrt();
    if chroma_a < 1e-9 || chroma_b < 1e-9 {
        return 0.0;
    }
    let mut delta = libm::atan2(a[2], a[1]) - libm::atan2(b[2], b[1]);
    if delta > core::f64::consts::PI {
        delta -= core::f64::consts::TAU;
    } else if delta < -core::f64::consts::PI {
        delta += core::f64::consts::TAU;
    }
    2.0 * (chroma_a * chroma_b).sqrt() * libm::sin(0.5 * delta.abs())
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
