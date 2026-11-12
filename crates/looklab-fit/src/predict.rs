// SPDX-License-Identifier: AGPL-3.0-or-later
//! Forward prediction and its analytic Jacobian for one sample.

use core::f64::consts::PI;
use looklab_color::ucs::{Ucs, UcsSpace, ucs_to_jmh};
use looklab_color::{Mat3, Rgb, RgbSpace, WhiteRelativeXyz, Xyz};
use looklab_model::basis::{S_COEFS, TINT_COEFS, TONE_COEFS};
use looklab_model::clip::{
    soft_clip_derivative as clip_derivative, soft_clip_k_derivative as clip_k_derivative,
};
use looklab_model::composite::{self, CompositeChart, CompositeDerivative, CompositeRegularity};
use looklab_model::domain_penalty::{
    self, DOMAIN_AXES, DomainViolation, DomainViolationKind, InversePath, PenaltyDerivatives,
};
use looklab_model::realization::{LayerKind, ResponseComponent, feature_coefficient};
use looklab_model::solve::{reference_model, ucs_of_xyz};
use looklab_model::{GrlComponent, LayerValues, Look, LookInput, soft_clip};

use crate::basis;
use crate::cam;
use crate::layout::Layout;

mod curvature;
pub(crate) use curvature::{
    CurvatureError, CurvatureScratch, PredictionCurvature, SampleCurvature,
};

/// How the layout clip coordinate maps to the physical clip `k`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ClipGeometry {
    #[default]
    Logistic,
    Linear,
}

/// Pre-clamp sign of a realized tone beta.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ToneClamp {
    #[default]
    Interior,
    Clamped,
    Boundary,
}

pub(crate) struct ImageCtx {
    pub transform: Mat3,
    /// Prior mean `A0`; `transform = a0·(I + Δ)`.
    pub a0: Mat3,
    pub space: RgbSpace,
    /// `2^exposure`, shared by every sample of the image.
    pub exposure_scale: f64,
    pub wb_r: f64,
    pub wb_b: f64,
    pub tone_clamp: [ToneClamp; TONE_COEFS],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ResponseTerm {
    pub index: usize,
    pub phi: f64,
    pub component: ResponseComponent,
    pub feature: u32,
}

/// Global parameter indices whose columns the current stage wants.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ColumnMap {
    pub image: Option<usize>,
    pub mode: Option<usize>,
    pub wb_r: Option<usize>,
    pub wb_b: Option<usize>,
    pub clip: Option<usize>,
    /// Body-level exposure `ξ`, with the same derivative as `image`.
    pub exposure_extra: Option<usize>,
    /// Body-level WB `ω`, with the same derivative as the per-image nuisances.
    pub wb_r_extra: Option<usize>,
    pub wb_b_extra: Option<usize>,
    /// Body-level clip, with the same derivative as `clip`.
    pub clip_extra: Option<usize>,
    /// Six body input-transform parameters, ordered as [`looklab_body::Delta6`].
    pub delta: Option<[usize; 6]>,
    /// Also return the [`SecondOrder`] derivatives of the prediction along the layer values.
    pub second: bool,
    /// Build the complete analytic second-order operator.
    pub curvature: bool,
    pub clip_geometry: ClipGeometry,
}

/// Layer value a column moves, in the order of [`Look::d_ucs_d_layer_values`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layer {
    Tone,
    TintA,
    TintB,
    LogG,
    Rotation,
    Lambda,
    ResA,
    ResB,
    ResC,
}

impl Layer {
    pub(crate) fn index(self) -> usize {
        match self {
            Layer::Tone => 0,
            Layer::TintA => 1,
            Layer::TintB => 2,
            Layer::LogG => 3,
            Layer::Rotation => 4,
            Layer::Lambda => 5,
            Layer::ResA => 6,
            Layer::ResB => 7,
            Layer::ResC => 8,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Jac {
    pub cols: Vec<u32>,
    pub derivs: Vec<[f64; 3]>,
    /// `∂axis/∂θ` per column, aligned with `cols`.
    pub axes: Vec<[f64; DOMAIN_AXES]>,
    /// Layer value the column moves; `None` when the column mixes layers.
    pub layers: Vec<Option<Layer>>,
    /// `∂(t, n_a, n_b)/∂θ` in the first three slots and `∂layer/∂θ` of `layers` in
    /// the fourth.
    pub layer_weights: Vec<[f64; 4]>,
    /// `∂²t/∂θ²` of a tone column, `∂w_m/∂t` of a tint column, zero otherwise.
    pub layer_slopes: Vec<f64>,
    /// `∂axis_a/∂l_k` over the nine layer values at the current sample.
    pub axis_l: [[f64; DOMAIN_AXES]; 9],
}

impl Jac {
    pub fn clear(&mut self) {
        self.cols.clear();
        self.derivs.clear();
        self.axes.clear();
        self.layers.clear();
        self.layer_weights.clear();
        self.layer_slopes.clear();
    }

    fn push_axis(&mut self, index: usize, derivative: [f64; 3], axis: [f64; DOMAIN_AXES]) {
        self.push_record(index, derivative, axis, None, [0.0; 4], 0.0);
    }

    fn push_record(
        &mut self,
        index: usize,
        derivative: [f64; 3],
        axis: [f64; DOMAIN_AXES],
        layer: Option<Layer>,
        weights: [f64; 4],
        slope: f64,
    ) {
        self.cols.push(index as u32);
        self.derivs.push(derivative);
        self.axes.push(axis);
        self.layers.push(layer);
        self.layer_weights.push(weights);
        self.layer_slopes.push(slope);
    }

    /// `Σ_l ∂axis/∂l_k · ∂l_k/∂θ` for one column's layer-coordinate derivative.
    fn axis_from_delta(&self, delta: [f64; 9]) -> [f64; DOMAIN_AXES] {
        let mut axis = [0.0_f64; DOMAIN_AXES];
        for (layer, value) in delta.iter().enumerate() {
            if *value != 0.0 {
                for (slot, total) in axis.iter_mut().enumerate() {
                    *total += self.axis_l[layer][slot] * value;
                }
            }
        }
        axis
    }

    fn push_layer(
        &mut self,
        index: usize,
        derivative: [f64; 3],
        layer: Layer,
        weights: [f64; 4],
        slope: f64,
    ) {
        self.push_layer_slope(index, derivative, Some(layer), weights, slope);
    }

    fn push_layer_slope(
        &mut self,
        index: usize,
        derivative: [f64; 3],
        layer: Option<Layer>,
        weights: [f64; 4],
        slope: f64,
    ) {
        let mut delta = [0.0_f64; 9];
        delta[0] = weights[0];
        delta[1] = weights[1];
        delta[2] = weights[2];
        if let Some(layer) = layer {
            let slot = layer.index();
            if slot >= 3 {
                delta[slot] += weights[3];
            }
        }
        let axis = self.axis_from_delta(delta);
        self.cols.push(index as u32);
        self.derivs.push(derivative);
        self.axes.push(axis);
        self.layers.push(layer);
        self.layer_weights.push(weights);
        self.layer_slopes.push(slope);
    }

    fn apply(&mut self, matrix: &Mat3) {
        for derivative in &mut self.derivs {
            *derivative = matrix.mul_vec(*derivative);
        }
    }
}

pub(crate) struct EvalOutput {
    pub prediction: Ucs,
    pub residual: [f64; 3],
    pub residual_weights: [f64; 64],
    pub residual_indices: [usize; 64],
    pub clip_jacobian: Mat3,
    /// Domain penalty derivatives; absent when the sample left the inverse domain.
    pub penalty: Option<PenaltyDerivatives>,
    /// First hard-feasibility failure, for rejection and diagnostics.
    pub violation: Option<DomainViolation>,
    /// `∂axis/∂θ` per column, aligned with `jac.cols`.
    pub axis_terms: Vec<[f64; DOMAIN_AXES]>,
    /// The outer post-clip response is neutral while the output clip is active.
    pub crossing: bool,
    /// Layer-value second derivatives, present when [`ColumnMap::second`] is set.
    pub second: Option<Box<SecondOrder>>,
    /// Complete analytic second-order operator, present when [`ColumnMap::curvature`] is set.
    pub curvature: Option<Box<PredictionCurvature>>,
}

impl EvalOutput {
    fn infeasible(violation: DomainViolation) -> Self {
        Self {
            prediction: Ucs::new(0.0, 0.0, 0.0),
            residual: [0.0; 3],
            residual_weights: [0.0; 64],
            residual_indices: [0; 64],
            clip_jacobian: Mat3::IDENTITY,
            penalty: None,
            violation: Some(violation),
            axis_terms: Vec::new(),
            crossing: false,
            second: None,
            curvature: None,
        }
    }
}

/// An observation in both fitting coordinate systems.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Target {
    pub ucs: Ucs,
    pub internal: [f64; 2],
}

impl Target {
    pub fn new(ucs: Ucs) -> Self {
        Self {
            ucs,
            internal: looklab_model::look::internal_chroma_from_ucs([ucs.a, ucs.b], ucs.j),
        }
    }
}

enum ForwardPoint {
    Absolute {
        value: Ucs,
    },
    WhiteRelative {
        departure: WhiteRelativeXyz,
        value: Ucs,
    },
}

/// One chroma path through the output clip.
struct PathState {
    xyz_post: Xyz,
    forward: ForwardPoint,
    /// Pre-clip linear RGB channels.
    channels: [f64; 3],
    /// `d(pre-clip XYZ)/d(pre-clip appearance)`.
    preclip: Mat3,
    /// `d(post-clip XYZ)/d(pre-clip XYZ)`.
    clip: Mat3,
    /// `d(output)/d(post-clip XYZ)`.
    rows: Mat3,
    /// `d(post-clip residual coordinates)/d(pre-clip appearance)`.
    post: Mat3,
    /// `d(post-clip residual coordinates)/dk`.
    dk: [f64; 3],
    /// At least one channel is on a nonlinear clip branch.
    clip_active: bool,
    /// `Some` only for the outer path from [`composite::evaluate`].
    regularity: Option<CompositeRegularity>,
}

impl PathState {
    fn value(&self) -> Ucs {
        match &self.forward {
            ForwardPoint::Absolute { value } | ForwardPoint::WhiteRelative { value, .. } => *value,
        }
    }

    fn forward_second(&self) -> [[f64; 3]; 6] {
        match &self.forward {
            ForwardPoint::Absolute { .. } => looklab_model::look::ucs_xyz_second(self.xyz_post),
            ForwardPoint::WhiteRelative { departure, .. } => {
                looklab_model::look::ucs_xyz_second_departure(*departure)
            }
        }
    }
}

fn path_state(
    space: RgbSpace,
    xyz_pre: Xyz,
    clip_k: f64,
    preclip: [[f64; 3]; 3],
    rows: impl Fn(Xyz) -> [[f64; 3]; 3],
) -> PathState {
    let channels = xyz_channels(space, xyz_pre);
    let clipped = channels.map(|value| soft_clip(value, clip_k));
    let xyz_post = space.rgb_linear_to_xyz(Rgb::new(clipped[0], clipped[1], clipped[2]));
    let unclipped = channels
        .iter()
        .all(|value| *value >= clip_k && *value <= 1.0 - clip_k);
    let (clip, dk) = if unclipped {
        (Mat3::IDENTITY, [0.0; 3])
    } else {
        let per_channel = Mat3::diag(channels.map(|value| clip_derivative(value, clip_k)));
        let matrix = space
            .to_xyz_matrix()
            .mul(&per_channel)
            .mul(&space.from_xyz_matrix());
        let dk = space
            .to_xyz_matrix()
            .mul_vec(channels.map(|value| clip_k_derivative(value, clip_k)));
        (matrix, dk)
    };
    let rows = Mat3(rows(xyz_post));
    PathState {
        xyz_post,
        forward: ForwardPoint::Absolute {
            value: ucs_of_xyz(xyz_post),
        },
        channels,
        preclip: Mat3(preclip),
        clip,
        rows,
        post: rows.mul(&clip).mul(&Mat3(preclip)),
        dk: rows.mul_vec(dk),
        clip_active: !unclipped,
        regularity: None,
    }
}

fn xyz_channels(space: RgbSpace, xyz: Xyz) -> [f64; 3] {
    let rgb = space.xyz_to_rgb_linear(xyz);
    [rgb.r, rgb.g, rgb.b]
}

/// Outer UCS path through the published inverse, the output clip and the forward model.
fn outer_path_state(space: RgbSpace, pre: Ucs, clip_k: f64) -> Result<PathState, DomainViolation> {
    let point = composite::evaluate(pre, space, clip_k)?;
    let CompositeDerivative::Finite {
        jacobian,
        jacobian_k,
        regularity,
    } = point.derivative
    else {
        return Err(DomainViolation {
            kind: DomainViolationKind::NonFinite,
            path: Some(InversePath::Ucs),
            channel: None,
            minimum_margin: point.minimum_margin,
        });
    };
    let preclip = point.preclip_jacobian;
    let channels = point.channels;
    let unclipped = channels
        .iter()
        .all(|value| *value >= clip_k && *value <= 1.0 - clip_k);
    let clip = if unclipped {
        Mat3::IDENTITY
    } else {
        space
            .to_xyz_matrix()
            .mul(&Mat3::diag(point.clip_slope))
            .mul(&space.from_xyz_matrix())
    };
    let rows = Mat3(point.forward_jacobian);
    let clip_active = match regularity {
        CompositeRegularity::Smooth => !unclipped,
        CompositeRegularity::NeutralCrossing { clip_active, .. } => clip_active,
        CompositeRegularity::NeutralInverse => true,
    };
    let post = if unclipped {
        Mat3::IDENTITY
    } else {
        Mat3(jacobian)
    };
    let dk = if unclipped { [0.0; 3] } else { jacobian_k };
    Ok(PathState {
        xyz_post: point.xyz_post,
        forward: match point.chart {
            CompositeChart::Absolute => ForwardPoint::Absolute { value: point.value },
            CompositeChart::WhiteRelative => ForwardPoint::WhiteRelative {
                departure: point.xyz_departure,
                value: point.value,
            },
        },
        channels,
        preclip: Mat3(preclip),
        clip,
        rows,
        post,
        dk,
        clip_active,
        regularity: Some(regularity),
    })
}

/// Second derivatives of the prediction over the nine layer values, or zero at a
/// clipped outer-neutral crossing where the classical second derivative is undefined.
pub(crate) struct SecondOrder {
    /// `∂²p_c/∂l_k∂l_l` of the blended post-clip output, indexed `k * 9 + l`.
    pub prediction: [[f64; 81]; 3],
    /// `∂p/∂l_k` of the blended post-clip output.
    pub gradient: [[f64; 3]; 9],
    /// `Σ_m w''_m a_m` and `Σ_m w''_m b_m` of the tint weights at `t`.
    pub tint_curvature: [f64; 2],
    /// `Σ_m w'_m a_m` and `Σ_m w'_m b_m`.
    pub tint_slope: [f64; 2],
}

const PACKED: [(usize, usize); 6] = [(0, 0), (1, 1), (2, 2), (0, 1), (0, 2), (1, 2)];

/// `d² soft_clip(x, k)/dx²`.
fn clip_second_derivative(x: f64, k: f64) -> f64 {
    if x > 1.0 - k {
        let t = libm::tanh((x - (1.0 - k)) / k);
        -2.0 * t * (1.0 - t * t) / k
    } else if x < k {
        let t = libm::tanh((k - x) / k);
        2.0 * t * (1.0 - t * t) / k
    } else {
        0.0
    }
}

/// Bilinear form of a packed second derivative, `Σ_{a,b} u_a v_b H_ab`.
fn second_form(packed: &[[f64; 3]; 6], left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    let mut out = [0.0_f64; 3];
    for (slot, (a, b)) in PACKED.into_iter().enumerate() {
        let weight = if a == b {
            left[a] * right[b]
        } else {
            left[a] * right[b] + left[b] * right[a]
        };
        for (channel, value) in out.iter_mut().enumerate() {
            *value += weight * packed[slot][channel];
        }
    }
    out
}

/// Pre-clip `XYZ` first and second derivatives over the nine layer values.
fn preclip_chain(
    preclip: &Mat3,
    preclip_second: &[[[f64; 3]; 3]; 3],
    appearance_first: &[[f64; 9]; 3],
    appearance_second: &[[f64; 3]; 81],
) -> ([[f64; 3]; 9], [[f64; 3]; 81]) {
    let column = |index: usize| {
        [
            appearance_first[0][index],
            appearance_first[1][index],
            appearance_first[2][index],
        ]
    };
    let mut first = [[0.0_f64; 3]; 9];
    for (index, value) in first.iter_mut().enumerate() {
        *value = preclip.mul_vec(column(index));
    }
    let mut second = [[0.0_f64; 3]; 81];
    for k in 0..9 {
        for l in 0..9 {
            let index = k * 9 + l;
            let mut value = preclip.mul_vec(appearance_second[index]);
            for c in 0..3 {
                for d in 0..3 {
                    let weight = appearance_first[c][k] * appearance_first[d][l];
                    if weight != 0.0 {
                        for (row, entry) in value.iter_mut().enumerate() {
                            *entry += preclip_second[c][d][row] * weight;
                        }
                    }
                }
            }
            second[index] = value;
        }
    }
    (first, second)
}

/// Post-clip output second derivatives from the pre-clip `XYZ` chain.
fn output_chain(
    space: RgbSpace,
    path: &PathState,
    clip_k: f64,
    first: &[[f64; 3]; 9],
    second: &[[f64; 3]; 81],
    output_second: &[[f64; 3]; 6],
) -> [[f64; 3]; 81] {
    let from_xyz = space.from_xyz_matrix();
    let to_xyz = space.to_xyz_matrix();
    let mut rgb_first = [[0.0_f64; 3]; 9];
    for (index, value) in first.iter().enumerate() {
        rgb_first[index] = from_xyz.mul_vec(*value);
    }
    let curvature = path
        .channels
        .map(|value| clip_second_derivative(value, clip_k));
    let mut post_first = [[0.0_f64; 3]; 9];
    for (index, value) in first.iter().enumerate() {
        post_first[index] = path.clip.mul_vec(*value);
    }
    let mut out = [[0.0_f64; 3]; 81];
    for k in 0..9 {
        for l in 0..9 {
            let index = k * 9 + l;
            let mut value = path.clip.mul_vec(second[index]);
            for channel in 0..3 {
                let weight = curvature[channel] * rgb_first[k][channel] * rgb_first[l][channel];
                for (row, entry) in value.iter_mut().enumerate() {
                    *entry += to_xyz.0[row][channel] * weight;
                }
            }
            let mapped = path.rows.mul_vec(value);
            let extra = second_form(output_second, post_first[k], post_first[l]);
            out[index] = [
                mapped[0] + extra[0],
                mapped[1] + extra[1],
                mapped[2] + extra[2],
            ];
        }
    }
    out
}

fn row_mat(v: [f64; 3], m: &Mat3) -> [f64; 3] {
    [
        v[0] * m.0[0][0] + v[1] * m.0[1][0] + v[2] * m.0[2][0],
        v[0] * m.0[0][1] + v[1] * m.0[1][1] + v[2] * m.0[2][1],
        v[0] * m.0[0][2] + v[1] * m.0[1][2] + v[2] * m.0[2][2],
    ]
}

fn add(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale(a: [f64; 3], k: f64) -> [f64; 3] {
    [a[0] * k, a[1] * k, a[2] * k]
}

fn dot3(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn scale_mat(m: &Mat3, k: f64) -> Mat3 {
    let mut out = *m;
    for row in &mut out.0 {
        for value in row {
            *value *= k;
        }
    }
    out
}

fn appearance_slope(
    d_layer: &[[f64; 9]; 3],
    na_slope: f64,
    nb_slope: f64,
    grl: [GrlTerms; 3],
    dres: [[f64; 3]; 3],
    dt_ds: f64,
) -> [f64; 3] {
    let d_t = add(
        add(column(d_layer, 0), scale(column(d_layer, 1), na_slope)),
        scale(column(d_layer, 2), nb_slope),
    );
    let mut out = scale(d_t, dt_ds);
    out = add(out, scale(column(d_layer, 3), grl[0].s));
    out = add(out, scale(column(d_layer, 4), grl[1].s));
    out = add(out, scale(column(d_layer, 5), grl[2].s));
    for channel in 0..3 {
        out = add(out, scale(column(d_layer, 6 + channel), dres[0][channel]));
    }
    out
}

fn mat_add(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut out = Mat3([[0.0; 3]; 3]);
    for row in 0..3 {
        for column in 0..3 {
            out.0[row][column] = a.0[row][column] + b.0[row][column];
        }
    }
    out
}

fn column(d_layer: &[[f64; 9]; 3], index: usize) -> [f64; 3] {
    [d_layer[0][index], d_layer[1][index], d_layer[2][index]]
}

fn tone_slope(look: &Look, input: &LookInput) -> f64 {
    match look.tone.betas() {
        Some(beta) => {
            let derivative = basis::tone_basis_derivative(input.s_lookup);
            let slope: f64 = (0..TONE_COEFS)
                .map(|k| beta[k] * derivative[k])
                .sum::<f64>();
            slope * looklab_model::basis::exposure_lookup_derivative(input.s)
        }
        None => {
            let h = 1e-6;
            (looklab_model::neutral_j_prime(0.18 * libm::pow(2.0, input.s + h))
                - looklab_model::neutral_j_prime(0.18 * libm::pow(2.0, input.s - h)))
                / (2.0 * h)
        }
    }
}

/// Partial derivatives of one chroma/hue/lightness component: physical `s`,
/// UCS `a`/`b`, and normalized opponent `n_a`/`n_b`.
#[derive(Clone, Copy, Debug, Default)]
struct GrlTerms {
    s: f64,
    a: f64,
    b: f64,
    n_a: f64,
    n_b: f64,
}

fn grl_terms(
    component: &GrlComponent,
    input: &LookInput,
    s_derivative: &[f64; 4],
    hue_derivative: &[f64; 4],
    c_derivative: &[f64; 4],
) -> GrlTerms {
    let weights = &input.grl;
    let mut plane = 0.0;
    let mut plane_s = 0.0;
    let mut ds = weights.c_w[0]
        * weights
            .s_idx
            .iter()
            .zip(s_derivative.iter())
            .map(|(index, weight)| weight * component.plane[*index])
            .sum::<f64>();
    let mut dc = weights
        .s_idx
        .iter()
        .zip(weights.s_w.iter())
        .map(|(index, weight)| c_derivative[0] * weight * component.plane[*index])
        .sum::<f64>();
    for (position, index) in weights.s_idx.iter().enumerate() {
        plane += weights.s_w[position] * component.plane[*index];
        plane_s += s_derivative[position] * component.plane[*index];
    }
    let mut volume = 0.0;
    let mut dh = 0.0;
    for h in 0..4 {
        for s in 0..4 {
            let base = (weights.hue_idx[h] * S_COEFS + weights.s_idx[s]) * 3;
            let spatial = weights.hue_w[h] * weights.s_w[s];
            for c in 1..4 {
                let node = base + (c - 1);
                let value = component.volume[node];
                volume += spatial * weights.c_w[c] * value;
                ds += weights.hue_w[h] * s_derivative[s] * weights.c_w[c] * value;
                dh += hue_derivative[h] * weights.s_w[s] * weights.c_w[c] * value;
                dc += weights.hue_w[h] * weights.s_w[s] * c_derivative[c] * value;
            }
        }
    }
    let outer = weights.c_w[0] * plane + volume;

    let basis = looklab_model::basis::core_weights(input.neutral.a, input.neutral.b);
    let inv_join = 1.0 / looklab_model::basis::NEUTRAL_JOIN_C;
    let x = input.neutral.a * inv_join;
    let y = input.neutral.b * inv_join;
    let basis_a = [inv_join, 0.0, 2.0 * x * inv_join, y * inv_join, 0.0];
    let basis_b = [0.0, inv_join, 0.0, x * inv_join, 2.0 * y * inv_join];
    let mut core = 0.0;
    let mut core_s = 0.0;
    let mut core_a = 0.0;
    let mut core_b = 0.0;
    for (position, index) in weights.s_idx.iter().enumerate() {
        let base = *index * looklab_model::basis::CORE_TERMS;
        let spatial = weights.s_w[position];
        for term in 0..looklab_model::basis::CORE_TERMS {
            let value = component.core[base + term];
            core += spatial * basis[term] * value;
            core_s += s_derivative[position] * basis[term] * value;
            core_a += spatial * basis_a[term] * value;
            core_b += spatial * basis_b[term] * value;
        }
    }

    let w = input.blend.w;
    let near = input.blend.core();
    let c = input.c_in;
    let (outer_a, outer_b) = if c > 0.0 {
        let inv_c = 1.0 / c;
        let inv_c2 = inv_c * inv_c;
        let degree = 180.0 / core::f64::consts::PI;
        (
            dc * (input.a_in * inv_c) - dh * (input.b_in * inv_c2) * degree,
            dc * (input.b_in * inv_c) + dh * (input.a_in * inv_c2) * degree,
        )
    } else {
        (0.0, 0.0)
    };
    let magnitude = input.neutral.magnitude();
    let (w_a, w_b) = if magnitude > 0.0 {
        (
            input.blend.dw_dn * input.neutral.a / magnitude,
            input.blend.dw_dn * input.neutral.b / magnitude,
        )
    } else {
        (0.0, 0.0)
    };
    let base = outer - plane - core;
    GrlTerms {
        s: w * ds + near * (plane_s + core_s),
        a: w * outer_a,
        b: w * outer_b,
        n_a: w_a * base + near * core_a,
        n_b: w_b * base + near * core_b,
    }
}

fn residual_terms(
    look: &Look,
    input: &LookInput,
    du: &[f64; 64],
    dv: &[f64; 64],
    dw: &[f64; 64],
) -> [[f64; 3]; 3] {
    let mut out = [[0.0_f64; 3]; 3];
    for (m, node) in input.residual.indices.iter().enumerate() {
        for channel in 0..3 {
            let value = look.residual.values[node * 3 + channel];
            out[0][channel] += du[m] * value;
            out[1][channel] += dv[m] * value;
            out[2][channel] += dw[m] * value;
        }
    }
    out
}

fn push_grl(jac: &mut Jac, start: usize, layer: Layer, input: &LookInput, d: [f64; 3]) {
    let weights = &input.grl;
    let outer = input.blend.w;
    let near = input.blend.core();
    let plane_weight = outer * weights.c_w[0] + near;
    for s in 0..4 {
        let weight = plane_weight * weights.s_w[s];
        jac.push_layer(
            start + weights.s_idx[s],
            scale(d, weight),
            layer,
            [0.0, 0.0, 0.0, weight],
            0.0,
        );
    }
    for h in 0..4 {
        for s in 0..4 {
            let base = (weights.hue_idx[h] * S_COEFS + weights.s_idx[s]) * 3;
            for c in 1..4 {
                let weight = outer * weights.hue_w[h] * weights.s_w[s] * weights.c_w[c];
                jac.push_layer(
                    start + S_COEFS + base + c - 1,
                    scale(d, weight),
                    layer,
                    [0.0, 0.0, 0.0, weight],
                    0.0,
                );
            }
        }
    }
    let basis = looklab_model::basis::core_weights(input.neutral.a, input.neutral.b);
    for s in 0..4 {
        for (term, basis_weight) in basis.iter().enumerate() {
            let weight = near * weights.s_w[s] * basis_weight;
            jac.push_layer(
                start
                    + S_COEFS
                    + looklab_model::basis::VOLUME_COEFS
                    + weights.s_idx[s] * looklab_model::basis::CORE_TERMS
                    + term,
                scale(d, weight),
                layer,
                [0.0, 0.0, 0.0, weight],
                0.0,
            );
        }
    }
}

fn push_look_columns(
    jac: &mut Jac,
    look: &Look,
    layout: &Layout,
    input: &LookInput,
    values: &LayerValues,
    d_layer: &[[f64; 9]; 3],
    beta_slope: Option<&[f64; TONE_COEFS]>,
) {
    let (tint_indices, tint_weights, tint_derivative) = basis::tint_values(values.t);
    let d_na = column(d_layer, 1);
    let d_nb = column(d_layer, 2);
    let (na_slope, nb_slope) = look.d_tint_d_t(values.t);
    let d_t = add(
        add(column(d_layer, 0), scale(d_na, na_slope)),
        scale(d_nb, nb_slope),
    );
    let tone_weights = |weight: f64| [weight, na_slope * weight, nb_slope * weight, 0.0];
    jac.push_layer(layout.tone_c0, d_t, Layer::Tone, tone_weights(1.0), 0.0);
    for k in 0..TONE_COEFS {
        let (weight, curvature) = match beta_slope {
            Some(slope) => {
                let slope = slope[k];
                (
                    input.tone_weights[k + 1] * slope,
                    input.tone_weights[k + 1] * slope * (1.0 - slope),
                )
            }
            None => (input.tone_weights[k + 1], 0.0),
        };
        jac.push_layer(
            layout.tone_beta.start + k,
            scale(d_t, weight),
            Layer::Tone,
            tone_weights(weight),
            curvature,
        );
    }
    for m in 0..4 {
        jac.push_layer(
            layout.tint_a.start + tint_indices[m],
            scale(d_na, tint_weights[m]),
            Layer::TintA,
            [0.0, tint_weights[m], 0.0, 0.0],
            tint_derivative[m],
        );
        jac.push_layer(
            layout.tint_b.start + tint_indices[m],
            scale(d_nb, tint_weights[m]),
            Layer::TintB,
            [0.0, 0.0, tint_weights[m], 0.0],
            tint_derivative[m],
        );
    }
    push_grl(jac, layout.g.start, Layer::LogG, input, column(d_layer, 3));
    push_grl(
        jac,
        layout.r.start,
        Layer::Rotation,
        input,
        column(d_layer, 4),
    );
    push_grl(
        jac,
        layout.lambda.start,
        Layer::Lambda,
        input,
        column(d_layer, 5),
    );
    for (m, node) in input.residual.indices.iter().enumerate() {
        let weight = input.residual.weights[m];
        for channel in 0..3 {
            jac.push_layer(
                layout.residual.start + node * 3 + channel,
                [
                    d_layer[0][6 + channel] * weight,
                    d_layer[1][6 + channel] * weight,
                    d_layer[2][6 + channel] * weight,
                ],
                [Layer::ResA, Layer::ResB, Layer::ResC][channel],
                [0.0, 0.0, 0.0, weight],
                0.0,
            );
        }
    }
}

/// Fills `jac` with `d residual / dθ`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_sample(
    look: &Look,
    layout: &Layout,
    image: &ImageCtx,
    columns: &ColumnMap,
    rgb: [f64; 3],
    clip_k: f64,
    clip_slope: f64,
    beta_slope: &[f64; TONE_COEFS],
    terms: &[ResponseTerm],
    target: &Target,
    jac: &mut Jac,
) -> EvalOutput {
    jac.clear();
    let exposure_scale = image.exposure_scale;
    let cam = [
        rgb[0] * image.wb_r * exposure_scale,
        rgb[1] * exposure_scale,
        rgb[2] * image.wb_b * exposure_scale,
    ];
    let xyz = Xyz::from_array(image.transform.mul_vec(cam));
    let input = LookInput::new(xyz);
    let values = look.layer_values(&input);
    if let Err(violation) = domain_penalty::check_reference_tone(&values) {
        return EvalOutput::infeasible(violation);
    }
    let paths = look.chroma_paths(&input, &values);
    let penalty = match domain_penalty::evaluate(&values, &paths) {
        Ok(penalty) => penalty,
        Err(violation) => return EvalOutput::infeasible(violation),
    };
    let w = paths.blend;
    let near = 1.0 - w;
    let space = image.space;
    let mut active_beta = *beta_slope;
    for (k, slope) in active_beta.iter_mut().enumerate() {
        if image.tone_clamp[k] != ToneClamp::Interior {
            *slope = 0.0;
        }
    }

    let lookup_slope = looklab_model::basis::exposure_lookup_derivative(input.s);
    let s_derivative = basis::s_values(input.s_lookup)
        .2
        .map(|value| value * lookup_slope);
    let (_, _, hue_derivative) = basis::hue_values(input.h_in);
    let (_, _, c_derivative) = basis::c_values(input.c_in);
    let dt_ds = tone_slope(look, &input);
    let g = grl_terms(
        &look.color.g,
        &input,
        &s_derivative,
        &hue_derivative,
        &c_derivative,
    );
    let r = grl_terms(
        &look.color.r,
        &input,
        &s_derivative,
        &hue_derivative,
        &c_derivative,
    );
    let lambda = grl_terms(
        &look.color.lambda,
        &input,
        &s_derivative,
        &hue_derivative,
        &c_derivative,
    );
    let (residual_indices, residual_weights, [du, dv, dw]) =
        looklab_model::basis::residual_values(input.s_lookup, input.residual_a, input.residual_b);
    let du = du.map(|value| value * lookup_slope);
    let dres = residual_terms(look, &input, &du, &dv, &dw);
    let (na_slope, nb_slope) = look.d_tint_d_t(values.t);

    let d_layer_ucs = look.d_ucs_d_layer_values(&input, &values);
    let d_internal = look.d_internal_d_layer_values(&input, &values);
    let mut d_layer_int = [[0.0_f64; 9]; 3];
    d_layer_int[0] = d_layer_ucs[0];
    for (column, derivative) in d_internal.iter().enumerate() {
        d_layer_int[1][column] = derivative[0];
        d_layer_int[2][column] = derivative[1];
    }

    let ucs_path = if w > 0.0 {
        match outer_path_state(space, paths.ucs, clip_k) {
            Ok(path) => Some(path),
            Err(violation) => return EvalOutput::infeasible(violation),
        }
    } else {
        None
    };
    let int_path = (near > 0.0).then(|| {
        let (xyz_pre, preclip) =
            looklab_model::clip::preclip_internal_xyz_jacobian(paths.internal, paths.ucs.j);
        path_state(space, xyz_pre, clip_k, preclip, |post| {
            let (_, cam_jacobian) = cam::ucs_jacobian(post);
            let (_, opponent) = looklab_model::look::neutral_chroma_jacobian(post);
            [cam_jacobian[0], opponent[0], opponent[1]]
        })
    });

    let ucs_post = ucs_path.as_ref().map(PathState::value);
    let internal_post = int_path.as_ref().map(|path| {
        let chroma = looklab_model::look::neutral_chroma_jacobian(path.xyz_post).0;
        ([chroma.a, chroma.b], path.value())
    });
    let crossing = w > 0.0
        && ucs_path.as_ref().is_some_and(|path| {
            matches!(
                path.regularity,
                Some(CompositeRegularity::NeutralCrossing {
                    clip_active: true,
                    ..
                })
            )
        });
    let outer_identity = ucs_path.as_ref().is_some_and(|path| !path.clip_active);
    let inner_identity = int_path.as_ref().is_some_and(|path| {
        paths.internal[0] == 0.0 && paths.internal[1] == 0.0 && !path.clip_active
    });
    let r_ucs = ucs_post.map(|post| {
        [
            post.j - target.ucs.j,
            post.a - target.ucs.a,
            post.b - target.ucs.b,
        ]
    });
    let r_int = internal_post.map(|(chroma, post)| {
        [
            post.j - target.ucs.j,
            chroma[0] - target.internal[0],
            chroma[1] - target.internal[1],
        ]
    });
    let residual = match (r_ucs, r_int) {
        (Some(_outer), Some(inner)) if w == 0.0 => inner,
        (Some(outer), Some(inner)) => add(scale(outer, w), scale(inner, near)),
        (Some(outer), None) => outer,
        (None, Some(inner)) => inner,
        (None, None) => [0.0; 3],
    };
    let delta_residual = match (r_ucs, r_int) {
        (Some(outer), Some(inner)) => add(outer, scale(inner, -1.0)),
        _ => [0.0; 3],
    };
    let prediction = match (ucs_post, internal_post) {
        (Some(_outer), Some((_, inner))) if w == 0.0 => inner,
        (Some(outer), Some((_, inner))) => Ucs::new(
            w * outer.j + near * inner.j,
            w * outer.a + near * inner.a,
            w * outer.b + near * inner.b,
        ),
        (Some(outer), None) => outer,
        (None, Some((_, inner))) => inner,
        (None, None) => Ucs::new(0.0, 0.0, 0.0),
    };

    // At neutral the reference chroma is zero, so its derivatives are zero, but the
    // dual hue chain returns NaN there.
    let mut d_layer_ucs_safe = d_layer_ucs;
    if w == 0.0 {
        d_layer_ucs_safe[1][0] = 0.0;
        d_layer_ucs_safe[2][0] = 0.0;
    }
    let mut axis_l = [[0.0_f64; DOMAIN_AXES]; 9];
    for layer in 0..9 {
        axis_l[layer][0] = if layer == 0 { 1.0 } else { 0.0 };
        axis_l[layer][1] = d_layer_ucs[0][layer];
        axis_l[layer][2] = d_layer_ucs[1][layer];
        axis_l[layer][3] = d_layer_ucs[2][layer];
        axis_l[layer][4] = d_layer_int[1][layer];
        axis_l[layer][5] = d_layer_int[2][layer];
    }
    if w == 0.0 {
        for entry in &mut axis_l {
            entry[2] = 0.0;
            entry[3] = 0.0;
        }
    }
    if near == 0.0 {
        for entry in &mut axis_l {
            entry[4] = 0.0;
            entry[5] = 0.0;
        }
    }
    let second = (columns.second && !columns.curvature).then(|| {
        if crossing {
            // A clipped outer-neutral crossing has no classical residual Hessian.
            return Box::new(SecondOrder {
                prediction: [[0.0_f64; 81]; 3],
                gradient: [[0.0_f64; 3]; 9],
                tint_curvature: [0.0; 2],
                tint_slope: [0.0; 2],
            });
        }
        let (ucs_second, internal_second) = look.path_second(&input, &values);
        let mut int_second = [[0.0_f64; 3]; 81];
        for (index, value) in int_second.iter_mut().enumerate() {
            *value = [
                ucs_second[index][0],
                internal_second[index][0],
                internal_second[index][1],
            ];
        }
        let outer = ucs_path.as_ref().map(|path| {
            let preclip = looklab_model::clip::preclip_xyz_second(paths.ucs);
            let (first, chain) = preclip_chain(&path.preclip, &preclip, &d_layer_ucs, &ucs_second);
            let post = if outer_identity {
                ucs_second
            } else {
                let output = path.forward_second();
                output_chain(space, path, clip_k, &first, &chain, &output)
            };
            (first, chain, post)
        });
        let inner = int_path.as_ref().map(|path| {
            let preclip =
                looklab_model::clip::preclip_internal_xyz_second(paths.internal, paths.ucs.j);
            let (first, chain) = preclip_chain(&path.preclip, &preclip, &d_layer_int, &int_second);
            let post = if inner_identity {
                int_second
            } else {
                let ucs = path.forward_second();
                let neutral = looklab_model::look::neutral_chroma_second(path.xyz_post);
                let mut output = [[0.0_f64; 3]; 6];
                for (slot, value) in output.iter_mut().enumerate() {
                    *value = [ucs[slot][0], neutral[slot][0], neutral[slot][1]];
                }
                output_chain(space, path, clip_k, &first, &chain, &output)
            };
            (first, chain, post)
        });
        let mut prediction = [[0.0_f64; 81]; 3];
        let mut gradient = [[0.0_f64; 3]; 9];
        if let (Some(path), Some((_, _, post))) = (&ucs_path, &outer) {
            for (index, value) in post.iter().enumerate() {
                for channel in 0..3 {
                    prediction[channel][index] += w * value[channel];
                }
            }
            for (k, value) in gradient.iter_mut().enumerate() {
                let column = [d_layer_ucs[0][k], d_layer_ucs[1][k], d_layer_ucs[2][k]];
                let contribution = if outer_identity {
                    column
                } else {
                    path.post.mul_vec(column)
                };
                *value = scale(contribution, w);
            }
        }
        if let (Some(path), Some((_, _, post))) = (&int_path, &inner) {
            for (index, value) in post.iter().enumerate() {
                for channel in 0..3 {
                    prediction[channel][index] += near * value[channel];
                }
            }
            for (k, value) in gradient.iter_mut().enumerate() {
                let column = [d_layer_int[0][k], d_layer_int[1][k], d_layer_int[2][k]];
                let contribution = if inner_identity {
                    column
                } else {
                    path.post.mul_vec(column)
                };
                *value = add(*value, scale(contribution, near));
            }
        }
        let tint_curvature = {
            let (a, b) = look.d2_tint_d_t2(values.t);
            [a, b]
        };
        Box::new(SecondOrder {
            prediction,
            gradient,
            tint_curvature,
            tint_slope: [na_slope, nb_slope],
        })
    });
    let curvature = columns.curvature.then(|| {
        Box::new(curvature::build(
            look,
            layout,
            image,
            columns,
            rgb,
            clip_k,
            clip_slope,
            beta_slope,
            terms,
            target,
            &penalty,
            crossing,
            outer_identity,
        ))
    });

    let y_ref = looklab_model::solve_y_ref(values.t, input.y);
    let ucs_reference =
        (w > 0.0).then(|| looklab_model::look::ucs_reference_jacobian(input.unit_xyz, y_ref).1);
    let internal_reference = (near > 0.0)
        .then(|| looklab_model::look::neutral_chroma_jacobian(input.unit_xyz.scale(y_ref)).1);
    let j_gradient = looklab_model::look::j_prime_xyz_gradient(input.unit_xyz.scale(y_ref));
    let needs_xyz = columns.image.is_some()
        || columns.mode.is_some()
        || columns.exposure_extra.is_some()
        || columns.wb_r.is_some()
        || columns.wb_b.is_some()
        || columns.wb_r_extra.is_some()
        || columns.wb_b_extra.is_some()
        || columns.delta.is_some()
        || terms.iter().any(|term| {
            matches!(
                term.component,
                ResponseComponent::WbR | ResponseComponent::WbB
            )
        });
    let d_ucs_xyz = (needs_xyz && w > 0.0).then(|| {
        d_out_d_xyz(
            &input,
            &values,
            &d_layer_ucs_safe,
            ucs_reference.unwrap_or([[0.0; 3]; 2]),
            j_gradient,
            na_slope,
            nb_slope,
            [g, r, lambda],
            dres,
            dt_ds,
            xyz,
        )
    });
    let d_int_xyz = (needs_xyz && near > 0.0).then(|| {
        d_out_d_xyz(
            &input,
            &values,
            &d_layer_int,
            internal_reference.unwrap_or([[0.0; 3]; 2]),
            j_gradient,
            na_slope,
            nb_slope,
            [g, r, lambda],
            dres,
            dt_ds,
            xyz,
        )
    });
    let dw_dxyz = if input.blend.dw_dn == 0.0 {
        [0.0; 3]
    } else {
        let (neutral, jacobian) =
            looklab_model::look::neutral_chroma_jacobian(input.unit_xyz.scale(0.18));
        let inv_y = 1.0 / input.y;
        let d_unit = Mat3([
            [inv_y, -xyz.x / (input.y * input.y), 0.0],
            [0.0, 0.0, 0.0],
            [0.0, -xyz.z / (input.y * input.y), inv_y],
        ]);
        let dn_a = row_mat(scale(jacobian[0], 0.18), &d_unit);
        let dn_b = row_mat(scale(jacobian[1], 0.18), &d_unit);
        let magnitude = neutral.magnitude();
        let mut out = [0.0_f64; 3];
        if magnitude > 0.0 {
            for axis in 0..3 {
                out[axis] = input.blend.dw_dn * (neutral.a * dn_a[axis] + neutral.b * dn_b[axis])
                    / magnitude;
            }
        }
        out
    };
    let mut axis_x = [[0.0_f64; 3]; DOMAIN_AXES];
    axis_x[0] = scale([0.0, 1.0 / (input.y * core::f64::consts::LN_2), 0.0], dt_ds);
    // Both paths differentiate the same shared output lightness in row zero.
    if let Some(jacobian) = d_ucs_xyz.as_ref().or(d_int_xyz.as_ref()) {
        axis_x[1] = jacobian.0[0];
    }
    if let Some(jacobian) = &d_ucs_xyz {
        axis_x[2] = jacobian.0[1];
        axis_x[3] = jacobian.0[2];
    }
    if let Some(jacobian) = &d_int_xyz {
        axis_x[4] = jacobian.0[1];
        axis_x[5] = jacobian.0[2];
    }
    axis_x[6] = dw_dxyz;
    if w == 0.0 {
        axis_x[2] = [0.0; 3];
        axis_x[3] = [0.0; 3];
    }
    if near == 0.0 {
        axis_x[4] = [0.0; 3];
        axis_x[5] = [0.0; 3];
    }
    let axis_for = |dxyz: [f64; 3]| -> [f64; DOMAIN_AXES] {
        let mut out = [0.0_f64; DOMAIN_AXES];
        for (axis, row) in axis_x.iter().enumerate() {
            out[axis] = dot3(*row, dxyz);
        }
        out
    };
    let xyz_derivative = |component: usize| {
        [
            image.transform.0[0][component] * cam[component],
            image.transform.0[1][component] * cam[component],
            image.transform.0[2][component] * cam[component],
        ]
    };

    let mut jac_ucs = Jac {
        axis_l,
        ..Jac::default()
    };
    push_look_columns(
        &mut jac_ucs,
        look,
        layout,
        &input,
        &values,
        &d_layer_ucs_safe,
        Some(&active_beta),
    );
    let mut jac_int = Jac {
        axis_l,
        ..Jac::default()
    };
    if near > 0.0 {
        push_look_columns(
            &mut jac_int,
            look,
            layout,
            &input,
            &values,
            &d_layer_int,
            Some(&active_beta),
        );
    }
    let wb_ucs = |component: usize| {
        d_ucs_xyz
            .as_ref()
            .map(|jacobian| jacobian.mul_vec(xyz_derivative(component)))
    };
    let wb_int = |component: usize| {
        d_int_xyz
            .as_ref()
            .map(|jacobian| jacobian.mul_vec(xyz_derivative(component)))
    };
    let wb_axis = |component: usize| axis_for(xyz_derivative(component));
    if !terms.is_empty() {
        push_response_columns(
            &mut jac_ucs,
            look,
            layout,
            &image.tone_clamp,
            &input,
            &values,
            &d_layer_ucs_safe,
            terms,
            &wb_ucs,
            &wb_axis,
        );
        if near > 0.0 {
            push_response_columns(
                &mut jac_int,
                look,
                layout,
                &image.tone_clamp,
                &input,
                &values,
                &d_layer_int,
                terms,
                &wb_int,
                &wb_axis,
            );
        }
    }

    if let Some(path) = &ucs_path {
        jac_ucs.apply(&path.post);
    }
    if let Some(path) = &int_path {
        jac_int.apply(&path.post);
    }
    match (&ucs_path, &int_path) {
        (Some(_), Some(_)) => {
            for (position, ((column, outer), inner)) in jac_ucs
                .cols
                .iter()
                .zip(jac_ucs.derivs.iter())
                .zip(jac_int.derivs.iter())
                .enumerate()
            {
                jac.push_record(
                    *column as usize,
                    add(scale(*outer, w), scale(*inner, near)),
                    jac_ucs.axes[position],
                    jac_ucs.layers[position],
                    jac_ucs.layer_weights[position],
                    jac_ucs.layer_slopes[position],
                );
            }
        }
        (Some(_), None) => {
            for (position, (column, derivative)) in
                jac_ucs.cols.iter().zip(jac_ucs.derivs.iter()).enumerate()
            {
                jac.push_record(
                    *column as usize,
                    *derivative,
                    jac_ucs.axes[position],
                    jac_ucs.layers[position],
                    jac_ucs.layer_weights[position],
                    jac_ucs.layer_slopes[position],
                );
            }
        }
        (None, Some(_)) => {
            for (position, (column, derivative)) in
                jac_int.cols.iter().zip(jac_int.derivs.iter()).enumerate()
            {
                jac.push_record(
                    *column as usize,
                    *derivative,
                    jac_int.axes[position],
                    jac_int.layers[position],
                    jac_int.layer_weights[position],
                    jac_int.layer_slopes[position],
                );
            }
        }
        (None, None) => {}
    }

    let d_out_ds_ucs = appearance_slope(
        &d_layer_ucs_safe,
        na_slope,
        nb_slope,
        [g, r, lambda],
        dres,
        dt_ds,
    );
    let d_out_ds_int = (near > 0.0).then(|| {
        appearance_slope(
            &d_layer_int,
            na_slope,
            nb_slope,
            [g, r, lambda],
            dres,
            dt_ds,
        )
    });
    let ds_column = match (&ucs_path, &int_path) {
        (Some(outer), Some(inner)) => add(
            scale(outer.post.mul_vec(d_out_ds_ucs), w),
            scale(inner.post.mul_vec(d_out_ds_int.unwrap_or([0.0; 3])), near),
        ),
        (Some(outer), None) => outer.post.mul_vec(d_out_ds_ucs),
        (None, Some(inner)) => inner.post.mul_vec(d_out_ds_int.unwrap_or([0.0; 3])),
        (None, None) => [0.0; 3],
    };

    let exposure_xyz = [
        xyz.x * core::f64::consts::LN_2,
        xyz.y * core::f64::consts::LN_2,
        xyz.z * core::f64::consts::LN_2,
    ];
    for index in [columns.image, columns.mode, columns.exposure_extra]
        .into_iter()
        .flatten()
    {
        jac.push_axis(index, ds_column, axis_for(exposure_xyz));
    }

    if w > 0.0 && near > 0.0 {
        for term in terms {
            let component = match term.component {
                ResponseComponent::WbR => 0usize,
                ResponseComponent::WbB => 2,
                _ => continue,
            };
            let index = layout.response.start + term.index;
            if let Some(position) = jac.cols.iter().position(|column| *column as usize == index) {
                let blend = scale(delta_residual, dot3(dw_dxyz, xyz_derivative(component)));
                jac.derivs[position] = add(jac.derivs[position], scale(blend, term.phi));
            }
        }
    }
    let nuisance = |dxyz: [f64; 3]| -> [f64; 3] {
        let mut derivative = [0.0_f64; 3];
        if let (Some(outer), Some(jacobian)) = (&ucs_path, &d_ucs_xyz) {
            derivative = add(
                derivative,
                scale(outer.post.mul_vec(jacobian.mul_vec(dxyz)), w),
            );
        }
        if let (Some(inner), Some(jacobian)) = (&int_path, &d_int_xyz) {
            derivative = add(
                derivative,
                scale(inner.post.mul_vec(jacobian.mul_vec(dxyz)), near),
            );
        }
        if w > 0.0 && near > 0.0 {
            derivative = add(derivative, scale(delta_residual, dot3(dw_dxyz, dxyz)));
        }
        derivative
    };
    for (column, component) in [
        (columns.wb_r, 0usize),
        (columns.wb_b, 2),
        (columns.wb_r_extra, 0),
        (columns.wb_b_extra, 2),
    ] {
        if let Some(index) = column {
            let dxyz = xyz_derivative(component);
            jac.push_axis(index, nuisance(dxyz), axis_for(dxyz));
        }
    }
    if let Some(indices) = columns.delta {
        for (index, slot) in indices.iter().enumerate() {
            let dxyz = image
                .a0
                .mul_vec(looklab_body::Delta6::basis(index).mul_vec(cam));
            jac.push_axis(*slot, nuisance(dxyz), axis_for(dxyz));
        }
    }

    let clip_jacobian = match (&ucs_path, &int_path) {
        (Some(outer), Some(inner)) => {
            mat_add(&scale_mat(&outer.post, w), &scale_mat(&inner.post, near))
        }
        (Some(outer), None) => outer.post,
        (None, Some(inner)) => inner.post,
        (None, None) => Mat3::IDENTITY,
    };
    let dk = match (&ucs_path, &int_path) {
        (Some(outer), Some(inner)) => add(scale(outer.dk, w), scale(inner.dk, near)),
        (Some(outer), None) => outer.dk,
        (None, Some(inner)) => inner.dk,
        (None, None) => [0.0; 3],
    };
    for index in [columns.clip, columns.clip_extra].into_iter().flatten() {
        jac.push_axis(index, scale(dk, clip_slope), [0.0; DOMAIN_AXES]);
    }

    EvalOutput {
        prediction,
        residual,
        residual_weights,
        residual_indices,
        clip_jacobian,
        penalty: Some(penalty),
        violation: None,
        axis_terms: core::mem::take(&mut jac.axes),
        crossing,
        second,
        curvature,
    }
}
#[allow(clippy::too_many_arguments)]
fn push_response_columns(
    jac: &mut Jac,
    look: &Look,
    layout: &Layout,
    tone_clamp: &[ToneClamp; TONE_COEFS],
    input: &LookInput,
    values: &LayerValues,
    d_layer: &[[f64; 9]; 3],
    terms: &[ResponseTerm],
    wb_derivative: &impl Fn(usize) -> Option<[f64; 3]>,
    axis_wb: &impl Fn(usize) -> [f64; DOMAIN_AXES],
) {
    let d_na = column(d_layer, 1);
    let d_nb = column(d_layer, 2);
    let (na_slope, nb_slope) = look.d_tint_d_t(values.t);
    let d_t = add(
        add(column(d_layer, 0), scale(d_na, na_slope)),
        scale(d_nb, nb_slope),
    );
    let (tint_idx, tint_w, _) = basis::tint_values(values.t);
    let tint_weight = |feature: usize| {
        tint_idx
            .iter()
            .position(|index| *index == feature)
            .map(|position| tint_w[position])
            .unwrap_or(0.0)
    };
    let mut delta = [0.0_f64; 9];
    for term in terms {
        let index = layout.response.start + term.index;
        delta.fill(0.0);
        match term.component {
            ResponseComponent::Tone => {
                if look.tone.is_colorimetric() {
                    continue;
                }
                if term.feature != 0 && tone_clamp[term.feature as usize - 1] != ToneClamp::Interior
                {
                    continue;
                }
                let weight = if term.feature == 0 {
                    1.0
                } else {
                    input.tone_weights[term.feature as usize]
                };
                let value = term.phi * weight;
                delta[0] = value;
                delta[1] = na_slope * value;
                delta[2] = nb_slope * value;
                jac.push_axis(index, scale(d_t, value), jac.axis_from_delta(delta));
            }
            ResponseComponent::Tint => {
                let feature = term.feature as usize;
                let (column, weight, slot) = if feature < TINT_COEFS {
                    (d_na, tint_weight(feature), 1)
                } else {
                    (d_nb, tint_weight(feature - TINT_COEFS), 2)
                };
                let value = term.phi * weight;
                delta[slot] = value;
                jac.push_axis(index, scale(column, value), jac.axis_from_delta(delta));
            }
            ResponseComponent::Chroma | ResponseComponent::Hue | ResponseComponent::Lightness => {
                let layer = term.component.layer().unwrap_or(LayerKind::Chroma);
                let column_index = match layer {
                    LayerKind::Chroma => 3,
                    LayerKind::Hue => 4,
                    _ => 5,
                };
                let weight = grl_response_weight(input, layer, term.feature);
                let value = term.phi * weight;
                delta[column_index] = value;
                jac.push_axis(
                    index,
                    scale(column(d_layer, column_index), value),
                    jac.axis_from_delta(delta),
                );
            }
            ResponseComponent::WbR => {
                if let Some(derivative) = wb_derivative(0) {
                    jac.push_axis(
                        index,
                        scale(derivative, term.phi),
                        axis_wb(0).map(|value| value * term.phi),
                    );
                }
            }
            ResponseComponent::WbB => {
                if let Some(derivative) = wb_derivative(2) {
                    jac.push_axis(
                        index,
                        scale(derivative, term.phi),
                        axis_wb(2).map(|value| value * term.phi),
                    );
                }
            }
        }
    }
}

fn grl_response_weight(input: &LookInput, layer: LayerKind, feature: u32) -> f64 {
    let weights = &input.grl;
    let outer = input.blend.w;
    let near = input.blend.core();
    let plane_weight = outer * weights.c_w[0] + near;
    let mut sum = plane_weight
        * weights
            .s_w
            .iter()
            .zip(weights.s_idx.iter())
            .map(|(weight, index)| weight * feature_coefficient(layer, feature, *index))
            .sum::<f64>();
    for h in 0..4 {
        for s in 0..4 {
            let base = (weights.hue_idx[h] * S_COEFS + weights.s_idx[s]) * 3;
            for c in 1..4 {
                let node = S_COEFS + base + (weights.c_idx[c] - 1);
                sum += outer
                    * weights.hue_w[h]
                    * weights.s_w[s]
                    * weights.c_w[c]
                    * feature_coefficient(layer, feature, node);
            }
        }
    }
    sum
}

#[allow(clippy::too_many_arguments)]
fn d_out_d_xyz(
    input: &LookInput,
    values: &LayerValues,
    d_layer: &[[f64; 9]; 3],
    reference_chroma: [[f64; 3]; 2],
    j_gradient: [f64; 3],
    na_slope: f64,
    nb_slope: f64,
    grl: [GrlTerms; 3],
    dres: [[f64; 3]; 3],
    dt_ds: f64,
    xyz: Xyz,
) -> Mat3 {
    if input.y <= 0.0 {
        return Mat3([[0.0; 3]; 3]);
    }
    let y_ref = looklab_model::solve_y_ref(values.t, input.y);
    let inv_y = 1.0 / input.y;
    let d_unit = Mat3([
        [inv_y, -xyz.x / (input.y * input.y), 0.0],
        [0.0, 0.0, 0.0],
        [0.0, -xyz.z / (input.y * input.y), inv_y],
    ]);
    let mut d_reference = Mat3([[0.0; 3]; 3]);
    for row in 0..2 {
        d_reference.0[row + 1] = row_mat(scale(reference_chroma[row], y_ref), &d_unit);
    }
    d_reference.0[0] = scale(row_mat(j_gradient, &d_unit), y_ref);

    let gain = libm::exp(values.log_g);
    let radians = values.r * (PI / 180.0);
    let (sin_r, cos_r) = (libm::sin(radians), libm::cos(radians));
    let d_output = Mat3([
        [1.0, 0.0, 0.0],
        [0.0, gain * cos_r, -gain * sin_r],
        [0.0, gain * sin_r, gain * cos_r],
    ]);
    let mut out = d_output.mul(&d_reference);

    let (da_dxyz, db_dxyz) = if input.blend.w > 0.0 {
        let (_, chromaticity) = cam::ucs_jacobian(input.unit_xyz.scale(0.18));
        let da_du = scale(
            [chromaticity[1][0], chromaticity[1][1], chromaticity[1][2]],
            0.18,
        );
        let db_du = scale(
            [chromaticity[2][0], chromaticity[2][1], chromaticity[2][2]],
            0.18,
        );
        (row_mat(da_du, &d_unit), row_mat(db_du, &d_unit))
    } else {
        ([0.0; 3], [0.0; 3])
    };
    let (_, neutral_jacobian) =
        looklab_model::look::neutral_chroma_jacobian(input.unit_xyz.scale(0.18));
    let dn_a_dxyz = row_mat(scale(neutral_jacobian[0], 0.18), &d_unit);
    let dn_b_dxyz = row_mat(scale(neutral_jacobian[1], 0.18), &d_unit);

    let near = input.blend.core();
    let magnitude = input.neutral.magnitude();
    let (w_a, w_b) = if magnitude > 0.0 {
        (
            input.blend.dw_dn * input.neutral.a / magnitude,
            input.blend.dw_dn * input.neutral.b / magnitude,
        )
    } else {
        (0.0, 0.0)
    };
    let mut d_res_a = [0.0_f64; 3];
    let mut d_res_b = [0.0_f64; 3];
    for axis in 0..3 {
        let d_w = w_a * dn_a_dxyz[axis] + w_b * dn_b_dxyz[axis];
        d_res_a[axis] = input.blend.w * da_dxyz[axis]
            + near * dn_a_dxyz[axis]
            + d_w * (input.a_in - input.neutral.a);
        d_res_b[axis] = input.blend.w * db_dxyz[axis]
            + near * dn_b_dxyz[axis]
            + d_w * (input.b_in - input.neutral.b);
    }
    // The supplied layer derivatives are physical `s` derivatives; the lookup
    // slope is already folded into `dt_ds`, `grl[*].s` and `dres[0]`.
    let ds_dxyz = [0.0, 1.0 / (input.y * core::f64::consts::LN_2), 0.0];

    let mut values_part = Mat3([[0.0; 3]; 3]);
    let mut accumulate = |index: usize, gradient: [f64; 3]| {
        for channel in 0..3 {
            for axis in 0..3 {
                values_part.0[channel][axis] += d_layer[channel][index] * gradient[axis];
            }
        }
    };
    accumulate(0, scale(ds_dxyz, dt_ds));
    accumulate(1, scale(ds_dxyz, dt_ds * na_slope));
    accumulate(2, scale(ds_dxyz, dt_ds * nb_slope));
    for (component, index) in [(0usize, 3usize), (1, 4), (2, 5)] {
        let terms = grl[component];
        accumulate(
            index,
            add(
                scale(ds_dxyz, terms.s),
                add(
                    add(scale(da_dxyz, terms.a), scale(db_dxyz, terms.b)),
                    add(scale(dn_a_dxyz, terms.n_a), scale(dn_b_dxyz, terms.n_b)),
                ),
            ),
        );
    }
    for channel in 0..3 {
        accumulate(
            6 + channel,
            add(
                scale(ds_dxyz, dres[0][channel]),
                add(
                    scale(d_res_a, dres[1][channel]),
                    scale(d_res_b, dres[2][channel]),
                ),
            ),
        );
    }
    out = mat_add(&out, &values_part);
    out
}

/// Linear `space` RGB of the pre-clip prediction.
pub(crate) fn output_channels(pre: Ucs, space: RgbSpace) -> [f64; 3] {
    let model = reference_model();
    let xyz = model.jmh_to_xyz(ucs_to_jmh(pre, UcsSpace::Ucs));
    let rgb = space.xyz_to_rgb_linear(xyz);
    [rgb.r, rgb.g, rgb.b]
}

/// Post-clip `J'a'b'` in `space` with soft clip `clip_k`.
pub fn post_clip(look: &Look, xyz: Xyz, space: RgbSpace, clip_k: f64) -> Ucs {
    let channels = output_channels(look.eval(xyz), space);
    let clipped = channels.map(|value| soft_clip(value, clip_k));
    ucs_of_xyz(space.rgb_linear_to_xyz(Rgb::new(clipped[0], clipped[1], clipped[2])))
}

/// Post-clip fitting residual without the Jacobian, for samples outside the
/// Hessian set, and its domain penalty.
pub(crate) fn predict_only(
    look: &Look,
    image: &ImageCtx,
    rgb: [f64; 3],
    clip_k: f64,
    target: &Target,
) -> (Ucs, [f64; 3], Option<PenaltyDerivatives>) {
    let exposure_scale = image.exposure_scale;
    let cam = [
        rgb[0] * image.wb_r * exposure_scale,
        rgb[1] * exposure_scale,
        rgb[2] * image.wb_b * exposure_scale,
    ];
    let xyz = Xyz::from_array(image.transform.mul_vec(cam));
    let input = LookInput::new(xyz);
    let values = look.layer_values(&input);
    if domain_penalty::check_reference_tone(&values).is_err() {
        return (Ucs::new(0.0, 0.0, 0.0), [0.0; 3], None);
    }
    let paths = look.chroma_paths(&input, &values);
    let penalty = match domain_penalty::evaluate(&values, &paths) {
        Ok(penalty) => Some(penalty),
        Err(_) => return (Ucs::new(0.0, 0.0, 0.0), [0.0; 3], None),
    };
    let w = paths.blend;
    let near = 1.0 - w;
    let space = image.space;
    let ucs_post = if w > 0.0 {
        if outer_path_state(space, paths.ucs, clip_k).is_err() {
            return (Ucs::new(0.0, 0.0, 0.0), [0.0; 3], None);
        }
        let channels = output_channels(paths.ucs, space);
        let clipped = channels.map(|value| soft_clip(value, clip_k));
        Some(ucs_of_xyz(space.rgb_linear_to_xyz(Rgb::new(
            clipped[0], clipped[1], clipped[2],
        ))))
    } else {
        None
    };
    let internal_post = (near > 0.0).then(|| {
        let xyz_pre = looklab_model::look::internal_chroma_to_xyz(paths.internal, paths.ucs.j);
        let channels = xyz_channels(space, xyz_pre);
        let clipped = channels.map(|value| soft_clip(value, clip_k));
        let xyz_post = space.rgb_linear_to_xyz(Rgb::new(clipped[0], clipped[1], clipped[2]));
        let chroma = looklab_model::look::neutral_chroma_jacobian(xyz_post).0;
        (ucs_of_xyz(xyz_post), [chroma.a, chroma.b])
    });
    let prediction = match (ucs_post, internal_post) {
        (Some(_outer), Some((inner, _))) if w == 0.0 => inner,
        (Some(outer), Some((inner, _))) => Ucs::new(
            w * outer.j + near * inner.j,
            w * outer.a + near * inner.a,
            w * outer.b + near * inner.b,
        ),
        (Some(outer), None) => outer,
        (None, Some((inner, _))) => inner,
        (None, None) => Ucs::new(0.0, 0.0, 0.0),
    };
    let residual = match (ucs_post, internal_post) {
        (Some(outer), Some((inner, chroma))) => {
            let fitting = [
                inner.j - target.ucs.j,
                chroma[0] - target.internal[0],
                chroma[1] - target.internal[1],
            ];
            if w == 0.0 {
                fitting
            } else {
                let outer = [
                    outer.j - target.ucs.j,
                    outer.a - target.ucs.a,
                    outer.b - target.ucs.b,
                ];
                add(scale(outer, w), scale(fitting, near))
            }
        }
        (Some(outer), None) => [
            outer.j - target.ucs.j,
            outer.a - target.ucs.a,
            outer.b - target.ucs.b,
        ],
        (None, Some((inner, chroma))) => [
            inner.j - target.ucs.j,
            chroma[0] - target.internal[0],
            chroma[1] - target.internal[1],
        ],
        (None, None) => [0.0; 3],
    };
    (prediction, residual, penalty)
}

/// Below 0.25 the pre-clip value is unidentifiable.
pub(crate) fn clip_derivative_min(pre: Ucs, space: RgbSpace, clip_k: f64) -> f64 {
    output_channels(pre, space)
        .iter()
        .map(|value| clip_derivative(*value, clip_k))
        .fold(1.0_f64, f64::min)
}

/// Finite `Φ` derivative from [`composite::evaluate`], or `None` at its singular
/// point and outside its inverse domain.
fn composite_clip_derivative(pre: Ucs, space: RgbSpace, clip_k: f64) -> Option<(Mat3, [f64; 3])> {
    let point = composite::evaluate(pre, space, clip_k).ok()?;
    match point.derivative {
        CompositeDerivative::Finite {
            jacobian,
            jacobian_k,
            ..
        } => Some((Mat3(jacobian), jacobian_k)),
        CompositeDerivative::Singular(_) => None,
    }
}

/// `d post-clip / d k` in `J'a'b'` coordinates.
pub(crate) fn clip_k_gradient(pre: Ucs, space: RgbSpace, clip_k: f64) -> [f64; 3] {
    let channels = output_channels(pre, space);
    if channels
        .iter()
        .all(|value| *value >= clip_k && *value <= 1.0 - clip_k)
    {
        return [0.0; 3];
    }
    let clipped = channels.map(|value| soft_clip(value, clip_k));
    let xyz1 = space.rgb_linear_to_xyz(Rgb::new(clipped[0], clipped[1], clipped[2]));
    let (_, j1) = cam::ucs_jacobian(xyz1);
    let gradient = Mat3(j1).mul(&space.to_xyz_matrix()).mul_vec([
        clip_k_derivative(channels[0], clip_k),
        clip_k_derivative(channels[1], clip_k),
        clip_k_derivative(channels[2], clip_k),
    ]);
    if gradient.iter().all(|value| value.is_finite()) {
        return gradient;
    }
    composite_clip_derivative(pre, space, clip_k).map_or(gradient, |(_, jacobian_k)| jacobian_k)
}

/// `d post-clip / d pre-clip` in `J'a'b'` coordinates.
pub(crate) fn clip_jacobian(pre: Ucs, space: RgbSpace, clip_k: f64) -> Mat3 {
    let channels = output_channels(pre, space);
    let unclipped = channels
        .iter()
        .all(|value| *value >= clip_k && *value <= 1.0 - clip_k);
    if unclipped {
        return Mat3::IDENTITY;
    }
    let clipped = channels.map(|value| soft_clip(value, clip_k));
    let xyz1 = space.rgb_linear_to_xyz(Rgb::new(clipped[0], clipped[1], clipped[2]));
    let (_, j1) = cam::ucs_jacobian(xyz1);
    let per_channel = Mat3::diag([
        clip_derivative(channels[0], clip_k),
        clip_derivative(channels[1], clip_k),
        clip_derivative(channels[2], clip_k),
    ]);
    let (_, d_xyz0) = looklab_model::clip::preclip_xyz_jacobian(pre);
    let jacobian = Mat3(j1)
        .mul(&space.to_xyz_matrix())
        .mul(&per_channel)
        .mul(&space.from_xyz_matrix())
        .mul(&Mat3(d_xyz0));
    if jacobian.0.iter().flatten().all(|value| value.is_finite()) {
        return jacobian;
    }
    composite_clip_derivative(pre, space, clip_k).map_or(jacobian, |(jacobian, _)| jacobian)
}

/// Pre-clip `J'a'b'` of one probe and `dJ'a'b'/dθ`, for the sandwich estimator.
pub(crate) fn evaluate_probe(
    look: &Look,
    layout: &Layout,
    xyz: Xyz,
    beta_slope: &[f64; TONE_COEFS],
    jac: &mut Jac,
) -> Ucs {
    jac.clear();
    let input = LookInput::new(xyz);
    let values = look.layer_values(&input);
    let pre = look.eval_from_layer_values(&input, &values);
    let d_layer = look.d_output_d_layer_values(&input, &values);
    push_look_columns(
        jac,
        look,
        layout,
        &input,
        &values,
        &d_layer,
        Some(beta_slope),
    );
    pre
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::layout::{Params, softplus_slope};
    use looklab_color::RgbSpace;
    use looklab_model::ClipSettings;
    use looklab_model::look::{ColorLayer, Residual, Tint};
    use looklab_model::tone::Tone;

    const LIPSCHITZ_ENVELOPE: f64 = 1.0;

    fn context(params: &Params, transform: Mat3, space: RgbSpace) -> ImageCtx {
        let nuisance = params.image(0);
        ImageCtx {
            transform,
            a0: transform,
            space,
            exposure_scale: libm::pow(2.0, nuisance[0]),
            wb_r: libm::exp(nuisance[1]),
            wb_b: libm::exp(nuisance[2]),
            tone_clamp: [ToneClamp::Interior; TONE_COEFS],
        }
    }

    fn slopes(params: &Params) -> [f64; TONE_COEFS] {
        let mut slope = [0.0; TONE_COEFS];
        for (k, value) in slope.iter_mut().enumerate() {
            *value = softplus_slope(params.theta[params.layout.tone_beta.start + k]);
        }
        slope
    }

    fn perturb(params: &Params, direction: &[f64], epsilon: f64) -> Params {
        let mut out = params.clone();
        for (value, delta) in out.theta.iter_mut().zip(direction.iter()) {
            *value += epsilon * delta;
        }
        out
    }

    fn target() -> Target {
        Target::new(Ucs::new(55.0, 0.5, -0.3))
    }

    #[test]
    fn predict_only_rejects_an_outer_path_violation() {
        let look = Look::new(
            Tone::spline(136.425_760_286_722_24, [0.0; TONE_COEFS]).expect("constant tone"),
            Tint::zeros(),
            ColorLayer::zeros(),
            Residual::zeros(),
            ClipSettings::default_v1(),
        )
        .expect("look");
        let context = ImageCtx {
            transform: Mat3::IDENTITY,
            a0: Mat3::IDENTITY,
            space: RgbSpace::Srgb,
            exposure_scale: 1.0,
            wb_r: 1.0,
            wb_b: 1.0,
            tone_clamp: [ToneClamp::Interior; TONE_COEFS],
        };
        let rgb = [2.8917183876037598, 3.0424540042877197, 3.313408136367798];
        let clip_k = 5.000_095e-3;
        let input = LookInput::new(Xyz::from_array(context.transform.mul_vec(rgb)));
        let values = look.layer_values(&input);
        let paths = look.chroma_paths(&input, &values);
        assert!(
            domain_penalty::check_reference_tone(&values).is_ok(),
            "the fixture fails the shared reference-tone check"
        );
        assert!(
            domain_penalty::evaluate(&values, &paths).is_ok(),
            "the fixture fails the shared domain penalty"
        );
        assert!(
            outer_path_state(context.space, paths.ucs, clip_k).is_err(),
            "the fixture outer path is feasible: {:?}",
            paths.ucs
        );
        let (_, _, penalty) = predict_only(&look, &context, rgb, clip_k, &Target::new(paths.ucs));
        assert!(
            penalty.is_none(),
            "predict_only accepted an outer path the full evaluation rejects"
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn residual(
        params: &Params,
        transform: Mat3,
        space: RgbSpace,
        rgb: [f64; 3],
        columns: &ColumnMap,
    ) -> [f64; 3] {
        let mut jac = Jac::default();
        evaluate_sample(
            &params.look(),
            &params.layout,
            &context(params, transform, space),
            columns,
            rgb,
            params.clip_k(),
            params.clip_slope(),
            &slopes(params),
            &[],
            &target(),
            &mut jac,
        )
        .residual
    }

    #[test]
    fn wb_paths_crossing_neutral_have_smooth_jacobians() {
        let layout = Layout::new(1, 0, 1);
        let mut params = Params::initial(layout.clone(), 1);
        for range in [layout.g.clone(), layout.r.clone(), layout.lambda.clone()] {
            for value in &mut params.theta[range.start + S_COEFS..range.end] {
                *value = 0.02;
            }
        }
        let white = looklab_model::solve::reference_white();
        let image = layout.image(0);
        params.theta[image.start] = 0.0;
        params.theta[image.start + 1] = libm::log(white.x / 0.18);
        params.theta[image.start + 2] = libm::log(white.z / 0.18);
        let columns = ColumnMap {
            image: None,
            mode: None,
            wb_r: Some(image.start + 1),
            wb_b: Some(image.start + 2),
            clip: None,
            exposure_extra: None,
            wb_r_extra: None,
            wb_b_extra: None,
            clip_extra: None,
            delta: None,
            second: false,
            curvature: false,
            clip_geometry: ClipGeometry::Logistic,
        };
        let rgb = [0.18, 0.18, 0.18];
        let mut jac = Jac::default();
        let _ = evaluate_sample(
            &params.look(),
            &layout,
            &context(&params, Mat3::IDENTITY, RgbSpace::Srgb),
            &columns,
            rgb,
            params.clip_k(),
            params.clip_slope(),
            &slopes(&params),
            &[],
            &target(),
            &mut jac,
        );
        let derivative = |index: usize| {
            let position = jac
                .cols
                .iter()
                .position(|column| *column as usize == index)
                .expect("column");
            jac.derivs[position]
        };
        let d_r = derivative(image.start + 1);
        let d_b = derivative(image.start + 2);
        for direction in 0..8 {
            let angle = 2.0 * core::f64::consts::PI * direction as f64 / 8.0;
            let (cos, sin) = (libm::cos(angle), libm::sin(angle));
            let analytic = [
                d_r[0] * cos + d_b[0] * sin,
                d_r[1] * cos + d_b[1] * sin,
                d_r[2] * cos + d_b[2] * sin,
            ];
            let mut fine = [0.0_f64; 3];
            for step in [1e-3, 1e-4] {
                let mut plus = params.clone();
                let mut minus = params.clone();
                plus.theta[image.start + 1] += step * cos;
                plus.theta[image.start + 2] += step * sin;
                minus.theta[image.start + 1] -= step * cos;
                minus.theta[image.start + 2] -= step * sin;
                let mut scratch = Jac::default();
                let a = evaluate_sample(
                    &plus.look(),
                    &layout,
                    &context(&plus, Mat3::IDENTITY, RgbSpace::Srgb),
                    &ColumnMap::default(),
                    rgb,
                    plus.clip_k(),
                    plus.clip_slope(),
                    &slopes(&plus),
                    &[],
                    &target(),
                    &mut scratch,
                )
                .residual;
                let b = evaluate_sample(
                    &minus.look(),
                    &layout,
                    &context(&minus, Mat3::IDENTITY, RgbSpace::Srgb),
                    &ColumnMap::default(),
                    rgb,
                    minus.clip_k(),
                    minus.clip_slope(),
                    &slopes(&minus),
                    &[],
                    &target(),
                    &mut scratch,
                )
                .residual;
                let numerical = [
                    (a[0] - b[0]) / (2.0 * step),
                    (a[1] - b[1]) / (2.0 * step),
                    (a[2] - b[2]) / (2.0 * step),
                ];
                for channel in 0..3 {
                    assert!(analytic[channel].is_finite());
                    assert!(numerical[channel].is_finite());
                    let scale = analytic[channel]
                        .abs()
                        .max(numerical[channel].abs())
                        .max(1e-2);
                    assert!(
                        (analytic[channel] - numerical[channel]).abs() / scale < 2e-2,
                        "direction {direction} step {step} channel {channel}: {} vs {}",
                        analytic[channel],
                        numerical[channel]
                    );
                    if step == 1e-3 {
                        fine[channel] = numerical[channel];
                    } else {
                        let scale = fine[channel].abs().max(numerical[channel].abs()).max(1e-2);
                        assert!(
                            (fine[channel] - numerical[channel]).abs() / scale < 5e-2,
                            "direction {direction} step dependence channel {channel}: {} vs {}",
                            fine[channel],
                            numerical[channel]
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn neutral_sample_has_finite_residual_columns() {
        let layout = Layout::new(1, 0, 1);
        let mut params = Params::initial(layout.clone(), 1);
        let image = layout.image(0);
        params.theta[image.start] = 0.0;
        params.theta[image.start + 1] = 0.0;
        params.theta[image.start + 2] = 0.0;
        let white = looklab_model::solve::reference_white();
        let rgb = [white.x * 0.18, 0.18, white.z * 0.18];
        assert_eq!(
            LookInput::new(Xyz::new(rgb[0], rgb[1], rgb[2])).blend.w,
            0.0
        );
        let columns = ColumnMap {
            image: Some(image.start),
            mode: None,
            wb_r: Some(image.start + 1),
            wb_b: Some(image.start + 2),
            clip: Some(layout.clip),
            exposure_extra: None,
            wb_r_extra: None,
            wb_b_extra: None,
            clip_extra: None,
            delta: None,
            second: false,
            curvature: false,
            clip_geometry: ClipGeometry::Logistic,
        };
        let mut jac = Jac::default();
        let evaluated = evaluate_sample(
            &params.look(),
            &layout,
            &context(&params, Mat3::IDENTITY, RgbSpace::Srgb),
            &columns,
            rgb,
            params.clip_k(),
            params.clip_slope(),
            &slopes(&params),
            &[],
            &target(),
            &mut jac,
        );
        assert!(!jac.derivs.is_empty());
        assert_eq!(jac.derivs.len(), evaluated.axis_terms.len());
        for derivative in &jac.derivs {
            for value in derivative {
                assert!(value.is_finite(), "{derivative:?}");
            }
        }
        for axis in &evaluated.axis_terms {
            for value in axis {
                assert!(value.is_finite(), "{axis:?}");
            }
        }
        assert!(evaluated.residual.iter().all(|value| value.is_finite()));
    }

    #[test]
    fn layer_second_order_terms_match_differences_of_the_directional_derivative() {
        let layout = Layout::new(1, 0, 1);
        let mut params = Params::initial(layout.clone(), 1);
        for (k, index) in layout.tone_beta.clone().enumerate() {
            params.theta[index] += 0.03 * (1 + k % 3) as f64;
        }
        for (k, index) in layout.tint_a.clone().enumerate() {
            params.theta[index] += 0.04 * ((k % 3) as f64 - 1.0);
        }
        for (k, index) in layout.tint_b.clone().enumerate() {
            params.theta[index] -= 0.03 * ((k % 2) as f64);
        }
        for range in [layout.g.clone(), layout.r.clone(), layout.lambda.clone()] {
            for (k, index) in range.enumerate() {
                params.theta[index] += 0.02 * (1 + k % 2) as f64;
            }
        }
        for (k, index) in layout.residual.clone().enumerate() {
            params.theta[index] += 0.01 * (1 + k % 3) as f64;
        }
        let rgb = [0.34, 0.26, 0.13];
        let target = Target::new(Ucs::new(52.0, 4.0, -3.0));
        let columns = ColumnMap {
            second: true,
            ..ColumnMap::default()
        };
        let mut jac = Jac::default();
        let out = evaluate_sample(
            &params.look(),
            &layout,
            &context(&params, Mat3::IDENTITY, RgbSpace::Srgb),
            &columns,
            rgb,
            params.clip_k(),
            params.clip_slope(),
            &slopes(&params),
            &[],
            &target,
            &mut jac,
        );
        let order = out.second.as_ref().expect("second order");
        let mut worst = 0.0_f64;
        // A direction supported on tint or on the linear layers leaves every layer
        // value's second derivative zero.
        for layer in LAYERS.into_iter().skip(1) {
            let position = jac
                .layers
                .iter()
                .enumerate()
                .filter(|(index, value)| {
                    **value == Some(layer) && jac.layer_weights[*index][3].abs() > 1e-3
                })
                .map(|(index, _)| index)
                .next();
            let Some(position) = position else {
                continue;
            };
            let weight = jac.layer_weights[position][3];
            let mut direction = vec![0.0_f64; layout.total];
            direction[jac.cols[position] as usize] = 1.0 / weight;
            let step = 1e-6;
            let mut plus = params.clone();
            let mut minus = params.clone();
            for (index, value) in direction.iter().enumerate() {
                plus.theta[index] += step * value;
                minus.theta[index] -= step * value;
            }
            let mut scratch = Jac::default();
            let mut evaluate = |at: &Params| {
                let _ = evaluate_sample(
                    &at.look(),
                    &layout,
                    &context(at, Mat3::IDENTITY, RgbSpace::Srgb),
                    &columns,
                    rgb,
                    at.clip_k(),
                    at.clip_slope(),
                    &slopes(at),
                    &[],
                    &target,
                    &mut scratch,
                );
                let mut total = [0.0_f64; 3];
                for (column, derivative) in scratch.cols.iter().zip(scratch.derivs.iter()) {
                    let value = direction[*column as usize];
                    total[0] += derivative[0] * value;
                    total[1] += derivative[1] * value;
                    total[2] += derivative[2] * value;
                }
                total
            };
            let forward = evaluate(&plus);
            let backward = evaluate(&minus);
            for channel in 0..3 {
                let numerical = (forward[channel] - backward[channel]) / (2.0 * step);
                let index = layer.index();
                let analytic = order.prediction[channel][index * 9 + index];
                let scale = numerical.abs().max(1e-3);
                let gap = (analytic - numerical).abs() / scale;
                worst = worst.max(gap);
                println!(
                    "layer {index} channel {channel}: analytic {analytic:.9e} numerical {numerical:.9e} gap {gap:.3e}"
                );
            }
        }
        println!("layer second order worst relative gap {worst:.3e}");
        assert!(worst < 1e-5, "worst relative gap {worst:.3e}");
        // A tone direction moves the two tint layers through `d w_m/d t`; the
        // residual contraction of that motion is a scalar per tone column.
        let position = jac
            .layers
            .iter()
            .enumerate()
            .position(|(index, layer)| {
                *layer == Some(Layer::Tone) && jac.cols[index] as usize == layout.tone_c0
            })
            .expect("tone c0 column");
        let mut direction = vec![0.0_f64; layout.total];
        direction[jac.cols[position] as usize] = 1.0;
        let dl = [1.0, order.tint_slope[0], order.tint_slope[1]];
        let step = 1e-6;
        let mut plus = params.clone();
        let mut minus = params.clone();
        for (index, value) in direction.iter().enumerate() {
            plus.theta[index] += step * value;
            minus.theta[index] -= step * value;
        }
        let mut scratch = Jac::default();
        let mut evaluate = |at: &Params| {
            let _ = evaluate_sample(
                &at.look(),
                &layout,
                &context(at, Mat3::IDENTITY, RgbSpace::Srgb),
                &columns,
                rgb,
                at.clip_k(),
                at.clip_slope(),
                &slopes(at),
                &[],
                &target,
                &mut scratch,
            );
            let mut total = [0.0_f64; 3];
            for (column, derivative) in scratch.cols.iter().zip(scratch.derivs.iter()) {
                let value = direction[*column as usize];
                total[0] += derivative[0] * value;
                total[1] += derivative[1] * value;
                total[2] += derivative[2] * value;
            }
            total
        };
        let forward = evaluate(&plus);
        let backward = evaluate(&minus);
        let numerical = (0..3)
            .map(|channel| {
                out.residual[channel] * (forward[channel] - backward[channel]) / (2.0 * step)
            })
            .sum::<f64>();
        let value = |k: usize| {
            (0..3)
                .map(|channel| out.residual[channel] * order.gradient[k][channel])
                .sum::<f64>()
        };
        let mut analytic = value(1) * order.tint_curvature[0] + value(2) * order.tint_curvature[1];
        for k in 0..3 {
            for l in 0..3 {
                let mut contracted = 0.0;
                for channel in 0..3 {
                    contracted += out.residual[channel] * order.prediction[channel][k * 9 + l];
                }
                analytic += contracted * dl[k] * dl[l];
            }
        }
        println!("tone contraction: analytic {analytic:.9e} numerical {numerical:.9e}");
        let scale = numerical.abs().max(1e-6);
        assert!(
            (analytic - numerical).abs() / scale < 1e-5,
            "tone contraction: {analytic:.9e} vs {numerical:.9e}"
        );
    }

    /// Layer values in the order of [`Look::d_ucs_d_layer_values`].
    const LAYERS: [Layer; 9] = [
        Layer::Tone,
        Layer::TintA,
        Layer::TintB,
        Layer::LogG,
        Layer::Rotation,
        Layer::Lambda,
        Layer::ResA,
        Layer::ResB,
        Layer::ResC,
    ];

    /// Layer velocity the parameter direction actually produces. A tone column moves
    /// `t`, `n_a` and `n_b` together, so this is not the declared direction.
    fn actual_layer_direction(jac: &Jac, direction: &[f64]) -> [f64; 9] {
        let mut dl = [0.0_f64; 9];
        for (position, layer) in jac.layers.iter().enumerate() {
            let value = direction[jac.cols[position] as usize];
            let weights = jac.layer_weights[position];
            for (k, entry) in dl.iter_mut().enumerate().take(3) {
                *entry += weights[k] * value;
            }
            if let Some(layer) = layer
                && layer.index() >= 3
            {
                dl[layer.index()] += weights[3] * value;
            }
        }
        dl
    }

    /// Parameter direction that moves each layer value by `dl[k]`.
    fn layer_direction(layout: &Layout, jac: &Jac, dl: &[f64; 9]) -> Vec<f64> {
        let mut direction = vec![0.0_f64; layout.total];
        for (k, target) in dl.iter().enumerate() {
            if *target == 0.0 {
                continue;
            }
            let slot = if k < 3 { k } else { 3 };
            let (index, _) = jac
                .layers
                .iter()
                .enumerate()
                .find(|(index, layer)| {
                    **layer == Some(LAYERS[k]) && jac.layer_weights[*index][slot].abs() > 1e-6
                })
                .unwrap_or_else(|| panic!("no column for layer {k}"));
            direction[jac.cols[index] as usize] = target / jac.layer_weights[index][slot];
        }
        direction
    }

    /// `∂²p_c/∂θ_i∂θ_v` for every column `i` from the analytic second order.
    fn expected_acceleration(
        jac: &Jac,
        order: &SecondOrder,
        dl: &[f64; 9],
        direction: &[f64],
    ) -> Vec<[f64; 3]> {
        let mut tint_a_direction = 0.0;
        let mut tint_b_direction = 0.0;
        for (position, layer) in jac.layers.iter().enumerate() {
            let value = direction[jac.cols[position] as usize];
            match layer {
                Some(Layer::TintA) => tint_a_direction += jac.layer_slopes[position] * value,
                Some(Layer::TintB) => tint_b_direction += jac.layer_slopes[position] * value,
                _ => {}
            }
        }
        let mut expected = Vec::with_capacity(jac.cols.len());
        for (index, _) in jac.cols.iter().enumerate() {
            let weights = jac.layer_weights[index];
            let value = direction[jac.cols[index] as usize];
            let slope = jac.layer_slopes[index];
            let mut accel = [0.0_f64; 3];
            for k in 0..9 {
                let weight = if k < 3 {
                    weights[k]
                } else if jac.layers[index] == Some(LAYERS[k]) {
                    weights[3]
                } else {
                    0.0
                };
                if weight == 0.0 {
                    continue;
                }
                for l in 0..9 {
                    for channel in 0..3 {
                        accel[channel] += weight * order.prediction[channel][k * 9 + l] * dl[l];
                    }
                }
            }
            // Motion of the column's own layer weights along the direction.
            let motion = match jac.layers[index] {
                Some(Layer::Tone) => {
                    let channel = weights[0];
                    [
                        slope * value,
                        order.tint_curvature[0] * channel * dl[0]
                            + order.tint_slope[0] * slope * value
                            + channel * tint_a_direction,
                        order.tint_curvature[1] * channel * dl[0]
                            + order.tint_slope[1] * slope * value
                            + channel * tint_b_direction,
                    ]
                }
                Some(Layer::TintA) => [0.0, slope * dl[0], 0.0],
                Some(Layer::TintB) => [0.0, 0.0, slope * dl[0]],
                _ => [0.0; 3],
            };
            for k in 0..3 {
                for channel in 0..3 {
                    accel[channel] += order.gradient[k][channel] * motion[k];
                }
            }
            expected.push(accel);
        }
        expected
    }

    #[test]
    fn column_acceleration_matches_the_directional_derivative() {
        let layout = Layout::new(1, 0, 1);
        let mut params = Params::initial(layout.clone(), 1);
        for (k, index) in layout.tone_beta.clone().enumerate() {
            params.theta[index] += 0.03 * (1 + k % 3) as f64;
        }
        for (k, index) in layout.tint_a.clone().enumerate() {
            params.theta[index] += 0.05 * ((k % 3) as f64 - 1.0);
        }
        for (k, index) in layout.tint_b.clone().enumerate() {
            params.theta[index] -= 0.04 * ((k % 2) as f64);
        }
        for range in [layout.g.clone(), layout.r.clone(), layout.lambda.clone()] {
            for (k, index) in range.enumerate() {
                params.theta[index] += 0.02 * (1 + k % 2) as f64;
            }
        }
        let target = Target::new(Ucs::new(52.0, 4.0, -3.0));
        let columns = ColumnMap {
            second: true,
            ..ColumnMap::default()
        };
        let white = looklab_model::solve::reference_white();
        let neutral = 0.18 / libm::pow(2.0, params.image(0)[0]);
        let context_at = context(&params, Mat3::IDENTITY, RgbSpace::Srgb);
        // Outer path, the near-neutral blend transition, and the clip shoulders.
        let samples = [
            ("outer", [0.34, 0.26, 0.13]),
            (
                "transition",
                [
                    white.x * neutral / context_at.wb_r * 1.003,
                    neutral,
                    white.z * neutral / context_at.wb_b,
                ],
            ),
            ("bright", [0.62, 0.5, 0.38]),
            ("clipped", [1.02, 0.95, 0.3]),
        ];
        let mut worst = 0.0_f64;
        let mut checked = 0usize;
        for (label, rgb) in samples {
            let mut jac = Jac::default();
            let out = evaluate_sample(
                &params.look(),
                &layout,
                &context_at,
                &columns,
                rgb,
                params.clip_k(),
                params.clip_slope(),
                &slopes(&params),
                &[],
                &target,
                &mut jac,
            );
            let order = out.second.as_ref().expect("second order");
            for k in 0..9 {
                for l in 0..9 {
                    for channel in 0..3 {
                        let left = order.prediction[channel][k * 9 + l];
                        let right = order.prediction[channel][l * 9 + k];
                        assert!(
                            (left - right).abs() <= 1e-9 * left.abs().max(right.abs()) + 1e-12,
                            "{label} symmetry {k} {l} channel {channel}: {left:.9e} vs {right:.9e}"
                        );
                    }
                }
            }
            let families: [(&str, [f64; 9]); 6] = [
                ("rotation", [0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0]),
                ("lambda", [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0]),
                (
                    "gain-rotation",
                    [0.0, 0.0, 0.0, 1.0, 0.8, 0.0, 0.0, 0.0, 0.0],
                ),
                ("gain-lambda", [0.0, 0.0, 0.0, 0.9, 0.0, 1.0, 0.0, 0.0, 0.0]),
                ("tint-gain", [0.0, 0.6, -0.4, 0.7, 0.0, 0.0, 0.0, 0.0, 0.0]),
                (
                    "tone-rotation",
                    [0.5, 0.3, -0.2, 0.0, 0.9, 0.0, 0.0, 0.0, 0.0],
                ),
            ];
            for (family, dl) in families {
                let direction = layer_direction(&layout, &jac, &dl);
                let dl = actual_layer_direction(&jac, &direction);
                let expected = expected_acceleration(&jac, order, &dl, &direction);
                let step = 1e-6;
                let mut plus = params.clone();
                let mut minus = params.clone();
                for (index, value) in direction.iter().enumerate() {
                    plus.theta[index] += step * value;
                    minus.theta[index] -= step * value;
                }
                let mut scratch = Jac::default();
                let mut evaluate = |at: &Params| {
                    let _ = evaluate_sample(
                        &at.look(),
                        &layout,
                        &context(at, Mat3::IDENTITY, RgbSpace::Srgb),
                        &columns,
                        rgb,
                        at.clip_k(),
                        at.clip_slope(),
                        &slopes(at),
                        &[],
                        &target,
                        &mut scratch,
                    );
                    let mut out = vec![[0.0_f64; 3]; layout.total];
                    for (column, derivative) in scratch.cols.iter().zip(scratch.derivs.iter()) {
                        for channel in 0..3 {
                            out[*column as usize][channel] += derivative[channel];
                        }
                    }
                    out
                };
                let forward = evaluate(&plus);
                let backward = evaluate(&minus);
                let mut analytic = vec![[0.0_f64; 3]; layout.total];
                for (index, (column, _)) in jac.cols.iter().zip(jac.derivs.iter()).enumerate() {
                    for channel in 0..3 {
                        analytic[*column as usize][channel] += expected[index][channel];
                    }
                }
                for index in 0..layout.total {
                    for channel in 0..3 {
                        let numerical =
                            (forward[index][channel] - backward[index][channel]) / (2.0 * step);
                        let value = analytic[index][channel];
                        let scale = numerical.abs().max(1e-3);
                        let gap = (value - numerical).abs() / scale;
                        if gap > worst {
                            worst = gap;
                            println!(
                                "{label} {family} global {index} channel {channel}: analytic {value:.9e} numerical {numerical:.9e}"
                            );
                        }
                        assert!(
                            gap < 1e-4,
                            "{label} {family} global {index} channel {channel}: analytic {value:.9e} numerical {numerical:.9e} gap {gap:.3e}"
                        );
                        checked += 1;
                    }
                }
            }
        }
        println!("column acceleration worst relative gap {worst:.3e} over {checked} entries");
        assert!(checked > 2000, "only {checked} derivatives checked");
    }

    /// `Look::path_second` against the difference of the first derivative, at the
    /// fitted coefficients the column diagnostic uses.
    #[test]
    fn fitted_look_path_second_matches_differences() {
        let layout = Layout::new(1, 0, 1);
        let mut params = Params::initial(layout.clone(), 1);
        for (k, index) in layout.tone_beta.clone().enumerate() {
            params.theta[index] += 0.03 * (1 + k % 3) as f64;
        }
        for (k, index) in layout.tint_a.clone().enumerate() {
            params.theta[index] += 0.05 * ((k % 3) as f64 - 1.0);
        }
        for (k, index) in layout.tint_b.clone().enumerate() {
            params.theta[index] -= 0.04 * ((k % 2) as f64);
        }
        for range in [layout.g.clone(), layout.r.clone(), layout.lambda.clone()] {
            for (k, index) in range.enumerate() {
                params.theta[index] += 0.02 * (1 + k % 2) as f64;
            }
        }
        let look = params.look();
        let shift = |values: &LayerValues, index: usize, delta: f64| {
            let mut moved = *values;
            match index {
                0 => moved.t += delta,
                1 => moved.n_a += delta,
                2 => moved.n_b += delta,
                3 => moved.log_g += delta,
                4 => moved.r += delta,
                5 => moved.lambda += delta,
                other => moved.residual[other - 6] += delta,
            }
            moved
        };
        for xyz in [Xyz::new(0.34, 0.26, 0.13), Xyz::new(0.62, 0.5, 0.38)] {
            let input = LookInput::new(xyz);
            let values = look.layer_values(&input);
            let (outer, _) = look.path_second(&input, &values);
            let h = 1e-6;
            let mut worst = 0.0_f64;
            for k in 0..9usize {
                let plus = shift(&values, k, h);
                let minus = shift(&values, k, -h);
                let a = look.d_ucs_d_layer_values(&input, &plus);
                let b = look.d_ucs_d_layer_values(&input, &minus);
                for l in 0..9usize {
                    for channel in 0..3 {
                        let numerical = (a[channel][l] - b[channel][l]) / (2.0 * h);
                        let analytic = outer[k * 9 + l][channel];
                        let scale = numerical.abs().max(1e-6);
                        let gap = (analytic - numerical).abs() / scale;
                        if gap > worst {
                            worst = gap;
                            println!(
                                "t {:.6} k {k} l {l} channel {channel}: analytic {analytic:.9e} numerical {numerical:.9e}",
                                values.t
                            );
                        }
                    }
                }
            }
            println!(
                "fitted look path_second t {:.6} worst {worst:.3e}",
                values.t
            );
            assert!(worst < 1e-5, "worst relative gap {worst:.3e}");
        }
    }

    #[test]
    fn output_chain_matches_differences_at_a_clipped_point() {
        let space = RgbSpace::Srgb;
        let clip_k = 0.03;
        let pre = Ucs::new(80.78, 22.3, 8.49);
        let (xyz_pre, preclip) = looklab_model::clip::preclip_xyz_jacobian(pre);
        let path = path_state(space, xyz_pre, clip_k, preclip, |post| {
            cam::ucs_jacobian(post).1
        });
        let preclip_second = looklab_model::clip::preclip_xyz_second(pre);
        let output = looklab_model::look::ucs_xyz_second(path.xyz_post);
        // One layer direction in the pre-clip appearance coordinates.
        let mut first = [[0.0_f64; 9]; 3];
        first[0][0] = 1.0;
        // Zero, so the comparison isolates the composition terms; the `A''` term is a
        // linear map applied to `second` and is covered by the first-derivative check.
        let second = [[0.0_f64; 3]; 81];
        let (chain_first, chain) = preclip_chain(&path.preclip, &preclip_second, &first, &second);
        let chain_post = output_chain(space, &path, clip_k, &chain_first, &chain, &output);
        let composed = |appearance: Ucs| {
            let (xyz_pre, _) = looklab_model::clip::preclip_xyz_jacobian(appearance);
            let channels = space.xyz_to_rgb_linear(xyz_pre);
            let clipped = [channels.r, channels.g, channels.b]
                .map(|value| looklab_model::soft_clip(value, clip_k));
            let xyz_post = space.rgb_linear_to_xyz(Rgb::new(clipped[0], clipped[1], clipped[2]));
            looklab_model::solve::ucs_of_xyz(xyz_post)
        };
        let direction = [first[0][0], first[1][0], first[2][0]];
        let post = path.post.mul_vec(direction);
        let shifted = |step: f64| {
            let mut plus = pre.to_array();
            let mut minus = pre.to_array();
            for axis in 0..3 {
                plus[axis] += step * direction[axis];
                minus[axis] -= step * direction[axis];
            }
            (
                composed(Ucs::from_array(plus)).to_array(),
                composed(Ucs::from_array(minus)).to_array(),
            )
        };
        let (forward, backward) = shifted(1e-6);
        let base = composed(pre).to_array();
        for channel in 0..3 {
            let numerical = (forward[channel] - backward[channel]) / (2e-6);
            let analytic = post[channel];
            let scale = numerical.abs().max(1e-4);
            println!("first channel {channel}: analytic {analytic:.9e} numerical {numerical:.9e}");
            assert!(
                (analytic - numerical).abs() / scale < 1e-5,
                "first channel {channel}"
            );
        }
        // The second difference trades `eps·|p|/h²` against `h²·p''''`, so take the best
        // step rather than one fixed value.
        for channel in 0..3 {
            let analytic = chain_post[0][channel];
            let mut best = f64::INFINITY;
            for step in [1e-4, 3e-4, 1e-3] {
                let (forward, backward) = shifted(step);
                let numerical =
                    (forward[channel] - 2.0 * base[channel] + backward[channel]) / (step * step);
                let scale = numerical.abs().max(1e-3);
                best = best.min((analytic - numerical).abs() / scale);
            }
            println!("second channel {channel}: analytic {analytic:.9e} best gap {best:.3e}");
            assert!(best < 1e-4, "second channel {channel}: gap {best:.3e}");
        }
    }

    #[test]
    fn analytic_columns_match_central_differences() {
        let layout = Layout::new(2, 0, 1);
        let mut params = Params::initial(layout.clone(), 1);
        let mut state = 0x1234_5678_9abc_def0_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        for index in 0..layout.total {
            if layout.sigma.contains(&index) || layout.tone_beta.contains(&index) {
                continue;
            }
            params.theta[index] = 0.15 * (next() - 0.5);
        }
        for index in layout.tone_beta.clone() {
            params.theta[index] += 0.01 * (next() - 0.5);
        }
        params.theta[layout.clip] = crate::layout::zeta_from_clip(0.05);
        let columns = ColumnMap {
            image: Some(layout.image(0).start),
            mode: None,
            wb_r: Some(layout.image(0).start + 1),
            wb_b: Some(layout.image(0).start + 2),
            clip: Some(layout.clip),
            exposure_extra: None,
            wb_r_extra: None,
            wb_b_extra: None,
            clip_extra: None,
            delta: None,
            second: false,
            curvature: false,
            clip_geometry: ClipGeometry::Logistic,
        };
        let white = Xyz::new(0.950_455_927_051_671_6, 1.0, 1.089_057_750_759_878_4);
        let neutral_context = context(&params, Mat3::IDENTITY, RgbSpace::Srgb);
        let base = 0.18 / neutral_context.exposure_scale;
        let transition = [
            white.x * base / neutral_context.wb_r * 1.003,
            base,
            white.z * base / neutral_context.wb_b,
        ];
        let cases = [
            (white.scale(0.18).to_array(), Mat3::IDENTITY),
            (transition, Mat3::IDENTITY),
            ([0.2, 0.1, 0.6], Mat3::IDENTITY),
            ([0.9, 0.9, 0.9], Mat3::IDENTITY),
            ([0.02, 0.02, 0.02], Mat3::IDENTITY),
            ([0.2, 0.1, 0.6], RgbSpace::Srgb.to_xyz_matrix()),
        ];
        let mut checked = 0usize;
        for (rgb, transform) in cases {
            let mut jac = Jac::default();
            let _ = evaluate_sample(
                &params.look(),
                &layout,
                &context(&params, transform, RgbSpace::Srgb),
                &columns,
                rgb,
                params.clip_k(),
                params.clip_slope(),
                &slopes(&params),
                &[],
                &target(),
                &mut jac,
            );
            let analytic: Vec<(usize, [f64; 3])> = jac
                .cols
                .iter()
                .map(|index| *index as usize)
                .zip(jac.derivs.iter().copied())
                .collect();
            assert!(!analytic.is_empty());
            for (index, derivative) in analytic {
                let step = 1e-6 * params.theta[index].abs().max(1.0);
                let mut plus = params.clone();
                let mut minus = params.clone();
                plus.theta[index] += step;
                minus.theta[index] -= step;
                let a = residual(&plus, transform, RgbSpace::Srgb, rgb, &columns);
                let b = residual(&minus, transform, RgbSpace::Srgb, rgb, &columns);
                let numerical = [
                    (a[0] - b[0]) / (2.0 * step),
                    (a[1] - b[1]) / (2.0 * step),
                    (a[2] - b[2]) / (2.0 * step),
                ];
                for channel in 0..3 {
                    let scale = numerical[channel]
                        .abs()
                        .max(derivative[channel].abs())
                        .max(1e-3);
                    assert!(
                        (derivative[channel] - numerical[channel]).abs() / scale < 2e-3,
                        "rgb {rgb:?} index {index} channel {channel}: {} vs {} (g {} residual {} response {})",
                        derivative[channel],
                        numerical[channel],
                        layout.g.start,
                        layout.residual.start,
                        layout.response.start
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 500, "only {checked} derivatives checked");
    }

    #[test]
    fn neutral_internal_path_keeps_the_preclip_penalty_chain() {
        let layout = Layout::new(1, 0, 1);
        let mut params = Params::initial(layout.clone(), 1);
        let image = layout.image(0);
        params.theta[image.start] = 0.0;
        params.theta[image.start + 1] = 0.0;
        params.theta[image.start + 2] = 0.0;
        let white = looklab_model::solve::reference_white();
        let rgb = [white.x * 0.18, 0.18, white.z * 0.18];
        assert_eq!(LookInput::new(Xyz::from_array(rgb)).blend.w, 0.0);
        let mut jac = Jac::default();
        let out = evaluate_sample(
            &params.look(),
            &layout,
            &context(&params, Mat3::IDENTITY, RgbSpace::Srgb),
            &ColumnMap::default(),
            rgb,
            params.clip_k(),
            params.clip_slope(),
            &slopes(&params),
            &[],
            &target(),
            &mut jac,
        );
        let margin_at = |at: &Params| {
            let ctx = context(at, Mat3::IDENTITY, RgbSpace::Srgb);
            let cam = [
                rgb[0] * ctx.wb_r * ctx.exposure_scale,
                rgb[1] * ctx.exposure_scale,
                rgb[2] * ctx.wb_b * ctx.exposure_scale,
            ];
            let xyz = Xyz::from_array(ctx.transform.mul_vec(cam));
            let input = LookInput::new(xyz);
            let look = at.look();
            let values = look.layer_values(&input);
            let paths = look.chroma_paths(&input, &values);
            domain_penalty::evaluate(&values, &paths)
                .expect("perturbed point stays in the inverse domain")
                .value
        };
        let step = 1e-6;
        let check = |global: usize, label: &str| {
            let position = jac
                .cols
                .iter()
                .position(|column| *column as usize == global)
                .unwrap_or_else(|| panic!("no {label} column"));
            let mut plus = params.clone();
            let mut minus = params.clone();
            plus.theta[global] += step;
            minus.theta[global] -= step;
            let numerical = (margin_at(&plus) - margin_at(&minus)) / (2.0 * step);
            let penalty = out.penalty.expect("the neutral fixture is feasible");
            let analytic: f64 = (0..DOMAIN_AXES)
                .map(|axis| penalty.gradient[axis] * out.axis_terms[position][axis])
                .sum();
            assert!(
                numerical.is_finite() && analytic.is_finite(),
                "{label}: non-finite penalty derivative {analytic} vs {numerical}"
            );
            assert!(
                numerical.abs() > 1e-4,
                "{label} penalty derivative is not discriminating: {numerical:.3e}"
            );
            assert!(
                (analytic - numerical).abs() <= 1e-4 * numerical.abs(),
                "{label}: analytic {analytic:.9e} numerical {numerical:.9e}"
            );
        };
        check(layout.tone_c0, "tone c0");
        let (node, weight) = out
            .residual_indices
            .iter()
            .zip(out.residual_weights.iter())
            .fold((0usize, 0.0_f64), |best, (node, weight)| {
                if *weight > best.1 {
                    (*node, *weight)
                } else {
                    best
                }
            });
        assert!(
            weight > 1e-2,
            "largest residual basis weight is too small: {weight:.3e}"
        );
        check(layout.residual.start + node * 3, "residual J'");
        let mut second_jac = Jac::default();
        let with_second = evaluate_sample(
            &params.look(),
            &layout,
            &context(&params, Mat3::IDENTITY, RgbSpace::Srgb),
            &ColumnMap {
                second: true,
                ..ColumnMap::default()
            },
            rgb,
            params.clip_k(),
            params.clip_slope(),
            &slopes(&params),
            &[],
            &target(),
            &mut second_jac,
        );
        let order = with_second.second.as_ref().expect("second order");
        for values in &order.prediction {
            assert!(values.iter().all(|value| value.is_finite()));
        }
        let penalty = with_second
            .penalty
            .expect("the neutral fixture is feasible");
        assert!(penalty.value.is_finite());
        assert!(penalty.gradient.iter().all(|value| value.is_finite()));
        assert!(
            penalty
                .hessian
                .iter()
                .flatten()
                .all(|value| value.is_finite())
        );
        assert!(
            with_second
                .axis_terms
                .iter()
                .flatten()
                .all(|value| value.is_finite())
        );
    }

    #[test]
    fn wb_response_column_matches_the_ordinary_wb_column() {
        let layout = Layout::with_response(1, 0, 1, 1);
        let mut params = Params::initial(layout.clone(), 1);
        let image = layout.image(0);
        params.theta[image.start] = 0.0;
        params.theta[image.start + 1] = 0.0;
        params.theta[image.start + 2] = 0.0;
        let terms = [ResponseTerm {
            index: 0,
            phi: 1.0,
            component: ResponseComponent::WbR,
            feature: 0,
        }];
        let white = looklab_model::solve::reference_white();
        let neutral = 0.18;
        let rgb = [white.x * neutral * 1.003, neutral, white.z * neutral];
        let input = LookInput::new(Xyz::from_array(rgb));
        assert!(input.blend.w > 0.0 && input.blend.w < 1.0);
        let columns = ColumnMap {
            wb_r: Some(image.start + 1),
            ..ColumnMap::default()
        };
        let mut jac = Jac::default();
        let _ = evaluate_sample(
            &params.look(),
            &layout,
            &context(&params, Mat3::IDENTITY, RgbSpace::Srgb),
            &columns,
            rgb,
            params.clip_k(),
            params.clip_slope(),
            &slopes(&params),
            &terms,
            &target(),
            &mut jac,
        );
        let derivative = |global: usize| {
            let position = jac
                .cols
                .iter()
                .position(|column| *column as usize == global)
                .unwrap_or_else(|| panic!("no column {global}"));
            jac.derivs[position]
        };
        let ordinary = derivative(image.start + 1);
        let response = derivative(layout.response.start);
        for channel in 0..3 {
            let expected = terms[0].phi * ordinary[channel];
            let scale = expected.abs().max(response[channel].abs()).max(1e-6);
            assert!(
                response[channel].is_finite() && expected.is_finite(),
                "channel {channel}: non-finite response {} vs {expected}",
                response[channel]
            );
            assert!(
                (response[channel] - expected).abs() / scale < 1e-6,
                "channel {channel}: response {:.9e} ordinary*phi {expected:.9e}",
                response[channel]
            );
        }
    }

    #[test]
    fn wb_response_column_matches_residual_differences() {
        let layout = Layout::with_response(1, 0, 1, 1);
        let params = Params::initial(layout.clone(), 1);
        let image = layout.image(0);
        let terms = [ResponseTerm {
            index: 0,
            phi: 1.0,
            component: ResponseComponent::WbR,
            feature: 0,
        }];
        let white = looklab_model::solve::reference_white();
        let neutral = 0.18;
        let rgb = [white.x * neutral * 1.003, neutral, white.z * neutral];
        let input = LookInput::new(Xyz::from_array(rgb));
        assert!(input.blend.w > 0.0 && input.blend.w < 1.0);
        let response_value = params.theta[layout.response.start] * terms[0].phi;
        let context = |offset: f64| ImageCtx {
            transform: Mat3::IDENTITY,
            a0: Mat3::IDENTITY,
            space: RgbSpace::Srgb,
            exposure_scale: libm::pow(2.0, params.image(0)[0]),
            wb_r: libm::exp(params.image(0)[1] + response_value + offset),
            wb_b: libm::exp(params.image(0)[2]),
            tone_clamp: [ToneClamp::Interior; TONE_COEFS],
        };
        let columns = ColumnMap {
            wb_r: Some(image.start + 1),
            ..ColumnMap::default()
        };
        let mut jac = Jac::default();
        let _ = evaluate_sample(
            &params.look(),
            &layout,
            &context(0.0),
            &columns,
            rgb,
            params.clip_k(),
            params.clip_slope(),
            &slopes(&params),
            &terms,
            &target(),
            &mut jac,
        );
        let position = jac
            .cols
            .iter()
            .position(|column| *column as usize == layout.response.start)
            .expect("response column");
        let analytic = jac.derivs[position];
        let step = 1e-6;
        let mut scratch = Jac::default();
        let mut residual_at = |offset: f64| {
            evaluate_sample(
                &params.look(),
                &layout,
                &context(offset),
                &ColumnMap::default(),
                rgb,
                params.clip_k(),
                params.clip_slope(),
                &slopes(&params),
                &terms,
                &target(),
                &mut scratch,
            )
            .residual
        };
        let plus = residual_at(step);
        let minus = residual_at(-step);
        for channel in 0..3 {
            let numerical = (plus[channel] - minus[channel]) / (2.0 * step);
            let scale = numerical.abs().max(analytic[channel].abs()).max(1e-6);
            assert!(
                (numerical - analytic[channel]).abs() / scale < 1e-5,
                "channel {channel}: analytic {:.9e} numerical {numerical:.9e}",
                analytic[channel]
            );
        }
    }

    #[test]
    fn white_balance_along_the_exposure_axis_matches_the_exposure_column() {
        let layout = Layout::new(1, 0, 1);
        let mut params = Params::initial(layout.clone(), 1);
        for index in layout.tone_beta.clone() {
            params.theta[index] = 0.5;
        }
        let image = layout.image(0);
        let white = looklab_model::solve::reference_white();
        // Column 0 of the transform is the reference white, so a `wb_R` move scales
        // XYZ uniformly and must equal an exposure move in `log2` units.
        let transform = Mat3([[white.x, 0.0, 0.0], [1.0, 0.0, 0.0], [white.z, 0.0, 1.0]]);
        let scale = 0.18 * libm::pow(2.0, -12.5);
        let rgb = [scale, 0.0, 0.0];
        let xyz = Xyz::from_array(transform.mul_vec(rgb));
        assert_eq!(LookInput::new(xyz).blend.w, 0.0);
        let columns = ColumnMap {
            image: Some(image.start),
            wb_r: Some(image.start + 1),
            ..ColumnMap::default()
        };
        let mut jac = Jac::default();
        let _ = evaluate_sample(
            &params.look(),
            &layout,
            &context(&params, transform, RgbSpace::Srgb),
            &columns,
            rgb,
            params.clip_k(),
            params.clip_slope(),
            &slopes(&params),
            &[],
            &target(),
            &mut jac,
        );
        let derivative = |global: usize| {
            let position = jac
                .cols
                .iter()
                .position(|column| *column as usize == global)
                .unwrap_or_else(|| panic!("no column {global}"));
            jac.derivs[position]
        };
        let exposure = derivative(image.start);
        let white_balance = derivative(image.start + 1);
        assert!(exposure.iter().any(|value| value.abs() > 1e-6));
        for channel in 0..3 {
            let expected = exposure[channel] / core::f64::consts::LN_2;
            let scale = expected.abs().max(1e-6);
            assert!(
                white_balance[channel].is_finite() && expected.is_finite(),
                "channel {channel}: non-finite wb derivative {} vs {expected}",
                white_balance[channel]
            );
            assert!(
                (white_balance[channel] - expected).abs() / scale < 1e-5,
                "channel {channel}: wb {:.9e} exposure/ln2 {expected:.9e}",
                white_balance[channel]
            );
        }
    }

    fn next_up(value: f64) -> f64 {
        if value.is_nan() || value == f64::INFINITY {
            return value;
        }
        if value == 0.0 {
            return f64::from_bits(1);
        }
        let bits = value.to_bits();
        if value > 0.0 {
            f64::from_bits(bits + 1)
        } else {
            f64::from_bits(bits - 1)
        }
    }

    fn next_down(value: f64) -> f64 {
        if value.is_nan() || value == f64::NEG_INFINITY {
            return value;
        }
        if value == 0.0 {
            return -f64::from_bits(1);
        }
        let bits = value.to_bits();
        if value > 0.0 {
            f64::from_bits(bits - 1)
        } else {
            f64::from_bits(bits + 1)
        }
    }

    fn tint_value(weights: &[f64; 4], coefficients: &[f64; 4]) -> f64 {
        let mut out = 0.0;
        for (weight, coefficient) in weights.iter().zip(coefficients.iter()) {
            out += weight * coefficient;
        }
        out
    }

    fn exact_tint(weights: &[f64; 4], ucs: f64, residual: f64) -> Option<(usize, f64)> {
        let mut order: Vec<usize> = (0..4).collect();
        order.sort_by(|left, right| weights[*right].abs().total_cmp(&weights[*left].abs()));
        for slot in order {
            let weight = weights[slot];
            if weight == 0.0 {
                continue;
            }
            let initial = -(ucs + residual) / weight;
            let mut up = initial;
            let mut down = initial;
            for _ in 0..4096 {
                for candidate in [up, down] {
                    let mut coefficients = [0.0_f64; 4];
                    coefficients[slot] = candidate;
                    let n = tint_value(weights, &coefficients);
                    if (n + ucs) + residual == 0.0 {
                        return Some((slot, candidate));
                    }
                }
                up = next_up(up);
                down = next_down(down);
            }
        }
        None
    }

    fn residual_value(weights: &[f64], coefficients: &[f64]) -> f64 {
        let mut out = 0.0;
        for (weight, coefficient) in weights.iter().zip(coefficients.iter()) {
            out += weight * coefficient;
        }
        out
    }

    fn exact_residual(weights: &[f64], base: f64) -> Option<(usize, f64)> {
        let mut order: Vec<usize> = (0..weights.len()).collect();
        order.sort_by(|left, right| weights[*right].abs().total_cmp(&weights[*left].abs()));
        for slot in order {
            let weight = weights[slot];
            if weight == 0.0 {
                continue;
            }
            let initial = -base / weight;
            let mut up = initial;
            let mut down = initial;
            for _ in 0..4096 {
                for candidate in [up, down] {
                    let mut coefficients = vec![0.0_f64; weights.len()];
                    coefficients[slot] = candidate;
                    let value = residual_value(weights, &coefficients);
                    if base + value == 0.0 {
                        return Some((slot, candidate));
                    }
                }
                up = next_up(up);
                down = next_down(down);
            }
        }
        None
    }

    pub(crate) fn fitter_crossing(
        space: RgbSpace,
        clip_k: f64,
        target_j: f64,
    ) -> (Layout, Params, ColumnMap, [f64; 3], Target, usize) {
        let layout = Layout::new(1, 0, 1);
        let image = layout.image(0);
        let mut params = Params::initial(layout.clone(), 1);
        params.theta[image.start] = 0.0;
        params.theta[image.start + 1] = 0.0;
        params.theta[image.start + 2] = 0.0;
        params.theta[layout.clip] = crate::layout::zeta_from_clip(clip_k);
        let rgb = [0.45, 0.30, 0.15];
        let input = LookInput::new(Xyz::from_array(rgb));
        let look = params.look();
        let values = look.layer_values(&input);
        let paths = look.chroma_paths(&input, &values);
        let lambda_offset = input.grl.s_idx[0];
        let mut one = params.clone();
        one.theta[layout.lambda.start + lambda_offset] = 1.0;
        let look_one = one.look();
        let values_one = look_one.layer_values(&input);
        let lambda_weight = values_one.lambda - values.lambda;
        assert!(
            lambda_weight.abs() > 1e-9,
            "the lambda plane weight is {lambda_weight}"
        );
        params.theta[layout.lambda.start + lambda_offset] =
            (target_j - paths.ucs.j) / lambda_weight;
        let look = params.look();
        let values = look.layer_values(&input);
        let paths = look.chroma_paths(&input, &values);
        let (indices, weights, _) = looklab_model::basis::tint_weights(values.t);

        let mut tint_a = [0.0_f64; TINT_COEFS];
        let mut tint_b = [0.0_f64; TINT_COEFS];
        let exact_a = exact_tint(&weights, paths.ucs.a, values.residual[1]);
        let (slot_a, coefficient_a) = match exact_a {
            Some(pair) => pair,
            None => {
                let slot = (0..4)
                    .max_by(|left, right| weights[*left].abs().total_cmp(&weights[*right].abs()))
                    .expect("tint weights");
                (slot, -(paths.ucs.a + values.residual[1]) / weights[slot])
            }
        };
        tint_a[indices[slot_a]] = coefficient_a;
        if exact_a.is_none() {
            let active = [
                tint_a[indices[0]],
                tint_a[indices[1]],
                tint_a[indices[2]],
                tint_a[indices[3]],
            ];
            let base = tint_value(&weights, &active) + paths.ucs.a;
            if let Some((node_slot, coefficient)) = exact_residual(&input.residual.weights, base) {
                let node = input.residual.indices[node_slot];
                params.theta[layout.residual.start + node * 3 + 1] = coefficient;
            }
        }
        let exact_b = exact_tint(&weights, paths.ucs.b, values.residual[2]);
        let (slot_b, coefficient_b) = match exact_b {
            Some(pair) => pair,
            None => {
                let slot = (0..4)
                    .max_by(|left, right| weights[*left].abs().total_cmp(&weights[*right].abs()))
                    .expect("tint weights");
                (slot, -(paths.ucs.b + values.residual[2]) / weights[slot])
            }
        };
        tint_b[indices[slot_b]] = coefficient_b;
        if exact_b.is_none() {
            let active = [
                tint_b[indices[0]],
                tint_b[indices[1]],
                tint_b[indices[2]],
                tint_b[indices[3]],
            ];
            let base = tint_value(&weights, &active) + paths.ucs.b;
            if let Some((node_slot, coefficient)) = exact_residual(&input.residual.weights, base) {
                let node = input.residual.indices[node_slot];
                params.theta[layout.residual.start + node * 3 + 2] = coefficient;
            }
        }
        for (index, coefficient) in tint_a.iter().enumerate() {
            params.theta[layout.tint_a.start + index] = *coefficient;
        }
        for (index, coefficient) in tint_b.iter().enumerate() {
            params.theta[layout.tint_b.start + index] = *coefficient;
        }

        let look = params.look();
        let values = look.layer_values(&input);
        let paths = look.chroma_paths(&input, &values);
        if paths.ucs.a != 0.0 || paths.ucs.b != 0.0 {
            let point = composite::evaluate(paths.ucs, space, clip_k);
            panic!(
                "exact pre-clip zero not reached: pre {:?} post {:?} regularity {:?}",
                (paths.ucs.a, paths.ucs.b),
                point.as_ref().map(|point| (point.value.a, point.value.b)),
                point.as_ref().map(|point| point.derivative),
            );
        }
        assert!(paths.blend > 0.0, "the fitter outer path is inactive");
        let columns = ColumnMap {
            image: Some(image.start),
            wb_r: Some(image.start + 1),
            wb_b: Some(image.start + 2),
            clip: Some(layout.clip),
            clip_geometry: ClipGeometry::Logistic,
            ..ColumnMap::default()
        };
        let target = Target::new(Ucs::new(52.0, 4.0, -3.0));
        let tint_index = layout.tint_a.start + indices[slot_a];
        (layout, params, columns, rgb, target, tint_index)
    }

    #[test]
    fn clipped_output_neutral_crossing_is_c1_with_a_curvature_jump() {
        let clip_k = 0.05;
        for space in [RgbSpace::Srgb, RgbSpace::AdobeRgb] {
            for j in [8.0_f64, 99.5] {
                let pre = Ucs::new(j, 0.0, 0.0);
                let point = composite::evaluate(pre, space, clip_k).expect("neutral pre");
                let CompositeDerivative::Finite {
                    jacobian,
                    regularity,
                    ..
                } = point.derivative
                else {
                    panic!("neutral pre has no finite composite derivative");
                };
                let CompositeRegularity::NeutralCrossing { clip_active, .. } = regularity else {
                    panic!("neutral pre is not a neutral crossing: {regularity:?}");
                };
                assert!(clip_active, "the neutral fixture must clip at j {j}");

                let slope = jacobian[1][1];
                for h in [1e-3_f64, 1e-4, 1e-5] {
                    let at = |offset: f64| {
                        composite::evaluate(Ucs::new(j, offset, 0.0), space, clip_k)
                            .expect("perturbed pre is feasible")
                            .value
                            .a
                    };
                    let base = point.value.a;
                    let right = (at(h) - base) / h;
                    let left = (base - at(-h)) / h;
                    let value_scale = base.abs().max(at(h).abs()).max(at(-h).abs());
                    let roundoff = 64.0 * f64::EPSILON * (value_scale / h + slope.abs().max(1.0));
                    let allowed = LIPSCHITZ_ENVELOPE * h / 2.0 + roundoff;
                    let difference_allowed = LIPSCHITZ_ENVELOPE * h + roundoff;
                    assert!(
                        (right - slope).abs() <= allowed,
                        "j {j} h {h:e} right {right:.9e} slope {slope:.9e} allowed {allowed:.9e}"
                    );
                    assert!(
                        (left - slope).abs() <= allowed,
                        "j {j} h {h:e} left {left:.9e} slope {slope:.9e} allowed {allowed:.9e}"
                    );
                    assert!(
                        (right - left).abs() <= difference_allowed,
                        "j {j} h {h:e} one-sided gradients differ: {right:.9e} vs {left:.9e} allowed {difference_allowed:.9e}"
                    );
                    let right_curvature = (at(2.0 * h) - 2.0 * at(h) + base) / (h * h);
                    let left_curvature = (base - 2.0 * at(-h) + at(-2.0 * h)) / (h * h);
                    assert!(
                        (right_curvature - left_curvature).abs() > 1e-9,
                        "j {j} h {h:e}: the one-sided curvature did not jump"
                    );
                }

                let (layout, params, columns, rgb, target, tint_index) =
                    fitter_crossing(space, clip_k, j);
                let sample = |params: &Params| {
                    let look = params.look();
                    let mut jac = Jac::default();
                    let out = evaluate_sample(
                        &look,
                        &layout,
                        &context(params, Mat3::IDENTITY, space),
                        &columns,
                        rgb,
                        params.clip_k(),
                        params.clip_slope(),
                        &slopes(params),
                        &[],
                        &target,
                        &mut jac,
                    );
                    let gradient = jac
                        .cols
                        .iter()
                        .position(|column| *column as usize == tint_index)
                        .map(|position| jac.derivs[position])
                        .expect("the tint column is present");
                    (gradient, out.residual, out.crossing)
                };
                let (base_gradient, base_residual, base_crossing) = sample(&params);
                assert!(
                    base_crossing,
                    "the exact fitter base is not the crossing branch"
                );
                assert!(
                    base_gradient.iter().all(|value| value.is_finite()),
                    "the base production gradient is not finite: {base_gradient:?}"
                );
                let mut unit = vec![0.0_f64; layout.total];
                unit[tint_index] = 1.0;
                let steps = [1e-2_f64, 1e-3, 1e-4, 1e-5, 1e-6];
                let largest = steps[0];
                let (plus, _, _) = sample(&perturb(&params, &unit, largest));
                let (minus, _, _) = sample(&perturb(&params, &unit, -largest));
                let gradient_scale = plus
                    .iter()
                    .chain(minus.iter())
                    .fold(0.0_f64, |largest, value| largest.max(value.abs()));
                let roundoff = 64.0 * f64::EPSILON * gradient_scale;
                let mut change_scale = 0.0_f64;
                for &h in &steps {
                    let (gradient_plus, residual_plus, _) = sample(&perturb(&params, &unit, h));
                    let (gradient_minus, residual_minus, _) = sample(&perturb(&params, &unit, -h));
                    let secant_right = [
                        (residual_plus[0] - base_residual[0]) / h,
                        (residual_plus[1] - base_residual[1]) / h,
                        (residual_plus[2] - base_residual[2]) / h,
                    ];
                    let secant_left = [
                        (base_residual[0] - residual_minus[0]) / h,
                        (base_residual[1] - residual_minus[1]) / h,
                        (base_residual[2] - residual_minus[2]) / h,
                    ];
                    let base_change = [
                        (gradient_plus[0] - base_gradient[0]).abs(),
                        (gradient_plus[1] - base_gradient[1]).abs(),
                        (gradient_plus[2] - base_gradient[2]).abs(),
                        (gradient_minus[0] - base_gradient[0]).abs(),
                        (gradient_minus[1] - base_gradient[1]).abs(),
                        (gradient_minus[2] - base_gradient[2]).abs(),
                    ];
                    let envelope = LIPSCHITZ_ENVELOPE * h + roundoff;
                    for component in 0..6 {
                        assert!(
                            base_change[component] <= envelope,
                            "j {j} h {h:e} component {component}: gradient change {:.9e} exceeds the finite Lipschitz envelope {envelope:.9e}",
                            base_change[component]
                        );
                    }
                    for channel in 0..3 {
                        assert!(
                            (gradient_plus[channel] - secant_right[channel]).abs() <= envelope,
                            "j {j} h {h:e} channel {channel}: right gradient {:.9e} vs value secant {:.9e} (envelope {envelope:.9e})",
                            gradient_plus[channel],
                            secant_right[channel]
                        );
                        assert!(
                            (gradient_minus[channel] - secant_left[channel]).abs() <= envelope,
                            "j {j} h {h:e} channel {channel}: left gradient {:.9e} vs value secant {:.9e} (envelope {envelope:.9e})",
                            gradient_minus[channel],
                            secant_left[channel]
                        );
                        change_scale = change_scale
                            .max(base_change[channel])
                            .max(base_change[channel + 3]);
                    }
                }
                assert!(
                    change_scale > 1e-9,
                    "the fitter residual gradient did not change across the crossing"
                );
            }
        }
    }

    #[test]
    fn saturated_neutral_clip_chain_is_finite_and_floored() {
        let pre = Ucs::new(116.0, 0.0, 0.0);
        let clip_k = 0.03;
        let channels = output_channels(pre, RgbSpace::Srgb);
        let clipped = channels.map(|value| soft_clip(value, clip_k));
        let xyz = RgbSpace::Srgb.rgb_linear_to_xyz(Rgb::new(clipped[0], clipped[1], clipped[2]));
        let (_, naive) = cam::ucs_jacobian(xyz);
        assert!(
            naive.iter().flatten().any(|value| !value.is_finite()),
            "the naive clip chain is finite, so the test does not reach the fallback"
        );
        let chain = clip_jacobian(pre, RgbSpace::Srgb, clip_k);
        assert!(
            chain.0.iter().flatten().all(|value| value.is_finite()),
            "saturated neutral clip Jacobian is not finite: {:?}",
            chain.0
        );
        assert!(
            clip_k_gradient(pre, RgbSpace::Srgb, clip_k)
                .iter()
                .all(|value| value.is_finite())
        );
        let pre_covariance = [[4.0, 0.0, 0.0], [0.0, 4.0, 0.0], [0.0, 0.0, 4.0]];
        let mut post = chain.mul(&Mat3(pre_covariance)).mul(&chain.transpose()).0;
        crate::uncertainty::floor_post_covariance(&pre_covariance, &mut post);
        let trace = post[0][0] + post[1][1] + post[2][2];
        let floor = 0.0625 * 12.0;
        assert!(
            (trace - floor).abs() < 1e-9,
            "post trace {trace} does not reach the documented floor {floor}"
        );
        let flags = crate::uncertainty::mark_support(
            trace.sqrt(),
            0.0,
            1.0,
            3,
            0.0,
            false,
            false,
            &crate::types::FitOptions::default(),
        );
        assert!(flags.sigma, "the floored probe was forced unsupported");
    }
}
