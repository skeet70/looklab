// SPDX-License-Identifier: AGPL-3.0-or-later
//! Forward prediction and its analytic Jacobian for one sample.

use core::f64::consts::PI;
use looklab_color::ucs::{Ucs, UcsSpace, ucs_to_jmh};
use looklab_color::{Mat3, Rgb, RgbSpace, Xyz};
use looklab_model::basis::{S_COEFS, TINT_COEFS, TONE_COEFS};
use looklab_model::look::RESIDUAL_DOMAIN;
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
    /// `2^exposure`.
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
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Jac {
    pub cols: Vec<u32>,
    pub derivs: Vec<[f64; 3]>,
}

impl Jac {
    pub fn clear(&mut self) {
        self.cols.clear();
        self.derivs.clear();
    }

    fn push(&mut self, index: usize, derivative: [f64; 3]) {
        self.cols.push(index as u32);
        self.derivs.push(derivative);
    }

    fn apply(&mut self, matrix: &Mat3) {
        for derivative in &mut self.derivs {
            *derivative = matrix.mul_vec(*derivative);
        }
    }
}

pub(crate) struct EvalOutput {
    pub prediction: Ucs,
    pub residual_weights: [f64; 8],
    pub residual_indices: [usize; 8],
    pub clip_jacobian: Mat3,
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

fn mat_add(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut out = Mat3([[0.0; 3]; 3]);
    for row in 0..3 {
        for column in 0..3 {
            out.0[row][column] = a.0[row][column] + b.0[row][column];
        }
    }
    out
}

fn scale_mat(m: &Mat3, k: f64) -> Mat3 {
    Mat3(m.0.map(|row| row.map(|value| value * k)))
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

fn tone_slope(look: &Look, s: f64) -> f64 {
    match look.tone.betas() {
        Some(beta) => {
            let derivative = basis::tone_basis_derivative(s);
            (0..TONE_COEFS)
                .map(|k| beta[k] * derivative[k])
                .sum::<f64>()
        }
        None => {
            let h = 1e-6;
            (looklab_model::neutral_j_prime(0.18 * libm::pow(2.0, s + h))
                - looklab_model::neutral_j_prime(0.18 * libm::pow(2.0, s - h)))
                / (2.0 * h)
        }
    }
}

/// `(d/ds, d/dh, d/dC)` of one chroma/hue/lightness component at `input`.
fn grl_terms(
    component: &GrlComponent,
    input: &LookInput,
    s_derivative: &[f64; 4],
    hue_derivative: &[f64; 4],
    c_derivative: &[f64; 4],
) -> (f64, f64, f64) {
    let weights = &input.grl;
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
    let mut dh = 0.0;
    for h in 0..4 {
        for s in 0..4 {
            let base = (weights.hue_idx[h] * S_COEFS + weights.s_idx[s]) * 3;
            for c in 1..4 {
                let node = base + (c - 1);
                ds += weights.hue_w[h] * s_derivative[s] * weights.c_w[c] * component.volume[node];
                dh += hue_derivative[h] * weights.s_w[s] * weights.c_w[c] * component.volume[node];
                dc += weights.hue_w[h] * weights.s_w[s] * c_derivative[c] * component.volume[node];
            }
        }
    }
    (ds, dh, dc)
}

fn residual_terms(
    look: &Look,
    input: &LookInput,
    du: &[f64; 8],
    dv: &[f64; 8],
    dw: &[f64; 8],
) -> [[f64; 3]; 3] {
    let span_s = RESIDUAL_DOMAIN[0][1] - RESIDUAL_DOMAIN[0][0];
    let span_a = RESIDUAL_DOMAIN[1][1] - RESIDUAL_DOMAIN[1][0];
    let span_b = RESIDUAL_DOMAIN[2][1] - RESIDUAL_DOMAIN[2][0];
    let mut out = [[0.0_f64; 3]; 3];
    for (m, node) in input.residual.indices.iter().enumerate() {
        for channel in 0..3 {
            let value = look.residual.values[node * 3 + channel];
            out[0][channel] += du[m] * value / span_s;
            out[1][channel] += dv[m] * value / span_a;
            out[2][channel] += dw[m] * value / span_b;
        }
    }
    out
}

fn push_grl(jac: &mut Jac, start: usize, input: &LookInput, d: [f64; 3]) {
    let weights = &input.grl;
    for s in 0..4 {
        jac.push(
            start + weights.s_idx[s],
            scale(d, weights.c_w[0] * weights.s_w[s]),
        );
    }
    for h in 0..4 {
        for s in 0..4 {
            let base = (weights.hue_idx[h] * S_COEFS + weights.s_idx[s]) * 3;
            for c in 1..4 {
                let weight = weights.hue_w[h] * weights.s_w[s] * weights.c_w[c];
                jac.push(start + S_COEFS + base + c - 1, scale(d, weight));
            }
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
    let (tint_indices, tint_weights, _) = basis::tint_values(values.t);
    let d_na = column(d_layer, 1);
    let d_nb = column(d_layer, 2);
    let (na_slope, nb_slope) = look.d_tint_d_t(values.t);
    let d_t = add(
        add(column(d_layer, 0), scale(d_na, na_slope)),
        scale(d_nb, nb_slope),
    );
    jac.push(layout.tone_c0, d_t);
    for k in 0..TONE_COEFS {
        let weight = match beta_slope {
            Some(slope) => input.tone_weights[k + 1] * slope[k],
            None => input.tone_weights[k + 1],
        };
        jac.push(layout.tone_beta.start + k, scale(d_t, weight));
    }
    for m in 0..4 {
        jac.push(
            layout.tint_a.start + tint_indices[m],
            scale(d_na, tint_weights[m]),
        );
        jac.push(
            layout.tint_b.start + tint_indices[m],
            scale(d_nb, tint_weights[m]),
        );
    }
    push_grl(jac, layout.g.start, input, column(d_layer, 3));
    push_grl(jac, layout.r.start, input, column(d_layer, 4));
    push_grl(jac, layout.lambda.start, input, column(d_layer, 5));
    for (m, node) in input.residual.indices.iter().enumerate() {
        let weight = input.residual.weights[m];
        for channel in 0..3 {
            jac.push(
                layout.residual.start + node * 3 + channel,
                [
                    d_layer[0][6 + channel] * weight,
                    d_layer[1][6 + channel] * weight,
                    d_layer[2][6 + channel] * weight,
                ],
            );
        }
    }
}

/// Predicts one stored sample and fills `jac` with `d J'a'b' / dθ`.
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
    let pre = look.eval_from_layer_values(&input, &values);
    let d_layer = look.d_output_d_layer_values(&input, &values);

    let model = reference_model();
    let xyz0 = model.jmh_to_xyz(ucs_to_jmh(pre, UcsSpace::Ucs));
    let channels = output_channels(pre, image.space);
    let clipped = channels.map(|value| soft_clip(value, clip_k));
    let xyz1 = image
        .space
        .rgb_linear_to_xyz(Rgb::new(clipped[0], clipped[1], clipped[2]));
    let prediction = ucs_of_xyz(xyz1);
    let from_xyz = image.space.from_xyz_matrix();
    let to_xyz = image.space.to_xyz_matrix();

    let unclipped = channels
        .iter()
        .all(|value| *value >= clip_k && *value <= 1.0 - clip_k);
    let (clip_jacobian, dk) = if unclipped {
        (Mat3::IDENTITY, [0.0; 3])
    } else {
        let (_, j0) = cam::ucs_jacobian(xyz0);
        let (_, j1) = cam::ucs_jacobian(xyz1);
        let per_channel = Mat3::diag([
            clip_derivative(channels[0], clip_k),
            clip_derivative(channels[1], clip_k),
            clip_derivative(channels[2], clip_k),
        ]);
        let chain = Mat3(j1).mul(&to_xyz).mul(&per_channel).mul(&from_xyz);
        let jacobian = match Mat3(j0).inverse() {
            Some(inverse) => chain.mul(&inverse),
            None => Mat3::IDENTITY,
        };
        let dk = Mat3(j1).mul(&to_xyz).mul_vec([
            clip_k_derivative(channels[0], clip_k),
            clip_k_derivative(channels[1], clip_k),
            clip_k_derivative(channels[2], clip_k),
        ]);
        (jacobian, dk)
    };

    push_look_columns(
        jac,
        look,
        layout,
        &input,
        &values,
        &d_layer,
        Some(beta_slope),
    );

    let s_derivative = basis::s_values(input.s).2;
    let (_, _, hue_derivative) = basis::hue_values(input.h_in);
    let (_, _, c_derivative) = basis::c_values(input.c_in);
    let dt_ds = tone_slope(look, input.s);
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
    let coords = basis::residual_coords(input.s, input.a_in, input.b_in);
    let (residual_indices, residual_weights, [du, dv, dw]) =
        basis::trilinear(coords[0], coords[1], coords[2]);
    let dres = residual_terms(look, &input, &du, &dv, &dw);

    let (na_slope, nb_slope) = look.d_tint_d_t(values.t);
    let d_t = add(
        add(column(&d_layer, 0), scale(column(&d_layer, 1), na_slope)),
        scale(column(&d_layer, 2), nb_slope),
    );

    let raw_s = libm::log(xyz.y / 0.18) / core::f64::consts::LN_2;
    let s_active = raw_s == input.s;
    let mut d_out_ds = scale(d_t, if s_active { dt_ds } else { 0.0 });
    d_out_ds = add(d_out_ds, scale(column(&d_layer, 3), g.0));
    d_out_ds = add(d_out_ds, scale(column(&d_layer, 4), r.0));
    d_out_ds = add(d_out_ds, scale(column(&d_layer, 5), lambda.0));
    for channel in 0..3 {
        d_out_ds = add(
            d_out_ds,
            scale(column(&d_layer, 6 + channel), dres[0][channel]),
        );
    }
    if let Some(index) = columns.image {
        jac.push(index, d_out_ds);
    }
    if let Some(index) = columns.mode {
        jac.push(index, d_out_ds);
    }
    if let Some(index) = columns.exposure_extra {
        jac.push(index, d_out_ds);
    }

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
    let d_out_dxyz = needs_xyz.then(|| {
        d_out_d_xyz(
            &input,
            &values,
            &d_layer,
            na_slope,
            nb_slope,
            [g, r, lambda],
            dres,
            dt_ds,
            xyz,
            s_active,
        )
    });
    let wb_derivative = |component: usize| {
        d_out_dxyz.as_ref().map(|jacobian| {
            jacobian.mul_vec([
                image.transform.0[0][component] * cam[component],
                image.transform.0[1][component] * cam[component],
                image.transform.0[2][component] * cam[component],
            ])
        })
    };
    if let Some(index) = columns.wb_r
        && let Some(derivative) = wb_derivative(0)
    {
        jac.push(index, derivative);
    }
    if let Some(index) = columns.wb_b
        && let Some(derivative) = wb_derivative(2)
    {
        jac.push(index, derivative);
    }
    if let Some(index) = columns.wb_r_extra
        && let Some(derivative) = wb_derivative(0)
    {
        jac.push(index, derivative);
    }
    if let Some(index) = columns.wb_b_extra
        && let Some(derivative) = wb_derivative(2)
    {
        jac.push(index, derivative);
    }
    if let (Some(indices), Some(jacobian)) = (columns.delta, d_out_dxyz.as_ref()) {
        for (index, slot) in indices.iter().enumerate() {
            let column = image
                .a0
                .mul_vec(looklab_body::Delta6::basis(index).mul_vec(cam));
            jac.push(*slot, jacobian.mul_vec(column));
        }
    }
    if !terms.is_empty() {
        push_response_columns(
            jac,
            look,
            layout,
            &input,
            &values,
            &d_layer,
            terms,
            &wb_derivative,
        );
    }

    jac.apply(&clip_jacobian);
    if let Some(index) = columns.clip {
        jac.push(index, scale(dk, clip_slope));
    }
    if let Some(index) = columns.clip_extra {
        jac.push(index, scale(dk, clip_slope));
    }

    EvalOutput {
        prediction,
        residual_weights,
        residual_indices,
        clip_jacobian,
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
    let mut sum = weights.c_w[0]
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
                sum += weights.hue_w[h]
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
    na_slope: f64,
    nb_slope: f64,
    grl: [(f64, f64, f64); 3],
    dres: [[f64; 3]; 3],
    dt_ds: f64,
    xyz: Xyz,
    s_active: bool,
) -> Mat3 {
    if input.y <= 0.0 {
        return Mat3([[0.0; 3]; 3]);
    }
    let y_ref = looklab_model::solve_y_ref(values.t, input.y);
    let (_, reference_jacobian) = cam::ucs_jacobian(input.unit_xyz.scale(y_ref));
    let inv_y = 1.0 / input.y;
    let d_unit = Mat3([
        [inv_y, -xyz.x / (input.y * input.y), 0.0],
        [0.0, 0.0, 0.0],
        [0.0, 0.0, inv_y],
    ]);
    let d_reference = scale_mat(&Mat3(reference_jacobian).mul(&d_unit), y_ref);

    let gain = libm::exp(values.log_g);
    let radians = values.r * (PI / 180.0);
    let (sin_r, cos_r) = (libm::sin(radians), libm::cos(radians));
    let d_output = Mat3([
        [1.0, 0.0, 0.0],
        [0.0, gain * cos_r, -gain * sin_r],
        [0.0, gain * sin_r, gain * cos_r],
    ]);
    let mut out = d_output.mul(&d_reference);

    let (_, chromaticity) = cam::ucs_jacobian(input.unit_xyz.scale(0.18));
    let da_du = scale(
        [chromaticity[1][0], chromaticity[1][1], chromaticity[1][2]],
        0.18,
    );
    let db_du = scale(
        [chromaticity[2][0], chromaticity[2][1], chromaticity[2][2]],
        0.18,
    );
    let denominator = input.a_in * input.a_in + input.b_in * input.b_in;
    let dh_du = if denominator == 0.0 {
        [0.0; 3]
    } else {
        let k = (180.0 / PI) / denominator;
        add(scale(da_du, -input.b_in * k), scale(db_du, input.a_in * k))
    };
    let dc_du = if input.c_in == 0.0 {
        [0.0; 3]
    } else {
        let k = 1.0 / input.c_in;
        add(scale(da_du, input.a_in * k), scale(db_du, input.b_in * k))
    };
    let dh_dxyz = row_mat(dh_du, &d_unit);
    let dc_dxyz = row_mat(dc_du, &d_unit);
    let da_dxyz = row_mat(da_du, &d_unit);
    let db_dxyz = row_mat(db_du, &d_unit);
    let ds_dxyz = [
        0.0,
        if s_active {
            1.0 / (input.y * core::f64::consts::LN_2)
        } else {
            0.0
        },
        0.0,
    ];

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
        let (ds, dh, dc) = grl[component];
        accumulate(
            index,
            add(
                scale(ds_dxyz, ds),
                add(scale(dh_dxyz, dh), scale(dc_dxyz, dc)),
            ),
        );
    }
    for channel in 0..3 {
        accumulate(
            6 + channel,
            add(
                scale(ds_dxyz, dres[0][channel]),
                add(
                    scale(da_dxyz, dres[1][channel]),
                    scale(db_dxyz, dres[2][channel]),
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

/// Post-clip prediction without the Jacobian, for samples outside the Hessian set.
pub(crate) fn predict_only(look: &Look, image: &ImageCtx, rgb: [f64; 3], clip_k: f64) -> Ucs {
    let exposure_scale = image.exposure_scale;
    let cam = [
        rgb[0] * image.wb_r * exposure_scale,
        rgb[1] * exposure_scale,
        rgb[2] * image.wb_b * exposure_scale,
    ];
    let xyz = Xyz::from_array(image.transform.mul_vec(cam));
    let input = LookInput::new(xyz);
    let values = look.layer_values(&input);
    let pre = look.eval_from_layer_values(&input, &values);
    let channels = output_channels(pre, image.space);
    let clipped = channels.map(|value| soft_clip(value, clip_k));
    ucs_of_xyz(
        image
            .space
            .rgb_linear_to_xyz(Rgb::new(clipped[0], clipped[1], clipped[2])),
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
    let model = reference_model();
    let xyz0 = model.jmh_to_xyz(ucs_to_jmh(pre, UcsSpace::Ucs));
    let clipped = channels.map(|value| soft_clip(value, clip_k));
    let xyz1 = space.rgb_linear_to_xyz(Rgb::new(clipped[0], clipped[1], clipped[2]));
    let (_, j0) = cam::ucs_jacobian(xyz0);
    let (_, j1) = cam::ucs_jacobian(xyz1);
    let per_channel = Mat3::diag([
        clip_derivative(channels[0], clip_k),
        clip_derivative(channels[1], clip_k),
        clip_derivative(channels[2], clip_k),
    ]);
    let chain = Mat3(j1)
        .mul(&space.to_xyz_matrix())
        .mul(&per_channel)
        .mul(&space.from_xyz_matrix());
    match Mat3(j0).inverse() {
        Some(inverse) => chain.mul(&inverse),
        None => Mat3::IDENTITY,
    }
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

    #[allow(clippy::too_many_arguments)]
    fn prediction(
        params: &Params,
        transform: Mat3,
        space: RgbSpace,
        rgb: [f64; 3],
        columns: &ColumnMap,
    ) -> Ucs {
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
            &mut jac,
        )
        .prediction
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
        };
        let white = Xyz::new(0.950_455_927_051_671_6, 1.0, 1.089_057_750_759_878_4);
        let cases = [
            (white.scale(0.18).to_array(), Mat3::IDENTITY),
            ([0.2, 0.1, 0.6], Mat3::IDENTITY),
            ([0.9, 0.9, 0.9], Mat3::IDENTITY),
            ([0.02, 0.02, 0.02], Mat3::IDENTITY),
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
                let a = prediction(&plus, transform, RgbSpace::Srgb, rgb, &columns);
                let b = prediction(&minus, transform, RgbSpace::Srgb, rgb, &columns);
                let numerical = [
                    (a.j - b.j) / (2.0 * step),
                    (a.a - b.a) / (2.0 * step),
                    (a.b - b.b) / (2.0 * step),
                ];
                for channel in 0..3 {
                    let scale = numerical[channel]
                        .abs()
                        .max(derivative[channel].abs())
                        .max(1e-3);
                    assert!(
                        (derivative[channel] - numerical[channel]).abs() / scale < 2e-3,
                        "rgb {rgb:?} index {index} channel {channel}: {} vs {}",
                        derivative[channel],
                        numerical[channel]
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 500, "only {checked} derivatives checked");
    }
}
