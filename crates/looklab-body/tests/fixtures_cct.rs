// SPDX-License-Identifier: AGPL-3.0-or-later
//! CCT against the LibRaw/colour-science reference on the committed fixtures
//! (docs/09 P4.1). Fixtures are CC0 samples from raw.pixls.us.
#![allow(clippy::disallowed_methods)]

use looklab_body::{CameraProfile, Cct, estimate_cct};
use looklab_raw::{DecodedRaw, RawDecoder, RawlerDecoder};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

const TOLERANCE_K: f64 = 150.0;
const TOLERANCE_XY: f64 = 0.002;
const WB_TOLERANCE: f64 = 1e-4;

#[derive(Deserialize)]
struct Golden {
    fixtures: BTreeMap<String, Reference>,
}

#[derive(Clone, Deserialize)]
struct Reference {
    as_shot_wb: [f64; 3],
    cct_k: f64,
    xy: [f64; 2],
}

fn reference(name: &str) -> Reference {
    let golden: Golden =
        serde_json::from_str(include_str!("golden/fixtures_cct.json")).expect("golden file");
    golden.fixtures.get(name).expect("fixture entry").clone()
}

fn decode(name: &str) -> DecodedRaw {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name);
    let bytes = std::fs::read(path).expect("committed fixture");
    RawlerDecoder.decode(&bytes).expect("decodes")
}

fn check(name: &str) {
    let expected = reference(name);
    let image = decode(name);
    for (channel, value) in image.as_shot_wb.iter().enumerate() {
        assert!(
            (value - expected.as_shot_wb[channel]).abs() <= WB_TOLERANCE * value.abs().max(1.0),
            "{name}: as-shot multiplier {channel} is {value}, reference {}",
            expected.as_shot_wb[channel]
        );
    }
    let profile = CameraProfile::from_raw(image.color_matrices.as_ref().expect("colour matrices"))
        .expect("supported calibration illuminants");
    let cct = estimate_cct(&profile, image.as_shot_wb).expect("neutral solve");
    println!(
        "{name}: {:.1} K against a reference of {:.1} K",
        cct.kelvin(),
        expected.cct_k
    );
    assert!(
        (cct.kelvin() - expected.cct_k).abs() <= TOLERANCE_K,
        "{name}: {:.1} K against a reference of {:.1} K",
        cct.kelvin(),
        expected.cct_k
    );
    assert!((cct.xy().0 - expected.xy[0]).abs() <= TOLERANCE_XY);
    assert!((cct.xy().1 - expected.xy[1]).abs() <= TOLERANCE_XY);
    assert!(Cct::from_kelvin(cct.kelvin()).is_ok());
}

#[test]
fn canon_eos_60d_cct_matches_the_reference() {
    check("canon-eos-60d.dng");
}

#[test]
fn fujifilm_x_s10_cct_matches_the_reference() {
    check("fujifilm-x-s10.raf");
}
