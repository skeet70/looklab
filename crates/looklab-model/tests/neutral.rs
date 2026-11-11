// SPDX-License-Identifier: AGPL-3.0-or-later
//! Regularity of the near-neutral Cartesian core (docs/02 §3.3).

use core::f64::consts::PI;
use looklab_color::Xyz;
use looklab_model::basis::{CORE_TERMS, NEUTRAL_JOIN_C, S_COEFS, outer_weight};
use looklab_model::look::{ColorLayer, Look, Residual, Tint};
use looklab_model::solve::reference_white;
use looklab_model::{ClipSettings, LookInput, Tone};

fn core_look() -> Look {
    let mut color = ColorLayer::zeros();
    for component in [&mut color.g, &mut color.r, &mut color.lambda] {
        for value in component.core.iter_mut() {
            *value = 0.1;
        }
        for value in component.volume.iter_mut() {
            *value = 0.1;
        }
    }
    let tone = Tone::colorimetric(&looklab_model::basis::TONE_KNOTS).expect("tone");
    Look::new(
        tone,
        Tint::zeros(),
        color,
        Residual::zeros(),
        ClipSettings::default_v1(),
    )
    .expect("look")
}

fn neutral(y: f64) -> Xyz {
    reference_white().scale(y)
}

fn layers(look: &Look, xyz: Xyz) -> [f64; 3] {
    let values = look.layer_values(&LookInput::new(xyz));
    [values.log_g, values.r, values.lambda]
}

#[test]
fn blend_weight_is_flat_at_both_ends() {
    let mut previous = 0.0;
    for step in 0..=400 {
        let c = 2.5 * step as f64 / 400.0;
        let (w, dw) = outer_weight(c);
        assert!(w.is_finite() && dw.is_finite(), "c={c}");
        assert!((0.0..=1.0).contains(&w), "c={c} w={w}");
        assert!(w >= previous - 1e-12, "c={c} not monotone");
        previous = w;
        assert!(dw.abs() < 2.5, "c={c} dw={dw}");
    }
    assert_eq!(outer_weight(0.0), (0.0, 0.0));
    assert_eq!(outer_weight(-1.0), (0.0, 0.0));
    assert_eq!(outer_weight(NEUTRAL_JOIN_C), (1.0, 0.0));
    let (w, dw) = outer_weight(NEUTRAL_JOIN_C - 1e-9);
    assert!((w - 1.0).abs() < 1e-6, "w={w}");
    assert!(dw.abs() < 1e-6, "dw={dw}");
}

#[test]
fn core_paths_crossing_neutral_are_smooth() {
    let look = core_look();
    let y = 0.18;
    for direction in 0..8 {
        let angle = 2.0 * PI * direction as f64 / 8.0;
        let xyz_direction = Xyz::new(libm::cos(angle), 0.0, libm::sin(angle));
        let at = |t: f64| {
            layers(
                &look,
                Xyz::new(
                    neutral(y).x + t * xyz_direction.x,
                    y,
                    neutral(y).z + t * xyz_direction.z,
                ),
            )
        };
        let base = at(0.0);
        for value in base {
            assert!(value.is_finite());
        }
        for t in [-1e-4, -1e-5, 1e-5, 1e-4] {
            for value in at(t) {
                assert!(value.is_finite(), "direction {direction} t={t}");
            }
        }
        let left = at(-1e-5);
        let right = at(1e-5);
        for channel in 0..3 {
            assert!((left[channel] - right[channel]).abs() < 1.0);
        }
    }
}

#[test]
fn ordinary_chromatic_paths_are_finite() {
    let look = core_look();
    let y = 0.18;
    for direction in 0..8 {
        let angle = 2.0 * PI * direction as f64 / 8.0;
        for t in [-0.3, 0.1, 0.3, 0.5] {
            let xyz = Xyz::new(
                neutral(y).x + t * libm::cos(angle),
                y,
                neutral(y).z + t * libm::sin(angle),
            );
            let values = look.layer_values(&LookInput::new(xyz));
            assert!(values.log_g.is_finite() && values.r.is_finite() && values.lambda.is_finite());
            let output = look.eval(xyz);
            assert!(output.j.is_finite() && output.a.is_finite() && output.b.is_finite());
        }
    }
}

#[test]
fn outer_is_unchanged_above_the_join() {
    let look = core_look();
    let y = 0.18;
    let xyz = Xyz::new(neutral(y).x + 0.3, y, neutral(y).z + 0.2);
    let input = LookInput::new(xyz);
    assert!(input.c_in > NEUTRAL_JOIN_C, "c={}", input.c_in);
    let values = look.layer_values(&input);
    let component = |g: &looklab_model::GrlComponent| {
        let mut plane = 0.0;
        for (weight, index) in input.grl.s_w.iter().zip(input.grl.s_idx.iter()) {
            plane += weight * g.plane[*index];
        }
        let mut volume = 0.0;
        for h in 0..4 {
            for s in 0..4 {
                let base = (input.grl.hue_idx[h] * S_COEFS + input.grl.s_idx[s]) * 3;
                for c in 1..4 {
                    volume += input.grl.hue_w[h]
                        * input.grl.s_w[s]
                        * input.grl.c_w[c]
                        * g.volume[base + c - 1];
                }
            }
        }
        input.grl.c_w[0] * plane + volume
    };
    assert!((values.log_g - component(&look.color.g)).abs() < 1e-12);
    assert!((values.r - component(&look.color.r)).abs() < 1e-12);
    assert!((values.lambda - component(&look.color.lambda)).abs() < 1e-12);
}

#[test]
fn core_coefficients_round_trip_through_the_vector() {
    let mut look = core_look();
    let layout = look.layout();
    let mut coef = look.coef_vector();
    for offset in 0..CORE_TERMS {
        coef[layout.g.start + S_COEFS + looklab_model::basis::VOLUME_COEFS + offset] = -0.2;
    }
    look.apply_coef(&coef).expect("apply");
    assert_eq!(look.coef_vector(), coef);
}

fn internal_second_difference(at: impl Fn(f64) -> [f64; 2], channel: usize, step: f64) -> f64 {
    let value = |t: f64| at(t)[channel];
    (value(step) + value(-step) - 2.0 * value(0.0)) / (step * step)
}

#[test]
fn neutral_wb_path_second_difference_does_not_diverge() {
    let look = core_look();
    let white = reference_white();
    let y = 0.18;
    let at = |w: f64| {
        let input = LookInput::new(Xyz::new(white.x * y * libm::exp(w), y, white.z * y));
        let values = look.layer_values(&input);
        look.chroma_paths(&input, &values).internal
    };
    let identity = Look::colorimetric();
    let identity_at = |w: f64| identity.eval(Xyz::new(white.x * y * libm::exp(w), y, white.z * y));
    for (channel, name) in [(0usize, "n_a"), (1, "n_b")] {
        let coarse = internal_second_difference(at, channel, 1e-3);
        let middle = internal_second_difference(at, channel, 1e-4);
        let fine = internal_second_difference(at, channel, 1e-5);
        let exponent = -(libm::log(fine.abs()) - libm::log(coarse.abs()))
            / (libm::log(1e-5) - libm::log(1e-3));
        println!("{name}: fd2 {coarse:e} {middle:e} {fine:e} exponent {exponent}");
        assert!(coarse.is_finite() && middle.is_finite() && fine.is_finite());
        assert!(
            (-0.1..=0.1).contains(&exponent),
            "{name} exponent {exponent}, fd2 {coarse:e} {middle:e} {fine:e}"
        );
    }
    for w in [-1e-3, -1e-4, 0.0, 1e-4, 1e-3] {
        let xyz = Xyz::new(white.x * y * libm::exp(w), y, white.z * y);
        let expected = looklab_model::solve::ucs_of_xyz(xyz);
        let output = identity_at(w);
        let error = (output.j - expected.j)
            .abs()
            .max((output.a - expected.a).abs())
            .max((output.b - expected.b).abs());
        assert!(error < 1e-9, "w {w}: {output:?} vs {expected:?}");
    }
    let neutral = identity_at(0.0);
    assert!(neutral.a.abs() < 1e-12 && neutral.b.abs() < 1e-12);
}

#[test]
fn tinted_crossings_from_four_directions_are_continuous() {
    let mut look = core_look();
    for (index, value) in look.tint.a.iter_mut().enumerate() {
        *value = 0.4 * ((index % 3) as f64 - 1.0);
    }
    for (index, value) in look.tint.b.iter_mut().enumerate() {
        *value = -0.3 * ((index % 4) as f64 - 1.5);
    }
    let white = reference_white();
    let y = 0.18;
    let directions = [
        Xyz::new(1.0, 0.0, 0.0),
        Xyz::new(0.0, 0.0, 1.0),
        Xyz::new(1.0, 0.0, 1.0),
        Xyz::new(1.0, 0.0, -1.0),
    ];
    for direction in directions {
        let at = |t: f64| {
            look.eval(Xyz::new(
                white.x * y + t * direction.x,
                y,
                white.z * y + t * direction.z,
            ))
        };
        let centre = at(0.0);
        for value in centre.to_array() {
            assert!(value.is_finite());
        }
        let mut slopes = [[0.0_f64; 3]; 2];
        for (step_index, step) in [1e-4, 1e-5].into_iter().enumerate() {
            let left = at(-step).to_array();
            let right = at(step).to_array();
            for (channel, slope) in slopes[step_index].iter_mut().enumerate() {
                *slope = (right[channel] - left[channel]) / (2.0 * step);
                assert!(slope.is_finite(), "step {step} channel {channel}");
            }
        }
        for (channel, pair) in slopes[0].iter().zip(slopes[1].iter()).enumerate() {
            let scale = pair.0.abs().max(pair.1.abs()).max(1e-3);
            assert!(
                (pair.0 - pair.1).abs() / scale < 0.05,
                "step dependence channel {channel}: {} vs {}",
                pair.0,
                pair.1
            );
        }
    }
}
