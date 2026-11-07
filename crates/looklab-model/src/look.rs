// SPDX-License-Identifier: AGPL-3.0-or-later

use core::f64::consts::PI;
use core::ops::Range;
use looklab_color::Xyz;
use looklab_color::cam16::{Cam16Model, ConeOpponent, ConeOpponentSlope};
use looklab_color::ucs::{Ucs, UcsSpace, ucs_to_jmh};
use std::collections::BTreeMap;
use std::sync::LazyLock;

use crate::basis::{
    CORE_COEFS, CORE_TERMS, NEUTRAL_JOIN_C, RESIDUAL_COEFS, S_COEFS, TINT_COEFS, TONE_COEFS,
    ToneBasis, VOLUME_COEFS, c_weights, core_weights, exposure_lookup, hue_weights, outer_weight,
    residual_weights, s_weights, tint_weights,
};
use crate::clip::ClipSettings;
use crate::error::{ModelError, Result};
use crate::solve;
use crate::tone::Tone;

pub const GRL_COEFS: usize = S_COEFS + VOLUME_COEFS + CORE_COEFS;

/// Residual grid bounds in `(s, a_in, b_in)`; v1 fixes them.
pub const RESIDUAL_DOMAIN: [[f64; 2]; 3] = [[-12.0, 6.0], [-80.0, 80.0], [-80.0, 80.0]];

static TONE_BASIS: LazyLock<ToneBasis> = LazyLock::new(ToneBasis::new);

/// `K` of the normalized coordinates; see `local://opponent-coords.md`.
static NEUTRAL_SCALE: LazyLock<f64> = LazyLock::new(|| {
    let neutral = solve::reference_white().scale(0.18);
    solve::reference_model().neutral_chroma_scale(neutral, NEUTRAL_JOIN_C)
});

/// Reference-neutral temporary-magnitude denominator, the scale of the domain margin.
static REFERENCE_DENOMINATOR: LazyLock<f64> = LazyLock::new(|| {
    solve::reference_model()
        .cone_opponent(solve::reference_white().scale(0.18))
        .denominator
});

/// Cone-response scale of the domain margin.
const CONE_SCALE: f64 = 100.0;

/// Near-neutral normalized opponent chroma (docs/02 §3.3).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NeutralChroma {
    pub a: f64,
    pub b: f64,
}

impl NeutralChroma {
    pub fn magnitude(self) -> f64 {
        (self.a * self.a + self.b * self.b).sqrt()
    }
}

fn neutral_chroma(opponent: &ConeOpponent) -> NeutralChroma {
    let scale = *NEUTRAL_SCALE / opponent.denominator;
    NeutralChroma {
        a: scale * opponent.a,
        b: scale * opponent.b,
    }
}

/// `d/dy` of [`neutral_chroma`].
fn neutral_chroma_slope(opponent: &ConeOpponent, slope: &ConeOpponentSlope) -> NeutralChroma {
    let k = *NEUTRAL_SCALE;
    let denominator = opponent.denominator;
    NeutralChroma {
        a: k * (slope.a * denominator - opponent.a * slope.denominator)
            / (denominator * denominator),
        b: k * (slope.b * denominator - opponent.b * slope.denominator)
            / (denominator * denominator),
    }
}

/// CAM16-UCS reference chroma at `unit_xyz · y`.
fn ucs_reference(unit_xyz: Xyz, y: f64) -> Ucs {
    crate::deriv::reference_cam16().ucs(unit_xyz.scale(y))
}

/// CAM16-UCS reference chroma at `unit_xyz · y` and `d/dy`.
fn ucs_reference_and_derivative(unit_xyz: Xyz, y: f64) -> (Ucs, [f64; 3]) {
    crate::deriv::reference_cam16().ucs_and_derivative(unit_xyz, y)
}

/// Internal opponent reference chroma at `unit_xyz · y`.
fn internal_reference(unit_xyz: Xyz, y: f64) -> NeutralChroma {
    neutral_chroma(&solve::reference_model().cone_opponent(unit_xyz.scale(y)))
}

/// Internal opponent reference chroma at `unit_xyz · y` and `d/dy`.
fn internal_reference_and_derivative(unit_xyz: Xyz, y: f64) -> (NeutralChroma, NeutralChroma) {
    let (opponent, slope) = solve::reference_model().cone_opponent_and_slope(unit_xyz, y);
    (
        neutral_chroma(&opponent),
        neutral_chroma_slope(&opponent, &slope),
    )
}

/// Φ: internal opponent chroma to CAM16-UCS chroma at `j_prime`.
pub fn ucs_chroma_from_internal(chroma: [f64; 2], j_prime: f64) -> [f64; 2] {
    solve::reference_model().ucs_chroma_from_opponent(chroma, j_prime, *NEUTRAL_SCALE)
}

/// Ψ: CAM16-UCS chroma to internal opponent chroma at `j_prime`.
pub fn internal_chroma_from_ucs(chroma: [f64; 2], j_prime: f64) -> [f64; 2] {
    solve::reference_model().opponent_chroma_from_ucs(chroma, j_prime, *NEUTRAL_SCALE)
}

/// Smooth internal inverse: XYZ of the internal chroma at `j_prime`.
pub fn internal_chroma_to_xyz(chroma: [f64; 2], j_prime: f64) -> Xyz {
    solve::reference_model().opponent_chroma_to_xyz(chroma, j_prime, *NEUTRAL_SCALE)
}

/// `K` of the normalized coordinates.
pub fn neutral_scale() -> f64 {
    *NEUTRAL_SCALE
}

/// `dJ'/dXYZ` under the reference conditions.
pub fn j_prime_xyz_gradient(xyz: Xyz) -> [f64; 3] {
    crate::deriv::reference_cam16().j_prime_xyz_gradient(xyz)
}

fn ucs_chroma_jacobian(chroma: [f64; 2], j_prime: f64) -> ([[f64; 2]; 2], [f64; 2]) {
    solve::reference_model().ucs_chroma_from_opponent_jacobian(chroma, j_prime, *NEUTRAL_SCALE)
}

/// Normalized opponent chroma at `xyz` and `d(n_a, n_b)/dXYZ`.
pub fn neutral_chroma_jacobian(xyz: Xyz) -> (NeutralChroma, [[f64; 3]; 2]) {
    let opponent = solve::reference_model().cone_opponent(xyz);
    let (_, opponent_jacobian) = solve::reference_model().cone_opponent_jacobian(xyz);
    let neutral = neutral_chroma(&opponent);
    let scale = *NEUTRAL_SCALE;
    let denominator = opponent.denominator;
    let mut jacobian = [[0.0_f64; 3]; 2];
    for axis in 0..3 {
        jacobian[0][axis] = scale
            * (opponent_jacobian[0][axis] * denominator - opponent.a * opponent_jacobian[2][axis])
            / (denominator * denominator);
        jacobian[1][axis] = scale
            * (opponent_jacobian[1][axis] * denominator - opponent.b * opponent_jacobian[2][axis])
            / (denominator * denominator);
    }
    (neutral, jacobian)
}

/// CAM16-UCS chroma at `unit_xyz · y` and `d(a', b')/dXYZ` at fixed `y`.
pub fn ucs_reference_jacobian(unit_xyz: Xyz, y: f64) -> (Ucs, [[f64; 3]; 2]) {
    let (ucs, jacobian) = crate::deriv::reference_cam16().ucs_xyz_jacobian(unit_xyz.scale(y));
    (
        ucs,
        [
            [jacobian[1][0], jacobian[1][1], jacobian[1][2]],
            [jacobian[2][0], jacobian[2][1], jacobian[2][2]],
        ],
    )
}

/// `∂²UCS/∂XYZ_a∂XYZ_b`, packed as `(0,0) (1,1) (2,2) (0,1) (0,2) (1,2)`.
pub fn ucs_xyz_second(xyz: Xyz) -> [[f64; 3]; 6] {
    crate::deriv::reference_cam16().ucs_xyz_second(xyz)
}

/// [`neutral_chroma_jacobian`]'s second derivative, packed as
/// `(0,0) (1,1) (2,2) (0,1) (0,2) (1,2)`.
pub fn neutral_chroma_second(xyz: Xyz) -> [[f64; 2]; 6] {
    crate::deriv::reference_cam16().neutral_xyz_second(xyz, *NEUTRAL_SCALE)
}

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
    pub indices: [usize; 64],
    pub weights: [f64; 64],
}

/// Near-neutral blend weight `w(|n|)` and `dw/d|n|` at the input.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlendWeights {
    pub w: f64,
    pub dw_dn: f64,
}

impl BlendWeights {
    pub fn new(chroma: NeutralChroma) -> Self {
        let (w, dw_dn) = outer_weight(chroma.magnitude());
        Self { w, dw_dn }
    }

    pub fn core(self) -> f64 {
        1.0 - self.w
    }
}

/// The two pre-clip chroma paths and the input join weight.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChromaPaths {
    pub ucs: Ucs,
    pub internal: [f64; 2],
    pub blend: f64,
}

impl ChromaPaths {
    /// Published pre-clip appearance, `w·UCS + (1 − w)·Φ(internal)`.
    pub fn published(self) -> Ucs {
        if self.blend >= 1.0 {
            return self.ucs;
        }
        let phi = ucs_chroma_from_internal(self.internal, self.ucs.j);
        Ucs::new(
            self.ucs.j,
            self.blend * self.ucs.a + (1.0 - self.blend) * phi[0],
            self.blend * self.ucs.b + (1.0 - self.blend) * phi[1],
        )
    }

    /// True where Φ has no finite Jacobian, at a neutral internal chroma.
    pub fn singular(self) -> bool {
        self.internal[0] == 0.0 && self.internal[1] == 0.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LookInput {
    pub xyz: Xyz,
    pub unit_xyz: Xyz,
    pub y: f64,
    /// Physical scene exposure `log2(Y / 0.18)`.
    pub s: f64,
    /// Bounded lookup coordinate of [`crate::basis::exposure_lookup`].
    pub s_lookup: f64,
    pub h_in: f64,
    pub c_in: f64,
    pub a_in: f64,
    pub b_in: f64,
    /// Normalized opponent chroma of the input at `Y = 0.18`; the core input.
    pub neutral: NeutralChroma,
    pub blend: BlendWeights,
    /// Blended residual lookup coordinates `w·(a', b') + (1 - w)·n`.
    pub residual_a: f64,
    pub residual_b: f64,
    /// Input-side domain margin; `> 0` strictly inside.
    pub margin: f64,
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

/// Whether CAM16's forward model and the chroma denominator are finite and positive.
fn cam16_defined(chromaticity: Ucs, opponent: &ConeOpponent) -> bool {
    chromaticity
        .to_array()
        .iter()
        .all(|value| value.is_finite())
        && opponent.denominator > 0.0
        && opponent.rgb_c.iter().all(|value| value.is_finite())
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
        let (chromaticity, opponent) = crate::deriv::reference_cam16()
            .ucs_and_opponent(chromaticity_input(candidate).2.scale(0.18));
        if cam16_defined(chromaticity, &opponent) {
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
        let (chromaticity, opponent) =
            crate::deriv::reference_cam16().ucs_and_opponent(unit_xyz.scale(0.18));
        let (xyz, y, s, unit_xyz, chromaticity, opponent) =
            if y > 0.0 && cam16_defined(chromaticity, &opponent) {
                (xyz, y, s, unit_xyz, chromaticity, opponent)
            } else {
                // Negative or zero Y has no chromaticity; its neutral is black.
                let contracted = if y > 0.0 {
                    contract_to_cam16(xyz)
                } else {
                    neutral_xyz(0.0)
                };
                let (y, s, unit_xyz) = chromaticity_input(contracted);
                let (chromaticity, opponent) =
                    crate::deriv::reference_cam16().ucs_and_opponent(unit_xyz.scale(0.18));
                (contracted, y, s, unit_xyz, chromaticity, opponent)
            };
        let s_lookup = exposure_lookup(s);
        let neutral = neutral_chroma(&opponent);
        let blend = BlendWeights::new(neutral);
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
        tone_weights[1..].copy_from_slice(&TONE_BASIS.values(s_lookup));

        let (hue_idx, hue_w) = hue_weights(h_in);
        let (s_idx, s_w) = s_weights(s_lookup);
        let (c_idx, c_w) = c_weights(c_in);

        let near = blend.core();
        let residual_a = blend.w * chromaticity.a + near * neutral.a;
        let residual_b = blend.w * chromaticity.b + near * neutral.b;
        let (indices, weights) = residual_weights(s_lookup, residual_a, residual_b);

        Self {
            xyz,
            unit_xyz,
            y,
            s,
            s_lookup,
            h_in,
            c_in,
            a_in: chromaticity.a,
            b_in: chromaticity.b,
            neutral,
            blend,
            residual_a,
            residual_b,
            margin: input_margin(y, &opponent),
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

/// Distance to the input-side domain boundary, in units of the reference scales.
fn input_margin(y: f64, opponent: &ConeOpponent) -> f64 {
    let luminance = y / 0.18;
    let cone = opponent
        .rgb_c
        .iter()
        .map(|value| value.abs())
        .fold(f64::INFINITY, f64::min)
        / CONE_SCALE;
    let denominator = opponent.denominator / *REFERENCE_DENOMINATOR;
    luminance.min(cone).min(denominator)
}

pub fn domain_margin(input: &LookInput) -> f64 {
    input.margin
}

/// The active piece of [`prediction_margin`].
struct MarginSelection {
    margin: f64,
    channel: Option<(usize, f64)>,
    jacobian: [[f64; 3]; 3],
}

fn margin_selection(pre: Ucs) -> MarginSelection {
    let model = solve::reference_model();
    let j_limit = model.inverse_j_prime_limit();
    let mut margin = (j_limit - pre.j) / j_limit;
    let mut channel = None;
    let jmh = ucs_to_jmh(pre, UcsSpace::Ucs);
    let (responses, jacobian) = if jmh.m == 0.0 {
        let responses = model.inverse_responses(jmh);
        let step = 1e-6 * jmh.j.abs().max(1.0);
        let plus = model.inverse_responses(looklab_color::cam16::Jmh {
            j: jmh.j + step,
            ..jmh
        });
        let minus = model.inverse_responses(looklab_color::cam16::Jmh {
            j: jmh.j - step,
            ..jmh
        });
        let mut jacobian = [[0.0_f64; 3]; 3];
        for channel in 0..3 {
            jacobian[channel][0] = (plus[channel] - minus[channel]) / (2.0 * step);
        }
        (responses, jacobian)
    } else {
        model.inverse_responses_and_jacobian(jmh)
    };
    let limit = Cam16Model::RESPONSE_LIMIT;
    for (index, response) in responses.iter().enumerate() {
        let signed = response - 0.1;
        let distance = (limit - signed.abs()) / limit;
        if distance < margin {
            margin = distance;
            channel = Some((index, -signed.signum() / limit));
        }
    }
    MarginSelection {
        margin,
        channel,
        jacobian,
    }
}

/// Pre-clip inverse-domain margin and its gradient in `J'a'b'`.
///
/// `> 0` strictly inside; `<= 0` is an infeasible prediction.
pub fn prediction_margin(pre: Ucs) -> (f64, [f64; 3]) {
    let selection = margin_selection(pre);
    let gradient = match selection.channel {
        Some((channel, slope)) => compose_inverse_gradient(
            pre,
            [
                slope * selection.jacobian[channel][0],
                slope * selection.jacobian[channel][1],
                slope * selection.jacobian[channel][2],
            ],
        ),
        None => {
            let j_limit = solve::reference_model().inverse_j_prime_limit();
            [-1.0 / j_limit, 0.0, 0.0]
        }
    };
    (selection.margin, gradient)
}

/// `∂margin/∂XYZ` and `∂²margin/∂XYZ²` of the active [`prediction_margin`] piece
/// at the pre-clip `xyz`.
pub fn prediction_margin_xyz_derivatives(pre: Ucs, xyz: Xyz) -> ([f64; 3], [[f64; 3]; 3]) {
    let selection = margin_selection(pre);
    let model = crate::deriv::reference_cam16();
    let packed = [(0usize, 0usize), (1, 1), (2, 2), (0, 1), (0, 2), (1, 2)];
    let mut gradient = [0.0_f64; 3];
    let mut hessian = [[0.0_f64; 3]; 3];
    match selection.channel {
        Some((channel, slope)) => {
            let (_, jacobian, second) = model.response_xyz_derivatives(xyz);
            for (axis, value) in gradient.iter_mut().enumerate() {
                *value = slope * jacobian[channel][axis];
            }
            for (slot, (a, b)) in packed.into_iter().enumerate() {
                let value = slope * second[slot][channel];
                hessian[a][b] = value;
                hessian[b][a] = value;
            }
        }
        None => {
            let j_limit = solve::reference_model().inverse_j_prime_limit();
            let jacobian = model.j_prime_xyz_gradient(xyz);
            for (axis, value) in gradient.iter_mut().enumerate() {
                *value = -jacobian[axis] / j_limit;
            }
            let second = model.ucs_xyz_second(xyz);
            for (slot, (a, b)) in packed.into_iter().enumerate() {
                let value = -second[slot][0] / j_limit;
                hessian[a][b] = value;
                hessian[b][a] = value;
            }
        }
    }
    (gradient, hessian)
}

/// `(j, M, h)` gradient to `J'a'b'` coordinates.
fn compose_inverse_gradient(pre: Ucs, d_jmh: [f64; 3]) -> [f64; 3] {
    let c1 = 0.007;
    let denominator = 1.0 + 100.0 * c1 - c1 * pre.j;
    let d_j_prime = (1.0 + 100.0 * c1) / (denominator * denominator);
    let c2 = looklab_color::ucs::UcsSpace::Ucs.coefficients().c2;
    let m_prime = (pre.a * pre.a + pre.b * pre.b).sqrt();
    let (d_a, d_b) = if m_prime > 0.0 {
        let d_m = libm::exp(c2 * m_prime);
        let degrees = 180.0 / PI;
        (
            d_jmh[1] * d_m * (pre.a / m_prime)
                + d_jmh[2] * (-pre.b / (m_prime * m_prime)) * degrees,
            d_jmh[1] * d_m * (pre.b / m_prime) + d_jmh[2] * (pre.a / (m_prime * m_prime)) * degrees,
        )
    } else {
        (0.0, 0.0)
    };
    [d_jmh[0] * d_j_prime, d_a, d_b]
}

/// Log barrier on a domain margin.
pub fn barrier(margin: f64) -> f64 {
    if margin > 0.0 {
        -libm::log(margin)
    } else {
        f64::INFINITY
    }
}

/// `(barrier', barrier'')` at `margin`.
pub fn barrier_derivatives(margin: f64) -> (f64, f64) {
    if margin > 0.0 {
        (-1.0 / margin, 1.0 / (margin * margin))
    } else {
        (f64::NEG_INFINITY, f64::INFINITY)
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
                    .neutral_j_and_dj_dy(0.18 * libm::pow(2.0, input.s.clamp(-12.0, 6.0)))
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

    pub fn chroma_paths(&self, input: &LookInput, values: &LayerValues) -> ChromaPaths {
        let y_ref = solve::solve_y_ref(values.t, input.y);
        let (reference, opponent) =
            crate::deriv::reference_cam16().ucs_and_opponent(input.unit_xyz.scale(y_ref));
        let neutral = neutral_chroma(&opponent);
        let gain = libm::exp(values.log_g);
        let radians = values.r * (PI / 180.0);
        let (sin_r, cos_r) = (libm::sin(radians), libm::cos(radians));
        let rotate = |a: f64, b: f64| {
            (
                gain * (a * cos_r - b * sin_r),
                gain * (a * sin_r + b * cos_r),
            )
        };
        let (ucs_a, ucs_b) = rotate(reference.a, reference.b);
        let (internal_a, internal_b) = rotate(neutral.a, neutral.b);
        ChromaPaths {
            ucs: Ucs::new(
                reference.j + values.lambda + values.residual[0],
                values.n_a + ucs_a + values.residual[1],
                values.n_b + ucs_b + values.residual[2],
            ),
            internal: [
                values.n_a + internal_a + values.residual[1],
                values.n_b + internal_b + values.residual[2],
            ],
            blend: input.blend.w,
        }
    }

    pub fn eval_from_layer_values(&self, input: &LookInput, values: &LayerValues) -> Ucs {
        self.chroma_paths(input, values).published()
    }

    /// Second derivatives of the two pre-clip chroma paths over the nine layer
    /// values, indexed `k * 9 + l`; the first array is the outer `(J', a', b')`,
    /// the second the internal `(n_a, n_b)`.
    pub fn path_second(
        &self,
        input: &LookInput,
        values: &LayerValues,
    ) -> ([[f64; 3]; 81], [[f64; 2]; 81]) {
        let model = crate::deriv::reference_cam16();
        let (y_ref, dy, d2y) = match model.neutral_y_and_dy2_for_j_prime(values.t) {
            Some(triple) => triple,
            None => (solve::solve_y_ref(values.t, input.y), 0.0, 0.0),
        };
        let (reference, reference_d, reference_dd) =
            model.ucs_and_derivative_d2(input.unit_xyz, y_ref);
        let (internal, internal_d, internal_dd) = model.neutral_second(
            input.unit_xyz.scale(y_ref),
            input.unit_xyz.to_array(),
            *NEUTRAL_SCALE,
        );
        let ct = [reference_d[1] * dy, reference_d[2] * dy];
        let ctt = [
            reference_dd[1] * dy * dy + reference_d[1] * d2y,
            reference_dd[2] * dy * dy + reference_d[2] * d2y,
        ];
        let mt = [internal_d[0] * dy, internal_d[1] * dy];
        let mtt = [
            internal_dd[0] * dy * dy + internal_d[0] * d2y,
            internal_dd[1] * dy * dy + internal_d[1] * d2y,
        ];
        let jtt = reference_dd[0] * dy * dy + reference_d[0] * d2y;
        let c = [reference.a, reference.b];
        let gain = libm::exp(values.log_g);
        let radians = values.r * (PI / 180.0);
        let (sin_r, cos_r) = (libm::sin(radians), libm::cos(radians));
        let degrees = PI / 180.0;

        let rotate = |a: f64, b: f64| (a * cos_r - b * sin_r, a * sin_r + b * cos_r);
        let rotate_r = |a: f64, b: f64| {
            (
                (-a * sin_r - b * cos_r) * degrees,
                (a * cos_r - b * sin_r) * degrees,
            )
        };
        let rotate_rr = |a: f64, b: f64| {
            (
                (-a * cos_r + b * sin_r) * degrees * degrees,
                (-a * sin_r - b * cos_r) * degrees * degrees,
            )
        };

        let mut outer = [[0.0_f64; 3]; 81];
        let mut inner = [[0.0_f64; 2]; 81];
        let (oa, ob) = rotate(ctt[0], ctt[1]);
        let (ia, ib) = rotate(mtt[0], mtt[1]);
        outer[0] = [jtt, gain * oa, gain * ob];
        inner[0] = [gain * ia, gain * ib];
        let mut set = |k: usize, l: usize, oa: f64, ob: f64, ia: f64, ib: f64| {
            outer[k * 9 + l] = [0.0, oa, ob];
            inner[k * 9 + l] = [ia, ib];
            if k != l {
                outer[l * 9 + k] = [0.0, oa, ob];
                inner[l * 9 + k] = [ia, ib];
            }
        };
        let (oa, ob) = rotate(ct[0], ct[1]);
        let (ia, ib) = rotate(mt[0], mt[1]);
        set(0, 3, gain * oa, gain * ob, gain * ia, gain * ib);
        let (oa, ob) = rotate_r(ct[0], ct[1]);
        let (ia, ib) = rotate_r(mt[0], mt[1]);
        set(0, 4, gain * oa, gain * ob, gain * ia, gain * ib);
        let (oa, ob) = rotate(c[0], c[1]);
        let (ia, ib) = rotate(internal[0], internal[1]);
        set(3, 3, gain * oa, gain * ob, gain * ia, gain * ib);
        let (oa, ob) = rotate_r(c[0], c[1]);
        let (ia, ib) = rotate_r(internal[0], internal[1]);
        set(3, 4, gain * oa, gain * ob, gain * ia, gain * ib);
        let (oa, ob) = rotate_rr(c[0], c[1]);
        let (ia, ib) = rotate_rr(internal[0], internal[1]);
        set(4, 4, gain * oa, gain * ob, gain * ia, gain * ib);
        (outer, inner)
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
    pub fn d_ucs_d_layer_values(&self, input: &LookInput, values: &LayerValues) -> [[f64; 9]; 3] {
        let gain = libm::exp(values.log_g);
        let hue = values.r * (PI / 180.0);
        let (sin_r, cos_r) = (libm::sin(hue), libm::cos(hue));
        let (reference, d_ref) = match solve::try_solve_y_ref(values.t, input.y) {
            Ok(y_ref) => {
                let dy_dt = crate::deriv::reference_cam16()
                    .neutral_y_and_dy_for_j_prime(values.t)
                    .map(|(_, derivative)| derivative)
                    .unwrap_or(0.0);
                let (reference, derivative) = ucs_reference_and_derivative(input.unit_xyz, y_ref);
                (
                    reference,
                    [
                        derivative[0] * dy_dt,
                        derivative[1] * dy_dt,
                        derivative[2] * dy_dt,
                    ],
                )
            }
            Err(_) => (
                ucs_reference(input.unit_xyz, solve::solve_y_ref(values.t, input.y)),
                [0.0; 3],
            ),
        };
        let (a_ref, b_ref) = (reference.a, reference.b);
        let d_a_dt = gain * (cos_r * d_ref[1] - sin_r * d_ref[2]);
        let d_b_dt = gain * (sin_r * d_ref[1] + cos_r * d_ref[2]);
        let scale = PI / 180.0;
        [
            [d_ref[0], 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0],
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

    /// [`Self::d_ucs_d_layer_values`] for the internal chroma path.
    pub fn d_internal_d_layer_values(
        &self,
        input: &LookInput,
        values: &LayerValues,
    ) -> [[f64; 2]; 9] {
        let gain = libm::exp(values.log_g);
        let hue = values.r * (PI / 180.0);
        let (sin_r, cos_r) = (libm::sin(hue), libm::cos(hue));
        let (reference, d_ref) = match solve::try_solve_y_ref(values.t, input.y) {
            Ok(y_ref) => {
                let dy_dt = crate::deriv::reference_cam16()
                    .neutral_y_and_dy_for_j_prime(values.t)
                    .map(|(_, derivative)| derivative)
                    .unwrap_or(0.0);
                let (reference, derivative) =
                    internal_reference_and_derivative(input.unit_xyz, y_ref);
                (reference, [derivative.a * dy_dt, derivative.b * dy_dt])
            }
            Err(_) => (
                internal_reference(input.unit_xyz, solve::solve_y_ref(values.t, input.y)),
                [0.0; 2],
            ),
        };
        let (a_ref, b_ref) = (reference.a, reference.b);
        let d_a_dt = gain * (cos_r * d_ref[0] - sin_r * d_ref[1]);
        let d_b_dt = gain * (sin_r * d_ref[0] + cos_r * d_ref[1]);
        let scale = PI / 180.0;
        [
            [d_a_dt, d_b_dt],
            [1.0, 0.0],
            [0.0, 1.0],
            [
                gain * (a_ref * cos_r - b_ref * sin_r),
                gain * (a_ref * sin_r + b_ref * cos_r),
            ],
            [
                gain * (-a_ref * sin_r - b_ref * cos_r) * scale,
                gain * (a_ref * cos_r - b_ref * sin_r) * scale,
            ],
            [0.0, 0.0],
            [0.0, 0.0],
            [1.0, 0.0],
            [0.0, 1.0],
        ]
    }

    /// Columns: `[t, n_a, n_b, log_g, r, lambda, res_a, res_b, res_c]`.
    pub fn d_output_d_layer_values(
        &self,
        input: &LookInput,
        values: &LayerValues,
    ) -> [[f64; 9]; 3] {
        let paths = self.chroma_paths(input, values);
        let mut out = self.d_ucs_d_layer_values(input, values);
        if paths.blend >= 1.0 || paths.singular() {
            return out;
        }
        let internal = self.d_internal_d_layer_values(input, values);
        let (jacobian, gradient) = ucs_chroma_jacobian(paths.internal, paths.ucs.j);
        let near = 1.0 - paths.blend;
        for column in 0..9 {
            let mut d_a = 0.0;
            let mut d_b = 0.0;
            for axis in 0..2 {
                d_a += jacobian[0][axis] * internal[column][axis];
                d_b += jacobian[1][axis] * internal[column][axis];
            }
            let d_j = out[0][column];
            out[1][column] = paths.blend * out[1][column] + near * (d_a + gradient[0] * d_j);
            out[2][column] = paths.blend * out[2][column] + near * (d_b + gradient[1] * d_j);
        }
        out
    }

    /// Derivative of the tint layer `(a', b')` with respect to `t`.
    pub fn d_tint_d_t(&self, t: f64) -> (f64, f64) {
        let (indices, _, derivative) = tint_weights(t);
        let mut a = 0.0;
        let mut b = 0.0;
        for (position, index) in indices.iter().enumerate() {
            a += derivative[position] * self.tint.a[*index];
            b += derivative[position] * self.tint.b[*index];
        }
        (a, b)
    }

    pub fn d2_tint_d_t2(&self, t: f64) -> (f64, f64) {
        let (indices, _, _, second) = crate::basis::tint_weights_second(t);
        let mut a = 0.0;
        let mut b = 0.0;
        for (position, index) in indices.iter().enumerate() {
            a += second[position] * self.tint.a[*index];
            b += second[position] * self.tint.b[*index];
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
                    let weight = outer * input.grl.hue_w[h] * input.grl.s_w[s] * input.grl.c_w[c];
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
        let basis = core_weights(input.neutral.a, input.neutral.b);
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
        let basis = core_weights(input.neutral.a, input.neutral.b);
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
            if degrees < 0.0 {
                degrees + 360.0
            } else {
                degrees
            }
        } else {
            0.0
        };
        let (hue_idx, hue_w) = hue_weights(h);
        let (s_idx, s_w) = s_weights(0.0);
        let (c_idx, c_w) = c_weights(c);
        let neutral = NeutralChroma { a, b };
        LookInput {
            xyz: Xyz::new(0.0, 0.18, 0.0),
            unit_xyz: Xyz::new(0.0, 1.0, 0.0),
            y: 0.18,
            s: 0.0,
            s_lookup: 0.0,
            h_in: h,
            c_in: c,
            a_in: a,
            b_in: b,
            neutral,
            blend: BlendWeights::new(neutral),
            residual_a: a,
            residual_b: b,
            margin: 1.0,
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
                indices: [0; 64],
                weights: [0.0; 64],
            },
        }
    }

    #[test]
    fn scan_margin_pieces() {
        let model = solve::reference_model();
        let j_limit = model.inverse_j_prime_limit();
        for j in [40.0_f64, 80.0, 120.0, 160.0, 180.0, 200.0, 220.0] {
            for radius in [60.0_f64, 120.0, 200.0, 300.0, 400.0] {
                for angle in 0..6 {
                    let theta = 2.0 * PI * angle as f64 / 6.0;
                    let pre = Ucs::new(j, radius * libm::cos(theta), radius * libm::sin(theta));
                    let jmh = ucs_to_jmh(pre, looklab_color::ucs::UcsSpace::Ucs);
                    let responses = model.inverse_responses(jmh);
                    let j_margin = (j_limit - j) / j_limit;
                    let channel: Vec<f64> = responses
                        .iter()
                        .map(|r| (400.0 - (r - 0.1).abs()) / 400.0)
                        .collect();
                    let best = channel.iter().cloned().fold(f64::INFINITY, f64::min);
                    if best < j_margin {
                        println!(
                            "CHANNEL WINS j {j} r {radius} angle {angle}: j_margin {j_margin:.4} channels {channel:?}"
                        );
                    }
                }
            }
        }
        for j in [40.0_f64, 80.0, 120.0, 160.0, 180.0, 200.0, 220.0] {
            for (a, b) in [
                (0.0, 0.0),
                (40.0, 0.0),
                (-40.0, 0.0),
                (0.0, 40.0),
                (0.0, -40.0),
                (30.0, 30.0),
            ] {
                let pre = Ucs::new(j, a, b);
                let jmh = ucs_to_jmh(pre, looklab_color::ucs::UcsSpace::Ucs);
                let responses = model.inverse_responses(jmh);
                let j_margin = (j_limit - j) / j_limit;
                let channel: Vec<f64> = responses
                    .iter()
                    .map(|r| (400.0 - (r - 0.1).abs()) / 400.0)
                    .collect();
                println!("j {j} ab ({a},{b}): j_margin {j_margin:.4} channels {channel:?}");
            }
        }
    }

    #[test]
    fn prediction_margin_gradient_matches_differences() {
        let model = solve::reference_model();
        let j_limit = model.inverse_j_prime_limit();
        let mut chromatic = 0;
        for pre in [
            Ucs::new(55.0, 30.0, -12.0),
            Ucs::new(80.0, -8.0, 22.0),
            Ucs::new(225.0, 40.0, -20.0),
            Ucs::new(200.0, -60.0, 30.0),
            Ucs::new(235.0, 5.0, 5.0),
            Ucs::new(200.0, -200.0, 0.0),
            Ucs::new(160.0, -120.0, 0.0),
        ] {
            let (margin, gradient) = prediction_margin(pre);
            if gradient[1] != 0.0 || gradient[2] != 0.0 {
                chromatic += 1;
            }
            println!(
                "pre {pre:?}: margin {margin:.6e} gradient {gradient:?} j_limit {j_limit:.4} j margin {:.6e}",
                (j_limit - pre.j) / j_limit
            );
            let h = 1e-6;
            for axis in 0..3 {
                let mut plus = pre.to_array();
                let mut minus = pre.to_array();
                plus[axis] += h;
                minus[axis] -= h;
                let (up, _) = prediction_margin(Ucs::from_array(plus));
                let (down, _) = prediction_margin(Ucs::from_array(minus));
                let numerical = (up - down) / (2.0 * h);
                let scale = numerical.abs().max(1.0);
                assert!(
                    (gradient[axis] - numerical).abs() / scale < 1e-5,
                    "pre {pre:?} axis {axis}: {} vs {}",
                    gradient[axis],
                    numerical
                );
            }
        }
        assert!(chromatic > 0, "no chromatic margin piece covered");
    }

    #[test]
    fn prediction_margin_hessian_matches_differences() {
        let white = solve::reference_white();
        let mut worst = 0.0_f64;
        for scale in [0.02_f64, 0.18, 0.6] {
            for (x, z) in [(1.0, 1.0), (0.9, 1.15), (1.1, 0.85)] {
                let xyz = Xyz::new(white.x * x * scale, white.y * scale, white.z * z * scale);
                let pre = solve::ucs_of_xyz(xyz);
                let (gradient, hessian) = prediction_margin_xyz_derivatives(pre, xyz);
                let margin_at = |offset: [f64; 3]| {
                    let moved = Xyz::new(xyz.x + offset[0], xyz.y + offset[1], xyz.z + offset[2]);
                    prediction_margin(solve::ucs_of_xyz(moved)).0
                };
                let h = 1e-6 * scale.max(0.05);
                let h2 = 1e-4 * scale.max(0.05);
                for a in 0..3 {
                    let mut step = [0.0_f64; 3];
                    step[a] = h;
                    let numerical =
                        (margin_at(step) - margin_at([-step[0], -step[1], -step[2]])) / (2.0 * h);
                    let scale = numerical.abs().max(1e-2);
                    let gap = (gradient[a] - numerical).abs() / scale;
                    if gap > worst {
                        worst = gap;
                        println!(
                            "xyz {xyz:?} gradient {a}: analytic {} numerical {numerical:.9e}",
                            gradient[a]
                        );
                    }
                    for b in 0..3 {
                        let mut both = [0.0_f64; 3];
                        both[a] = h2;
                        both[b] += h2;
                        let mut left = [0.0_f64; 3];
                        left[a] = h2;
                        left[b] -= h2;
                        let mut right = [0.0_f64; 3];
                        right[a] = -h2;
                        right[b] += h2;
                        let mut back = [0.0_f64; 3];
                        back[a] = -h2;
                        back[b] -= h2;
                        let numerical = (margin_at(both) - margin_at(left) - margin_at(right)
                            + margin_at(back))
                            / (4.0 * h2 * h2);
                        let analytic = hessian[a][b];
                        let scale = numerical.abs().max(1e-2);
                        let gap = (analytic - numerical).abs() / scale;
                        if gap > worst {
                            worst = gap;
                            println!(
                                "xyz {xyz:?} a {a} b {b}: analytic {analytic:.9e} numerical {numerical:.9e} margin {:.4}",
                                prediction_margin(pre).0
                            );
                        }
                    }
                }
            }
        }
        println!("prediction_margin_xyz_derivatives worst relative gap {worst:.3e}");
        assert!(worst < 1e-4, "worst relative gap {worst:.3e}");
    }

    #[test]
    fn path_second_matches_differences_of_the_first_derivative() {
        let look = Look::colorimetric_spline().unwrap();
        let input = LookInput::new(Xyz::new(0.31, 0.42, 0.12));
        let mut values = look.layer_values(&input);
        values.log_g = 0.08;
        values.r = 14.0;
        let (outer, inner) = look.path_second(&input, &values);
        let h = 1e-6;
        let shift = |index: usize, delta: f64| {
            let mut moved = values;
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
        let first_outer = |moved: &LayerValues| look.d_ucs_d_layer_values(&input, moved);
        let first_inner = |moved: &LayerValues| look.d_internal_d_layer_values(&input, moved);
        let mut worst = 0.0_f64;
        for k in 0..9 {
            let plus = shift(k, h);
            let minus = shift(k, -h);
            let outer_plus = first_outer(&plus);
            let outer_minus = first_outer(&minus);
            let inner_plus = first_inner(&plus);
            let inner_minus = first_inner(&minus);
            for l in 0..9 {
                for channel in 0..3 {
                    let numerical = (outer_plus[channel][l] - outer_minus[channel][l]) / (2.0 * h);
                    let analytic = outer[k * 9 + l][channel];
                    let scale = numerical.abs().max(1e-6);
                    let gap = (analytic - numerical).abs() / scale;
                    if gap > worst {
                        worst = gap;
                        println!(
                            "outer k {k} l {l} channel {channel}: analytic {analytic:.9e} numerical {numerical:.9e}"
                        );
                    }
                }
                for channel in 0..2 {
                    let numerical = (inner_plus[l][channel] - inner_minus[l][channel]) / (2.0 * h);
                    let analytic = inner[k * 9 + l][channel];
                    let scale = numerical.abs().max(1e-6);
                    let gap = (analytic - numerical).abs() / scale;
                    if gap > worst {
                        worst = gap;
                        println!(
                            "inner k {k} l {l} channel {channel}: analytic {analytic:.9e} numerical {numerical:.9e}"
                        );
                    }
                }
            }
        }
        println!("path_second worst relative gap {worst:.3e}");
        assert!(worst < 1e-5, "worst relative gap {worst:.3e}");
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
                    look.color.values(&synthetic_input(
                        radius * cos + t * cos,
                        radius * sin + t * sin,
                    ))
                };
                let [coarse_step, fine_step] = steps;
                let coarse = at(coarse_step);
                let coarse_back = at(-coarse_step);
                let fine = at(fine_step);
                let fine_back = at(-fine_step);
                for channel in 0..3 {
                    let coarse_derivative =
                        (coarse[channel] - coarse_back[channel]) / (2.0 * coarse_step);
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
