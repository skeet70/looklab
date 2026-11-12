// SPDX-License-Identifier: AGPL-3.0-or-later
//! Neutral-anchored domain penalty over the two fitting inverse paths
//! (docs/02 §3.9).
//!
//! The reference tone `T` and the shared output lightness `j` carry the upper
//! wall `-log1p(-x/limit)` and the toe-scoped lower term below the anchor
//! `j0`: `-log(x/j0) + (x/j0 - 1) - (x/j0 - 1)^2/2`, zero at and above the
//! anchor. Both use one fixed strength and the caller's sample weight (Fiacco &
//! McCormick, *Nonlinear Programming*, 1968, §3). The chroma response paths
//! carry the neutral-anchored Bregman divergence below.
// The negated comparisons reject NaN together with out-of-range values.
#![allow(clippy::neg_cmp_op_on_partial_ord)]

use core::fmt;
use std::sync::LazyLock;

use looklab_color::cam16::{Cam16Model, ResponsePathDerivatives, temporary_denominator};
use looklab_color::ucs::{Ucs, UcsSpace, ucs_to_jmh};

use crate::look::{ChromaPaths, LayerValues};
use crate::solve;

pub const DOMAIN_AXES: usize = 7;

/// Ordered coordinates of [`PenaltyDerivatives`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DomainAxis {
    Tone,
    Lightness,
    UcsA,
    UcsB,
    InternalA,
    InternalB,
    Blend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InversePath {
    Ucs,
    Internal,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DomainViolationKind {
    ReferenceTone { tone: f64, limit: f64 },
    Lightness { lightness: f64, limit: f64 },
    Response { response: f64, limit: f64 },
    ReconstructionDenominator { denominator: f64 },
    NonFinite,
}

/// A hard feasibility failure, independent of any penalty weight.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DomainViolation {
    pub kind: DomainViolationKind,
    /// `None` for tone and lightness.
    pub path: Option<InversePath>,
    /// `None` for tone, lightness and the reconstruction denominator.
    pub channel: Option<usize>,
    /// Smallest feasibility margin seen at the rejected point.
    pub minimum_margin: f64,
}

impl fmt::Display for DomainViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            DomainViolationKind::ReferenceTone { tone, limit } => {
                write!(formatter, "reference tone {tone} is outside (0, {limit})")
            }
            DomainViolationKind::Lightness { lightness, limit } => write!(
                formatter,
                "output lightness {lightness} is outside (0, {limit})"
            ),
            DomainViolationKind::Response { response, limit } => write!(
                formatter,
                "inverse response {response} is outside |R'a - 0.1| < {limit}"
            ),
            DomainViolationKind::ReconstructionDenominator { denominator } => write!(
                formatter,
                "opponent reconstruction denominator {denominator} is not positive and finite"
            ),
            DomainViolationKind::NonFinite => write!(formatter, "non-finite inverse intermediate"),
        }
    }
}

impl std::error::Error for DomainViolation {}

/// Value, gradient and Hessian of the domain penalty in [`DomainAxis`] order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PenaltyDerivatives {
    pub value: f64,
    pub gradient: [f64; DOMAIN_AXES],
    pub hessian: [[f64; DOMAIN_AXES]; DOMAIN_AXES],
    /// Positive Gauss-Newton part of `hessian`: the `phi''` and `b''` curvature
    /// through first derivatives only. Symmetric positive semidefinite.
    pub gn_hessian: [[f64; DOMAIN_AXES]; DOMAIN_AXES],
    /// Feasibility diagnostic; never differentiated.
    pub minimum_margin: f64,
}

/// Strict validation result for the published pre-clip appearance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PublishedInverse {
    pub appearance: Ucs,
    pub responses: [f64; 3],
    pub minimum_margin: f64,
}

fn violation(
    kind: DomainViolationKind,
    path: Option<InversePath>,
    channel: Option<usize>,
    minimum_margin: f64,
) -> DomainViolation {
    DomainViolation {
        kind,
        path,
        channel,
        minimum_margin,
    }
}

fn lightness_limit() -> f64 {
    solve::reference_model().inverse_j_prime_limit()
}

/// `J'` anchor of the toe-scoped lower term: the reference neutral at linear
/// output `0.03`, the clip prior mean.
static TOE_ANCHOR: LazyLock<f64> = LazyLock::new(|| {
    solve::reference_model()
        .ucs_j_and_dj_dy(solve::reference_white(), 0.03)
        .0
});

fn toe_anchor() -> f64 {
    *TOE_ANCHOR
}

const TOE_THRESHOLD: f64 = 1e-2;

/// `f(x) = -log(x) + (x - 1) - (x - 1)^2/2` below the anchor, zero above.
fn toe_value(x: f64) -> f64 {
    if x >= 1.0 {
        return 0.0;
    }
    let u = 1.0 - x;
    if u < TOE_THRESHOLD {
        let mut acc = 0.0;
        for k in (3..=10).rev() {
            acc = 1.0 / k as f64 + u * acc;
        }
        u * u * u * acc
    } else {
        -libm::log1p(-u) - u - 0.5 * u * u
    }
}

/// `f'(x) = -1/x + 2 - x` below the anchor, zero above.
fn toe_slope(x: f64) -> f64 {
    if x >= 1.0 {
        return 0.0;
    }
    let u = 1.0 - x;
    if u < TOE_THRESHOLD {
        let mut acc = 0.0;
        for _ in 0..8 {
            acc = 1.0 + u * acc;
        }
        -(u * u * acc)
    } else {
        -1.0 / x + 2.0 - x
    }
}

/// `f''(x) = 1/x^2 - 1` below the anchor, zero above.
fn toe_curvature(x: f64) -> f64 {
    if x >= 1.0 {
        return 0.0;
    }
    let u = 1.0 - x;
    if u < TOE_THRESHOLD {
        let mut acc = 9.0;
        for coefficient in (2..=8).rev() {
            acc = coefficient as f64 + u * acc;
        }
        u * acc
    } else {
        1.0 / (x * x) - 1.0
    }
}

/// Guard the tone consumed by the neutral-reference inverse.
pub fn check_reference_tone(values: &LayerValues) -> Result<(), DomainViolation> {
    let limit = lightness_limit();
    let tone = values.t;
    let margin = (limit - tone) / limit;
    if !tone.is_finite() || !(tone > 0.0) || !(tone < limit) {
        return Err(violation(
            DomainViolationKind::ReferenceTone { tone, limit },
            None,
            None,
            margin,
        ));
    }
    Ok(())
}

/// Guard the shared output lightness.
pub fn check_reference_lightness(lightness: f64) -> Result<(), DomainViolation> {
    let limit = lightness_limit();
    let margin = (limit - lightness) / limit;
    if !lightness.is_finite() || !(lightness > 0.0) || !(lightness < limit) {
        return Err(violation(
            DomainViolationKind::Lightness { lightness, limit },
            None,
            None,
            margin,
        ));
    }
    Ok(())
}

fn check_anchor(
    anchor: &ResponsePathDerivatives<1>,
    minimum: &mut f64,
) -> Result<(), DomainViolation> {
    for channel in 0..3 {
        let response = anchor.responses[channel];
        if !response.is_finite() {
            return Err(violation(
                DomainViolationKind::NonFinite,
                None,
                Some(channel),
                *minimum,
            ));
        }
        let margin =
            (Cam16Model::RESPONSE_LIMIT - (response - 0.1).abs()) / Cam16Model::RESPONSE_LIMIT;
        *minimum = minimum.min(margin);
        if !(margin > 0.0) {
            return Err(violation(
                DomainViolationKind::Response {
                    response,
                    limit: Cam16Model::RESPONSE_LIMIT,
                },
                None,
                Some(channel),
                *minimum,
            ));
        }
        if !anchor.first[channel][0].is_finite() || !anchor.second[channel][0][0].is_finite() {
            return Err(violation(
                DomainViolationKind::NonFinite,
                None,
                Some(channel),
                *minimum,
            ));
        }
    }
    Ok(())
}

fn check_path(
    path: InversePath,
    responses: &ResponsePathDerivatives<3>,
    minimum: &mut f64,
) -> Result<(), DomainViolation> {
    if matches!(path, InversePath::Internal)
        && (!responses.denominator.is_finite() || !(responses.denominator > 0.0))
    {
        return Err(violation(
            DomainViolationKind::ReconstructionDenominator {
                denominator: responses.denominator,
            },
            Some(path),
            None,
            *minimum,
        ));
    }
    let temporary = temporary_denominator(responses.responses);
    if !temporary.is_finite() || !(temporary > 0.0) {
        return Err(violation(
            DomainViolationKind::ReconstructionDenominator {
                denominator: temporary,
            },
            Some(path),
            None,
            *minimum,
        ));
    }
    for channel in 0..3 {
        let response = responses.responses[channel];
        if !response.is_finite() {
            return Err(violation(
                DomainViolationKind::NonFinite,
                Some(path),
                Some(channel),
                *minimum,
            ));
        }
        let margin =
            (Cam16Model::RESPONSE_LIMIT - (response - 0.1).abs()) / Cam16Model::RESPONSE_LIMIT;
        *minimum = minimum.min(margin);
        if !(margin > 0.0) {
            return Err(violation(
                DomainViolationKind::Response {
                    response,
                    limit: Cam16Model::RESPONSE_LIMIT,
                },
                Some(path),
                Some(channel),
                *minimum,
            ));
        }
        for axis in 0..3 {
            if !responses.first[channel][axis].is_finite() {
                return Err(violation(
                    DomainViolationKind::NonFinite,
                    Some(path),
                    Some(channel),
                    *minimum,
                ));
            }
            for other in 0..3 {
                if !responses.second[channel][axis][other].is_finite() {
                    return Err(violation(
                        DomainViolationKind::NonFinite,
                        Some(path),
                        Some(channel),
                        *minimum,
                    ));
                }
            }
        }
    }
    Ok(())
}

fn phi_second(q: f64) -> f64 {
    let square = q * q;
    2.0 * (1.0 + square) / ((1.0 - square) * (1.0 - square))
}

fn phi_third(q: f64) -> f64 {
    let square = q * q;
    4.0 * q * (3.0 + square) / ((1.0 - square) * (1.0 - square) * (1.0 - square))
}

/// `R(x) = x - log1p(x)`, evaluated by its series for small `|x|`.
pub(crate) fn bregman_remainder(x: f64) -> f64 {
    const THRESHOLD: f64 = 1e-2;
    if x.abs() < THRESHOLD {
        let mut acc = 0.0_f64;
        for k in (2..=10).rev() {
            let coefficient = if k % 2 == 0 {
                1.0 / k as f64
            } else {
                -1.0 / k as f64
            };
            acc = coefficient + x * acc;
        }
        x * x * acc
    } else {
        x - libm::log1p(x)
    }
}

fn bregman_divergence(q: f64, q0: f64) -> f64 {
    let delta = q - q0;
    bregman_remainder(-delta / (1.0 - q0)) + bregman_remainder(delta / (1.0 + q0))
}

#[derive(Clone, Copy, Debug, Default)]
struct PathPenalty {
    value: f64,
    gradient: [f64; 3],
    hessian: [[f64; 3]; 3],
    gn_hessian: [[f64; 3]; 3],
}

fn path_penalty(
    path: &ResponsePathDerivatives<3>,
    anchor: &ResponsePathDerivatives<1>,
) -> PathPenalty {
    let mut out = PathPenalty::default();
    for channel in 0..3 {
        let q = (path.responses[channel] - 0.1) / 400.0;
        let q0 = (anchor.responses[channel] - 0.1) / 400.0;
        let delta = q - q0;
        out.value += bregman_divergence(q, q0);

        let d_q = 2.0 * delta * (1.0 + q * q0) / ((1.0 - q * q) * (1.0 - q0 * q0));
        let d_q0 = -phi_second(q0) * delta;
        let d_qq = phi_second(q);
        let d_qq0 = -phi_second(q0);
        let d_q0q0 = phi_second(q0) - phi_third(q0) * delta;

        let q_x = [
            path.first[channel][0] / 400.0,
            path.first[channel][1] / 400.0,
            path.first[channel][2] / 400.0,
        ];
        let q0_x = [anchor.first[channel][0] / 400.0, 0.0, 0.0];

        for x in 0..3 {
            out.gradient[x] += d_q * q_x[x] + d_q0 * q0_x[x];
        }
        for x in 0..3 {
            for y in 0..3 {
                let q_xy = path.second[channel][x][y] / 400.0;
                let q0_xy = if x == 0 && y == 0 {
                    anchor.second[channel][0][0] / 400.0
                } else {
                    0.0
                };
                out.hessian[x][y] += d_qq * q_x[x] * q_x[y]
                    + d_q * q_xy
                    + d_qq0 * (q_x[x] * q0_x[y] + q_x[y] * q0_x[x])
                    + d_q0q0 * q0_x[x] * q0_x[y]
                    + d_q0 * q0_xy;
                out.gn_hessian[x][y] += d_qq * q_x[x] * q_x[y];
            }
        }
    }
    out
}

fn set_symmetric(
    matrix: &mut [[f64; DOMAIN_AXES]; DOMAIN_AXES],
    row: usize,
    column: usize,
    value: f64,
) {
    matrix[row][column] = value;
    matrix[column][row] = value;
}

/// Evaluate the penalty and its exact derivatives in the seven domain axes.
///
/// The value is `-log1p(-T/limit) - log1p(-j/limit) + toe(T/j0) + toe(j/j0)`
/// plus the weighted path Bregman divergences. The toe term contributes
/// `(-1/x + 2 - x)/j0` to the gradient and `(1/x^2 - 1)/j0^2` to the exact and
/// Gauss-Newton Hessians, zero at and above the anchor.
pub fn evaluate(
    values: &LayerValues,
    paths: &ChromaPaths,
) -> Result<PenaltyDerivatives, DomainViolation> {
    let limit = lightness_limit();
    let mut minimum = f64::INFINITY;

    let tone_margin = (limit - values.t) / limit;
    minimum = minimum.min(tone_margin);
    if !values.t.is_finite() || !(values.t > 0.0) || !(values.t < limit) {
        return Err(violation(
            DomainViolationKind::ReferenceTone {
                tone: values.t,
                limit,
            },
            None,
            None,
            minimum,
        ));
    }

    let lightness = paths.ucs.j;
    let lightness_margin = (limit - lightness) / limit;
    minimum = minimum.min(lightness_margin);
    if !lightness.is_finite() || !(lightness > 0.0) || !(lightness < limit) {
        return Err(violation(
            DomainViolationKind::Lightness { lightness, limit },
            None,
            None,
            minimum,
        ));
    }

    let model = solve::reference_model();
    let anchor = model.neutral_response_derivatives(lightness);
    check_anchor(&anchor, &mut minimum)?;
    let ucs = model.ucs_chroma_response_derivatives([paths.ucs.a, paths.ucs.b], lightness);
    let internal = model.opponent_chroma_response_derivatives(
        paths.internal,
        lightness,
        crate::look::neutral_scale(),
    );
    check_path(InversePath::Ucs, &ucs, &mut minimum)?;
    check_path(InversePath::Internal, &internal, &mut minimum)?;

    let outer = path_penalty(&ucs, &anchor);
    let inner = path_penalty(&internal, &anchor);
    let blend = paths.blend;
    let near = 1.0 - blend;

    let anchor_j = toe_anchor();
    let tone_x = values.t / anchor_j;
    let lightness_x = lightness / anchor_j;
    let tone_upper_slope = 1.0 / (limit - values.t);
    let tone_slope = tone_upper_slope + toe_slope(tone_x) / anchor_j;
    let tone_curvature =
        tone_upper_slope * tone_upper_slope + toe_curvature(tone_x) / (anchor_j * anchor_j);
    let lightness_upper_slope = 1.0 / (limit - lightness);
    let lightness_slope = lightness_upper_slope + toe_slope(lightness_x) / anchor_j;
    let lightness_curvature = lightness_upper_slope * lightness_upper_slope
        + toe_curvature(lightness_x) / (anchor_j * anchor_j);

    let value = -libm::log1p(-values.t / limit) - libm::log1p(-lightness / limit)
        + toe_value(tone_x)
        + toe_value(lightness_x)
        + near * inner.value
        + blend * outer.value;

    let mut gradient = [0.0_f64; DOMAIN_AXES];
    let mut hessian = [[0.0_f64; DOMAIN_AXES]; DOMAIN_AXES];
    let mut gn_hessian = [[0.0_f64; DOMAIN_AXES]; DOMAIN_AXES];

    let tone = DomainAxis::Tone as usize;
    let light = DomainAxis::Lightness as usize;
    let ucs_a = DomainAxis::UcsA as usize;
    let ucs_b = DomainAxis::UcsB as usize;
    let inner_a = DomainAxis::InternalA as usize;
    let inner_b = DomainAxis::InternalB as usize;
    let blend_axis = DomainAxis::Blend as usize;

    gradient[tone] = tone_slope;
    gradient[light] = lightness_slope + near * inner.gradient[0] + blend * outer.gradient[0];
    gradient[ucs_a] = blend * outer.gradient[1];
    gradient[ucs_b] = blend * outer.gradient[2];
    gradient[inner_a] = near * inner.gradient[1];
    gradient[inner_b] = near * inner.gradient[2];
    gradient[blend_axis] = outer.value - inner.value;

    set_symmetric(&mut hessian, tone, tone, tone_curvature);
    set_symmetric(
        &mut hessian,
        light,
        light,
        lightness_curvature + near * inner.hessian[0][0] + blend * outer.hessian[0][0],
    );
    set_symmetric(&mut hessian, light, ucs_a, blend * outer.hessian[0][1]);
    set_symmetric(&mut hessian, light, ucs_b, blend * outer.hessian[0][2]);
    set_symmetric(&mut hessian, light, inner_a, near * inner.hessian[0][1]);
    set_symmetric(&mut hessian, light, inner_b, near * inner.hessian[0][2]);
    set_symmetric(&mut hessian, ucs_a, ucs_a, blend * outer.hessian[1][1]);
    set_symmetric(&mut hessian, ucs_a, ucs_b, blend * outer.hessian[1][2]);
    set_symmetric(&mut hessian, ucs_b, ucs_b, blend * outer.hessian[2][2]);
    set_symmetric(&mut hessian, inner_a, inner_a, near * inner.hessian[1][1]);
    set_symmetric(&mut hessian, inner_a, inner_b, near * inner.hessian[1][2]);
    set_symmetric(&mut hessian, inner_b, inner_b, near * inner.hessian[2][2]);
    set_symmetric(
        &mut hessian,
        blend_axis,
        light,
        outer.gradient[0] - inner.gradient[0],
    );
    set_symmetric(&mut hessian, blend_axis, ucs_a, outer.gradient[1]);
    set_symmetric(&mut hessian, blend_axis, ucs_b, outer.gradient[2]);
    set_symmetric(&mut hessian, blend_axis, inner_a, -inner.gradient[1]);
    set_symmetric(&mut hessian, blend_axis, inner_b, -inner.gradient[2]);

    set_symmetric(&mut gn_hessian, tone, tone, tone_curvature);
    set_symmetric(
        &mut gn_hessian,
        light,
        light,
        lightness_curvature + near * inner.gn_hessian[0][0] + blend * outer.gn_hessian[0][0],
    );
    set_symmetric(
        &mut gn_hessian,
        light,
        ucs_a,
        blend * outer.gn_hessian[0][1],
    );
    set_symmetric(
        &mut gn_hessian,
        light,
        ucs_b,
        blend * outer.gn_hessian[0][2],
    );
    set_symmetric(
        &mut gn_hessian,
        light,
        inner_a,
        near * inner.gn_hessian[0][1],
    );
    set_symmetric(
        &mut gn_hessian,
        light,
        inner_b,
        near * inner.gn_hessian[0][2],
    );
    set_symmetric(
        &mut gn_hessian,
        ucs_a,
        ucs_a,
        blend * outer.gn_hessian[1][1],
    );
    set_symmetric(
        &mut gn_hessian,
        ucs_a,
        ucs_b,
        blend * outer.gn_hessian[1][2],
    );
    set_symmetric(
        &mut gn_hessian,
        ucs_b,
        ucs_b,
        blend * outer.gn_hessian[2][2],
    );
    set_symmetric(
        &mut gn_hessian,
        inner_a,
        inner_a,
        near * inner.gn_hessian[1][1],
    );
    set_symmetric(
        &mut gn_hessian,
        inner_a,
        inner_b,
        near * inner.gn_hessian[1][2],
    );
    set_symmetric(
        &mut gn_hessian,
        inner_b,
        inner_b,
        near * inner.gn_hessian[2][2],
    );

    let finite = value.is_finite()
        && gradient.iter().all(|value| value.is_finite())
        && hessian.iter().flatten().all(|value| value.is_finite())
        && gn_hessian.iter().flatten().all(|value| value.is_finite());
    if !finite {
        return Err(violation(
            DomainViolationKind::NonFinite,
            None,
            None,
            minimum,
        ));
    }

    Ok(PenaltyDerivatives {
        value,
        gradient,
        hessian,
        gn_hessian,
        minimum_margin: minimum,
    })
}

fn check_published_responses(
    appearance: Ucs,
    responses: [f64; 3],
    mut minimum: f64,
) -> Result<PublishedInverse, DomainViolation> {
    let temporary = temporary_denominator(responses);
    if !temporary.is_finite() || !(temporary > 0.0) {
        return Err(violation(
            DomainViolationKind::ReconstructionDenominator {
                denominator: temporary,
            },
            Some(InversePath::Ucs),
            None,
            minimum,
        ));
    }
    for (channel, &response) in responses.iter().enumerate() {
        if !response.is_finite() {
            return Err(violation(
                DomainViolationKind::NonFinite,
                Some(InversePath::Ucs),
                Some(channel),
                minimum,
            ));
        }
        let margin =
            (Cam16Model::RESPONSE_LIMIT - (response - 0.1).abs()) / Cam16Model::RESPONSE_LIMIT;
        minimum = minimum.min(margin);
        if !(margin > 0.0) {
            return Err(violation(
                DomainViolationKind::Response {
                    response,
                    limit: Cam16Model::RESPONSE_LIMIT,
                },
                Some(InversePath::Ucs),
                Some(channel),
                minimum,
            ));
        }
    }
    Ok(PublishedInverse {
        appearance,
        responses,
        minimum_margin: minimum,
    })
}

/// Validate the published pre-clip appearance's inverse response, value only.
pub fn check_published_inverse(appearance: Ucs) -> Result<PublishedInverse, DomainViolation> {
    let limit = lightness_limit();
    let lightness = appearance.j;
    let margin = (limit - lightness) / limit;
    if !lightness.is_finite() || !(lightness > 0.0) || !(lightness < limit) {
        return Err(violation(
            DomainViolationKind::Lightness { lightness, limit },
            None,
            None,
            margin,
        ));
    }
    let jmh = ucs_to_jmh(appearance, UcsSpace::Ucs);
    if !jmh.j.is_finite() || !jmh.m.is_finite() || !jmh.h.is_finite() {
        return Err(violation(
            DomainViolationKind::NonFinite,
            Some(InversePath::Ucs),
            None,
            margin,
        ));
    }
    let responses = solve::reference_model().inverse_responses(jmh);
    check_published_responses(appearance, responses, margin)
}

/// Validate the inverse of `paths.published()`, the appearance-before-inversion blend.
pub fn check_published_paths(paths: &ChromaPaths) -> Result<PublishedInverse, DomainViolation> {
    check_published_inverse(paths.published())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bregman_remainder_keeps_small_arguments() {
        for x in [1e-16, 1e-14, 1e-12, 1e-10, 1e-8, 1e-6, 1e-4, 1e-3] {
            let value = bregman_remainder(x);
            let leading = 0.5 * x * x;
            assert!(value > 0.0, "x {x:e}: {value:e}");
            assert!(
                (value - leading).abs() / leading < 2e-3,
                "x {x:e}: {value:e} vs {leading:e}"
            );
        }
        let x = 1e-16;
        let naive = x - libm::log1p(x);
        assert!(naive.abs() < 0.25 * x * x, "naive remainder {naive:e}");
        assert!(bregman_remainder(x) > 0.0);
    }

    #[test]
    fn bregman_divergence_matches_the_three_term_form() {
        let phi = |q: f64| -libm::log(1.0 - q * q);
        let phi_prime = |q: f64| 2.0 * q / (1.0 - q * q);
        for (q, q0) in [(0.2, 0.1), (-0.3, 0.2), (0.5, -0.4), (-0.7, -0.2)] {
            let direct = phi(q) - phi(q0) - phi_prime(q0) * (q - q0);
            let value = bregman_divergence(q, q0);
            assert!(
                (value - direct).abs() / direct.abs().max(1e-12) < 1e-12,
                "q {q} q0 {q0}: {value:e} vs {direct:e}"
            );
        }
    }

    #[test]
    fn bregman_divergence_matches_its_small_delta_limit() {
        let q0 = 0.2;
        for delta in [1e-12, 1e-10, 1e-8, 1e-6] {
            let q = q0 + delta;
            let value = bregman_divergence(q, q0);
            let limit = phi_second(q0) * delta * delta / 2.0;
            assert!(
                (value - limit).abs() / limit < 1e-3,
                "delta {delta:e}: {value:e} vs {limit:e}"
            );
        }
        assert_eq!(bregman_divergence(q0, q0), 0.0);
    }

    fn toe_series_value(u: f64) -> f64 {
        let mut acc = 0.0;
        for k in (3..=10).rev() {
            acc = 1.0 / k as f64 + u * acc;
        }
        u * u * u * acc
    }

    #[test]
    fn toe_series_matches_direct_forms() {
        for u in [1e-12, 1e-10, 1e-8, 1e-6, 1e-4, 1e-3] {
            let x = 1.0 - u;
            let u = 1.0 - x;
            let expected = toe_series_value(u);
            let value = toe_value(x);
            let scale = expected.abs().max(1e-300);
            assert!(
                (value - expected).abs() / scale < 1e-12,
                "u {u:e}: value {value:e} vs {expected:e}"
            );
            let expected_slope = -(u * u * (1.0 + u + u * u + u * u * u));
            let slope = toe_slope(x);
            let scale = expected_slope.abs().max(1e-300);
            assert!(
                (slope - expected_slope).abs() / scale < 1e-9,
                "u {u:e}: slope {slope:e} vs {expected_slope:e}"
            );
            let expected_curvature = u * (2.0 + 3.0 * u + 4.0 * u * u + 5.0 * u * u * u);
            let curvature = toe_curvature(x);
            let scale = expected_curvature.abs().max(1e-300);
            assert!(
                (curvature - expected_curvature).abs() / scale < 1e-9,
                "u {u:e}: curvature {curvature:e} vs {expected_curvature:e}"
            );
        }
        for u in [0.05, 0.1, 0.3] {
            let x = 1.0 - u;
            let direct = -libm::log1p(-u) - u - 0.5 * u * u;
            let value = toe_value(x);
            let scale = direct.abs().max(1e-300);
            assert!(
                (value - direct).abs() / scale < 1e-12,
                "u {u:e}: value {value:e} vs {direct:e}"
            );
        }
    }

    #[test]
    fn toe_is_zero_above_the_anchor() {
        for x in [1.0, 1.1, 1.5, 2.0] {
            assert_eq!(toe_value(x), 0.0);
            assert_eq!(toe_slope(x), 0.0);
            assert_eq!(toe_curvature(x), 0.0);
        }
    }

    #[test]
    fn toe_is_continuous_at_the_anchor() {
        let near = 1.0 - 1e-12;
        assert!(toe_value(near) < 1e-36);
        assert!(toe_slope(near).abs() < 1e-23);
        assert!(toe_curvature(near).abs() < 1e-11);
        assert_eq!(toe_value(1.0), 0.0);
        assert_eq!(toe_slope(1.0), 0.0);
        assert_eq!(toe_curvature(1.0), 0.0);
        for x in [0.9, 0.99] {
            let step = 1e-6;
            let numerical = (toe_value(x + step) - toe_value(x - step)) / (2.0 * step);
            let analytic = toe_slope(x);
            let scale = analytic.abs().max(numerical.abs()).max(1e-12);
            assert!(
                (analytic - numerical).abs() / scale < 1e-6,
                "x {x}: slope {analytic:e} vs {numerical:e}"
            );
            let numerical = (toe_slope(x + step) - toe_slope(x - step)) / (2.0 * step);
            let analytic = toe_curvature(x);
            let scale = analytic.abs().max(numerical.abs()).max(1e-12);
            assert!(
                (analytic - numerical).abs() / scale < 1e-6,
                "x {x}: curvature {analytic:e} vs {numerical:e}"
            );
        }
    }

    #[test]
    fn toe_push_ratio_falls_with_the_anchor_distance() {
        let anchor = toe_anchor();
        for (lightness, expected) in [(10.0, 1.0 / 3.0), (15.0, 1.0 / 9.0)] {
            let ratio = (toe_slope(lightness / anchor) / anchor) / (-1.0 / lightness);
            let closed = (1.0 - lightness / anchor) * (1.0 - lightness / anchor);
            assert!(
                (ratio - closed).abs() < 1e-12,
                "J' {lightness}: {ratio} vs {closed}"
            );
            assert!(
                (ratio - expected).abs() < 0.03,
                "J' {lightness}: ratio {ratio} vs {expected}"
            );
            println!("toe push ratio at J' {lightness}: {ratio:.9e} (bare-term fraction)");
        }
    }
}
