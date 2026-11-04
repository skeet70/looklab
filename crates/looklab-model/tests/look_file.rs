// SPDX-License-Identifier: AGPL-3.0-or-later
//! Look file round-trip: bit-identical coefficients and byte-identical re-encode.

use looklab_model::basis::{TONE_COEFS, TONE_KNOTS};
use looklab_model::file::identity;
use looklab_model::look::{ColorLayer, Look, Residual, Tint};
use looklab_model::{ClipSettings, LookDocument, Tone};

fn decorated() -> Look {
    let mut beta = [0.0_f64; TONE_COEFS];
    beta[2] = 0.05;
    beta[13] = 0.3;
    let tone = Tone::spline(0.25, beta).expect("non-negative betas");
    let tint = Tint {
        a: (0..12).map(|index| 0.1 * index as f64 - 0.5).collect(),
        b: (0..12).map(|index| -0.07 * index as f64 + 0.3).collect(),
    };
    let mut color = ColorLayer::zeros();
    for component in [&mut color.g, &mut color.r, &mut color.lambda] {
        for (index, coefficient) in component
            .plane
            .iter_mut()
            .chain(component.volume.iter_mut())
            .enumerate()
        {
            *coefficient = libm::sin(index as f64 * 0.001);
        }
    }
    let residual = Residual {
        values: (0..looklab_model::basis::RESIDUAL_COEFS)
            .map(|index| libm::sin(index as f64 * 0.01))
            .collect(),
    };
    Look::new(tone, tint, color, residual, ClipSettings::default_v1()).expect("valid look")
}

#[test]
fn round_trip_is_exact_for_spline_looks() {
    let look = decorated();
    let document = LookDocument::from_look(identity("round-trip", "1.0.0").unwrap(), look.clone())
        .expect("document");
    let bytes = document.encode().expect("encode");
    let decoded = LookDocument::decode(&bytes).expect("decode");
    assert_eq!(decoded.look, look);
    assert_eq!(decoded.file, document.file);
    assert_eq!(decoded.encode().expect("re-encode"), bytes);
}

#[test]
fn round_trip_is_exact_for_colorimetric_looks() {
    let look = Look::colorimetric();
    let document = LookDocument::from_look(identity("identity", "0.1.0").unwrap(), look.clone())
        .expect("document");
    let bytes = document.encode().expect("encode");
    let decoded = LookDocument::decode(&bytes).expect("decode");
    assert_eq!(decoded.look, look);
    assert_eq!(decoded.encode().expect("re-encode"), bytes);
}

#[test]
fn coefficients_survive_bit_for_bit() {
    let look = decorated();
    let document = LookDocument::from_look(identity("bits", "2.0.0").unwrap(), look.clone())
        .expect("document");
    let decoded = LookDocument::decode(&document.encode().expect("encode")).expect("decode");
    let original = look.coef_vector();
    let recovered = decoded.look.coef_vector();
    assert_eq!(original.len(), recovered.len());
    for (a, b) in original.iter().zip(recovered.iter()) {
        assert_eq!(a.to_bits(), b.to_bits());
    }
}

#[test]
fn negative_tone_slopes_are_rejected() {
    let mut beta = [0.0_f64; TONE_COEFS];
    beta[0] = -1.0;
    assert!(Tone::spline(0.0, beta).is_err());
    assert!(Tone::colorimetric(&TONE_KNOTS).is_ok());
    assert!(Tone::colorimetric(&[0.0; 4]).is_err());
}
