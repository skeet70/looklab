// SPDX-License-Identifier: AGPL-3.0-or-later
//! Strict UCS inverse → linear-RGB soft clip → UCS composite and its first
//! derivative.
//!
//! The composite is `F ∘ C ∘ P`: `P` is the CAM16-UCS inverse of the pre-clip
//! appearance, `C` the per-channel soft clip in the output space's linear RGB
//! (docs/02 §3.4) and `F` the reference CAM16-UCS forward model. The stages do
//! not compose as a Jacobian product at a pre-clip neutral: `P`'s chromatic
//! derivative is zero there (the response departure is `O(|z|^(10/9))`), `F`'s
//! is unbounded (its chroma scales as `|z|^(9/10)`), and `F ∘ P` is the
//! identity.
//!
//! At a pre-clip neutral whose clipped output is also the reference neutral,
//! with clip slope `a = c'(Y)`, pre-clip neutral luminance `Y` and post-clip
//! neutral luminance `Yc`:
//!
//! ```text
//! ∂J'/∂J'₀ = a·g'(Yc)/g'(Y)
//! ∂a'/∂a'₀ = ∂b'/∂b'₀ = a^(9/10)·(s_c/s)^(9/10)·(J_c/J)^(1/2)·(D/D_c)^(9/10)
//! ```
//!
//! `g` is the neutral `J'(Y)` curve, `s = dR'a/dR'c` the response-compression
//! slope, `J` the CAM16 lightness and `D = temporary_denominator(R'a)` at each
//! neutral; the cross terms vanish. Both slopes are `1` with the clip inactive.
//!
//! The chromatic map is `λ·z + (c2/2)·λ·(1-λ)·|z|z + O(|z|^(19/9))`: gradient continuous and
//! locally Lipschitz, classical Hessian undefined at the crossing, fit curvature Gauss-Newton.
//!
//! A chromatic input whose clipped output is near the neutral axis is evaluated in
//! departure coordinates; only an appearance exactly on the axis is singular.

use looklab_color::ucs::Ucs;
use looklab_color::{Mat3, Rgb, RgbSpace, WhiteRelativeXyz, Xyz};

use crate::clip::{
    preclip_xyz_jacobian, soft_clip, soft_clip_departure, soft_clip_derivative,
    soft_clip_k_derivative,
};
use crate::domain_penalty::{DomainViolation, check_published_inverse};
use crate::look::{ucs_xyz_jacobian, ucs_xyz_jacobian_departure};
use crate::solve;

/// `∂(post-clip J'a'b')/∂(pre-clip J'a'b')`.
pub type CompositeJacobian = [[f64; 3]; 3];

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CompositeRegularity {
    /// Both points are chromatic; the stage Jacobians compose.
    Smooth,
    /// Pre-clip and post-clip are both the reference neutral.
    NeutralCrossing {
        clip_active: bool,
        /// Clip slope at the neutral channel; `1` with the clip inactive.
        clip_slope: f64,
        /// Chromatic slope; `1` with the clip inactive.
        chroma_slope: f64,
    },
    /// Pre-clip neutral whose clipped output is chromatic: the inverse's zero
    /// chromatic derivative multiplies a smooth forward map.
    NeutralInverse,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CompositeSingularity {
    /// A chromatic pre-clip point whose post-clip appearance is exactly on the neutral
    /// axis, where the forward UCS chroma scales as `|z|^(9/10)`.
    ForwardNeutral,
    /// A derivative row that is not finite, so the composed response is undefined.
    NonFinite,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CompositeDerivative {
    Finite {
        jacobian: CompositeJacobian,
        /// `∂(post-clip J'a'b')/∂k`.
        jacobian_k: [f64; 3],
        regularity: CompositeRegularity,
    },
    Singular(CompositeSingularity),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompositeChart {
    /// Absolute XYZ with the ordinary forward functions.
    Absolute,
    /// White-relative departure with the departure forward functions.
    WhiteRelative,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompositePoint {
    /// Post-clip appearance.
    pub value: Ucs,
    pub xyz_post: Xyz,
    /// White-relative departure; used when `chart` is `WhiteRelative`.
    pub xyz_departure: WhiteRelativeXyz,
    /// Coordinate chart that produced `value` and the finite derivative.
    pub chart: CompositeChart,
    /// `d(pre-clip XYZ)/d(pre-clip J'a'b')`.
    pub preclip_jacobian: [[f64; 3]; 3],
    /// Raw forward `d(J'a'b')/dXYZ` from the selected chart; NaN at an exact neutral
    /// post, where `derivative` remains the finite composite derivative.
    pub forward_jacobian: [[f64; 3]; 3],
    /// Pre-clip linear RGB channels.
    pub channels: [f64; 3],
    /// Per-channel `c'(x)`.
    pub clip_slope: [f64; 3],
    pub derivative: CompositeDerivative,
    /// Smallest strict inverse feasibility margin.
    pub minimum_margin: f64,
}

/// # Errors
///
/// [`DomainViolation`] when the pre-clip inverse is not strictly feasible.
pub fn evaluate(
    pre: Ucs,
    output_space: RgbSpace,
    k: f64,
) -> Result<CompositePoint, DomainViolation> {
    let strict = check_published_inverse(pre)?;
    let (xyz_pre, preclip) = preclip_xyz_jacobian(pre);
    let channels = xyz_channels(output_space, xyz_pre);
    let clipped = channels.map(|value| soft_clip(value, k));
    let xyz_post = output_space.rgb_linear_to_xyz(Rgb::new(clipped[0], clipped[1], clipped[2]));
    let clip_slope = channels.map(|value| soft_clip_derivative(value, k));
    let clip = output_space
        .to_xyz_matrix()
        .mul(&Mat3::diag(clip_slope))
        .mul(&output_space.from_xyz_matrix());
    let clip_k = output_space
        .to_xyz_matrix()
        .mul_vec(channels.map(|value| soft_clip_k_derivative(value, k)));

    let white = solve::reference_white();
    let xyz_departure = if output_space.white() == white {
        WhiteRelativeXyz::from_array(
            output_space
                .to_xyz_matrix()
                .mul_vec(channels.map(|value| soft_clip_departure(value, k))),
        )
    } else {
        WhiteRelativeXyz::between(xyz_post, white)
    };

    // White-relative coordinates retain sub-ULP clipped departures.
    let norm_inf = |value: [f64; 3]| {
        value
            .iter()
            .fold(0.0_f64, |largest, entry| largest.max(entry.abs()))
    };
    let (chart, value, rows) =
        if norm_inf(xyz_departure.to_array()) <= norm_inf(xyz_post.to_array()) {
            let (value, rows) = ucs_xyz_jacobian_departure(xyz_departure);
            (CompositeChart::WhiteRelative, value, rows)
        } else {
            let (value, rows) = ucs_xyz_jacobian(xyz_post);
            (CompositeChart::Absolute, value, rows)
        };

    // The inverse resolves no chroma when its reconstructed responses are equal, so such
    // an input is the neutral at the composition's resolution.
    let neutral_at_resolution = output_space.white() == white
        && strict.responses[0] == strict.responses[1]
        && strict.responses[1] == strict.responses[2];
    let derivative = if (pre.a == 0.0 && pre.b == 0.0) || neutral_at_resolution {
        checked(neutral_derivative(
            &Stages {
                xyz_pre,
                xyz_post,
                channels,
                preclip: Mat3(preclip),
                clip,
                clip_k,
            },
            output_space,
            k,
        ))
    } else if value.a == 0.0 && value.b == 0.0 {
        CompositeDerivative::Singular(CompositeSingularity::ForwardNeutral)
    } else {
        finite_derivative(
            Mat3(rows).mul(&clip).mul(&Mat3(preclip)).0,
            Mat3(rows).mul_vec(clip_k),
            CompositeRegularity::Smooth,
        )
    };
    Ok(CompositePoint {
        value,
        xyz_post,
        xyz_departure,
        chart,
        preclip_jacobian: preclip,
        forward_jacobian: rows,
        channels,
        clip_slope,
        derivative,
        minimum_margin: strict.minimum_margin,
    })
}

struct Stages {
    xyz_pre: Xyz,
    xyz_post: Xyz,
    channels: [f64; 3],
    preclip: Mat3,
    clip: Mat3,
    clip_k: [f64; 3],
}

fn checked(derivative: CompositeDerivative) -> CompositeDerivative {
    match derivative {
        CompositeDerivative::Finite {
            jacobian,
            jacobian_k,
            regularity,
        } => finite_derivative(jacobian, jacobian_k, regularity),
        other => other,
    }
}

fn finite_derivative(
    jacobian: CompositeJacobian,
    jacobian_k: [f64; 3],
    regularity: CompositeRegularity,
) -> CompositeDerivative {
    if jacobian.iter().flatten().all(|value| value.is_finite())
        && jacobian_k.iter().all(|value| value.is_finite())
    {
        CompositeDerivative::Finite {
            jacobian,
            jacobian_k,
            regularity,
        }
    } else {
        CompositeDerivative::Singular(CompositeSingularity::NonFinite)
    }
}

fn neutral_derivative(stages: &Stages, output_space: RgbSpace, k: f64) -> CompositeDerivative {
    let Stages {
        xyz_pre,
        xyz_post,
        channels,
        preclip,
        clip,
        clip_k,
    } = stages;
    let active = channels.iter().any(|value| !(k..=1.0 - k).contains(value));
    if !active {
        return CompositeDerivative::Finite {
            jacobian: Mat3::IDENTITY.0,
            jacobian_k: [0.0; 3],
            regularity: CompositeRegularity::NeutralCrossing {
                clip_active: false,
                clip_slope: 1.0,
                chroma_slope: 1.0,
            },
        };
    }
    if output_space.white() != solve::reference_white() {
        let rows = Mat3(ucs_xyz_jacobian(*xyz_post).1);
        return CompositeDerivative::Finite {
            jacobian: rows.mul(clip).mul(preclip).0,
            jacobian_k: rows.mul_vec(*clip_k),
            regularity: CompositeRegularity::NeutralInverse,
        };
    }
    let slope = soft_clip_derivative(xyz_pre.y, k);
    let chroma = chroma_slope(*xyz_pre, *xyz_post, slope);
    let lightness = lightness_slope(xyz_pre.y, xyz_post.y, slope);
    CompositeDerivative::Finite {
        jacobian: [
            [lightness, 0.0, 0.0],
            [0.0, chroma, 0.0],
            [0.0, 0.0, chroma],
        ],
        jacobian_k: [
            neutral_curve_slope(xyz_post.y) * soft_clip_k_derivative(xyz_pre.y, k),
            0.0,
            0.0,
        ],
        regularity: CompositeRegularity::NeutralCrossing {
            clip_active: true,
            clip_slope: slope,
            chroma_slope: chroma,
        },
    }
}

/// `a^(9/10)·(s_c/s)^(9/10)·(J_c/J)^(1/2)·(D/D_c)^(9/10)`.
fn chroma_slope(xyz_pre: Xyz, xyz_post: Xyz, clip_slope: f64) -> f64 {
    let before = neutral_state(xyz_pre);
    let after = neutral_state(xyz_post);
    libm::pow(clip_slope, 0.9)
        * libm::pow(after.response_slope / before.response_slope, 0.9)
        * (after.lightness / before.lightness).sqrt()
        * libm::pow(before.denominator / after.denominator, 0.9)
}

/// `a·g'(Yc)/g'(Y)` with `g` the neutral `J'(Y)` curve.
fn lightness_slope(y_before: f64, y_after: f64, clip_slope: f64) -> f64 {
    clip_slope * neutral_curve_slope(y_after) / neutral_curve_slope(y_before)
}

fn neutral_curve_slope(y: f64) -> f64 {
    crate::deriv::reference_cam16().neutral_j_and_dj_dy(y).1
}

struct NeutralState {
    response_slope: f64,
    lightness: f64,
    denominator: f64,
}

fn neutral_state(xyz: Xyz) -> NeutralState {
    let opponent = solve::reference_model().cone_opponent(xyz);
    NeutralState {
        response_slope: response_slope(opponent.rgb_c[0], opponent.rgb_a[0]),
        lightness: opponent.lightness,
        denominator: opponent.denominator,
    }
}

/// `dR'a/dR'c` of the response compression, recovered from the value pair.
///
/// With `f = (F_L·R'c/100)^0.42` and `R'a = 400·f/(27.13 + f) + 0.1`, the
/// inverse `f = 27.13·(R'a − 0.1)/(400.1 − R'a)` gives
/// `dR'a/dR'c = 0.42·(R'a − 0.1)·(400.1 − R'a)/(400·R'c)`.
fn response_slope(rgb_c: f64, rgb_a: f64) -> f64 {
    0.42 * (rgb_a - 0.1) * (400.1 - rgb_a) / (400.0 * rgb_c)
}

fn xyz_channels(space: RgbSpace, xyz: Xyz) -> [f64; 3] {
    let rgb = space.xyz_to_rgb_linear(xyz);
    [rgb.r, rgb.g, rgb.b]
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: f64 = 0.03;

    fn point(pre: Ucs, space: RgbSpace, k: f64) -> CompositePoint {
        evaluate(pre, space, k).expect("strict inverse")
    }

    fn finite(point: &CompositePoint) -> (CompositeJacobian, [f64; 3], CompositeRegularity) {
        match point.derivative {
            CompositeDerivative::Finite {
                jacobian,
                jacobian_k,
                regularity,
            } => (jacobian, jacobian_k, regularity),
            other => panic!("expected a finite derivative, got {other:?}"),
        }
    }

    fn numerical(pre: Ucs, space: RgbSpace, k: f64, step: f64) -> CompositeJacobian {
        let mut out = [[0.0_f64; 3]; 3];
        for axis in 0..3 {
            for sign in [1.0_f64, -1.0] {
                let mut moved = pre.to_array();
                moved[axis] += sign * step;
                let value = point(Ucs::from_array(moved), space, k).value.to_array();
                for row in 0..3 {
                    out[row][axis] += sign * value[row] / (2.0 * step);
                }
            }
        }
        out
    }

    fn worst_gap(analytic: &CompositeJacobian, numerical: &CompositeJacobian) -> f64 {
        let mut worst = 0.0_f64;
        for row in 0..3 {
            for column in 0..3 {
                worst = worst.max((analytic[row][column] - numerical[row][column]).abs());
            }
        }
        worst
    }

    fn channel_spread(channels: [f64; 3]) -> f64 {
        channels.iter().cloned().fold(f64::MIN, f64::max)
            - channels.iter().cloned().fold(f64::MAX, f64::min)
    }

    #[test]
    fn unclipped_neutral_composite_has_the_identity_derivative() {
        for space in [RgbSpace::Srgb, RgbSpace::AdobeRgb] {
            for lightness in [30.0, 50.0, 70.0, 90.0] {
                let pre = Ucs::new(lightness, 0.0, 0.0);
                let point = point(pre, space, K);
                let (jacobian, jacobian_k, regularity) = finite(&point);
                assert_eq!(
                    jacobian,
                    Mat3::IDENTITY.0,
                    "{} at J' {lightness}: {jacobian:?}",
                    space.name()
                );
                assert_eq!(jacobian_k, [0.0; 3]);
                assert!(matches!(
                    regularity,
                    CompositeRegularity::NeutralCrossing {
                        clip_active: false,
                        ..
                    }
                ));
                let value = point.value.to_array();
                let expected = pre.to_array();
                for channel in 0..3 {
                    assert!(
                        (value[channel] - expected[channel]).abs() < 1e-9,
                        "{} at J' {lightness}: {value:?} vs {expected:?}",
                        space.name()
                    );
                }
            }
        }
    }

    #[test]
    fn roundoff_neutral_preclip_takes_the_crossing_limit() {
        for space in [RgbSpace::Srgb, RgbSpace::AdobeRgb] {
            for lightness in [8.0, 99.5] {
                let (neutral, _, regularity) =
                    finite(&point(Ucs::new(lightness, 0.0, 0.0), space, K));
                assert!(
                    matches!(
                        regularity,
                        CompositeRegularity::NeutralCrossing {
                            clip_active: true,
                            ..
                        }
                    ),
                    "{} at J' {lightness}: {regularity:?}",
                    space.name()
                );
                for chroma in [1e-20_f64, -1e-20, 1e-18, -1e-18, 1e-16, -1e-16] {
                    let (jacobian, _, _) =
                        finite(&point(Ucs::new(lightness, chroma, chroma), space, K));
                    for row in 0..3 {
                        for column in 0..3 {
                            assert!(
                                (jacobian[row][column] - neutral[row][column]).abs() < 1e-9,
                                "{} at J' {lightness} chroma {chroma:e}: row {row} column {column}",
                                space.name()
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn reference_neutral_preserving_spaces_map_the_neutral_to_equal_channels() {
        for space in [RgbSpace::Srgb, RgbSpace::AdobeRgb] {
            for lightness in [14.0, 50.0, 99.5] {
                let point = point(Ucs::new(lightness, 0.0, 0.0), space, K);
                let spread = channel_spread(point.channels);
                assert!(
                    spread.abs() < 1e-14,
                    "{} at J' {lightness}: channels {:?} spread {spread:e}",
                    space.name(),
                    point.channels
                );
            }
        }
        let prophoto = point(Ucs::new(50.0, 0.0, 0.0), RgbSpace::ProPhoto, K);
        assert!(
            channel_spread(prophoto.channels) > 1e-3,
            "ProPhoto keeps the reference neutral chromatic: {:?}",
            prophoto.channels
        );
    }

    #[test]
    fn clipped_neutral_composite_matches_the_best_central_difference_on_the_step_ladder() {
        // C1 crossing: central-difference truncation trades against the value's
        // f64 resolution.
        for space in [RgbSpace::Srgb, RgbSpace::AdobeRgb] {
            for lightness in [8.0, 14.0, 20.0, 99.5] {
                let pre = Ucs::new(lightness, 0.0, 0.0);
                let (jacobian, _, regularity) = finite(&point(pre, space, K));
                let CompositeRegularity::NeutralCrossing {
                    clip_active,
                    clip_slope,
                    chroma_slope,
                } = regularity
                else {
                    panic!("{} at J' {lightness}: {regularity:?}", space.name());
                };
                assert!(clip_active, "{} at J' {lightness}", space.name());
                assert!(clip_slope.is_finite() && chroma_slope.is_finite());
                assert_eq!(jacobian[1][1], chroma_slope);
                assert_eq!(jacobian[2][2], chroma_slope);
                assert_eq!(jacobian[1][2], 0.0);
                assert_eq!(jacobian[2][1], 0.0);
                assert!(jacobian[0][0].is_finite());

                let mut best = f64::INFINITY;
                let mut best_step = 0.0_f64;
                for step in [1e-3, 3e-4, 1e-4, 3e-5, 1e-5, 1e-6] {
                    let gap = worst_gap(&jacobian, &numerical(pre, space, K, step));
                    println!(
                        "{} J' {lightness} step {step:e}: worst |analytic - central| {gap:e}",
                        space.name()
                    );
                    if gap < best {
                        best = gap;
                        best_step = step;
                    }
                }
                println!(
                    "{} J' {lightness}: best step {best_step:e} worst |analytic - central| {best:e}",
                    space.name()
                );
                assert!(
                    best < 1e-6,
                    "{} at J' {lightness}: best gap {best:e} at step {best_step:e}",
                    space.name()
                );
            }
        }
    }

    #[test]
    fn dark_neutral_stays_achromatic_on_both_value_sides() {
        let base = point(Ucs::new(8.0, 0.0, 0.0), RgbSpace::Srgb, K);
        assert_eq!(base.value.a, 0.0);
        assert_eq!(base.value.b, 0.0);
        let (jacobian, _, _) = finite(&base);
        for h in [1e-5, 1e-6] {
            let plus = point(Ucs::new(8.0, h, 0.0), RgbSpace::Srgb, K).value;
            let minus = point(Ucs::new(8.0, -h, 0.0), RgbSpace::Srgb, K).value;
            let right = (plus.a - base.value.a) / h;
            let left = (base.value.a - minus.a) / h;
            assert!(
                (right - jacobian[1][1]).abs() < 1e-6,
                "h {h:e}: right {right:e} vs slope {:e}",
                jacobian[1][1]
            );
            assert!(
                (left - jacobian[1][1]).abs() < 1e-6,
                "h {h:e}: left {left:e} vs slope {:e}",
                jacobian[1][1]
            );
            assert!(
                (right - left).abs() < 1e-6,
                "h {h:e}: right {right:e} vs left {left:e}"
            );
        }
    }

    #[test]
    fn clipped_neutral_chroma_slope_is_not_the_zeroed_inverse_factor() {
        let pre = Ucs::new(14.0, 0.0, 0.0);
        let (jacobian, _, regularity) = finite(&point(pre, RgbSpace::Srgb, K));
        let CompositeRegularity::NeutralCrossing {
            clip_slope,
            chroma_slope,
            ..
        } = regularity
        else {
            panic!("expected the crossing, got {regularity:?}");
        };
        assert!((0.0..1.0).contains(&clip_slope), "clip slope {clip_slope}");
        assert!(
            (0.0..1.0).contains(&chroma_slope),
            "chroma slope {chroma_slope}"
        );
        assert_eq!(jacobian[1][1], chroma_slope);
        assert_eq!(jacobian[2][2], chroma_slope);
        assert_eq!(jacobian[1][2], 0.0);
        assert_eq!(jacobian[2][1], 0.0);
        let numerical = numerical(pre, RgbSpace::Srgb, K, 1e-5);
        assert!(
            (numerical[1][1] - chroma_slope).abs() < 1e-6,
            "central difference {} vs slope {chroma_slope}",
            numerical[1][1]
        );
    }

    #[test]
    fn smooth_composite_matches_central_differences() {
        for pre in [Ucs::new(80.78, 22.3, 8.49), Ucs::new(45.0, -12.0, 20.0)] {
            let point = point(pre, RgbSpace::Srgb, K);
            let (jacobian, _, regularity) = finite(&point);
            assert_eq!(regularity, CompositeRegularity::Smooth);
            let numerical = numerical(pre, RgbSpace::Srgb, K, 1e-6);
            let mut worst = 0.0_f64;
            for row in 0..3 {
                for column in 0..3 {
                    let scale = numerical[row][column].abs().max(0.1);
                    worst =
                        worst.max((jacobian[row][column] - numerical[row][column]).abs() / scale);
                }
            }
            println!("smooth {pre:?}: worst relative |analytic - central| {worst:e}");
            assert!(worst < 1e-3, "{pre:?}: {worst:e}");
            let rendered = crate::clip::output_encode_clip(pre, RgbSpace::Srgb, K);
            let value = point.value.to_array();
            let rendered = rendered.to_array();
            for channel in 0..3 {
                assert!(
                    (value[channel] - rendered[channel]).abs() < 1e-9,
                    "{pre:?}: {value:?} vs {rendered:?}"
                );
            }
        }
    }

    #[test]
    fn clipped_neutral_derivative_in_k_matches_central_differences() {
        let step = 1e-6;
        for lightness in [8.0, 14.0, 99.5] {
            let pre = Ucs::new(lightness, 0.0, 0.0);
            let (_, jacobian_k, _) = finite(&point(pre, RgbSpace::Srgb, K));
            let forward = point(pre, RgbSpace::Srgb, K + step).value.to_array();
            let backward = point(pre, RgbSpace::Srgb, K - step).value.to_array();
            for channel in 0..3 {
                let analytic = jacobian_k[channel];
                let numerical = (forward[channel] - backward[channel]) / (2.0 * step);
                println!(
                    "J' {lightness} channel {channel}: analytic {analytic:.9e} central {numerical:.9e}"
                );
                assert!(
                    (analytic - numerical).abs() < 1e-5,
                    "J' {lightness} channel {channel}: {analytic} vs {numerical}"
                );
            }
        }
    }

    #[test]
    fn infeasible_preclip_is_typed() {
        for pre in [
            Ucs::new(0.0, 0.0, 0.0),
            Ucs::new(-1.0, 0.0, 0.0),
            Ucs::new(crate::solve::J_PRIME_MAX, 0.0, 0.0),
        ] {
            let failure = evaluate(pre, RgbSpace::Srgb, K).expect_err("infeasible");
            assert!(matches!(
                failure.kind,
                crate::domain_penalty::DomainViolationKind::Lightness { .. }
            ));
        }
    }

    #[test]
    fn neutral_preclip_with_a_non_preserving_space_keeps_the_zero_chroma_derivative() {
        let pre = Ucs::new(92.0, 0.0, 0.0);
        let point = point(pre, RgbSpace::ProPhoto, K);
        let (jacobian, _, regularity) = finite(&point);
        assert_eq!(regularity, CompositeRegularity::NeutralInverse);
        let numerical = numerical(pre, RgbSpace::ProPhoto, K, 1e-6);
        for row in 0..3 {
            assert_eq!(jacobian[row][1], 0.0, "row {row} column 1");
            assert_eq!(jacobian[row][2], 0.0, "row {row} column 2");
            assert!(
                (jacobian[row][0] - numerical[row][0]).abs() < 1e-4,
                "row {row}: {} vs central difference {}",
                jacobian[row][0],
                numerical[row][0]
            );
        }
        assert!(jacobian[0][0].is_finite() && jacobian[0][0] != 0.0);
    }

    #[test]
    fn chromatic_preclip_to_a_near_neutral_postclip_has_a_finite_derivative() {
        let space = RgbSpace::ProPhoto;
        let white = space.xyz_to_rgb_linear(solve::reference_white());
        let scale = 0.745;
        let unclip = |x: f64| {
            let inverse = |u: f64| 0.5 * libm::log((1.0 + u) / (1.0 - u));
            if x > 1.0 - K {
                (1.0 - K) + K * inverse((x - (1.0 - K)) / K)
            } else if x < K {
                K - K * inverse((K - x) / K)
            } else {
                x
            }
        };
        // The 1e-12 blue offset keeps the post-clip appearance off the neutral axis.
        let pre_xyz = space.rgb_linear_to_xyz(Rgb::new(
            unclip(scale * white.r),
            unclip(scale * white.g),
            unclip(scale * white.b) + 1e-12,
        ));
        let pre = solve::ucs_of_xyz(pre_xyz);
        assert!(
            pre.a != 0.0 || pre.b != 0.0,
            "the pre-clip point is chromatic: {pre:?}"
        );
        let point = point(pre, space, K);
        println!(
            "case C: pre chroma {:.6e}, post chroma {:.6e}",
            pre.a.abs().max(pre.b.abs()),
            point.value.a.abs().max(point.value.b.abs())
        );
        assert!(point.value.a.abs() < 1e-8 && point.value.b.abs() < 1e-8);
        assert!(point.value.a != 0.0 || point.value.b != 0.0);
        let (jacobian, jacobian_k, regularity) = finite(&point);
        assert_eq!(regularity, CompositeRegularity::Smooth);
        assert!(jacobian.iter().flatten().all(|value| value.is_finite()));
        assert!(jacobian_k.iter().all(|value| value.is_finite()));
        assert!(
            jacobian.iter().flatten().any(|value| value.abs() > 1e-3),
            "the near-neutral chroma derivative is steep: {jacobian:?}"
        );
    }

    #[test]
    fn clipped_white_witness_has_a_finite_value_and_jacobian() {
        let pre = Ucs::from_array([
            f64::from_bits(0x405c_801c_f3cc_8976),
            f64::from_bits(0xbfd5_7b9c_d494_e898),
            f64::from_bits(0xc019_b669_ea83_58f4),
        ]);
        let point = point(pre, RgbSpace::Srgb, K);
        let (jacobian, jacobian_k, regularity) = finite(&point);
        assert_eq!(regularity, CompositeRegularity::Smooth);
        // High-precision reference for the documented neutral convention and the
        // exact upper-tail deficits in target/numerical-centered-prior-v5.
        assert!((point.value.a - -3.443_340_054_941_9e-13).abs() < 1e-17);
        assert!((point.value.b - -1.093_066_009_629_2e-13).abs() < 1e-17);
        assert!(point.value.j.is_finite());
        assert!(jacobian.iter().flatten().all(|value| value.is_finite()));
        assert!(jacobian_k.iter().all(|value| value.is_finite()));
        assert!(jacobian.iter().flatten().any(|value| *value != 0.0));
        let numerical = numerical(pre, RgbSpace::Srgb, K, 1e-6);
        assert!(
            worst_gap(&jacobian, &numerical) < 1e-13,
            "analytic {jacobian:?} vs central {numerical:?}"
        );
    }
}
