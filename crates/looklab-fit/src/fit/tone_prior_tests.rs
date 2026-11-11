// SPDX-License-Identifier: AGPL-3.0-or-later
//! Numerical contracts for the tone-centered identity prior.

use super::*;
use crate::layout::softplus_inverse;
use looklab_model::Tone;
use looklab_model::basis::TONE_KNOTS;

const STEP: f64 = 1e-6;

fn test_layout() -> Layout {
    Layout::with_response(1, 0, 1, 0)
}

fn tone_block(layout: &Layout) -> BlockAcc {
    let mut params = vec![layout.tone_c0];
    params.extend(layout.tone_beta.clone());
    BlockAcc::new(BlockKind::Tone, params, layout.total)
}

fn mixed_block(layout: &Layout) -> BlockAcc {
    let params = vec![
        layout.g.start,
        layout.tone_c0,
        layout.tint_a.start,
        layout.tone_beta.start + 2,
        layout.residual.start,
        layout.tone_beta.start,
        layout.lambda.start,
    ];
    BlockAcc::new(BlockKind::Tone, params, layout.total)
}

pub(super) fn colorimetric_center(layout: &Layout) -> Vec<f64> {
    let mut theta = vec![0.0_f64; layout.total];
    match Tone::colorimetric(&TONE_KNOTS).expect("colorimetric tone") {
        Tone::Spline { c0, beta } => {
            theta[layout.tone_c0] = c0;
            for (offset, value) in beta.iter().enumerate() {
                theta[layout.tone_beta.start + offset] = softplus_inverse(*value);
            }
        }
        Tone::Colorimetric => panic!("the colorimetric tone over TONE_KNOTS is a spline"),
    }
    theta
}

fn precision(layout: &Layout, global: usize) -> f64 {
    let weight = if global == layout.tone_c0 || layout.tone_beta.contains(&global) {
        1.0
    } else if layout.tint_a.contains(&global) || layout.tint_b.contains(&global) {
        1.0 / (2.0 * 2.0)
    } else if layout.g.contains(&global) {
        1.0 / (0.3 * 0.3)
    } else if layout.r.contains(&global) {
        1.0 / (15.0 * 15.0)
    } else if layout.lambda.contains(&global) {
        1.0 / (8.0 * 8.0)
    } else if layout.residual.contains(&global) {
        1.0 / (0.5 * 0.5)
    } else {
        0.0
    };
    layout.identity_scale * weight
}

fn is_tone(layout: &Layout, global: usize) -> bool {
    global == layout.tone_c0 || layout.tone_beta.contains(&global)
}

fn displaced(layout: &Layout) -> Vec<f64> {
    let mut theta = colorimetric_center(layout);
    theta[layout.tone_c0] += 0.7;
    theta[layout.tone_beta.start + 2] -= 0.4;
    theta[layout.tint_a.start] = 0.3;
    theta[layout.g.start] = -0.2;
    theta[layout.residual.start] = 0.05;
    theta
}

#[test]
fn identity_prior_is_zero_at_the_colorimetric_initializer_and_positive_at_zero() {
    let layout = test_layout();
    let block = tone_block(&layout);
    let center = colorimetric_center(&layout);
    let at_center = identity_cost(&layout, &block, &center);
    assert!(
        at_center.abs() <= 1e-12,
        "prior energy at the colorimetric initializer is {at_center}"
    );
    let gradient = identity_gradient(&layout, &block, &center);
    for (local, value) in gradient.iter().enumerate() {
        assert!(
            value.abs() <= 1e-12,
            "prior gradient {local} at the colorimetric initializer is {value}"
        );
    }
    let zero_state = vec![0.0_f64; layout.total];
    let at_zero = identity_cost(&layout, &block, &zero_state);
    assert!(
        at_zero > 1e-6,
        "zero tone coordinates are not above the prior minimum: {at_zero}"
    );
}

#[test]
fn identity_prior_minimum_is_the_production_initializer() {
    let layout = test_layout();
    let block = mixed_block(&layout);
    let params = Params::initial(layout.clone(), 1);
    let value = identity_cost(&layout, &block, &params.theta);
    assert!(
        value.abs() <= 1e-12,
        "prior energy at the production initializer is {value}"
    );
    let gradient = identity_gradient(&layout, &block, &params.theta);
    for (local, value) in gradient.iter().enumerate() {
        assert!(
            value.abs() <= 1e-12,
            "prior gradient {local} at the production initializer is {value}"
        );
    }
    for (local, &global) in block.params.iter().enumerate() {
        let mut raised = params.theta.clone();
        raised[global] += 0.25;
        let energy = identity_cost(&layout, &block, &raised);
        assert!(
            energy > value + 1e-9,
            "coordinate {local} (global {global}) does not raise the prior energy: {energy}"
        );
    }
}

#[test]
fn identity_gradient_matches_finite_differences_around_a_displaced_tone() {
    let layout = test_layout();
    let block = mixed_block(&layout);
    let theta = displaced(&layout);
    let gradient = identity_gradient(&layout, &block, &theta);
    let tone = block
        .params
        .iter()
        .position(|global| *global == layout.tone_c0)
        .expect("the block carries tone c0");
    assert!(
        gradient[tone].abs() > 0.5,
        "the displaced tone has no prior gradient: {}",
        gradient[tone]
    );
    for (local, &global) in block.params.iter().enumerate() {
        let mut plus = theta.clone();
        let mut minus = theta.clone();
        plus[global] += STEP;
        minus[global] -= STEP;
        let estimate = (identity_cost(&layout, &block, &plus)
            - identity_cost(&layout, &block, &minus))
            / (2.0 * STEP);
        assert!(
            (gradient[local] - estimate).abs() <= 1e-7 * estimate.abs().max(1.0),
            "prior gradient {local} (global {global}): analytic {:.9e} finite difference {:.9e}",
            gradient[local],
            estimate
        );
    }
}

#[test]
fn identity_curvature_is_the_unchanged_linear_precision_map() {
    let layout = test_layout();
    let block = mixed_block(&layout);
    let center = colorimetric_center(&layout);
    let mut direction = vec![0.0_f64; layout.total];
    for (offset, &global) in block.params.iter().enumerate() {
        direction[global] = 0.1 * (offset as f64 + 1.0) * if offset % 2 == 0 { 1.0 } else { -1.0 };
    }

    let zero_state = vec![0.0_f64; layout.total];
    let zero = identity_apply(&layout, &block, &zero_state);
    assert!(zero.iter().all(|value| *value == 0.0), "A0 is not zero");
    let mut other = vec![0.0_f64; layout.total];
    for &global in &block.params {
        other[global] = 0.05;
    }
    let total: Vec<f64> = direction.iter().zip(&other).map(|(a, b)| a + b).collect();
    let sum = identity_apply(&layout, &block, &total);
    let left = identity_apply(&layout, &block, &direction);
    let right = identity_apply(&layout, &block, &other);
    let doubled = identity_apply(
        &layout,
        &block,
        &direction
            .iter()
            .map(|value| 2.0 * value)
            .collect::<Vec<_>>(),
    );
    for local in 0..block.len() {
        assert!(
            (sum[local] - (left[local] + right[local])).abs()
                <= 1e-12 * (left[local] + right[local]).abs().max(1.0),
            "identity map is not additive at {local}"
        );
        assert!(
            (doubled[local] - 2.0 * left[local]).abs() <= 1e-12 * left[local].abs().max(1.0),
            "identity map is not homogeneous at {local}"
        );
    }

    let applied_center = identity_apply(&layout, &block, &center);
    for (local, &global) in block.params.iter().enumerate() {
        let expected = precision(&layout, global) * center[global];
        assert!(
            (applied_center[local] - expected).abs() <= 1e-12 * expected.abs().max(1.0),
            "the linear map at {local} (global {global}) is not W·x: {}",
            applied_center[local]
        );
        if is_tone(&layout, global) {
            assert!(
                applied_center[local].abs() > 1e-3,
                "the linear map dropped the tone center at {local}"
            );
        }
    }

    let mut plus = center.clone();
    let mut minus = center.clone();
    for global in 0..layout.total {
        plus[global] += STEP * direction[global];
        minus[global] -= STEP * direction[global];
    }
    let g_plus = identity_gradient(&layout, &block, &plus);
    let g_minus = identity_gradient(&layout, &block, &minus);
    let curvature = identity_curvature(&layout, &block);
    let matrix = identity_matrix(&layout, &block);
    let n = block.len();
    for (local, &global) in block.params.iter().enumerate() {
        let hvp = (g_plus[local] - g_minus[local]) / (2.0 * STEP);
        assert!(
            (hvp - left[local]).abs() <= 1e-7 * left[local].abs().max(1.0),
            "prior Hessian-vector product at {local} (global {global}): {hvp:.9e} vs {:.9e}",
            left[local]
        );
        let expected = precision(&layout, global);
        assert!(
            (curvature[local] - expected).abs() <= 1e-12 * expected.abs().max(1.0),
            "prior curvature at {local} (global {global}) changed: {} vs {expected}",
            curvature[local]
        );
        for column in 0..n {
            let entry = if local == column { expected } else { 0.0 };
            assert!(
                (matrix[local * n + column] - entry).abs() <= 1e-12 * entry.abs().max(1.0),
                "prior curvature matrix entry ({local},{column}) is {}",
                matrix[local * n + column]
            );
        }
    }
}

#[test]
fn identity_prior_keeps_non_tone_layers_centered_at_zero() {
    let layout = test_layout();
    let block = mixed_block(&layout);
    let mut theta = colorimetric_center(&layout);
    for (offset, &global) in [
        layout.tint_a.start,
        layout.g.start,
        layout.residual.start,
        layout.lambda.start,
    ]
    .iter()
    .enumerate()
    {
        theta[global] = 0.1 * (offset as f64 + 1.0);
    }
    let gradient = identity_gradient(&layout, &block, &theta);
    let mut expected_value = 0.0;
    for (local, &global) in block.params.iter().enumerate() {
        if is_tone(&layout, global) {
            assert!(
                gradient[local].abs() <= 1e-12,
                "tone coordinate {local} is not at its prior minimum: {}",
                gradient[local]
            );
        } else {
            let expected = precision(&layout, global) * theta[global];
            assert!(
                (gradient[local] - expected).abs() <= 1e-12 * expected.abs().max(1.0),
                "non-tone gradient {local} (global {global}) changed: {} vs {expected}",
                gradient[local]
            );
            expected_value += 0.5 * expected * theta[global];
        }
    }
    let value = identity_cost(&layout, &block, &theta);
    assert!(
        (value - expected_value).abs() <= 1e-12 * expected_value.abs().max(1.0),
        "non-tone prior energy changed: {value} vs {expected_value}"
    );

    let zero_state = vec![0.0_f64; layout.total];
    let at_zero = identity_gradient(&layout, &block, &zero_state);
    for (local, &global) in block.params.iter().enumerate() {
        if is_tone(&layout, global) {
            let expected = -precision(&layout, global) * theta[global];
            assert!(
                (at_zero[local] - expected).abs() <= 1e-12 * expected.abs().max(1.0),
                "tone gradient at zero {local} does not use the center: {}",
                at_zero[local]
            );
        } else {
            assert_eq!(at_zero[local], 0.0, "non-tone gradient at zero {local}");
        }
    }
    assert!(
        at_zero.iter().any(|value| value.abs() > 1e-9),
        "the tone center is zero, so the zero-state contract is vacuous"
    );
}

#[test]
fn identity_prior_maps_mixed_and_non_contiguous_blocks_by_global_coordinate() {
    let layout = test_layout();
    let block = mixed_block(&layout);
    let center = colorimetric_center(&layout);
    let theta = displaced(&layout);
    let gradient = identity_gradient(&layout, &block, &theta);
    let mut expected_value = 0.0;
    for (local, &global) in block.params.iter().enumerate() {
        let displacement = theta[global] - center[global];
        let expected = precision(&layout, global) * displacement;
        assert!(
            (gradient[local] - expected).abs() <= 1e-12 * expected.abs().max(1.0),
            "prior gradient {local} (global {global}) uses the wrong coordinate: {} vs {expected}",
            gradient[local]
        );
        expected_value += 0.5 * expected * displacement;
    }
    let value = identity_cost(&layout, &block, &theta);
    assert!(
        (value - expected_value).abs() <= 1e-12 * expected_value.abs().max(1.0),
        "prior energy over the mixed block is {value}, expected {expected_value}"
    );
    assert!(
        block.params.windows(2).any(|pair| pair[0] > pair[1]),
        "the block order is not permuted"
    );
    assert!(
        block.params.windows(2).any(|pair| pair[1] > pair[0] + 1),
        "the block coordinates are contiguous"
    );
}

#[test]
fn identity_precision_scale_scales_and_disables_the_centered_prior() {
    let mut layout = test_layout();
    let block = mixed_block(&layout);
    let theta = displaced(&layout);
    let full_cost = identity_cost(&layout, &block, &theta);
    let full_gradient = identity_gradient(&layout, &block, &theta);
    let full_apply = identity_apply(&layout, &block, &theta);
    let full_curvature = identity_curvature(&layout, &block);
    assert!(full_cost > 1e-3, "the displaced state has no prior energy");

    layout.identity_scale = 0.5;
    let half_cost = identity_cost(&layout, &block, &theta);
    let half_gradient = identity_gradient(&layout, &block, &theta);
    let half_apply = identity_apply(&layout, &block, &theta);
    let half_curvature = identity_curvature(&layout, &block);
    assert!(
        (half_cost - 0.5 * full_cost).abs() <= 1e-12 * full_cost.abs().max(1.0),
        "half-scaled prior energy {half_cost} is not half of {full_cost}"
    );
    for local in 0..block.len() {
        assert!(
            (half_gradient[local] - 0.5 * full_gradient[local]).abs()
                <= 1e-12 * full_gradient[local].abs().max(1.0),
            "half-scaled prior gradient {local} is not half"
        );
        assert!(
            (half_apply[local] - 0.5 * full_apply[local]).abs()
                <= 1e-12 * full_apply[local].abs().max(1.0),
            "half-scaled identity map {local} is not half"
        );
        assert!(
            (half_curvature[local] - 0.5 * full_curvature[local]).abs()
                <= 1e-12 * full_curvature[local].abs().max(1.0),
            "half-scaled prior curvature {local} is not half"
        );
    }

    layout.identity_scale = 0.0;
    assert_eq!(identity_cost(&layout, &block, &theta), 0.0);
    assert!(
        identity_gradient(&layout, &block, &theta)
            .iter()
            .all(|value| *value == 0.0),
        "a disabled prior still has a gradient"
    );
    assert!(
        identity_apply(&layout, &block, &theta)
            .iter()
            .all(|value| *value == 0.0),
        "a disabled prior still has a curvature map"
    );
    assert!(
        identity_curvature(&layout, &block)
            .iter()
            .all(|value| *value == 0.0),
        "a disabled prior still has a curvature diagonal"
    );
}
