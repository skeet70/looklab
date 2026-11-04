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
fn documented_viewing_condition_names_decode() {
    let json = r#"{"white":"D65","L_A":4.074366543152521,"Y_b":20.0,"surround":"average","discount_illuminant":true}"#;
    let record: looklab_model::file::ViewingConditionsRecord =
        serde_json::from_str(json).expect("documented names");
    assert!((record.l_a - 4.074_366_543_152_521).abs() < 1e-15);
    assert_eq!(record.y_b, 20.0);
    let encoded = serde_json::to_string(&record).expect("serialize");
    assert!(encoded.contains("\"L_A\""));
    assert!(encoded.contains("\"Y_b\""));
}

#[test]
fn non_finite_coefficients_are_rejected() {
    let mut look = decorated();
    look.residual.values[0] = f64::NAN;
    assert!(look.validate().is_err());
    assert!(LookDocument::from_look(identity("nan", "1.0.0").unwrap(), look.clone()).is_err());

    let mut look = decorated();
    let layout = look.layout();
    let mut coef = look.coef_vector();
    coef[layout.tint_a.start] = f64::INFINITY;
    assert!(look.apply_coef(&coef).is_err());
}

#[test]
fn canonical_coefficient_refs_are_enforced() {
    let look = decorated();
    let mut file = looklab_model::LookFile::from_look(identity("refs", "1.0.0").unwrap(), &look)
        .expect("file");
    file.layers.hue.coef_ref = looklab_model::BinRef::new("bin:G").unwrap();
    assert!(LookDocument::new(file, look.clone()).is_err());

    let colorimetric = Look::colorimetric();
    let mut file =
        looklab_model::LookFile::from_look(identity("refs2", "1.0.0").unwrap(), &colorimetric)
            .expect("file");
    file.layers.tone.coef_ref = Some(looklab_model::BinRef::new("bin:T").unwrap());
    assert!(LookDocument::new(file, colorimetric).is_err());
}

#[test]
fn uncertainty_arrays_round_trip() {
    use looklab_model::{Density, ProbeCovariance, UncertaintyData, probes};

    let probes = probes::V1_PROBE_COUNT;
    let covariance = ProbeCovariance::new(probes, vec![0.25; probes * 9]).expect("covariance");
    let density = Density::new(vec![1.5; probes]).expect("density");
    let document = LookDocument::from_look(identity("unc", "1.0.0").unwrap(), decorated())
        .expect("document")
        .with_uncertainty(UncertaintyData {
            probe_cov: Some(covariance.clone()),
            density: Some(density.clone()),
        })
        .expect("uncertainty");
    let bytes = document.encode().expect("encode");
    let decoded = LookDocument::decode(&bytes).expect("decode");
    assert_eq!(decoded.uncertainty.probe_cov, Some(covariance));
    assert_eq!(decoded.uncertainty.density, Some(density));
    assert_eq!(decoded.encode().expect("re-encode"), bytes);
}

#[test]
fn uncertainty_dimensions_are_checked() {
    use looklab_model::{Density, ProbeCovariance, UncertaintyData, probes};

    let wrong = ProbeCovariance::new(2, vec![0.0; 17]);
    assert!(wrong.is_err());
    let document =
        LookDocument::from_look(identity("unc2", "1.0.0").unwrap(), decorated()).expect("document");
    let wrong_density = UncertaintyData {
        probe_cov: None,
        density: Some(Density::new(vec![1.0; probes::V1_PROBE_COUNT - 1]).unwrap()),
    };
    assert!(document.clone().with_uncertainty(wrong_density).is_err());
    let wrong_covariance = UncertaintyData {
        probe_cov: Some(
            ProbeCovariance::new(
                probes::V1_PROBE_COUNT - 1,
                vec![0.0; (probes::V1_PROBE_COUNT - 1) * 9],
            )
            .unwrap(),
        ),
        density: None,
    };
    assert!(document.with_uncertainty(wrong_covariance).is_err());
}

#[test]
fn negative_tone_slopes_are_rejected() {
    let mut beta = [0.0_f64; TONE_COEFS];
    beta[0] = -1.0;
    assert!(Tone::spline(0.0, beta).is_err());
    assert!(Tone::colorimetric(&TONE_KNOTS).is_ok());
    assert!(Tone::colorimetric(&[0.0; 4]).is_err());
}
