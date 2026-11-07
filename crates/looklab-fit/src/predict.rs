// SPDX-License-Identifier: AGPL-3.0-or-later
//! Forward prediction and its analytic Jacobian for one sample.

use core::f64::consts::PI;
use looklab_color::ucs::{Ucs, UcsSpace, ucs_to_jmh};
use looklab_color::{Mat3, Rgb, RgbSpace, Xyz};
use looklab_model::basis::{S_COEFS, TINT_COEFS, TONE_COEFS};
use looklab_model::realization::{LayerKind, ResponseComponent, feature_coefficient};
use looklab_model::solve::{reference_model, ucs_of_xyz};
use looklab_model::{GrlComponent, LayerValues, Look, LookInput, soft_clip};

use crate::basis;
use crate::cam;
use crate::layout::Layout;

pub(crate) struct ImageCtx {
    pub transform: Mat3,
    /// Prior mean `A0`; `transform = a0·(I + Δ)`.
    pub a0: Mat3,
    pub space: RgbSpace,
    /// `2^exposure`, shared by every sample of the image.
    pub exposure_scale: f64,
    pub wb_r: f64,
    pub wb_b: f64,
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
    /// Also return the second derivatives of the prediction along the layer values.
    pub second: bool,
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
    /// Layer value the column moves; `None` when the column mixes layers.
    pub layers: Vec<Option<Layer>>,
    /// `∂(t, n_a, n_b)/∂θ` in the first three slots and `∂layer/∂θ` of `layers` in
    /// the fourth.
    pub layer_weights: Vec<[f64; 4]>,
    /// `∂²t/∂θ²` of a tone column, `∂w_m/∂t` of a tint column, zero otherwise.
    pub layer_slopes: Vec<f64>,
}

impl Jac {
    pub fn clear(&mut self) {
        self.cols.clear();
        self.derivs.clear();
        self.layers.clear();
        self.layer_weights.clear();
        self.layer_slopes.clear();
    }

    fn push(&mut self, index: usize, derivative: [f64; 3]) {
        self.push_layer_slope(index, derivative, None, [0.0; 4], 0.0);
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
        self.cols.push(index as u32);
        self.derivs.push(derivative);
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
    /// Pre-clip domain margin; `<= 0` is infeasible.
    pub margin: f64,
    /// `d margin / dθ`, aligned with `jac.cols`.
    pub margin_terms: Vec<f64>,
    /// Layer-value second derivatives, present when [`ColumnMap::second`] is set.
    pub second: Option<Box<SecondOrder>>,
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

/// One chroma path through the output clip.
struct PathState {
    xyz_pre: Xyz,
    xyz_post: Xyz,
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
        xyz_pre,
        xyz_post,
        channels,
        preclip: Mat3(preclip),
        clip,
        rows,
        post: rows.mul(&clip).mul(&Mat3(preclip)),
        dk: rows.mul_vec(dk),
    }
}

fn xyz_channels(space: RgbSpace, xyz: Xyz) -> [f64; 3] {
    let rgb = space.xyz_to_rgb_linear(xyz);
    [rgb.r, rgb.g, rgb.b]
}

/// Second derivatives of the prediction over the nine layer values.
pub(crate) struct SecondOrder {
    /// `∂²p_c/∂l_k∂l_l` of the blended post-clip output, indexed `k * 9 + l`.
    pub prediction: [[f64; 81]; 3],
    /// `∂²margin/∂l_k∂l_l`.
    pub margin: [f64; 81],
    /// `∂p/∂l_k` of the blended post-clip output.
    pub gradient: [[f64; 3]; 9],
    /// `∇margin · ∂(pre-clip J'a'b')/∂l_k` for the tone and tint layers.
    pub margin_value: [f64; 3],
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

fn clip_derivative(x: f64, k: f64) -> f64 {
    if x > 1.0 - k || x < k {
        let u = if x > 1.0 - k {
            (x - (1.0 - k)) / k
        } else {
            (k - x) / k
        };
        let t = libm::tanh(u);
        1.0 - t * t
    } else {
        1.0
    }
}

/// `∂ soft_clip(x, k) / ∂k`, zero on the identity segment.
fn clip_k_derivative(x: f64, k: f64) -> f64 {
    if x > 1.0 - k {
        let t = libm::tanh((x - (1.0 - k)) / k);
        let du = (1.0 - x) / (k * k);
        -1.0 + t + k * (1.0 - t * t) * du
    } else if x < k {
        let t = libm::tanh((k - x) / k);
        let du = x / (k * k);
        1.0 - t - k * (1.0 - t * t) * du
    } else {
        0.0
    }
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
    push_grl(jac, layout.r.start, Layer::Rotation, input, column(d_layer, 4));
    push_grl(jac, layout.lambda.start, Layer::Lambda, input, column(d_layer, 5));
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
    let paths = look.chroma_paths(&input, &values);
    let w = paths.blend;
    let near = 1.0 - w;
    let space = image.space;

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

    let ucs_path = (w > 0.0).then(|| {
        let (xyz_pre, preclip) = looklab_model::clip::preclip_xyz_jacobian(paths.ucs);
        path_state(space, xyz_pre, clip_k, preclip, |post| {
            cam::ucs_jacobian(post).1
        })
    });
    let int_path = (near > 0.0).then(|| {
        let (xyz_pre, preclip) =
            looklab_model::clip::preclip_internal_xyz_jacobian(paths.internal, paths.ucs.j);
        path_state(space, xyz_pre, clip_k, preclip, |post| {
            let (_, cam_jacobian) = cam::ucs_jacobian(post);
            let (_, opponent) = looklab_model::look::neutral_chroma_jacobian(post);
            [cam_jacobian[0], opponent[0], opponent[1]]
        })
    });

    let ucs_post = ucs_path.as_ref().map(|path| ucs_of_xyz(path.xyz_post));
    let internal_post = int_path.as_ref().map(|path| {
        let chroma = looklab_model::look::neutral_chroma_jacobian(path.xyz_post).0;
        ([chroma.a, chroma.b], ucs_of_xyz(path.xyz_post))
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

    let (margin, margin_gradient) = looklab_model::look::prediction_margin(paths.ucs);
    let second = columns.second.then(|| {
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
            let output = looklab_model::look::ucs_xyz_second(path.xyz_post);
            let post = output_chain(space, path, clip_k, &first, &chain, &output);
            (first, chain, post)
        });
        let inner = int_path.as_ref().map(|path| {
            let preclip = looklab_model::clip::preclip_internal_xyz_second(paths.internal, paths.ucs.j);
            let (first, chain) = preclip_chain(&path.preclip, &preclip, &d_layer_int, &int_second);
            let ucs = looklab_model::look::ucs_xyz_second(path.xyz_post);
            let neutral = looklab_model::look::neutral_chroma_second(path.xyz_post);
            let mut output = [[0.0_f64; 3]; 6];
            for (slot, value) in output.iter_mut().enumerate() {
                *value = [ucs[slot][0], neutral[slot][0], neutral[slot][1]];
            }
            let post = output_chain(space, path, clip_k, &first, &chain, &output);
            (first, chain, post)
        });
        let mut prediction = [[0.0_f64; 81]; 3];
        let mut gradient = [[0.0_f64; 3]; 9];
        let mut margin = [0.0_f64; 81];
        if let (Some(path), Some((first, chain, post))) = (&ucs_path, &outer) {
            for (index, value) in post.iter().enumerate() {
                for channel in 0..3 {
                    prediction[channel][index] += w * value[channel];
                }
            }
            for (k, value) in gradient.iter_mut().enumerate() {
                let column = [d_layer_ucs[0][k], d_layer_ucs[1][k], d_layer_ucs[2][k]];
                *value = scale(path.post.mul_vec(column), w);
            }
            let (margin_gradient, margin_hessian) =
                looklab_model::look::prediction_margin_xyz_derivatives(paths.ucs, path.xyz_pre);
            for k in 0..9 {
                for l in 0..9 {
                    let index = k * 9 + l;
                    let mut value = dot3(chain[index], margin_gradient);
                    for a in 0..3 {
                        for b in 0..3 {
                            value += margin_hessian[a][b] * first[k][a] * first[l][b];
                        }
                    }
                    margin[index] = value;
                }
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
                *value = add(*value, scale(path.post.mul_vec(column), near));
            }
        }
        let mut margin_value = [0.0_f64; 3];
        for (k, value) in margin_value.iter_mut().enumerate() {
            *value = d_layer_ucs[0][k] * margin_gradient[0]
                + d_layer_ucs[1][k] * margin_gradient[1]
                + d_layer_ucs[2][k] * margin_gradient[2];
        }
        let tint_curvature = {
            let (a, b) = look.d2_tint_d_t2(values.t);
            [a, b]
        };
        Box::new(SecondOrder {
            prediction,
            margin,
            gradient,
            margin_value,
            tint_curvature,
            tint_slope: [na_slope, nb_slope],
        })
    });

    let y_ref = looklab_model::solve_y_ref(values.t, input.y);
    let (_, ucs_reference) = looklab_model::look::ucs_reference_jacobian(input.unit_xyz, y_ref);
    let (_, internal_reference) =
        looklab_model::look::neutral_chroma_jacobian(input.unit_xyz.scale(y_ref));
    let j_gradient = looklab_model::look::j_prime_xyz_gradient(input.unit_xyz.scale(y_ref));
    let needs_xyz = columns.wb_r.is_some()
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
            &d_layer_ucs,
            ucs_reference,
            j_gradient,
            na_slope,
            nb_slope,
            [g, r, lambda],
            dres,
            dt_ds,
            xyz,
            lookup_slope,
        )
    });
    let d_int_xyz = (needs_xyz && near > 0.0).then(|| {
        d_out_d_xyz(
            &input,
            &values,
            &d_layer_int,
            internal_reference,
            j_gradient,
            na_slope,
            nb_slope,
            [g, r, lambda],
            dres,
            dt_ds,
            xyz,
            lookup_slope,
        )
    });
    let xyz_derivative = |component: usize| {
        [
            image.transform.0[0][component] * cam[component],
            image.transform.0[1][component] * cam[component],
            image.transform.0[2][component] * cam[component],
        ]
    };

    let mut jac_ucs = Jac::default();
    if w > 0.0 {
        push_look_columns(
            &mut jac_ucs,
            look,
            layout,
            &input,
            &values,
            &d_layer_ucs,
            Some(beta_slope),
        );
    }
    let mut jac_int = Jac::default();
    if near > 0.0 {
        push_look_columns(
            &mut jac_int,
            look,
            layout,
            &input,
            &values,
            &d_layer_int,
            Some(beta_slope),
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
    if !terms.is_empty() {
        if w > 0.0 {
            push_response_columns(
                &mut jac_ucs,
                look,
                layout,
                &input,
                &values,
                &d_layer_ucs,
                terms,
                &wb_ucs,
            );
        }
        if near > 0.0 {
            push_response_columns(
                &mut jac_int,
                look,
                layout,
                &input,
                &values,
                &d_layer_int,
                terms,
                &wb_int,
            );
        }
    }

    let mut margin_terms: Vec<f64> = if w > 0.0 {
        jac_ucs
            .derivs
            .iter()
            .map(|derivative| dot3(*derivative, margin_gradient))
            .collect()
    } else {
        vec![0.0; jac_int.cols.len()]
    };
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
                jac.push_layer_slope(
                    *column as usize,
                    add(scale(*outer, w), scale(*inner, near)),
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
                jac.push_layer_slope(
                    *column as usize,
                    *derivative,
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
                jac.push_layer_slope(
                    *column as usize,
                    *derivative,
                    jac_int.layers[position],
                    jac_int.layer_weights[position],
                    jac_int.layer_slopes[position],
                );
            }
        }
        (None, None) => {}
    }

    let d_out_ds_ucs = (w > 0.0).then(|| {
        appearance_slope(
            &d_layer_ucs,
            na_slope,
            nb_slope,
            [g, r, lambda],
            dres,
            dt_ds,
        )
    });
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
            scale(outer.post.mul_vec(d_out_ds_ucs.unwrap_or([0.0; 3])), w),
            scale(inner.post.mul_vec(d_out_ds_int.unwrap_or([0.0; 3])), near),
        ),
        (Some(outer), None) => outer.post.mul_vec(d_out_ds_ucs.unwrap_or([0.0; 3])),
        (None, Some(inner)) => inner.post.mul_vec(d_out_ds_int.unwrap_or([0.0; 3])),
        (None, None) => [0.0; 3],
    };
    let ds_margin = d_out_ds_ucs
        .map(|derivative| dot3(derivative, margin_gradient))
        .unwrap_or(0.0);
    for index in [columns.image, columns.mode, columns.exposure_extra]
        .into_iter()
        .flatten()
    {
        jac.push(index, ds_column);
        margin_terms.push(ds_margin);
    }

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
    let nuisance = |dxyz: [f64; 3]| -> ([f64; 3], f64) {
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
        let margin_term = d_ucs_xyz
            .as_ref()
            .map(|jacobian| dot3(jacobian.mul_vec(dxyz), margin_gradient))
            .unwrap_or(0.0);
        (derivative, margin_term)
    };
    for (column, component) in [
        (columns.wb_r, 0usize),
        (columns.wb_b, 2),
        (columns.wb_r_extra, 0),
        (columns.wb_b_extra, 2),
    ] {
        if let Some(index) = column {
            let (derivative, term) = nuisance(xyz_derivative(component));
            jac.push(index, derivative);
            margin_terms.push(term);
        }
    }
    if let Some(indices) = columns.delta {
        for (index, slot) in indices.iter().enumerate() {
            let dxyz = image
                .a0
                .mul_vec(looklab_body::Delta6::basis(index).mul_vec(cam));
            let (derivative, term) = nuisance(dxyz);
            jac.push(*slot, derivative);
            margin_terms.push(term);
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
        jac.push(index, scale(dk, clip_slope));
        margin_terms.push(0.0);
    }

    EvalOutput {
        prediction,
        residual,
        residual_weights,
        residual_indices,
        clip_jacobian,
        margin,
        margin_terms,
        second,
    }
}
#[allow(clippy::too_many_arguments)]
fn push_response_columns(
    jac: &mut Jac,
    look: &Look,
    layout: &Layout,
    input: &LookInput,
    values: &LayerValues,
    d_layer: &[[f64; 9]; 3],
    terms: &[ResponseTerm],
    wb_derivative: &impl Fn(usize) -> Option<[f64; 3]>,
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
    for term in terms {
        let index = layout.response.start + term.index;
        match term.component {
            ResponseComponent::Tone => {
                if look.tone.is_colorimetric() {
                    continue;
                }
                let weight = if term.feature == 0 {
                    1.0
                } else {
                    input.tone_weights[term.feature as usize]
                };
                jac.push(index, scale(d_t, term.phi * weight));
            }
            ResponseComponent::Tint => {
                let feature = term.feature as usize;
                let (column, weight) = if feature < TINT_COEFS {
                    (d_na, tint_weight(feature))
                } else {
                    (d_nb, tint_weight(feature - TINT_COEFS))
                };
                jac.push(index, scale(column, term.phi * weight));
            }
            ResponseComponent::Chroma | ResponseComponent::Hue | ResponseComponent::Lightness => {
                let layer = term.component.layer().unwrap_or(LayerKind::Chroma);
                let column_index = match layer {
                    LayerKind::Chroma => 3,
                    LayerKind::Hue => 4,
                    _ => 5,
                };
                let weight = grl_response_weight(input, layer, term.feature);
                jac.push(
                    index,
                    scale(column(d_layer, column_index), term.phi * weight),
                );
            }
            ResponseComponent::WbR => {
                if let Some(derivative) = wb_derivative(0) {
                    jac.push(index, scale(derivative, term.phi));
                }
            }
            ResponseComponent::WbB => {
                if let Some(derivative) = wb_derivative(2) {
                    jac.push(index, scale(derivative, term.phi));
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
    lookup_slope: f64,
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
    let ds_dxyz = [0.0, lookup_slope / (input.y * core::f64::consts::LN_2), 0.0];

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
/// Hessian set, and its pre-clip domain margin.
pub(crate) fn predict_only(
    look: &Look,
    image: &ImageCtx,
    rgb: [f64; 3],
    clip_k: f64,
    target: &Target,
) -> (Ucs, [f64; 3], f64) {
    let exposure_scale = image.exposure_scale;
    let cam = [
        rgb[0] * image.wb_r * exposure_scale,
        rgb[1] * exposure_scale,
        rgb[2] * image.wb_b * exposure_scale,
    ];
    let xyz = Xyz::from_array(image.transform.mul_vec(cam));
    let input = LookInput::new(xyz);
    let values = look.layer_values(&input);
    let paths = look.chroma_paths(&input, &values);
    let w = paths.blend;
    let near = 1.0 - w;
    let space = image.space;
    let ucs_post = (w > 0.0).then(|| {
        let channels = output_channels(paths.ucs, space);
        let clipped = channels.map(|value| soft_clip(value, clip_k));
        ucs_of_xyz(space.rgb_linear_to_xyz(Rgb::new(clipped[0], clipped[1], clipped[2])))
    });
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
    (
        prediction,
        residual,
        looklab_model::look::prediction_margin(paths.ucs).0,
    )
}

/// Below 0.25 the pre-clip value is unidentifiable.
pub(crate) fn clip_derivative_min(pre: Ucs, space: RgbSpace, clip_k: f64) -> f64 {
    output_channels(pre, space)
        .iter()
        .map(|value| clip_derivative(*value, clip_k))
        .fold(1.0_f64, f64::min)
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
    Mat3(j1).mul(&space.to_xyz_matrix()).mul_vec([
        clip_k_derivative(channels[0], clip_k),
        clip_k_derivative(channels[1], clip_k),
        clip_k_derivative(channels[2], clip_k),
    ])
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
    Mat3(j1)
        .mul(&space.to_xyz_matrix())
        .mul(&per_channel)
        .mul(&space.from_xyz_matrix())
        .mul(&Mat3(d_xyz0))
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
mod tests {
    use super::*;
    use crate::layout::{Params, softplus_slope};
    use looklab_color::RgbSpace;

    fn context(params: &Params, transform: Mat3, space: RgbSpace) -> ImageCtx {
        let nuisance = params.image(0);
        ImageCtx {
            transform,
            a0: transform,
            space,
            exposure_scale: libm::pow(2.0, nuisance[0]),
            wb_r: libm::exp(nuisance[1]),
            wb_b: libm::exp(nuisance[2]),
        }
    }

    fn slopes(params: &Params) -> [f64; TONE_COEFS] {
        let mut slope = [0.0; TONE_COEFS];
        for (k, value) in slope.iter_mut().enumerate() {
            *value = softplus_slope(params.theta[params.layout.tone_beta.start + k]);
        }
        slope
    }

    fn target() -> Target {
        Target::new(Ucs::new(55.0, 0.5, -0.3))
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
        assert_eq!(jac.derivs.len(), evaluated.margin_terms.len());
        for derivative in &jac.derivs {
            for value in derivative {
                assert!(value.is_finite(), "{derivative:?}");
            }
        }
        for value in &evaluated.margin_terms {
            assert!(value.is_finite(), "{value}");
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
            .map(|channel| out.residual[channel] * (forward[channel] - backward[channel]) / (2.0 * step))
            .sum::<f64>();
        let value = |k: usize| {
            (0..3)
                .map(|channel| out.residual[channel] * order.gradient[k][channel])
                .sum::<f64>()
        };
        let mut analytic =
            value(1) * order.tint_curvature[0] + value(2) * order.tint_curvature[1];
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
                            (left - right).abs()
                                <= 1e-9 * left.abs().max(right.abs()) + 1e-12,
                            "{label} symmetry {k} {l} channel {channel}: {left:.9e} vs {right:.9e}"
                        );
                    }
                }
            }
            let families: [(&str, [f64; 9]); 3] = [
                ("tone", [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
                ("rotation", [0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0]),
                ("tone-rotation", [0.5, 0.3, -0.2, 0.0, 0.9, 0.0, 0.0, 0.0, 0.0]),
            ];
            for (family, dl) in families {
                let direction = layer_direction(&layout, &jac, &dl);
                let expected = expected_acceleration(&jac, order, &dl, &direction);
                let step = std::env::var("FIT_FD_STEP").ok().and_then(|v| v.parse().ok()).unwrap_or(1e-6);
                let mut plus = params.clone();
                let mut minus = params.clone();
                for (index, value) in direction.iter().enumerate() {
                    plus.theta[index] += step * value;
                    minus.theta[index] -= step * value;
                }
                let mut scratch = Jac::default();
                let mut evaluate = |at: &Params| {
                    let evaluated = evaluate_sample(
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
                    (
                        out,
                        evaluated.second.expect("second order").gradient,
                        evaluated.prediction,
                    )
                };
                let (forward, gradient_plus, prediction_plus) = evaluate(&plus);
                let (backward, gradient_minus, prediction_minus) = evaluate(&minus);
                let mut analytic = vec![[0.0_f64; 3]; layout.total];
                for (index, (column, _)) in jac.cols.iter().zip(jac.derivs.iter()).enumerate() {
                    for channel in 0..3 {
                        analytic[*column as usize][channel] += expected[index][channel];
                    }
                }
                if label == "clipped" && family == "tone-rotation" {
                    for probe in [0usize, 30usize, 1usize] {
                        let (position, _) = jac
                            .cols
                            .iter()
                            .enumerate()
                            .find(|(_, column)| **column as usize == probe)
                            .expect("probe column");
                        let weights = jac.layer_weights[position];
                        println!(
                            "probe {probe}: layer {:?} weights {weights:?} slope {} expected {:?}",
                            jac.layers[position], jac.layer_slopes[position], expected[position]
                        );
                        println!(
                            "   T[1,0] {:?} T[1,4] {:?} grad1 {:?}",
                            order.prediction[0][1 * 9],
                            order.prediction[0][1 * 9 + 4],
                            order.gradient[1]
                        );
                    }
                }
                if label == "bright" && family == "tone" {
                    {
                        let probe_xyz = {
                            let ctx = context(&params, Mat3::IDENTITY, RgbSpace::Srgb);
                            let cam = [
                                rgb[0] * ctx.wb_r * ctx.exposure_scale,
                                rgb[1] * ctx.exposure_scale,
                                rgb[2] * ctx.wb_b * ctx.exposure_scale,
                            ];
                            Xyz::from_array(ctx.transform.mul_vec(cam))
                        };
                        let post_at = |at: &Params| {
                            let input = LookInput::new(probe_xyz);
                            let values = at.look().layer_values(&input);
                            let paths = at.look().chroma_paths(&input, &values);
                            let a0 = at.look().d_ucs_d_layer_values(&input, &values);
                            let a00 = at.look().path_second(&input, &values).0[0];
                            let (xyz_pre, preclip) = looklab_model::clip::preclip_xyz_jacobian(paths.ucs);
                            let ch = RgbSpace::Srgb.xyz_to_rgb_linear(xyz_pre);
                            let rgb = [ch.r, ch.g, ch.b];
                            let clipped = rgb.map(|v| looklab_model::soft_clip(v, params.clip_k()));
                            let xyz_post = RgbSpace::Srgb.rgb_linear_to_xyz(Rgb::new(clipped[0], clipped[1], clipped[2]));
                            let rows = cam::ucs_jacobian(xyz_post).1;
                            let clip = if rgb.iter().all(|v| *v >= params.clip_k() && *v <= 1.0 - params.clip_k()) {
                                Mat3::IDENTITY
                            } else {
                                RgbSpace::Srgb.to_xyz_matrix()
                                    .mul(&Mat3::diag(rgb.map(|v| clip_derivative(v, params.clip_k()))))
                                    .mul(&RgbSpace::Srgb.from_xyz_matrix())
                            };
                            let post = Mat3(rows).mul(&clip).mul(&Mat3(preclip));
                            (post, [a0[0][0], a0[1][0], a0[2][0]], a00, rgb, xyz_post)
                        };
                        let hh = 1e-6;
                        let mut plus = params.clone();
                        let mut minus = params.clone();
                        plus.theta[layout.tone_c0] += hh;
                        minus.theta[layout.tone_c0] -= hh;
                        let (post, a0, a00, rgb, xyz_post) = post_at(&params);
                        let (post_p, _, _, _, _) = post_at(&plus);
                        let (post_m, _, _, _, _) = post_at(&minus);
                        println!("  channels {rgb:?} xyz_post {xyz_post:?}");
                        let mut fd = [0.0_f64; 3];
                        let mut plain = [0.0_f64; 3];
                        for row in 0..3 {
                            let mut first = 0.0;
                            for c in 0..3 {
                                first += post.0[row][c] * a00[c];
                            }
                            let mut second = 0.0;
                            for c in 0..3 {
                                second += (post_p.0[row][c] - post_m.0[row][c]) / (2.0 * hh) * a0[c];
                            }
                            plain[row] = first;
                            fd[row] = first + second;
                        }
                        println!("  composed plain {plain:?} with FD(post) {fd:?}");
                        println!("  analytic T[k][0] {:?} {:?} {:?}", order.prediction[0][0], order.prediction[0][9], order.prediction[0][18]);
                    }
                    let (c0, _) = jac
                        .cols
                        .iter()
                        .enumerate()
                        .find(|(_, column)| **column as usize == layout.tone_c0)
                        .expect("c0");
                    let big = 1e-3;
                    let mut far_plus = params.clone();
                    let mut far_minus = params.clone();
                    for (index, value) in direction.iter().enumerate() {
                        far_plus.theta[index] += big * value;
                        far_minus.theta[index] -= big * value;
                    }
                    let at = |at: &Params| {
                        let mut scratch = Jac::default();
                        let evaluated = evaluate_sample(
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
                        evaluated.prediction
                    };
                    let a = at(&far_plus);
                    let b = at(&far_minus);
                    let fd2 = [
                        (a.j - 2.0 * out.prediction.j + b.j) / (big * big),
                        (a.a - 2.0 * out.prediction.a + b.a) / (big * big),
                        (a.b - 2.0 * out.prediction.b + b.b) / (big * big),
                    ];
                    println!(
                        "  c0 expected {:.9e} {:.9e} {:.9e} prediction FD2 {:.9e} {:.9e} {:.9e}",
                        expected[c0][0], expected[c0][1], expected[c0][2], fd2[0], fd2[1], fd2[2]
                    );
                }
                for index in 0..layout.total {
                    for channel in 0..3 {
                        let numerical = (forward[index][channel] - backward[index][channel])
                            / (2.0 * step);
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
}
