// SPDX-License-Identifier: AGPL-3.0-or-later

use core::f64::consts::PI;
use core::ops::Range;
use looklab_color::Xyz;
use looklab_color::ucs::Ucs;
use std::collections::BTreeMap;
use std::sync::LazyLock;

use crate::basis::{
    CORE_COEFS, CORE_TERMS, RESIDUAL_COEFS, S_COEFS, TINT_COEFS, TONE_COEFS, ToneBasis,
    VOLUME_COEFS, c_weights, core_weights, hue_weights, outer_weight, s_weights, tint_weights,
    trilinear,
};
use crate::clip::ClipSettings;
use crate::deriv::ReferenceCam16;
use crate::error::{ModelError, Result};
use crate::solve;
use crate::tone::Tone;

pub const GRL_COEFS: usize = S_COEFS + VOLUME_COEFS + CORE_COEFS;

/// Residual grid bounds in `(s, a_in, b_in)`; v1 fixes them.
pub const RESIDUAL_DOMAIN: [[f64; 2]; 3] = [[-12.0, 6.0], [-80.0, 80.0], [-80.0, 80.0]];

static TONE_BASIS: LazyLock<ToneBasis> = LazyLock::new(ToneBasis::new);
static REFERENCE_CAM16: LazyLock<ReferenceCam16> = LazyLock::new(ReferenceCam16::new);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GrlWeights {
    pub hue_idx: [usize; 4],
    pub hue_w: [f64; 4],
    pub s_idx: [usize; 4],
    pub s_w: [f64; 4],
    pub c_idx: [usize; 4],
    pub c_w: [f64; 4],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResidualWeights {
    pub indices: [usize; 8],
    pub weights: [f64; 8],
}

/// Near-neutral blend weights at the input chroma.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlendWeights {
    pub w: f64,
    pub dw_dc: f64,
}

impl BlendWeights {
    pub fn new(c: f64) -> Self {
        let (w, dw_dc) = outer_weight(c);
        Self { w, dw_dc }
    }

    pub fn core(self) -> f64 {
        1.0 - self.w
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LookInput {
    pub xyz: Xyz,
    pub unit_xyz: Xyz,
    pub y: f64,
    pub s: f64,
    pub h_in: f64,
    pub c_in: f64,
    pub a_in: f64,
    pub b_in: f64,
    pub blend: BlendWeights,
    pub tone_weights: [f64; TONE_COEFS + 1],
    pub grl: GrlWeights,
    pub residual: ResidualWeights,
}

/// Input chromaticity with `Y = 1`, and its scene exposure.
fn chromaticity_input(xyz: Xyz) -> (f64, f64, Xyz) {
    if xyz.y > 0.0 {
        (
            xyz.y,
            libm::log(xyz.y / 0.18) / core::f64::consts::LN_2,
            Xyz::new(xyz.x / xyz.y, 1.0, xyz.z / xyz.y),
        )
    } else {
        (xyz.y, -12.0, solve::reference_white())
    }
}

/// Whether CAM16's forward model is finite for this input chromaticity.
fn cam16_chromaticity_defined(unit_xyz: Xyz) -> bool {
    // `x / y` overflows for a subnormal `y`.
    let ucs = crate::deriv::reference_cam16().ucs(unit_xyz.scale(0.18));
    ucs.to_array().iter().all(|value| value.is_finite())
}

/// D65 neutral at luminance `luminance`.
fn neutral_xyz(luminance: f64) -> Xyz {
    let white = solve::reference_white();
    Xyz::new(
        white.x * luminance,
        white.y * luminance,
        white.z * luminance,
    )
}

/// Contract an input outside CAM16's forward domain toward the D65 neutral at
/// the same luminance until it is inside (docs/02 §3).
fn contract_to_cam16(xyz: Xyz) -> Xyz {
    let luminance = xyz.y.max(0.0);
    let neutral = neutral_xyz(luminance);
    let mut inside = 0.0;
    let mut outside = 1.0;
    for _ in 0..32 {
        let middle = 0.5 * (inside + outside);
        let candidate = Xyz::new(
            neutral.x + middle * (xyz.x - neutral.x),
            luminance,
            neutral.z + middle * (xyz.z - neutral.z),
        );
        if cam16_chromaticity_defined(chromaticity_input(candidate).2) {
            inside = middle;
        } else {
            outside = middle;
        }
    }
    Xyz::new(
        neutral.x + inside * (xyz.x - neutral.x),
        luminance,
        neutral.z + inside * (xyz.z - neutral.z),
    )
}

/// A look input inside CAM16's forward domain, and the substitution when it was not.
impl LookInput {
    pub fn new(xyz: Xyz) -> Self {
        let (y, s, unit_xyz) = chromaticity_input(xyz);
        let chromaticity = crate::deriv::reference_cam16().ucs(unit_xyz.scale(0.18));
        let (xyz, y, s, unit_xyz, chromaticity) = if y > 0.0
            && chromaticity
                .to_array()
                .iter()
                .all(|value| value.is_finite())
        {
            (xyz, y, s, unit_xyz, chromaticity)
        } else {
            // Negative or zero Y has no chromaticity; its neutral is black.
            let contracted = if y > 0.0 {
                contract_to_cam16(xyz)
            } else {
                neutral_xyz(0.0)
            };
            let (y, s, unit_xyz) = chromaticity_input(contracted);
            let chromaticity = crate::deriv::reference_cam16().ucs(unit_xyz.scale(0.18));
            (contracted, y, s, unit_xyz, chromaticity)
        };
        let s = s.clamp(-12.0, 6.0);
        let c_in = (chromaticity.a * chromaticity.a + chromaticity.b * chromaticity.b).sqrt();
        let h_in = if c_in > 0.0 {
            let degrees = libm::atan2(chromaticity.b, chromaticity.a) * (180.0 / PI);
            if degrees < 0.0 {
                degrees + 360.0
            } else {
                degrees
            }
        } else {
            0.0
        };

        let mut tone_weights = [0.0_f64; TONE_COEFS + 1];
        tone_weights[0] = 1.0;
        tone_weights[1..].copy_from_slice(&TONE_BASIS.values(s));

        let (hue_idx, hue_w) = hue_weights(h_in);
        let (s_idx, s_w) = s_weights(s);
        let (c_idx, c_w) = c_weights(c_in);

        let normalize = |value: f64, bounds: [f64; 2]| {
            ((value - bounds[0]) / (bounds[1] - bounds[0])).clamp(0.0, 1.0)
        };
        let (indices, weights) = trilinear(
            normalize(s, RESIDUAL_DOMAIN[0]),
            normalize(chromaticity.a, RESIDUAL_DOMAIN[1]),
            normalize(chromaticity.b, RESIDUAL_DOMAIN[2]),
        );

        Self {
            xyz,
            unit_xyz,
            y,
            s,
            h_in,
            c_in,
            a_in: chromaticity.a,
            b_in: chromaticity.b,
            blend: BlendWeights::new(c_in),
            tone_weights,
            grl: GrlWeights {
                hue_idx,
                hue_w,
                s_idx,
                s_w,
                c_idx,
                c_w,
            },
            residual: ResidualWeights { indices, weights },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerValues {
    pub t: f64,
    pub n_a: f64,
    pub n_b: f64,
    pub log_g: f64,
    pub r: f64,
    pub lambda: f64,
    pub residual: [f64; 3],
}

/// Derivative of `J'a'b'` with respect to one coefficient.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CoefTerm {
    pub index: usize,
    pub derivative: [f64; 3],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamLayout {
    pub tone: Range<usize>,
    pub tint_a: Range<usize>,
    pub tint_b: Range<usize>,
    pub g: Range<usize>,
    pub r: Range<usize>,
    pub lambda: Range<usize>,
    pub residual: Range<usize>,
    pub total: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Tint {
    pub a: Vec<f64>,
    pub b: Vec<f64>,
}

impl Tint {
    pub fn zeros() -> Self {
        Self {
            a: vec![0.0; TINT_COEFS],
            b: vec![0.0; TINT_COEFS],
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GrlComponent {
    /// Coefficients at the `C = 0` knot, shared across hue.
    pub plane: Vec<f64>,
    /// Coefficients for `C > 0`, indexed `[hue][s][c - 1]`.
    pub volume: Vec<f64>,
    /// Near-neutral Cartesian coefficients, indexed `[s][term]`.
    pub core: Vec<f64>,
}

impl GrlComponent {
    pub fn zeros() -> Self {
        Self {
            plane: vec![0.0; S_COEFS],
            volume: vec![0.0; VOLUME_COEFS],
            core: vec![0.0; CORE_COEFS],
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColorLayer {
    pub g: GrlComponent,
    pub r: GrlComponent,
    pub lambda: GrlComponent,
}

impl ColorLayer {
    pub fn zeros() -> Self {
        Self {
            g: GrlComponent::zeros(),
            r: GrlComponent::zeros(),
            lambda: GrlComponent::zeros(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Residual {
    pub values: Vec<f64>,
}

impl Residual {
    pub fn zeros() -> Self {
        Self {
            values: vec![0.0; RESIDUAL_COEFS],
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Look {
    pub tone: Tone,
    pub tint: Tint,
    pub color: ColorLayer,
    pub residual: Residual,
    pub clip: ClipSettings,
}

impl Look {
    pub fn new(
        tone: Tone,
        tint: Tint,
        color: ColorLayer,
        residual: Residual,
        clip: ClipSettings,
    ) -> Result<Self> {
        let look = Self {
            tone,
            tint,
            color,
            residual,
            clip,
        };
        look.validate()?;
        Ok(look)
    }

    pub fn validate(&self) -> Result<()> {
        self.tint.validate()?;
        self.color.validate()?;
        if self.residual.values.len() != RESIDUAL_COEFS {
            return Err(ModelError::CoefficientLength {
                name: "res".to_string(),
                found: self.residual.values.len(),
                expected: RESIDUAL_COEFS,
            });
        }
        if self.residual.values.iter().any(|value| !value.is_finite()) {
            return Err(ModelError::Invalid("non-finite residual".to_string()));
        }
        if let Tone::Spline { c0, beta } = &self.tone {
            Tone::spline(*c0, *beta)?;
        }
        Ok(())
    }

    pub fn colorimetric() -> Self {
        Self {
            tone: Tone::Colorimetric,
            tint: Tint::zeros(),
            color: ColorLayer::zeros(),
            residual: Residual::zeros(),
            clip: ClipSettings::default_v1(),
        }
    }

    pub fn colorimetric_spline() -> Result<Self> {
        let mut look = Self::colorimetric();
        look.tone = Tone::colorimetric(&crate::basis::TONE_KNOTS)?;
        Ok(look)
    }

    pub fn layout(&self) -> ParamLayout {
        let tone_len = if self.tone.is_colorimetric() {
            0
        } else {
            1 + TONE_COEFS
        };
        let mut cursor = 0;
        let mut take = |length: usize| {
            let range = cursor..cursor + length;
            cursor += length;
            range
        };
        let layout = ParamLayout {
            tone: take(tone_len),
            tint_a: take(TINT_COEFS),
            tint_b: take(TINT_COEFS),
            g: take(GRL_COEFS),
            r: take(GRL_COEFS),
            lambda: take(GRL_COEFS),
            residual: take(RESIDUAL_COEFS),
            total: 0,
        };
        ParamLayout {
            total: cursor,
            ..layout
        }
    }

    pub fn coef_vector(&self) -> Vec<f64> {
        let layout = self.layout();
        let mut out = vec![0.0_f64; layout.total];
        if let Tone::Spline { c0, beta } = &self.tone {
            out[layout.tone.start] = *c0;
            out[layout.tone.start + 1..layout.tone.end].copy_from_slice(beta);
        }
        out[layout.tint_a.clone()].copy_from_slice(&self.tint.a);
        out[layout.tint_b.clone()].copy_from_slice(&self.tint.b);
        write_grl(&mut out[layout.g.clone()], &self.color.g);
        write_grl(&mut out[layout.r.clone()], &self.color.r);
        write_grl(&mut out[layout.lambda.clone()], &self.color.lambda);
        out[layout.residual.clone()].copy_from_slice(&self.residual.values);
        out
    }

    pub fn apply_coef(&mut self, coef: &[f64]) -> Result<()> {
        let layout = self.layout();
        if coef.len() != layout.total {
            return Err(ModelError::CoefficientLength {
                name: "look".to_string(),
                found: coef.len(),
                expected: layout.total,
            });
        }
        if coef.iter().any(|value| !value.is_finite()) {
            return Err(ModelError::Invalid("non-finite coefficient".to_string()));
        }
        if !self.tone.is_colorimetric() {
            let mut beta = [0.0_f64; TONE_COEFS];
            beta.copy_from_slice(&coef[layout.tone.start + 1..layout.tone.end]);
            self.tone = Tone::spline(coef[layout.tone.start], beta)?;
        }
        self.tint.a.copy_from_slice(&coef[layout.tint_a.clone()]);
        self.tint.b.copy_from_slice(&coef[layout.tint_b.clone()]);
        read_grl(&mut self.color.g, &coef[layout.g.clone()]);
        read_grl(&mut self.color.r, &coef[layout.r.clone()]);
        read_grl(&mut self.color.lambda, &coef[layout.lambda.clone()]);
        self.residual
            .values
            .copy_from_slice(&coef[layout.residual.clone()]);
        Ok(())
    }

    pub fn layer_values(&self, input: &LookInput) -> LayerValues {
        let t = match &self.tone {
            Tone::Colorimetric => {
                crate::deriv::reference_cam16()
                    .neutral_j_and_dj_dy(0.18 * libm::pow(2.0, input.s))
                    .0
            }
            Tone::Spline { c0, beta } => {
                let mut t = *c0 * input.tone_weights[0];
                for (value, weight) in beta.iter().zip(input.tone_weights[1..].iter()) {
                    t += value * weight;
                }
                t
            }
        };
        let (indices, weights, _) = tint_weights(t);
        let mut n_a = 0.0;
        let mut n_b = 0.0;
        for (m, index) in indices.iter().enumerate() {
            n_a += weights[m] * self.tint.a[*index];
            n_b += weights[m] * self.tint.b[*index];
        }
        let [log_g, r, lambda] = self.color.values(input);
        LayerValues {
            t,
            n_a,
            n_b,
            log_g,
            r,
            lambda,
            residual: self.residual_value(input),
        }
    }

    fn residual_value(&self, input: &LookInput) -> [f64; 3] {
        let mut out = [0.0_f64; 3];
        for (node, weight) in input
            .residual
            .indices
            .iter()
            .zip(input.residual.weights.iter())
        {
            for (channel, value) in out.iter_mut().enumerate() {
                *value += weight * self.residual.values[node * 3 + channel];
            }
        }
        out
    }

    pub fn eval_from_layer_values(&self, input: &LookInput, values: &LayerValues) -> Ucs {
        let y_ref = solve::solve_y_ref(values.t, input.y);
        let reference = crate::deriv::reference_cam16().ucs(input.unit_xyz.scale(y_ref));
        let gain = libm::exp(values.log_g);
        let radians = values.r * (PI / 180.0);
        let (sin_r, cos_r) = (libm::sin(radians), libm::cos(radians));
        let a_rotated = gain * (reference.a * cos_r - reference.b * sin_r);
        let b_rotated = gain * (reference.a * sin_r + reference.b * cos_r);
        Ucs::new(
            reference.j + values.lambda + values.residual[0],
            values.n_a + a_rotated + values.residual[1],
            values.n_b + b_rotated + values.residual[2],
        )
    }

    pub fn eval_input(&self, input: &LookInput) -> Ucs {
        let target = self.eval_once(input);
        if target.to_array().iter().all(|value| value.is_finite()) {
            return target;
        }
        self.eval_contracted(input.xyz)
    }

    pub fn eval(&self, xyz: Xyz) -> Ucs {
        self.eval_input(&LookInput::new(xyz))
    }

    fn eval_once(&self, input: &LookInput) -> Ucs {
        let values = self.layer_values(input);
        self.eval_from_layer_values(input, &values)
    }

    /// Contract the input chromaticity toward the D65 neutral until the evaluated
    /// point is finite (docs/02 §3).
    fn eval_contracted(&self, xyz: Xyz) -> Ucs {
        let luminance = xyz.y.max(0.0);
        let neutral = neutral_xyz(luminance);
        let mut inside = 0.0;
        let mut outside = 1.0;
        let mut best = self.eval_once(&LookInput::new(neutral));
        for _ in 0..32 {
            let middle = 0.5 * (inside + outside);
            let candidate = Xyz::new(
                neutral.x + middle * (xyz.x - neutral.x),
                luminance,
                neutral.z + middle * (xyz.z - neutral.z),
            );
            let target = self.eval_once(&LookInput::new(candidate));
            if target.to_array().iter().all(|value| value.is_finite()) {
                inside = middle;
                best = target;
            } else {
                outside = middle;
            }
        }
        best
    }

    /// Columns: `[t, n_a, n_b, log_g, r, lambda, res_a, res_b, res_c]`.
    pub fn d_output_d_layer_values(
        &self,
        input: &LookInput,
        values: &LayerValues,
    ) -> [[f64; 9]; 3] {
        let solved = solve::try_solve_y_ref(values.t, input.y);
        let (y_ref, dy_dt) = match solved {
            Ok(y) => {
                let derivative = crate::deriv::reference_cam16()
                    .neutral_y_and_dy_for_j_prime(values.t)
                    .map(|(_, derivative)| derivative)
                    .unwrap_or(0.0);
                (y, derivative)
            }
            Err(_) => (solve::solve_y_ref(values.t, input.y), 0.0),
        };
        let (reference, derivative) = REFERENCE_CAM16.ucs_and_derivative(input.unit_xyz, y_ref);
        let d_j_ref = derivative[0] * dy_dt;
        let d_a_ref = derivative[1] * dy_dt;
        let d_b_ref = derivative[2] * dy_dt;

        let gain = libm::exp(values.log_g);
        let hue = values.r * (PI / 180.0);
        let (sin_r, cos_r) = (libm::sin(hue), libm::cos(hue));
        let (a_ref, b_ref) = (reference.a, reference.b);

        let d_a_dt = gain * (cos_r * d_a_ref - sin_r * d_b_ref);
        let d_b_dt = gain * (sin_r * d_a_ref + cos_r * d_b_ref);
        let scale = PI / 180.0;
        [
            [d_j_ref, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0],
            [
                d_a_dt,
                1.0,
                0.0,
                gain * (a_ref * cos_r - b_ref * sin_r),
                gain * (-a_ref * sin_r - b_ref * cos_r) * scale,
                0.0,
                0.0,
                1.0,
                0.0,
            ],
            [
                d_b_dt,
                0.0,
                1.0,
                gain * (a_ref * sin_r + b_ref * cos_r),
                gain * (a_ref * cos_r - b_ref * sin_r) * scale,
                0.0,
                0.0,
                0.0,
                1.0,
            ],
        ]
    }

    /// Derivative of the tint layer `(a', b')` with respect to `t`.
    pub fn d_tint_d_t(&self, t: f64) -> (f64, f64) {
        if !(0.0..=100.0).contains(&t) {
            return (0.0, 0.0);
        }
        let (indices, _, derivative) = tint_weights(t);
        let mut a = 0.0;
        let mut b = 0.0;
        for (position, index) in indices.iter().enumerate() {
            a += derivative[position] * self.tint.a[*index];
            b += derivative[position] * self.tint.b[*index];
        }
        (a, b)
    }

    /// Row-wise derivative of `J'a'b'` with respect to every non-zero
    /// coefficient, indexed by [`ParamLayout`];
    pub fn coef_jacobian(&self, input: &LookInput) -> Vec<CoefTerm> {
        let values = self.layer_values(input);
        let jacobian = self.d_output_d_layer_values(input, &values);
        let layout = self.layout();
        let mut terms: BTreeMap<usize, [f64; 3]> = BTreeMap::new();
        let mut add = |index: usize, layer: [f64; 9]| {
            let mut row = [0.0_f64; 3];
            for (component, slot) in row.iter_mut().enumerate() {
                for (column, value) in layer.iter().enumerate() {
                    *slot += jacobian[component][column] * value;
                }
            }
            if row.iter().any(|value| *value != 0.0) {
                let entry = terms.entry(index).or_insert([0.0; 3]);
                for (total, value) in entry.iter_mut().zip(row.iter()) {
                    *total += value;
                }
            }
        };

        let (d_n_a, d_n_b) = self.d_tint_d_t(values.t);
        if !self.tone.is_colorimetric() {
            add(
                layout.tone.start,
                [1.0, d_n_a, d_n_b, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            );
            for k in 0..TONE_COEFS {
                let weight = input.tone_weights[k + 1];
                add(
                    layout.tone.start + 1 + k,
                    [
                        weight,
                        d_n_a * weight,
                        d_n_b * weight,
                        0.0,
                        0.0,
                        0.0,
                        0.0,
                        0.0,
                        0.0,
                    ],
                );
            }
        }

        let (indices, weights, _) = tint_weights(values.t);
        for (position, index) in indices.iter().enumerate() {
            add(
                layout.tint_a.start + *index,
                [0.0, weights[position], 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            );
            add(
                layout.tint_b.start + *index,
                [0.0, 0.0, weights[position], 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            );
        }

        let outer = input.blend.w;
        let near = input.blend.core();
        let plane_weight = outer * input.grl.c_w[0] + near;
        for (position, index) in input.grl.s_idx.iter().enumerate() {
            let weight = plane_weight * input.grl.s_w[position];
            add(
                layout.g.start + *index,
                [0.0, 0.0, 0.0, weight, 0.0, 0.0, 0.0, 0.0, 0.0],
            );
            add(
                layout.r.start + *index,
                [0.0, 0.0, 0.0, 0.0, weight, 0.0, 0.0, 0.0, 0.0],
            );
            add(
                layout.lambda.start + *index,
                [0.0, 0.0, 0.0, 0.0, 0.0, weight, 0.0, 0.0, 0.0],
            );
        }
        for (h, hue_index) in input.grl.hue_idx.iter().enumerate() {
            for (s, s_index) in input.grl.s_idx.iter().enumerate() {
                for (c, c_index) in input.grl.c_idx.iter().enumerate().skip(1) {
                    let weight =
                        outer * input.grl.hue_w[h] * input.grl.s_w[s] * input.grl.c_w[c];
                    let node = (*hue_index * S_COEFS + *s_index) * 3 + (c_index - 1);
                    add(
                        layout.g.start + S_COEFS + node,
                        [0.0, 0.0, 0.0, weight, 0.0, 0.0, 0.0, 0.0, 0.0],
                    );
                    add(
                        layout.r.start + S_COEFS + node,
                        [0.0, 0.0, 0.0, 0.0, weight, 0.0, 0.0, 0.0, 0.0],
                    );
                    add(
                        layout.lambda.start + S_COEFS + node,
                        [0.0, 0.0, 0.0, 0.0, 0.0, weight, 0.0, 0.0, 0.0],
                    );
                }
            }
        }
        let basis = core_weights(input.a_in, input.b_in);
        for (position, index) in input.grl.s_idx.iter().enumerate() {
            for (term, basis_weight) in basis.iter().enumerate() {
                let weight = near * input.grl.s_w[position] * basis_weight;
                let node = S_COEFS + VOLUME_COEFS + *index * CORE_TERMS + term;
                add(
                    layout.g.start + node,
                    [0.0, 0.0, 0.0, weight, 0.0, 0.0, 0.0, 0.0, 0.0],
                );
                add(
                    layout.r.start + node,
                    [0.0, 0.0, 0.0, 0.0, weight, 0.0, 0.0, 0.0, 0.0],
                );
                add(
                    layout.lambda.start + node,
                    [0.0, 0.0, 0.0, 0.0, 0.0, weight, 0.0, 0.0, 0.0],
                );
            }
        }

        for (corner, index) in input.residual.indices.iter().enumerate() {
            let weight = input.residual.weights[corner];
            for channel in 0..3 {
                let mut layer = [0.0_f64; 9];
                layer[6 + channel] = weight;
                add(layout.residual.start + index * 3 + channel, layer);
            }
        }

        terms
            .into_iter()
            .map(|(index, derivative)| CoefTerm { index, derivative })
            .collect()
    }

    pub fn output_encode(&self, ucs: Ucs, output_space: looklab_color::RgbSpace) -> Ucs {
        crate::clip::output_encode_clip(ucs, output_space, self.clip.k)
    }
}

fn write_grl(out: &mut [f64], component: &GrlComponent) {
    out[..S_COEFS].copy_from_slice(&component.plane);
    out[S_COEFS..S_COEFS + VOLUME_COEFS].copy_from_slice(&component.volume);
    out[S_COEFS + VOLUME_COEFS..].copy_from_slice(&component.core);
}

fn read_grl(component: &mut GrlComponent, input: &[f64]) {
    component.plane.copy_from_slice(&input[..S_COEFS]);
    component
        .volume
        .copy_from_slice(&input[S_COEFS..S_COEFS + VOLUME_COEFS]);
    component
        .core
        .copy_from_slice(&input[S_COEFS + VOLUME_COEFS..]);
}

impl Tint {
    fn validate(&self) -> Result<()> {
        if self.a.len() != TINT_COEFS || self.b.len() != TINT_COEFS {
            return Err(ModelError::CoefficientLength {
                name: "tint".to_string(),
                found: self.a.len() + self.b.len(),
                expected: 2 * TINT_COEFS,
            });
        }
        if self
            .a
            .iter()
            .chain(self.b.iter())
            .any(|value| !value.is_finite())
        {
            return Err(ModelError::Invalid("non-finite tint".to_string()));
        }
        Ok(())
    }
}

impl ColorLayer {
    fn values(&self, input: &LookInput) -> [f64; 3] {
        let weights = &input.grl;
        let mut plane = [0.0_f64; 3];
        for (weight, index) in weights.s_w.iter().zip(weights.s_idx.iter()) {
            plane[0] += weight * self.g.plane[*index];
            plane[1] += weight * self.r.plane[*index];
            plane[2] += weight * self.lambda.plane[*index];
        }
        let mut volume = [0.0_f64; 3];
        for (h, hue_index) in weights.hue_idx.iter().enumerate() {
            for (s, s_index) in weights.s_idx.iter().enumerate() {
                let base = (*hue_index * S_COEFS + *s_index) * 3;
                let spatial = weights.hue_w[h] * weights.s_w[s];
                for (c, c_index) in weights.c_idx.iter().enumerate().skip(1) {
                    let weight = spatial * weights.c_w[c];
                    let node = base + (c_index - 1);
                    volume[0] += weight * self.g.volume[node];
                    volume[1] += weight * self.r.volume[node];
                    volume[2] += weight * self.lambda.volume[node];
                }
            }
        }
        let basis = core_weights(input.a_in, input.b_in);
        let mut core = [0.0_f64; 3];
        for (position, index) in weights.s_idx.iter().enumerate() {
            let weight = weights.s_w[position];
            let base = *index * CORE_TERMS;
            for (component, values) in [&self.g.core, &self.r.core, &self.lambda.core]
                .into_iter()
                .enumerate()
            {
                core[component] += weight
                    * (0..CORE_TERMS)
                        .map(|term| values[base + term] * basis[term])
                        .sum::<f64>();
            }
        }
        let outer = input.blend.w;
        let near = input.blend.core();
        let c0 = weights.c_w[0];
        [
            outer * (c0 * plane[0] + volume[0]) + near * (plane[0] + core[0]),
            outer * (c0 * plane[1] + volume[1]) + near * (plane[1] + core[1]),
            outer * (c0 * plane[2] + volume[2]) + near * (plane[2] + core[2]),
        ]
    }

    fn validate(&self) -> Result<()> {
        for component in [&self.g, &self.r, &self.lambda] {
            if component.plane.len() != S_COEFS
                || component.volume.len() != VOLUME_COEFS
                || component.core.len() != CORE_COEFS
            {
                return Err(ModelError::CoefficientLength {
                    name: "grl".to_string(),
                    found: component.plane.len() + component.volume.len() + component.core.len(),
                    expected: GRL_COEFS,
                });
            }
            if component
                .plane
                .iter()
                .chain(component.volume.iter())
                .chain(component.core.iter())
                .any(|value| !value.is_finite())
            {
                return Err(ModelError::Invalid("non-finite grl".to_string()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coefficient_round_trip_is_exact() {
        let mut look = Look::colorimetric_spline().unwrap();
        let layout = look.layout();
        let mut coef = look.coef_vector();
        coef[layout.tint_a.start] = 0.25;
        coef[layout.g.start + 3] = -0.5;
        coef[layout.residual.end - 1] = 2.0;
        look.apply_coef(&coef).unwrap();
        assert_eq!(look.coef_vector(), coef);
    }

    #[test]
    fn param_layout_covers_every_coefficient() {
        let look = Look::colorimetric();
        assert_eq!(look.layout().tone.len(), 0);
        let spline = Look::colorimetric_spline().unwrap();
        let layout = spline.layout();
        assert_eq!(layout.tone.len(), 1 + TONE_COEFS);
        assert_eq!(layout.total, spline.coef_vector().len());
        assert_eq!(layout.residual.end, layout.total);
    }

    fn synthetic_input(a: f64, b: f64) -> LookInput {
        let c = (a * a + b * b).sqrt();
        let h = if c > 0.0 {
            let degrees = libm::atan2(b, a) * (180.0 / PI);
            if degrees < 0.0 { degrees + 360.0 } else { degrees }
        } else {
            0.0
        };
        let (hue_idx, hue_w) = hue_weights(h);
        let (s_idx, s_w) = s_weights(0.0);
        let (c_idx, c_w) = c_weights(c);
        LookInput {
            xyz: Xyz::new(0.0, 0.18, 0.0),
            unit_xyz: Xyz::new(0.0, 1.0, 0.0),
            y: 0.18,
            s: 0.0,
            h_in: h,
            c_in: c,
            a_in: a,
            b_in: b,
            blend: BlendWeights::new(c),
            tone_weights: [0.0; TONE_COEFS + 1],
            grl: GrlWeights {
                hue_idx,
                hue_w,
                s_idx,
                s_w,
                c_idx,
                c_w,
            },
            residual: ResidualWeights {
                indices: [0; 8],
                weights: [0.0; 8],
            },
        }
    }

    #[test]
    fn core_and_join_are_smooth_in_cartesian_chroma() {
        let mut look = Look::colorimetric_spline().unwrap();
        for component in [&mut look.color.g, &mut look.color.r, &mut look.color.lambda] {
            for value in component.core.iter_mut() {
                *value = 0.1;
            }
            for value in component.volume.iter_mut() {
                *value = 0.1;
            }
        }
        for (radius, steps) in [(0.0, [1e-3, 1e-4]), (1.5, [1e-4, 1e-5])] {
            for direction in 0..8 {
                let angle = 2.0 * PI * direction as f64 / 8.0;
                let (cos, sin) = (libm::cos(angle), libm::sin(angle));
                let at = |t: f64| {
                    look.color
                        .values(&synthetic_input(radius * cos + t * cos, radius * sin + t * sin))
                };
                let [coarse_step, fine_step] = steps;
                let coarse = at(coarse_step);
                let coarse_back = at(-coarse_step);
                let fine = at(fine_step);
                let fine_back = at(-fine_step);
                for channel in 0..3 {
                    let coarse_derivative = (coarse[channel] - coarse_back[channel]) / (2.0 * coarse_step);
                    let fine_derivative = (fine[channel] - fine_back[channel]) / (2.0 * fine_step);
                    assert!(coarse_derivative.is_finite() && fine_derivative.is_finite());
                    assert!(
                        (coarse_derivative - fine_derivative).abs() < 1e-3,
                        "radius {radius} direction {direction} channel {channel}: {} vs {}",
                        coarse_derivative,
                        fine_derivative
                    );
                }
            }
        }
    }
}
