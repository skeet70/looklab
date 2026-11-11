// SPDX-License-Identifier: AGPL-3.0-or-later
//! Comparison of two colour functions on the probe set (docs/02 §7).
//!
//! A function maps adapted scene XYZ to CAM16-UCS `J'a'b'`: a [`Look`], a
//! fitted realization, or a baked artifact. Layer diagnostics and attribution
//! appear only for functions that expose a [`Look`].

use std::collections::BTreeMap;

use looklab_color::cam16::{Cam16Model, ViewingConditions};
use looklab_color::ciede2000::delta_e_ciede2000;
use looklab_color::lab::{Lab, xyz_to_lab};
use looklab_color::ucs::{Ucs, UcsSpace, delta_e_ucs, ucs_to_jmh};
use looklab_color::{Illuminant, Xyz};
use looklab_model::domain_penalty::check_published_paths;
use looklab_model::probes::{Probe, ProbeGroup, ProbeId, ProbeSet};
use looklab_model::{LayerValues, Look, LookInput};
use serde::Serialize;

use crate::summary::HueRegion;

/// Width of a hue bin in degrees; the probe set's hue grid has 24 bins.
pub const HUE_BIN_WIDTH: f64 = 15.0;
/// Number of hue bins in the region map and the chroma/hue/lightness curves.
pub const HUE_BIN_COUNT: usize = 24;

pub const CURVE_MIN_S: f64 = -6.0;
pub const CURVE_MAX_S: f64 = 3.0;
pub const CURVE_STEP_S: f64 = 0.25;

/// Lightness values for the chroma/hue/lightness small multiples.
pub const GRL_S_VALUES: [f64; 4] = [-4.0, -2.0, 0.0, 2.0];

/// Maximum ΔE reported as "no meaningful difference" (a tenth of the 1.0 JND).
pub const NO_DIFFERENCE_DE: f64 = 0.1;

const HISTOGRAM_BINS: usize = 20;
const WORST_COUNT: usize = 10;
pub(crate) const TONE_EFFECT_FLOOR: f64 = 0.1;
pub(crate) const TINT_EFFECT_FLOOR: f64 = 0.05;
pub(crate) const CHROMA_EFFECT_FLOOR: f64 = 0.005;
pub(crate) const HUE_EFFECT_FLOOR: f64 = 0.5;
pub(crate) const LIGHTNESS_EFFECT_FLOOR: f64 = 0.1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct FunctionName(String);

impl FunctionName {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for FunctionName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether the training data identifies a probe's prediction (docs/02 §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    Supported,
    Unsupported,
}

/// Predictive covariance of a `J'a'b'` prediction, in UCS units squared.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Covariance3 {
    pub jj: f64,
    pub ja: f64,
    pub jb: f64,
    pub aa: f64,
    pub ab: f64,
    pub bb: f64,
}

impl Covariance3 {
    pub const ZERO: Self = Self {
        jj: 0.0,
        ja: 0.0,
        jb: 0.0,
        aa: 0.0,
        ab: 0.0,
        bb: 0.0,
    };

    pub fn diagonal(sigma: [f64; 3]) -> Self {
        Self {
            jj: sigma[0] * sigma[0],
            aa: sigma[1] * sigma[1],
            bb: sigma[2] * sigma[2],
            ..Self::ZERO
        }
    }

    pub fn trace(self) -> f64 {
        self.jj + self.aa + self.bb
    }

    /// `σ_ΔE = sqrt(tr Σ)` (docs/02 §5).
    pub fn sigma(self) -> f64 {
        self.trace().max(0.0).sqrt()
    }
}

/// Per-layer predictive variances at an input, in each layer's units squared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct LayerCovariance {
    pub t: f64,
    pub n_a: f64,
    pub n_b: f64,
    pub log_g: f64,
    /// Degrees squared.
    pub r: f64,
    pub lambda: f64,
}

impl LayerCovariance {
    pub fn sigma(variance: f64) -> f64 {
        variance.max(0.0).sqrt()
    }
}

/// A colour transform that can be compared on the probe set.
pub trait LookFunction {
    /// `J'a'b'` prediction for adapted scene XYZ.
    fn predict(&self, xyz: Xyz) -> Ucs;

    /// Predictive covariance at a probe, when the function carries one.
    fn covariance(&self, _probe: &Probe) -> Option<Covariance3> {
        None
    }

    /// Probe support from the fit's coverage model, when the function has one.
    fn support(&self, _probe: &Probe) -> Support {
        Support::Supported
    }

    fn layer_values(&self, _input: &LookInput) -> Option<LayerValues> {
        None
    }

    /// A probe's input or a synthesized curve sample; probe-only callers pass
    /// `LookInput::new(probe.xyz)`.
    fn layer_covariance(&self, _input: &LookInput) -> Option<LayerCovariance> {
        None
    }

    /// The underlying [`Look`], which enables layer attribution.
    fn look(&self) -> Option<&Look> {
        None
    }
}

impl LookFunction for Look {
    fn predict(&self, xyz: Xyz) -> Ucs {
        self.eval(xyz)
    }

    fn support(&self, probe: &Probe) -> Support {
        let input = LookInput::new(probe.xyz);
        let values = self.layer_values(&input);
        if check_published_paths(&self.chroma_paths(&input, &values)).is_ok() {
            Support::Supported
        } else {
            Support::Unsupported
        }
    }

    fn layer_values(&self, input: &LookInput) -> Option<LayerValues> {
        Some(Look::layer_values(self, input))
    }

    fn look(&self) -> Option<&Look> {
        Some(self)
    }
}

/// The interpretable layers of a Look (docs/02 §3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LayerId {
    Tone,
    Tint,
    Chroma,
    Hue,
    Lightness,
    Residual,
}

impl LayerId {
    pub const ALL: [Self; 6] = [
        Self::Tone,
        Self::Tint,
        Self::Chroma,
        Self::Hue,
        Self::Lightness,
        Self::Residual,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Tone => "tone",
            Self::Tint => "neutral tint",
            Self::Chroma => "chroma gain",
            Self::Hue => "hue rotation",
            Self::Lightness => "lightness shift",
            Self::Residual => "residual",
        }
    }
}

/// How much swapping one layer of the second function into the first reduces
/// the mean ΔE on the probe set.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LayerContribution {
    pub layer: LayerId,
    /// Mean ΔE reduction, in CAM16-UCS units.
    pub contribution: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct HueBin {
    pub index: usize,
    pub start_deg: f64,
    pub end_deg: f64,
    pub region: HueRegion,
}

pub fn hue_bins() -> Vec<HueBin> {
    (0..HUE_BIN_COUNT)
        .map(|index| HueBin {
            index,
            start_deg: index as f64 * HUE_BIN_WIDTH,
            end_deg: (index + 1) as f64 * HUE_BIN_WIDTH,
            region: HueRegion::of_bin(index),
        })
        .collect()
}

pub fn hue_bin_index(hue_degrees: f64) -> usize {
    let mut hue = hue_degrees % 360.0;
    if hue < 0.0 {
        hue += 360.0;
    }
    let index = (hue / HUE_BIN_WIDTH).floor();
    if index < 0.0 {
        0
    } else {
        (index as usize).min(HUE_BIN_COUNT - 1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct TonePoint {
    pub s: f64,
    pub j: f64,
    pub sigma: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToneCurve {
    pub points: Vec<TonePoint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct TintPoint {
    pub j: f64,
    pub a: f64,
    pub b: f64,
    pub sigma_a: Option<f64>,
    pub sigma_b: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TintCurve {
    pub points: Vec<TintPoint>,
}

/// Tone curve of both functions plus the difference.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToneComparison {
    pub points: Vec<ToneSample>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct ToneSample {
    pub s: f64,
    pub a: f64,
    pub b: f64,
    pub delta: f64,
    /// One-sigma layer uncertainty of `J'`; index 0 is the first function.
    pub sigma: [Option<f64>; 2],
}

/// Neutral tint curve of both functions plus the difference.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TintComparison {
    pub points: Vec<TintSample>,
}

/// Suffixes `_a` and `_b` name the first and second function.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct TintSample {
    pub j_a: f64,
    pub j_b: f64,
    pub a_a: f64,
    pub a_b: f64,
    pub b_a: f64,
    pub b_b: f64,
    pub delta_a: f64,
    pub delta_b: f64,
    pub sigma_a: [Option<f64>; 2],
    pub sigma_b: [Option<f64>; 2],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GrlComponentKind {
    /// `G = exp(log_g)`, the chroma gain relative to colorimetric.
    ChromaGain,
    /// `R`, hue rotation in degrees.
    HueRotation,
    /// `Λ`, lightness shift in `J'`.
    LightnessShift,
}

impl GrlComponentKind {
    pub const ALL: [Self; 3] = [Self::ChromaGain, Self::HueRotation, Self::LightnessShift];

    pub fn name(self) -> &'static str {
        match self {
            Self::ChromaGain => "Chroma gain G",
            Self::HueRotation => "Hue rotation R (deg)",
            Self::LightnessShift => "Lightness shift Λ",
        }
    }
}

/// A layer value with its one-sigma uncertainty, when the function has one.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct GrlValue {
    pub value: f64,
    pub sigma: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GrlSeries {
    pub component: GrlComponentKind,
    pub s: f64,
    pub a: Vec<Option<GrlValue>>,
    pub b: Vec<Option<GrlValue>>,
}

/// Chroma, hue and lightness layers by hue, with small multiples over `s`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GrlPlot {
    pub hue_bins: Vec<HueBin>,
    pub s_values: Vec<f64>,
    pub series: Vec<GrlSeries>,
}

/// Single-function diagnostics: the layer curves a Look page shows.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LookDiagnostics {
    pub tone: ToneCurve,
    pub tint: TintCurve,
    pub grl: Option<GrlPlot>,
}

/// One cell of the ΔE region map; `None` means unsupported or no probe.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RegionCell {
    pub delta_e: Option<f64>,
    /// Combined one-sigma prediction uncertainty in CAM16-UCS units, zero when
    /// unknown.
    pub sigma: f64,
    pub probe: Option<ProbeId>,
}

impl RegionCell {
    fn empty() -> Self {
        Self {
            delta_e: None,
            sigma: 0.0,
            probe: None,
        }
    }
}

/// ΔE over `(hue, s)` at mid chroma; `cells[hue][s]`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RegionMap {
    pub hue_bins: Vec<HueBin>,
    pub s_values: Vec<f64>,
    pub cells: Vec<Vec<RegionCell>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DistributionMetric {
    DeltaEUcs,
    Ciede2000,
}

impl DistributionMetric {
    pub fn label(self) -> &'static str {
        match self {
            Self::DeltaEUcs => "CAM16-UCS ΔE",
            Self::Ciede2000 => "CIEDE2000 ΔE",
        }
    }
}

/// One probe at the tail of a distribution.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WorstProbe {
    pub id: ProbeId,
    pub delta_e: f64,
    /// Combined one-sigma prediction uncertainty in CAM16-UCS units, zero when
    /// unknown; not transformed by the distribution metric.
    pub sigma: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct HistogramBin {
    pub start: f64,
    pub end: f64,
    pub count: usize,
}

/// Summary statistics of a ΔE distribution over supported probes.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DeltaEDistribution {
    pub metric: DistributionMetric,
    pub count: usize,
    pub excluded: usize,
    pub median: f64,
    pub p95: f64,
    pub max: f64,
    pub mean: f64,
    /// Weighted by `1 / (1 + σ²_F₁ + σ²_F₂)`, normalised.
    pub weighted_mean: f64,
    pub weighted_median: f64,
    pub histogram: Vec<HistogramBin>,
    pub worst: Vec<WorstProbe>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Comparison {
    pub a: FunctionName,
    pub b: FunctionName,
    pub summary: String,
    pub tone: ToneComparison,
    pub tint: TintComparison,
    pub grl: Option<GrlPlot>,
    pub delta_e: DeltaEDistribution,
    pub ciede2000: DeltaEDistribution,
    pub region_map: RegionMap,
    pub attribution: Vec<LayerContribution>,
}

pub struct ComparisonInput<'a> {
    pub a: &'a dyn LookFunction,
    pub b: &'a dyn LookFunction,
    pub probes: &'a ProbeSet,
    pub a_name: &'a FunctionName,
    pub b_name: &'a FunctionName,
}

/// Compare two functions on the probe set (docs/02 §7).
pub fn compare(input: &ComparisonInput<'_>) -> Comparison {
    let tone = tone_comparison(input.a, input.b);
    let tint = tint_comparison(input.a, input.b);
    let grl = grl_plot(input.a, input.b, input.probes);
    let delta_e = distribution(
        DistributionMetric::DeltaEUcs,
        input.a,
        input.b,
        input.probes,
    );
    let ciede2000 = distribution(
        DistributionMetric::Ciede2000,
        input.a,
        input.b,
        input.probes,
    );
    let region_map = region_map(input.a, input.b, input.probes);
    let attribution = attribution(input.a, input.b, input.probes);
    let summary = crate::summary::summary_line(
        input.a_name,
        input.b_name,
        &tone,
        &tint,
        grl.as_ref(),
        &delta_e,
        &attribution,
    );
    Comparison {
        a: input.a_name.clone(),
        b: input.b_name.clone(),
        summary,
        tone,
        tint,
        grl,
        delta_e,
        ciede2000,
        region_map,
        attribution,
    }
}

/// Layer curves for a single function, for the Look report page.
pub fn look_diagnostics(function: &dyn LookFunction, probes: &ProbeSet) -> LookDiagnostics {
    LookDiagnostics {
        tone: ToneCurve {
            points: curve_s_values()
                .into_iter()
                .map(|s| tone_point(function, s))
                .collect(),
        },
        tint: TintCurve {
            points: curve_s_values()
                .into_iter()
                .map(|s| tint_point(function, s))
                .collect(),
        },
        grl: single_grl(function, probes),
    }
}

fn curve_s_values() -> Vec<f64> {
    let count = ((CURVE_MAX_S - CURVE_MIN_S) / CURVE_STEP_S).round() as usize + 1;
    (0..count)
        .map(|i| CURVE_MIN_S + i as f64 * CURVE_STEP_S)
        .collect()
}

fn neutral_xyz(s: f64) -> Xyz {
    Illuminant::D65.xyz().scale(0.18 * libm::pow(2.0, s))
}

fn tone_point(function: &dyn LookFunction, s: f64) -> TonePoint {
    let input = LookInput::new(neutral_xyz(s));
    match function.layer_values(&input) {
        Some(values) => TonePoint {
            s,
            j: values.t,
            sigma: function
                .layer_covariance(&input)
                .map(|covariance| LayerCovariance::sigma(covariance.t)),
        },
        None => TonePoint {
            s,
            j: function.predict(input.xyz).j,
            sigma: None,
        },
    }
}

fn tint_point(function: &dyn LookFunction, s: f64) -> TintPoint {
    let input = LookInput::new(neutral_xyz(s));
    let ucs = function.predict(input.xyz);
    match function.layer_values(&input) {
        Some(values) => {
            let covariance = function.layer_covariance(&input);
            TintPoint {
                j: values.t,
                a: values.n_a,
                b: values.n_b,
                sigma_a: covariance.map(|c| LayerCovariance::sigma(c.n_a)),
                sigma_b: covariance.map(|c| LayerCovariance::sigma(c.n_b)),
            }
        }
        None => TintPoint {
            j: ucs.j,
            a: ucs.a,
            b: ucs.b,
            sigma_a: None,
            sigma_b: None,
        },
    }
}

fn tone_comparison(a: &dyn LookFunction, b: &dyn LookFunction) -> ToneComparison {
    ToneComparison {
        points: curve_s_values()
            .into_iter()
            .map(|s| {
                let first = tone_point(a, s);
                let second = tone_point(b, s);
                ToneSample {
                    s,
                    a: first.j,
                    b: second.j,
                    delta: second.j - first.j,
                    sigma: [first.sigma, second.sigma],
                }
            })
            .collect(),
    }
}

fn tint_comparison(a: &dyn LookFunction, b: &dyn LookFunction) -> TintComparison {
    TintComparison {
        points: curve_s_values()
            .into_iter()
            .map(|s| {
                let first = tint_point(a, s);
                let second = tint_point(b, s);
                TintSample {
                    j_a: first.j,
                    j_b: second.j,
                    a_a: first.a,
                    a_b: second.a,
                    b_a: first.b,
                    b_b: second.b,
                    delta_a: second.a - first.a,
                    delta_b: second.b - first.b,
                    sigma_a: [first.sigma_a, second.sigma_a],
                    sigma_b: [first.sigma_b, second.sigma_b],
                }
            })
            .collect(),
    }
}

/// Probes selected per `(s, hue bin)` for the region map and the layer curves.
struct HueGrid {
    s_values: Vec<f64>,
    /// `probes[s_index][hue_bin]` indexes into `ProbeSet::probes`.
    probes: Vec<Vec<Option<usize>>>,
}

/// Selects the mid-chroma HueGrid probe of every `(s, hue bin)` cell.
fn hue_grid(set: &ProbeSet) -> HueGrid {
    let mut buckets: BTreeMap<(i64, usize), Vec<(f64, usize)>> = BTreeMap::new();
    for (index, probe) in set.probes.iter().enumerate() {
        if probe.group != ProbeGroup::HueGrid {
            continue;
        }
        let input = LookInput::new(probe.xyz);
        let s = input.s.round();
        if (input.s - s).abs() > 0.01 {
            continue;
        }
        buckets
            .entry((s as i64, hue_bin_index(input.h_in)))
            .or_default()
            .push((input.c_in, index));
    }

    let mut s_values: Vec<f64> = Vec::new();
    for (s, _) in buckets.keys() {
        let value = *s as f64;
        if !s_values.contains(&value) {
            s_values.push(value);
        }
    }
    s_values.sort_by(f64::total_cmp);

    let mut probes = vec![vec![None; HUE_BIN_COUNT]; s_values.len()];
    for ((s, bin), mut entries) in buckets {
        entries.sort_by(|left, right| left.0.total_cmp(&right.0));
        let middle = (entries.len() - 1) / 2;
        let row = s_values
            .iter()
            .position(|value| *value == s as f64)
            .unwrap_or(0);
        probes[row][bin.min(HUE_BIN_COUNT - 1)] = Some(entries[middle].1);
    }
    HueGrid { s_values, probes }
}

fn has_layers(function: &dyn LookFunction) -> bool {
    let input = LookInput::new(neutral_xyz(0.0));
    function.layer_values(&input).is_some()
}

fn grl_value(
    function: &dyn LookFunction,
    probe: &Probe,
    component: GrlComponentKind,
) -> Option<GrlValue> {
    if function.support(probe) == Support::Unsupported {
        return None;
    }
    let input = LookInput::new(probe.xyz);
    let values = function.layer_values(&input)?;
    let covariance = function.layer_covariance(&input);
    let (value, sigma) = match component {
        GrlComponentKind::ChromaGain => (
            libm::exp(values.log_g),
            covariance.map(|c| libm::exp(values.log_g) * LayerCovariance::sigma(c.log_g)),
        ),
        GrlComponentKind::HueRotation => {
            (values.r, covariance.map(|c| LayerCovariance::sigma(c.r)))
        }
        GrlComponentKind::LightnessShift => (
            values.lambda,
            covariance.map(|c| LayerCovariance::sigma(c.lambda)),
        ),
    };
    Some(GrlValue { value, sigma })
}

fn grl_plot(a: &dyn LookFunction, b: &dyn LookFunction, set: &ProbeSet) -> Option<GrlPlot> {
    grl_curves(a, Some(b), set)
}

fn single_grl(function: &dyn LookFunction, set: &ProbeSet) -> Option<GrlPlot> {
    grl_curves(function, None, set)
}

fn grl_curves(
    first_function: &dyn LookFunction,
    second_function: Option<&dyn LookFunction>,
    set: &ProbeSet,
) -> Option<GrlPlot> {
    if !has_layers(first_function) || second_function.is_some_and(|f| !has_layers(f)) {
        return None;
    }
    let grid = hue_grid(set);
    let s_values: Vec<f64> = GRL_S_VALUES
        .iter()
        .copied()
        .filter(|s| grid.s_values.contains(s))
        .collect();
    if s_values.is_empty() {
        return None;
    }
    let has_second = second_function.is_some();
    let mut series = Vec::with_capacity(s_values.len() * GrlComponentKind::ALL.len());
    for s in &s_values {
        let row = grid.s_values.iter().position(|value| value == s)?;
        for component in GrlComponentKind::ALL {
            let mut first = Vec::with_capacity(HUE_BIN_COUNT);
            let mut second = if has_second {
                Vec::with_capacity(HUE_BIN_COUNT)
            } else {
                Vec::new()
            };
            for bin in 0..HUE_BIN_COUNT {
                let probe = grid.probes[row][bin].map(|index| &set.probes[index]);
                first.push(probe.and_then(|probe| grl_value(first_function, probe, component)));
                if let Some(second_function) = second_function {
                    second
                        .push(probe.and_then(|probe| grl_value(second_function, probe, component)));
                }
            }
            series.push(GrlSeries {
                component,
                s: *s,
                a: first,
                b: second,
            });
        }
    }
    Some(GrlPlot {
        hue_bins: hue_bins(),
        s_values,
        series,
    })
}

fn region_map(a: &dyn LookFunction, b: &dyn LookFunction, set: &ProbeSet) -> RegionMap {
    let grid = hue_grid(set);
    let mut cells = vec![vec![RegionCell::empty(); grid.s_values.len()]; HUE_BIN_COUNT];
    for row in 0..grid.s_values.len() {
        for (bin, cell) in cells.iter_mut().enumerate() {
            let Some(probe) = grid.probes[row][bin].map(|index| &set.probes[index]) else {
                continue;
            };
            if a.support(probe) == Support::Unsupported || b.support(probe) == Support::Unsupported
            {
                continue;
            }
            let trace_a = a.covariance(probe).map_or(0.0, Covariance3::trace);
            let trace_b = b.covariance(probe).map_or(0.0, Covariance3::trace);
            cell[row] = RegionCell {
                delta_e: Some(delta_e_ucs(a.predict(probe.xyz), b.predict(probe.xyz))),
                sigma: (trace_a + trace_b).max(0.0).sqrt(),
                probe: Some(probe.id.clone()),
            };
        }
    }
    RegionMap {
        hue_bins: hue_bins(),
        s_values: grid.s_values,
        cells,
    }
}

fn ucs_to_lab(ucs: Ucs) -> Lab {
    let model = Cam16Model::new(&ViewingConditions::reference());
    let xyz = model.jmh_to_xyz(ucs_to_jmh(ucs, UcsSpace::Ucs));
    xyz_to_lab(xyz, Illuminant::D65.xyz())
}

fn distribution(
    metric: DistributionMetric,
    a: &dyn LookFunction,
    b: &dyn LookFunction,
    set: &ProbeSet,
) -> DeltaEDistribution {
    let mut values = Vec::new();
    let mut weights = Vec::new();
    let mut sigmas = Vec::new();
    let mut worst: Vec<WorstProbe> = Vec::new();
    let mut excluded = 0;
    for probe in &set.probes {
        if a.support(probe) == Support::Unsupported || b.support(probe) == Support::Unsupported {
            excluded += 1;
            continue;
        }
        let first = a.predict(probe.xyz);
        let second = b.predict(probe.xyz);
        let delta = match metric {
            DistributionMetric::DeltaEUcs => delta_e_ucs(first, second),
            DistributionMetric::Ciede2000 => {
                delta_e_ciede2000(ucs_to_lab(first), ucs_to_lab(second))
            }
        };
        let trace_a = a.covariance(probe).map_or(0.0, Covariance3::trace);
        let trace_b = b.covariance(probe).map_or(0.0, Covariance3::trace);
        let weight = 1.0 / (1.0 + trace_a + trace_b);
        let sigma = (trace_a + trace_b).max(0.0).sqrt();
        values.push(delta);
        weights.push(weight);
        sigmas.push(sigma);
        worst.push(WorstProbe {
            id: probe.id.clone(),
            delta_e: delta,
            sigma,
        });
    }

    let stats = Stats::new(&values, &weights);
    worst.sort_by(|left, right| right.delta_e.total_cmp(&left.delta_e));
    worst.truncate(WORST_COUNT);

    DeltaEDistribution {
        metric,
        count: values.len(),
        excluded,
        median: stats.median,
        p95: stats.p95,
        max: stats.max,
        mean: stats.mean,
        weighted_mean: stats.weighted_mean,
        weighted_median: stats.weighted_median,
        histogram: histogram(&values),
        worst,
    }
}

fn histogram(values: &[f64]) -> Vec<HistogramBin> {
    if values.is_empty() {
        return Vec::new();
    }
    let max = values.iter().copied().fold(0.0_f64, f64::max);
    if max <= 0.0 {
        return vec![HistogramBin {
            start: 0.0,
            end: 0.0,
            count: values.len(),
        }];
    }
    let width = max / HISTOGRAM_BINS as f64;
    let mut counts = vec![0_usize; HISTOGRAM_BINS];
    for value in values {
        let index = (value / width).floor() as usize;
        counts[index.min(HISTOGRAM_BINS - 1)] += 1;
    }
    counts
        .into_iter()
        .enumerate()
        .map(|(index, count)| HistogramBin {
            start: index as f64 * width,
            end: (index + 1) as f64 * width,
            count,
        })
        .collect()
}

struct Stats {
    median: f64,
    p95: f64,
    max: f64,
    mean: f64,
    weighted_mean: f64,
    weighted_median: f64,
}

impl Stats {
    fn new(values: &[f64], weights: &[f64]) -> Self {
        if values.is_empty() {
            return Self {
                median: 0.0,
                p95: 0.0,
                max: 0.0,
                mean: 0.0,
                weighted_mean: 0.0,
                weighted_median: 0.0,
            };
        }
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        let mean = values.iter().sum::<f64>() / values.len() as f64;

        let total: f64 = weights.iter().sum();
        let mut pairs: Vec<(f64, f64)> = values
            .iter()
            .copied()
            .zip(weights.iter().copied())
            .collect();
        pairs.sort_by(|left, right| left.0.total_cmp(&right.0));
        let weighted_mean = if total > 0.0 {
            pairs
                .iter()
                .map(|(value, weight)| value * weight)
                .sum::<f64>()
                / total
        } else {
            mean
        };
        let weighted_median = if total > 0.0 {
            let mut cumulative = 0.0;
            let mut median = pairs[pairs.len() - 1].0;
            for (value, weight) in &pairs {
                cumulative += weight / total;
                if cumulative >= 0.5 {
                    median = *value;
                    break;
                }
            }
            median
        } else {
            quantile(&sorted, 0.5)
        };

        Self {
            median: quantile(&sorted, 0.5),
            p95: quantile(&sorted, 0.95),
            max: sorted[sorted.len() - 1],
            mean,
            weighted_mean,
            weighted_median,
        }
    }
}

fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let position = (sorted.len() - 1) as f64 * q;
    let lower = position.floor();
    let upper = (lower + 1.0).min((sorted.len() - 1) as f64);
    let fraction = position - lower;
    let low_index = lower as usize;
    let high_index = upper as usize;
    sorted[low_index] + fraction * (sorted[high_index] - sorted[low_index])
}

fn with_layer(base: &Look, other: &Look, layer: LayerId) -> Look {
    let mut hybrid = base.clone();
    match layer {
        LayerId::Tone => hybrid.tone = other.tone,
        LayerId::Tint => hybrid.tint = other.tint.clone(),
        LayerId::Chroma => hybrid.color.g = other.color.g.clone(),
        LayerId::Hue => hybrid.color.r = other.color.r.clone(),
        LayerId::Lightness => hybrid.color.lambda = other.color.lambda.clone(),
        LayerId::Residual => hybrid.residual = other.residual.clone(),
    }
    hybrid
}

/// Rank layers by the mean ΔE their swap removes (docs/02 §7 item 8).
fn attribution(
    a: &dyn LookFunction,
    b: &dyn LookFunction,
    set: &ProbeSet,
) -> Vec<LayerContribution> {
    let (Some(look_a), Some(look_b)) = (a.look(), b.look()) else {
        return Vec::new();
    };
    let supported: Vec<&Probe> = set
        .probes
        .iter()
        .filter(|probe| {
            a.support(probe) == Support::Supported && b.support(probe) == Support::Supported
        })
        .collect();
    if supported.is_empty() {
        return Vec::new();
    }
    let mut baseline = 0.0;
    for probe in &supported {
        baseline += delta_e_ucs(a.predict(probe.xyz), b.predict(probe.xyz));
    }
    baseline /= supported.len() as f64;

    let mut contributions = Vec::with_capacity(LayerId::ALL.len());
    for layer in LayerId::ALL {
        let hybrid = with_layer(look_a, look_b, layer);
        let mut error = 0.0;
        for probe in &supported {
            error += delta_e_ucs(hybrid.eval(probe.xyz), b.predict(probe.xyz));
        }
        error /= supported.len() as f64;
        contributions.push(LayerContribution {
            layer,
            contribution: (baseline - error).max(0.0),
        });
    }
    contributions.sort_by(|left, right| right.contribution.total_cmp(&left.contribution));
    contributions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hue_bins_cover_the_circle() {
        let bins = hue_bins();
        assert_eq!(bins.len(), HUE_BIN_COUNT);
        assert_eq!(bins[0].start_deg, 0.0);
        assert_eq!(bins[HUE_BIN_COUNT - 1].end_deg, 360.0);
        for bin in &bins {
            assert_eq!(hue_bin_index(bin.start_deg + 0.5), bin.index);
        }
    }

    #[test]
    fn quantile_interpolates() {
        assert_eq!(quantile(&[0.0, 2.0], 0.5), 1.0);
        assert_eq!(quantile(&[0.0, 2.0], 1.0), 2.0);
    }
}
