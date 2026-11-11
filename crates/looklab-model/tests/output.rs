// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_color::ucs::Ucs;
use looklab_model::{OutputContext, OutputPin};
use looklab_schema::OutputEncoding;

fn pin(encoding: OutputEncoding, clip_k: f64) -> OutputPin {
    OutputPin::new(encoding, clip_k).expect("valid pin")
}

#[test]
fn pin_and_context_round_trip_through_their_canonical_forms() {
    let target = pin(OutputEncoding::Srgb, 0.03);
    let camera = pin(OutputEncoding::AdobeRgb, 0.02);
    assert_eq!(target.canonical(), "srgb@0.03");
    assert_eq!(OutputPin::parse("srgb@0.03").expect("parse"), target);
    let context = OutputContext { target, camera };
    assert_eq!(
        OutputContext::parse(&context.canonical()).expect("parse"),
        context
    );
}

#[test]
fn context_rejects_duplicate_missing_and_unknown_pins() {
    assert!(OutputContext::parse("target=srgb@0.03").is_err());
    assert!(OutputContext::parse("camera=srgb@0.03").is_err());
    assert!(OutputContext::parse("target=srgb@0.03;camera=srgb@0.03;target=srgb@0.03").is_err());
    assert!(OutputContext::parse("target=srgb@0.03;camera=srgb@0.03;camera=srgb@0.03").is_err());
    assert!(OutputContext::parse("target=srgb@0.03;camera=srgb@0.03;other=srgb@0.03").is_err());
}

#[test]
fn pin_parse_rejects_invalid_clips_and_shapes() {
    for text in ["srgb@0.0", "srgb@0.5", "srgb@0.03@0.04", "srgb", "@0.03"] {
        assert!(OutputPin::parse(text).is_err(), "{text}");
    }
}

#[test]
fn render_jacobian_neutral_identity_and_active_clip_direction() {
    for encoding in [OutputEncoding::Srgb, OutputEncoding::AdobeRgb] {
        let pin = pin(encoding, 0.03);
        for lightness in [30.0, 50.0, 70.0, 90.0] {
            let jacobian = pin
                .render_jacobian(Ucs::new(lightness, 0.0, 0.0))
                .expect("inactive neutral jacobian");
            for row in 0..3 {
                for column in 0..3 {
                    let expected = if row == column { 1.0 } else { 0.0 };
                    assert!(
                        (jacobian.0[row][column] - expected).abs() < 1e-7,
                        "{encoding:?} J {lightness}: row {row} column {column} is {}",
                        jacobian.0[row][column]
                    );
                }
            }
        }
    }

    let pin = pin(OutputEncoding::Srgb, 0.03);
    for lightness in [8.0, 14.0, 99.5] {
        let neutral = Ucs::new(lightness, 0.0, 0.0);
        let jacobian = pin
            .render_jacobian(neutral)
            .expect("active neutral jacobian");
        assert!(jacobian.0.iter().flatten().all(|value| value.is_finite()));
        assert!(
            jacobian.0[1][1].abs() > 0.0,
            "J {lightness}: chroma derivative {}",
            jacobian.0[1][1]
        );
        let step = 1e-4;
        let base = pin.render(neutral);
        let shifted = pin.render(Ucs::new(lightness, step, 0.0));
        let forward = (shifted.a - base.a) / step;
        assert!(
            (jacobian.0[1][1] - forward).abs() < 5e-3,
            "J {lightness}: analytic {} vs forward {forward}",
            jacobian.0[1][1]
        );
    }
}

#[test]
fn pin_json_deserialization_enforces_the_clip_invariant() {
    let valid = pin(OutputEncoding::Srgb, 0.03);
    let text = serde_json::to_string(&valid).expect("encode");
    assert_eq!(
        serde_json::from_str::<OutputPin>(&text).expect("decode"),
        valid
    );
    assert!(serde_json::from_str::<OutputPin>(r#"{"encoding":"srgb","clip_k":0.5}"#).is_err());
    assert!(serde_json::from_str::<OutputPin>(r#"{"encoding":"srgb","clip_k":0.0}"#).is_err());
    assert!(
        serde_json::from_str::<OutputPin>(r#"{"encoding":"srgb","clip_k":0.03,"extra":1}"#)
            .is_err()
    );
    assert!(
        serde_json::from_str::<OutputContext>(
            r#"{"target":{"encoding":"srgb","clip_k":0.5},"camera":{"encoding":"srgb","clip_k":0.03}}"#
        )
        .is_err()
    );
}
