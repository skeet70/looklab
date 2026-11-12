// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_color::ucs::Ucs;
use looklab_model::domain_penalty::{
    DOMAIN_AXES, DomainAxis, DomainViolationKind, InversePath, check_published_inverse,
    check_published_paths, check_reference_lightness, check_reference_tone, evaluate,
};
use looklab_model::{ChromaPaths, LayerValues};

fn layer_values() -> LayerValues {
    LayerValues {
        t: 50.0,
        n_a: 0.0,
        n_b: 0.0,
        log_g: 0.0,
        r: 0.0,
        lambda: 0.0,
        residual: [0.0; 3],
    }
}

fn chroma_paths() -> ChromaPaths {
    ChromaPaths {
        ucs: Ucs::new(50.0, 30.0, -20.0),
        internal: [0.12, -0.08],
        blend: 0.5,
    }
}

fn shifted(
    values: &LayerValues,
    paths: &ChromaPaths,
    axis: DomainAxis,
    delta: f64,
) -> (LayerValues, ChromaPaths) {
    let mut values = *values;
    let mut paths = *paths;
    match axis {
        DomainAxis::Tone => values.t += delta,
        DomainAxis::Lightness => paths.ucs.j += delta,
        DomainAxis::UcsA => paths.ucs.a += delta,
        DomainAxis::UcsB => paths.ucs.b += delta,
        DomainAxis::InternalA => paths.internal[0] += delta,
        DomainAxis::InternalB => paths.internal[1] += delta,
        DomainAxis::Blend => paths.blend += delta,
    }
    (values, paths)
}

fn axis_at(index: usize) -> DomainAxis {
    [
        DomainAxis::Tone,
        DomainAxis::Lightness,
        DomainAxis::UcsA,
        DomainAxis::UcsB,
        DomainAxis::InternalA,
        DomainAxis::InternalB,
        DomainAxis::Blend,
    ][index]
}

#[test]
fn gradient_matches_central_differences() {
    let values = layer_values();
    let paths = chroma_paths();
    let base = evaluate(&values, &paths).expect("base point is feasible");
    let steps = [1e-3, 1e-4, 1e-5, 1e-6];
    let mut best = f64::INFINITY;
    for step in steps {
        let mut worst = 0.0_f64;
        for axis in 0..DOMAIN_AXES {
            let (plus_values, plus_paths) = shifted(&values, &paths, axis_at(axis), step);
            let (minus_values, minus_paths) = shifted(&values, &paths, axis_at(axis), -step);
            let plus = evaluate(&plus_values, &plus_paths)
                .expect("positive perturbation is feasible")
                .value;
            let minus = evaluate(&minus_values, &minus_paths)
                .expect("negative perturbation is feasible")
                .value;
            let numerical = (plus - minus) / (2.0 * step);
            let analytic = base.gradient[axis];
            let scale = analytic.abs().max(numerical.abs()).max(1e-6);
            worst = worst.max((analytic - numerical).abs() / scale);
        }
        best = best.min(worst);
        println!("gradient step {step:e}: worst relative error {worst:e}");
    }
    assert!(best < 1e-5, "best gradient relative error {best:e}");
}

#[test]
fn hessian_matches_central_differences_of_the_gradient() {
    let values = layer_values();
    let paths = chroma_paths();
    let base = evaluate(&values, &paths).expect("base point is feasible");
    let steps = [1e-3, 1e-4, 1e-5];
    let mut best = f64::INFINITY;
    for step in steps {
        let mut worst = 0.0_f64;
        for axis in 0..DOMAIN_AXES {
            let (plus_values, plus_paths) = shifted(&values, &paths, axis_at(axis), step);
            let (minus_values, minus_paths) = shifted(&values, &paths, axis_at(axis), -step);
            let plus = evaluate(&plus_values, &plus_paths)
                .expect("positive perturbation is feasible")
                .gradient;
            let minus = evaluate(&minus_values, &minus_paths)
                .expect("negative perturbation is feasible")
                .gradient;
            for column in 0..DOMAIN_AXES {
                let numerical = (plus[column] - minus[column]) / (2.0 * step);
                let analytic = base.hessian[axis][column];
                let scale = analytic.abs().max(numerical.abs()).max(1e-5);
                worst = worst.max((analytic - numerical).abs() / scale);
            }
        }
        best = best.min(worst);
        println!("hessian step {step:e}: worst relative error {worst:e}");
    }
    assert!(best < 2e-4, "best hessian relative error {best:e}");
}

#[test]
fn mixed_direction_second_difference_matches_the_quadratic_form() {
    let values = layer_values();
    let paths = chroma_paths();
    let base = evaluate(&values, &paths).expect("base point is feasible");
    let direction = [0.3, -0.2, 0.5, 0.1, -0.4, 0.25, 0.15];
    let expected = (0..DOMAIN_AXES)
        .flat_map(|row| {
            (0..DOMAIN_AXES)
                .map(move |column| direction[row] * base.hessian[row][column] * direction[column])
        })
        .sum::<f64>();
    let scalar = |offset: f64| {
        let mut values = values;
        let mut paths = paths;
        values.t += offset * direction[0];
        paths.ucs.j += offset * direction[1];
        paths.ucs.a += offset * direction[2];
        paths.ucs.b += offset * direction[3];
        paths.internal[0] += offset * direction[4];
        paths.internal[1] += offset * direction[5];
        paths.blend += offset * direction[6];
        evaluate(&values, &paths)
            .expect("direction stays feasible")
            .value
    };
    let mut best = f64::INFINITY;
    for step in [1e-2, 1e-3, 1e-4] {
        let numerical = (scalar(step) - 2.0 * scalar(0.0) + scalar(-step)) / (step * step);
        let scale = expected.abs().max(numerical.abs()).max(1e-6);
        let error = (expected - numerical).abs() / scale;
        best = best.min(error);
        println!("mixed direction step {step:e}: relative error {error:e}");
    }
    assert!(best < 5e-3, "best mixed second-difference error {best:e}");
}

fn toe_anchor() -> f64 {
    looklab_model::solve::reference_model()
        .ucs_j_and_dj_dy(looklab_model::solve::reference_white(), 0.03)
        .0
}

fn toe(x: f64, anchor: f64) -> f64 {
    let ratio = x / anchor;
    if ratio >= 1.0 {
        return 0.0;
    }
    let u = 1.0 - ratio;
    -libm::log1p(-u) - u - 0.5 * u * u
}

fn toe_slope(x: f64, anchor: f64) -> f64 {
    let ratio = x / anchor;
    if ratio >= 1.0 {
        return 0.0;
    }
    (-1.0 / ratio + 2.0 - ratio) / anchor
}

fn toe_curvature(x: f64, anchor: f64) -> f64 {
    let ratio = x / anchor;
    if ratio >= 1.0 {
        return 0.0;
    }
    (1.0 / (ratio * ratio) - 1.0) / (anchor * anchor)
}

fn wall(x: f64, limit: f64, anchor: f64) -> f64 {
    -libm::log1p(-x / limit) + toe(x, anchor)
}

#[test]
fn toe_wall_value_matches_the_closed_form() {
    let anchor = toe_anchor();
    let paths = chroma_paths();
    let limit = looklab_model::neutral_j_prime_max();
    let base_values = layer_values();
    let base = evaluate(&base_values, &paths).expect("base point is feasible");
    for tone in [1.0, 5.0, 10.0, 20.0, 50.0, 80.0, 0.9 * limit] {
        let mut values = base_values;
        values.t = tone;
        let penalty = evaluate(&values, &paths).expect("tone stays feasible");
        let expected = base.value + wall(tone, limit, anchor) - wall(base_values.t, limit, anchor);
        let scale = expected.abs().max(penalty.value.abs()).max(1.0);
        assert!(
            (penalty.value - expected).abs() / scale < 1e-12,
            "tone {tone}: {} vs {expected}",
            penalty.value
        );
    }
    for lightness in [5.0, 10.0, 15.0, 20.0, 30.0, 50.0] {
        let paths = ChromaPaths {
            ucs: Ucs::new(lightness, 0.0, 0.0),
            internal: [0.0, 0.0],
            blend: 0.5,
        };
        let penalty = evaluate(&base_values, &paths).expect("neutral point is feasible");
        let expected = wall(base_values.t, limit, anchor) + wall(lightness, limit, anchor);
        let scale = expected.abs().max(penalty.value.abs()).max(1.0);
        assert!(
            (penalty.value - expected).abs() / scale < 1e-12,
            "lightness {lightness}: {} vs {expected}",
            penalty.value
        );
    }
}

#[test]
fn toe_wall_derivatives_match_the_closed_form_at_neutral_chroma() {
    let values = layer_values();
    let anchor = toe_anchor();
    let limit = looklab_model::neutral_j_prime_max();
    for lightness in [10.0, 15.0, 20.0, 50.0] {
        let paths = ChromaPaths {
            ucs: Ucs::new(lightness, 0.0, 0.0),
            internal: [0.0, 0.0],
            blend: 0.5,
        };
        let penalty = evaluate(&values, &paths).expect("neutral point is feasible");
        for (axis, x) in [
            (DomainAxis::Tone, values.t),
            (DomainAxis::Lightness, lightness),
        ] {
            let index = axis as usize;
            let slope = 1.0 / (limit - x) + toe_slope(x, anchor);
            let scale = slope.abs().max(penalty.gradient[index].abs()).max(1.0);
            assert!(
                (penalty.gradient[index] - slope).abs() / scale < 1e-12,
                "lightness {lightness} {axis:?} gradient {} vs {slope}",
                penalty.gradient[index]
            );
            let upper_curvature = 1.0 / ((limit - x) * (limit - x));
            let curvature = upper_curvature + toe_curvature(x, anchor);
            let scale = curvature.abs().max(1.0);
            assert!(
                (penalty.hessian[index][index] - curvature).abs() / scale < 1e-12,
                "lightness {lightness} {axis:?} hessian {} vs {curvature}",
                penalty.hessian[index][index]
            );
            let gn = penalty.gn_hessian[index][index];
            assert!(
                gn / scale >= curvature / scale - 1e-12,
                "lightness {lightness} {axis:?} gn_hessian {gn} lost the wall curvature"
            );
        }
    }
}

#[test]
fn toe_lower_wall_blows_up_at_neutral_chroma() {
    let values = layer_values();
    let anchor = toe_anchor();
    let limit = looklab_model::neutral_j_prime_max();
    let mut previous = f64::NEG_INFINITY;
    for lightness in [1e-2, 1e-4, 1e-6, 1e-9] {
        let paths = ChromaPaths {
            ucs: Ucs::new(lightness, 0.0, 0.0),
            internal: [0.0, 0.0],
            blend: 0.5,
        };
        let penalty = evaluate(&values, &paths).expect("positive lightness stays feasible");
        assert!(penalty.value.is_finite());
        assert!(
            penalty.value > previous,
            "lightness {lightness:e}: value {} did not increase from {previous}",
            penalty.value
        );
        let index = DomainAxis::Lightness as usize;
        let slope = 1.0 / (limit - lightness) + toe_slope(lightness, anchor);
        let scale = slope.abs().max(1.0);
        assert!(
            (penalty.gradient[index] - slope).abs() / scale < 1e-9,
            "lightness {lightness:e}: gradient {} vs {slope}",
            penalty.gradient[index]
        );
        let curvature =
            1.0 / ((limit - lightness) * (limit - lightness)) + toe_curvature(lightness, anchor);
        let scale = curvature.abs().max(1.0);
        assert!(
            (penalty.hessian[index][index] - curvature).abs() / scale < 1e-9,
            "lightness {lightness:e}: hessian {} vs {curvature}",
            penalty.hessian[index][index]
        );
        previous = penalty.value;
    }
}

#[test]
fn toe_wall_blows_up_and_rejects_the_boundary() {
    let paths = chroma_paths();
    let anchor = toe_anchor();
    let limit = looklab_model::neutral_j_prime_max();
    let mut previous = f64::NEG_INFINITY;
    for tone in [1e-2, 1e-4, 1e-6, 1e-9] {
        let mut values = layer_values();
        values.t = tone;
        let penalty = evaluate(&values, &paths).expect("positive tone stays feasible");
        assert!(penalty.value.is_finite());
        assert!(
            penalty.value > previous,
            "tone {tone:e}: value {} did not increase from {previous}",
            penalty.value
        );
        let slope = 1.0 / (limit - tone) + toe_slope(tone, anchor);
        let scale = slope.abs().max(1.0);
        assert!(
            (penalty.gradient[DomainAxis::Tone as usize] - slope).abs() / scale < 1e-9,
            "tone {tone:e}: gradient {} vs {slope}",
            penalty.gradient[DomainAxis::Tone as usize]
        );
        previous = penalty.value;
    }
    let mut values = layer_values();
    values.t = limit * (1.0 - 1e-9);
    let penalty = evaluate(&values, &paths).expect("tone below the upper limit is feasible");
    assert!(
        penalty.value > 15.0,
        "upper wall did not blow up: {}",
        penalty.value
    );
    values.t = 0.0;
    assert!(matches!(
        evaluate(&values, &paths)
            .expect_err("zero tone is outside the domain")
            .kind,
        DomainViolationKind::ReferenceTone { .. }
    ));
    let values = layer_values();
    let paths = ChromaPaths {
        ucs: Ucs::new(0.0, 0.0, 0.0),
        ..chroma_paths()
    };
    assert!(matches!(
        evaluate(&values, &paths)
            .expect_err("zero lightness is outside the domain")
            .kind,
        DomainViolationKind::Lightness { .. }
    ));
}

#[test]
fn neutral_anchor_has_zero_output_chromatic_curvature_and_nonzero_lightness_curvature() {
    let values = layer_values();
    let paths = ChromaPaths {
        ucs: Ucs::new(50.0, 0.0, 0.0),
        internal: [0.0, 0.0],
        blend: 0.5,
    };
    let penalty = evaluate(&values, &paths).expect("neutral point is feasible");
    assert!(penalty.minimum_margin > 0.0);
    for axis in [DomainAxis::UcsA, DomainAxis::UcsB, DomainAxis::Blend] {
        let index = axis as usize;
        assert_eq!(penalty.gradient[index], 0.0, "{axis:?} gradient");
        for other in 0..DOMAIN_AXES {
            assert_eq!(penalty.hessian[index][other], 0.0, "{axis:?} hessian row");
            assert_eq!(
                penalty.hessian[other][index], 0.0,
                "{axis:?} hessian column"
            );
        }
    }
    let lightness = DomainAxis::Lightness as usize;
    let ucs_a = DomainAxis::UcsA as usize;
    let ucs_b = DomainAxis::UcsB as usize;
    let blend = DomainAxis::Blend as usize;
    for axis in [DomainAxis::InternalA, DomainAxis::InternalB] {
        let index = axis as usize;
        assert_eq!(penalty.gradient[index], 0.0, "{axis:?} gradient");
        assert!(penalty.hessian[lightness][index].abs() < 1e-12);
        assert_eq!(penalty.hessian[blend][index], 0.0);
        assert_eq!(penalty.hessian[ucs_a][index], 0.0);
        assert_eq!(penalty.hessian[ucs_b][index], 0.0);
    }
    assert!(penalty.hessian[DomainAxis::InternalA as usize][DomainAxis::InternalA as usize] > 0.0);
    assert!(penalty.hessian[DomainAxis::InternalB as usize][DomainAxis::InternalB as usize] > 0.0);
    let limit = looklab_model::neutral_j_prime_max();
    let expected = 1.0 / ((limit - 50.0) * (limit - 50.0));
    let scale = expected
        .abs()
        .max(penalty.hessian[lightness][lightness].abs());
    assert!(
        (penalty.hessian[lightness][lightness] - expected).abs() / scale < 1e-9,
        "lightness curvature {} vs {expected}",
        penalty.hessian[lightness][lightness]
    );
    assert!(penalty.hessian[DomainAxis::Tone as usize][DomainAxis::Tone as usize] > 0.0);
}

#[test]
fn output_chromatic_hessian_tends_to_zero_at_neutral() {
    let values = layer_values();
    let internal = [0.12, -0.08];
    let mut previous = f64::INFINITY;
    for step in [1e-2, 1e-3, 1e-4] {
        let paths = ChromaPaths {
            ucs: Ucs::new(50.0, step, 0.0),
            internal,
            blend: 0.5,
        };
        let penalty = evaluate(&values, &paths).expect("near-neutral point is feasible");
        let curvature = penalty.hessian[DomainAxis::UcsA as usize][DomainAxis::UcsA as usize].abs();
        let mixed =
            penalty.hessian[DomainAxis::Lightness as usize][DomainAxis::UcsA as usize].abs();
        println!("output chroma step {step:e}: curvature {curvature:e} mixed {mixed:e}");
        assert!(curvature > 0.0);
        assert!(
            curvature < previous,
            "curvature did not decrease at {step:e}"
        );
        previous = curvature;
    }
    let neutral = evaluate(
        &values,
        &ChromaPaths {
            ucs: Ucs::new(50.0, 0.0, 0.0),
            internal,
            blend: 0.5,
        },
    )
    .expect("neutral output point is feasible");
    assert_eq!(
        neutral.hessian[DomainAxis::UcsA as usize][DomainAxis::UcsA as usize],
        0.0
    );
    assert_eq!(
        neutral.hessian[DomainAxis::Lightness as usize][DomainAxis::UcsA as usize],
        0.0
    );
}

#[test]
fn both_paths_are_required_at_blend_endpoints() {
    let values = layer_values();
    let mut outer_only = chroma_paths();
    outer_only.blend = 1.0;
    assert!(evaluate(&values, &outer_only).is_ok());
    let mut inner_only = chroma_paths();
    inner_only.blend = 0.0;
    assert!(evaluate(&values, &inner_only).is_ok());

    let mut bad_internal = chroma_paths();
    bad_internal.blend = 1.0;
    bad_internal.internal = [0.0, -1000.0];
    let violation = evaluate(&values, &bad_internal).expect_err("zero weight is not feasibility");
    assert_eq!(violation.path, Some(InversePath::Internal));

    let mut bad_outer = chroma_paths();
    bad_outer.blend = 0.0;
    bad_outer.ucs = Ucs::new(50.0, f64::NAN, 0.0);
    let violation = evaluate(&values, &bad_outer).expect_err("zero weight is not feasibility");
    assert_eq!(violation.path, Some(InversePath::Ucs));
}

#[test]
fn tone_and_lightness_guards_are_separate() {
    let mut values = layer_values();
    values.t = 0.0;
    let violation = check_reference_tone(&values).expect_err("black tone is not regular");
    assert!(matches!(
        violation.kind,
        DomainViolationKind::ReferenceTone { .. }
    ));
    assert!(check_reference_lightness(50.0).is_ok());
    assert!(matches!(
        check_reference_lightness(0.0)
            .expect_err("black lightness is not regular")
            .kind,
        DomainViolationKind::Lightness { .. }
    ));
    let limit = looklab_model::neutral_j_prime_max();
    assert!(matches!(
        check_reference_lightness(limit + 1.0)
            .expect_err("limit is not interior")
            .kind,
        DomainViolationKind::Lightness { .. }
    ));
    let values = layer_values();
    let paths = ChromaPaths {
        ucs: Ucs::new(0.0, 0.0, 0.0),
        internal: [0.0, 0.0],
        blend: 0.5,
    };
    let violation = evaluate(&values, &paths).expect_err("black lightness is not regular");
    assert!(matches!(
        violation.kind,
        DomainViolationKind::Lightness { .. }
    ));
}

#[test]
fn blend_gradient_keeps_the_zero_weight_path_value() {
    let values = layer_values();
    let mut paths = chroma_paths();
    paths.blend = 0.0;
    let at_zero = evaluate(&values, &paths).expect("endpoint is feasible");
    paths.blend = 1.0;
    let at_one = evaluate(&values, &paths).expect("endpoint is feasible");
    let scale = at_zero.value.abs().max(at_one.value.abs()).max(1.0);
    assert!(
        (at_zero.value - at_one.value).abs() / scale < 1.0,
        "blend endpoints should differ only by the path penalty"
    );
    assert!(at_zero.gradient[DomainAxis::Blend as usize].is_finite());
    assert!(at_one.gradient[DomainAxis::Blend as usize].is_finite());
    assert_eq!(
        at_zero.gradient[DomainAxis::UcsA as usize],
        0.0,
        "zero-weight outer chroma gradient"
    );
    assert_eq!(
        at_zero.hessian[DomainAxis::UcsA as usize][DomainAxis::UcsA as usize],
        0.0
    );
    assert_eq!(
        at_one.gradient[DomainAxis::InternalA as usize],
        0.0,
        "zero-weight internal chroma gradient"
    );
    assert_eq!(
        at_one.hessian[DomainAxis::InternalA as usize][DomainAxis::InternalA as usize],
        0.0
    );
    for axis in 0..DOMAIN_AXES {
        assert_eq!(at_zero.gn_hessian[axis][DomainAxis::Blend as usize], 0.0);
        assert_eq!(at_one.gn_hessian[axis][DomainAxis::Blend as usize], 0.0);
    }
}

#[test]
fn gn_hessian_is_symmetric_and_positive_on_probe_directions() {
    let values = layer_values();
    let paths = chroma_paths();
    let penalty = evaluate(&values, &paths).expect("base point is feasible");
    for row in 0..DOMAIN_AXES {
        for column in 0..DOMAIN_AXES {
            assert_eq!(
                penalty.gn_hessian[row][column],
                penalty.gn_hessian[column][row]
            );
        }
    }
    for direction in [
        [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, -1.0, 0.5, 0.5, 0.0],
        [0.1, 0.2, -0.3, 0.4, -0.5, 0.6, 0.7],
    ] {
        let quadratic = (0..DOMAIN_AXES)
            .flat_map(|row| {
                (0..DOMAIN_AXES).map(move |column| {
                    direction[row] * penalty.gn_hessian[row][column] * direction[column]
                })
            })
            .sum::<f64>();
        assert!(quadratic >= -1e-12, "GN quadratic form {quadratic}");
    }
}

#[test]
fn published_inverse_validation_is_strict() {
    let feasible = check_published_inverse(Ucs::new(50.0, 30.0, -20.0))
        .expect("ordinary appearance is strictly feasible");
    assert!(feasible.minimum_margin > 0.0);
    assert!(feasible.responses.iter().all(|value| value.is_finite()));
    assert!(matches!(
        check_published_inverse(Ucs::new(0.0, 0.0, 0.0))
            .expect_err("black is a rendering boundary")
            .kind,
        DomainViolationKind::Lightness { .. }
    ));
    assert!(check_published_inverse(Ucs::new(50.0, f64::NAN, 0.0)).is_err());
    let paths = ChromaPaths {
        ucs: Ucs::new(50.0, 0.0, 0.0),
        internal: [0.0, 0.0],
        blend: 0.5,
    };
    let published = check_published_paths(&paths).expect("neutral published appearance");
    assert_eq!(published.appearance, Ucs::new(50.0, 0.0, 0.0));
}

#[test]
fn neutral_neighborhood_has_no_false_hue_boundaries() {
    let values = layer_values();
    for [a, b] in [
        [0.02, 0.01],
        [0.01, 0.02],
        [-0.02, 0.01],
        [-0.01, 0.02],
        [-0.01, -0.01],
        [-0.02, -0.01],
        [-0.01, -0.02],
        [0.02, -0.01],
        [0.01, -0.02],
    ] {
        let mut paths = chroma_paths();
        paths.blend = 0.0;
        paths.ucs = Ucs::new(50.0, a, b);
        let penalty = evaluate(&values, &paths).expect("feasible neutral neighborhood");
        assert!(penalty.value.is_finite() && penalty.minimum_margin > 0.0);
        let published =
            check_published_inverse(paths.ucs).expect("feasible published neutral neighborhood");
        assert!(published.minimum_margin > 0.0);
    }
    let mut folded = chroma_paths();
    folded.blend = 0.0;
    folded.ucs = Ucs::new(50.0, -100.0, -100.0);
    let rejected = evaluate(&values, &folded).expect_err("infeasible inactive inverse path");
    assert_eq!(rejected.path, Some(InversePath::Ucs));
    assert!(matches!(
        rejected.kind,
        DomainViolationKind::ReconstructionDenominator { .. }
    ));
    assert!(matches!(
        check_published_inverse(folded.ucs)
            .expect_err("infeasible published inverse")
            .kind,
        DomainViolationKind::ReconstructionDenominator { .. }
    ));
}
