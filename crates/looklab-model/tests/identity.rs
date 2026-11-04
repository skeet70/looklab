// SPDX-License-Identifier: AGPL-3.0-or-later
//! P1.4 identity: a colorimetric Look reproduces colorimetric CAM16-UCS.

use looklab_color::cam16::{Cam16Model, Jmh, ViewingConditions};
use looklab_color::ucs::{UcsSpace, jmh_to_ucs};
use looklab_model::basis::TONE_KNOTS;
use looklab_model::look::LookInput;
use looklab_model::probes::{ProbeGroup, v1};
use looklab_model::{Look, Tone, neutral_j_prime, solve_y_ref};

fn colorimetric(xyz: looklab_color::Xyz) -> looklab_color::ucs::Ucs {
    let model = Cam16Model::new(&ViewingConditions::reference());
    let cam = model.xyz_to_cam16(xyz);
    jmh_to_ucs(
        Jmh {
            j: cam.j,
            m: cam.m,
            h: cam.h,
        },
        UcsSpace::Ucs,
    )
}

#[test]
fn y_ref_range_is_explicit() {
    use looklab_model::{J_PRIME_MAX, SolveError, neutral_j_prime_max, try_solve_y_ref};

    let max = neutral_j_prime_max();
    assert!(max > 0.0 && max < J_PRIME_MAX);
    assert!(max > neutral_j_prime(1e6));
    assert!(matches!(
        try_solve_y_ref(max + 1.0, 0.18),
        Err(SolveError::Unattainable { .. })
    ));
    assert!(matches!(
        try_solve_y_ref(-1.0, 0.18),
        Err(SolveError::Unattainable { .. })
    ));
    assert!(solve_y_ref(0.0, 0.18) == 0.0);

    let relaxed = max * (1.0 - 1e-12);
    let y = try_solve_y_ref(relaxed, 0.18).expect("attainable near the limit");
    println!("max {max} y {y}");
    assert!(y.is_finite() && y > 1e8);
    assert!((neutral_j_prime(y) - relaxed).abs() < 1e-9);

    let saturated = solve_y_ref(max + 10.0, 0.18);
    assert!(saturated.is_finite());
    assert!((neutral_j_prime(saturated) - max * (1.0 - 1e-12)).abs() < 1e-9);
}

#[test]
fn colorimetric_look_matches_cam16_ucs() {
    let look = Look::colorimetric();
    let mut groups = std::collections::BTreeSet::new();
    for probe in v1().probes {
        groups.insert(probe.group);
        let expected = colorimetric(probe.xyz);
        let actual = look.eval(probe.xyz);
        let error = (actual.j - expected.j)
            .abs()
            .max((actual.a - expected.a).abs())
            .max((actual.b - expected.b).abs());
        assert!(
            error < 1e-9,
            "probe {} expected {expected:?} actual {actual:?} error {error}",
            probe.id.as_str()
        );
    }
    for group in [
        ProbeGroup::GrayRamp,
        ProbeGroup::ChartPatches,
        ProbeGroup::HueGrid,
        ProbeGroup::Skin,
        ProbeGroup::MemoryColors,
    ] {
        assert!(groups.contains(&group), "identity test missed {group:?}");
    }
}

#[test]
fn colorimetric_identity_holds_on_chromatic_probes() {
    let look = Look::colorimetric();
    let mut checked = 0;
    for probe in v1().probes {
        if !matches!(
            probe.group,
            ProbeGroup::HueGrid | ProbeGroup::ChartPatches | ProbeGroup::Skin
        ) {
            continue;
        }
        let expected = colorimetric(probe.xyz);
        let actual = look.eval(probe.xyz);
        let error = (actual.j - expected.j)
            .abs()
            .max((actual.a - expected.a).abs())
            .max((actual.b - expected.b).abs());
        assert!(
            error < 1e-9,
            "probe {} expected {expected:?} actual {actual:?} error {error}",
            probe.id.as_str()
        );
        checked += 1;
    }
    assert!(checked > 900, "checked {checked}");
}

#[test]
fn neutral_probes_have_zero_chroma() {
    let look = Look::colorimetric();
    for probe in v1()
        .probes
        .into_iter()
        .filter(|probe| probe.group == ProbeGroup::GrayRamp)
    {
        let actual = look.eval(probe.xyz);
        assert!(
            actual.a.abs() < 1e-12 && actual.b.abs() < 1e-12,
            "probe {} a={} b={}",
            probe.id.as_str(),
            actual.a,
            actual.b
        );
    }
}

#[test]
fn tone_only_looks_keep_the_reference_chroma_and_hue() {
    let first = Look::colorimetric_spline().expect("v1 tone knots");
    let mut second = first.clone();
    let mut beta = *first.tone.betas().expect("spline tone");
    beta[6] += 0.25;
    second.tone = Tone::spline(0.0, beta).expect("non-negative betas");

    for probe in v1().probes {
        let input = LookInput::new(probe.xyz);
        let values_a = first.layer_values(&input);
        let values_b = second.layer_values(&input);
        assert_eq!(values_a.log_g, values_b.log_g);
        assert_eq!(values_a.r, values_b.r);
        assert_eq!(values_a.lambda, values_b.lambda);

        for look in [&first, &second] {
            let values = look.layer_values(&input);
            let y_ref = solve_y_ref(values.t, 0.18 * libm::pow(2.0, input.s));
            let reference = colorimetric(input.unit_xyz.scale(y_ref));
            let m_ref = (reference.a * reference.a + reference.b * reference.b).sqrt();
            if m_ref < 1e-9 {
                continue;
            }
            let output = look.eval_input(&input);
            let m_out = (output.a * output.a + output.b * output.b).sqrt();
            assert!(
                (m_out - m_ref).abs() < 1e-8,
                "probe {} m_out {m_out} m_ref {m_ref}",
                probe.id.as_str()
            );
            let h_ref = libm::atan2(reference.b, reference.a);
            let h_out = libm::atan2(output.b, output.a);
            let hue_error = (h_out - h_ref).abs();
            assert!(
                hue_error < 1e-8,
                "probe {} h_out {h_out} h_ref {h_ref}",
                probe.id.as_str()
            );
        }
    }
}

#[test]
fn y_ref_solve_converges() {
    let targets = [1e-6, 0.5, 40.0, 63.3, 100.0, 140.0, 143.5];
    for target in targets {
        let y = solve_y_ref(target, 0.18);
        assert!(y.is_finite() && y >= 0.0);
        let achieved = neutral_j_prime(y);
        assert!(
            (achieved - target).abs() < 1e-9,
            "target {target} achieved {achieved} at y {y}"
        );
    }
    for s in [-12.0, -8.0, -4.0, 0.0, 3.0, 6.0] {
        let target = neutral_j_prime(0.18 * libm::pow(2.0, s));
        let y = solve_y_ref(target, 0.18 * libm::pow(2.0, s));
        assert!((y - 0.18 * libm::pow(2.0, s)).abs() < 1e-9 * y.max(1.0));
    }
    for probe in v1().probes {
        let input = LookInput::new(probe.xyz);
        let target = neutral_j_prime(input.y);
        let y = solve_y_ref(target, input.y);
        assert!(
            (neutral_j_prime(y) - target).abs() < 1e-9,
            "probe {} target {target}",
            probe.id.as_str()
        );
    }
}

#[test]
fn neutral_output_equals_the_tint() {
    let mut look = Look::colorimetric_spline().expect("v1 tone knots");
    look.tint.a[3] = 0.6;
    look.tint.b[4] = -0.5;
    look.color.g.plane[2] = 0.3;
    look.color.r.plane[6] = 12.0;
    look.color.lambda.plane[8] = -2.0;
    look.color.g.volume[57] = 0.4;
    look.color.r.volume[123] = 7.0;
    look.color.lambda.volume[600] = 1.5;
    for probe in v1()
        .probes
        .into_iter()
        .filter(|probe| probe.group == ProbeGroup::GrayRamp)
    {
        let input = LookInput::new(probe.xyz);
        assert_eq!(input.c_in, 0.0);
        let values = look.layer_values(&input);
        let output = look.eval_input(&input);
        assert!(
            (output.a - values.n_a).abs() < 1e-12 && (output.b - values.n_b).abs() < 1e-12,
            "probe {} output ({}, {}) tint ({}, {})",
            probe.id.as_str(),
            output.a,
            output.b,
            values.n_a,
            values.n_b
        );
    }
}

#[test]
fn colorimetric_tone_spline_tracks_the_exact_tone() {
    let tone = Tone::colorimetric(&TONE_KNOTS).expect("v1 tone knots");
    let mut worst = 0.0_f64;
    for step in 0..=360 {
        let s = -12.0 + 18.0 * step as f64 / 360.0;
        let difference = (tone.eval(s) - Tone::Colorimetric.eval(s)).abs();
        worst = worst.max(difference);
    }
    assert!(worst < 1e-3, "worst tone fit error {worst}");
}
