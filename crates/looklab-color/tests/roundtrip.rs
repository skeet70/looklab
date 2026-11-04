// SPDX-License-Identifier: AGPL-3.0-or-later
//! Round-trip properties of the colour conversions.

use looklab_color::cat::{Cat, adapt, adaptation_matrix};
use looklab_color::lab::{Lab, lab_to_lchab, lab_to_xyz, lchab_to_lab, xyz_to_lab};
use looklab_color::spaces::RgbSpace;
use looklab_color::types::{Rgb, Xyy, Xyz};
use looklab_color::xyy::{Illuminant, xyy_to_xyz, xyz_to_xyy};
use proptest::prelude::*;

proptest! {
    #[test]
    fn rgb_xyz_round_trip(r in -0.5f64..1.5, g in -0.5f64..1.5, b in -0.5f64..1.5) {
        let rgb = Rgb::new(r, g, b);
        for space in RgbSpace::ALL {
            let back = space.xyz_to_rgb_linear(space.rgb_linear_to_xyz(rgb));
            prop_assert!((back.r - rgb.r).abs() < 1e-12, "{} red", space.name());
            prop_assert!((back.g - rgb.g).abs() < 1e-12, "{} green", space.name());
            prop_assert!((back.b - rgb.b).abs() < 1e-12, "{} blue", space.name());
        }
    }

    #[test]
    fn transfer_functions_round_trip(v in 0.0f64..=1.0) {
        for space in RgbSpace::ALL {
            let back = space.decode(space.encode(v));
            prop_assert!((back - v).abs() < 1e-10, "{} at {v}", space.name());
        }
    }

    #[test]
    fn xyy_round_trip(x in 0.0f64..1.0, y in 0.02f64..1.0, big_y in 0.0f64..2.0) {
        let xyy = Xyy { x, y, big_y };
        let back = xyz_to_xyy(xyy_to_xyz(xyy));
        prop_assert!((back.x - xyy.x).abs() < 1e-9);
        prop_assert!((back.y - xyy.y).abs() < 1e-9);
        prop_assert!((back.big_y - xyy.big_y).abs() < 1e-9);
    }

    #[test]
    fn lab_round_trip(x in 0.0f64..2.0, y in 0.0f64..2.0, z in 0.0f64..2.0) {
        let white = Illuminant::D65.xyz();
        let xyz = Xyz::new(x, y, z);
        let back = lab_to_xyz(xyz_to_lab(xyz, white), white);
        prop_assert!((back.x - xyz.x).abs() < 1e-10);
        prop_assert!((back.y - xyz.y).abs() < 1e-10);
        prop_assert!((back.z - xyz.z).abs() < 1e-10);
    }

    #[test]
    fn lchab_round_trip(l in -10.0f64..110.0, a in -120.0f64..120.0, b in -120.0f64..120.0) {
        let lab = Lab::new(l, a, b);
        let back = lchab_to_lab(lab_to_lchab(lab));
        prop_assert!((back.l - lab.l).abs() < 1e-9);
        prop_assert!((back.a - lab.a).abs() < 1e-9);
        prop_assert!((back.b - lab.b).abs() < 1e-9);
    }
}

#[test]
fn adaptation_matrices_are_mutual_inverses() {
    let d65 = Illuminant::D65.xyz();
    let d50 = Illuminant::D50.xyz();
    for cat in [Cat::Cat02, Cat::Cat16, Cat::Bradford] {
        let product = adaptation_matrix(cat, d65, d50).mul(&adaptation_matrix(cat, d50, d65));
        for (row, identity) in product.0.iter().zip(looklab_color::Mat3::IDENTITY.0.iter()) {
            for (value, expected) in row.iter().zip(identity) {
                assert!((value - expected).abs() < 1e-12, "{}", cat.name());
            }
        }
    }
}

#[test]
fn adaptation_restores_the_stimulus() {
    let d65 = Illuminant::D65.xyz();
    let d50 = Illuminant::D50.xyz();
    let xyz = Xyz::new(0.2, 0.3, 0.4);
    for cat in [Cat::Cat02, Cat::Cat16, Cat::Bradford] {
        let there = adapt(xyz, cat, d65, d50);
        let back = adapt(there, cat, d50, d65);
        assert!((back.x - xyz.x).abs() < 1e-12, "{}", cat.name());
        assert!((back.y - xyz.y).abs() < 1e-12, "{}", cat.name());
        assert!((back.z - xyz.z).abs() < 1e-12, "{}", cat.name());
    }
}
