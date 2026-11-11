// SPDX-License-Identifier: AGPL-3.0-or-later
//! `crm.json` coefficients must survive the JSON round trip bit for bit.

use looklab_match::ControlCovariance;
use looklab_match::synth;
use looklab_model::realization::{decode_crm, encode_crm};

#[test]
fn crm_json_preserves_coefficient_bits() {
    for seed in [7, 9, 11] {
        let body = synth::body(seed).expect("synthetic body");
        let encoded = encode_crm(&body.model.crm).expect("encode crm");
        let decoded = decode_crm(&encoded).expect("decode crm");
        assert_eq!(
            decoded, body.model.crm,
            "seed {seed}: decoding crm.json changed a coefficient"
        );
        assert_eq!(
            encode_crm(&decoded).expect("re-encode crm"),
            encoded,
            "seed {seed}: crm.json does not round-trip"
        );
    }
}

#[test]
fn covariance_json_preserves_coefficient_bits() {
    let body = synth::body(7).expect("synthetic body");
    let ids = body.model.crm.parameter_ids();
    let n = ids.len();
    let values = [
        0.499_146_808_664_935_2_f64,
        -0.312_313_181_311_170_14,
        1.0e-7 / 3.0,
    ];
    let covariance = ControlCovariance {
        parameter_ids: ids,
        covariance: (0..n * n)
            .map(|index| values[index % values.len()])
            .collect(),
    };
    let encoded = covariance.to_json().expect("encode covariance");
    let decoded = ControlCovariance::from_json(&encoded).expect("decode covariance");
    assert_eq!(
        decoded, covariance,
        "decoding covariance.json changed a coefficient"
    );
    assert_eq!(
        decoded.to_json().expect("re-encode covariance"),
        encoded,
        "covariance.json does not round-trip"
    );
}
