// SPDX-License-Identifier: AGPL-3.0-or-later
//! Chromatic adaptation with the von Kries model, ported from colour-science
//! (`matrix_chromatic_adaptation_VonKries`, `chromatic_adaptation_VonKries`;
//! BSD-3-Clause, Colour Developers; see `NOTICE`).

use crate::linalg::Mat3;
use crate::types::Xyz;
use crate::xyy::safe_div;
use serde::{Deserialize, Serialize};

/// Cone response transforms in `colour.CHROMATIC_ADAPTATION_TRANSFORMS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cat {
    Cat02,
    Cat16,
    Bradford,
}

impl Cat {
    /// Cone response matrix from `CHROMATIC_ADAPTATION_TRANSFORMS`.
    pub fn matrix(self) -> Mat3 {
        match self {
            Self::Cat02 => Mat3([
                [0.7328, 0.4296, -0.1624],
                [-0.7036, 1.6975, 0.0061],
                [0.0030, 0.0136, 0.9834],
            ]),
            Self::Cat16 => Mat3([
                [0.401_288, 0.650_173, -0.051_461],
                [-0.250_268, 1.204_414, 0.045_854],
                [-0.002_079, 0.048_952, 0.953_127],
            ]),
            Self::Bradford => Mat3([
                [0.8951, 0.2664, -0.1614],
                [-0.7502, 1.7135, 0.0367],
                [0.0389, -0.0685, 1.0296],
            ]),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Cat02 => "CAT02",
            Self::Cat16 => "CAT16",
            Self::Bradford => "Bradford",
        }
    }
}

/// colour-science `matrix_chromatic_adaptation_VonKries`. The whites are used
/// as given; pass Y = 1 values such as [`Illuminant::xyz`](crate::xyy::Illuminant::xyz).
pub fn adaptation_matrix(cat: Cat, src_white: Xyz, dst_white: Xyz) -> Mat3 {
    let m = cat.matrix();
    let src_cone = m.mul_vec(src_white.to_array());
    let dst_cone = m.mul_vec(dst_white.to_array());
    let scale = Mat3::diag([
        safe_div(dst_cone[0], src_cone[0]),
        safe_div(dst_cone[1], src_cone[1]),
        safe_div(dst_cone[2], src_cone[2]),
    ]);
    m.inverse()
        .expect("cone response transforms are non-singular")
        .mul(&scale)
        .mul(&m)
}

/// colour-science `chromatic_adaptation_VonKries`.
pub fn adapt(xyz: Xyz, cat: Cat, src_white: Xyz, dst_white: Xyz) -> Xyz {
    Xyz::from_array(adaptation_matrix(cat, src_white, dst_white).mul_vec(xyz.to_array()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xyy::Illuminant;

    #[test]
    fn d65_to_d50_bradford_matches_the_published_matrix() {
        let m = adaptation_matrix(Cat::Bradford, Illuminant::D65.xyz(), Illuminant::D50.xyz());
        let expected = [
            [
                1.047_929_792_544_996_6,
                0.022_946_870_601_609_61,
                -0.050_192_266_289_205_14,
            ],
            [
                0.029_627_808_770_055_802,
                0.990_434_426_753_879_8,
                -0.017_073_799_063_418_788,
            ],
            [
                -0.009_243_040_646_204_506,
                0.015_055_191_490_298_134,
                0.751_874_281_428_137_3,
            ],
        ];
        for (row, expected_row) in m.0.iter().zip(expected.iter()) {
            for (value, expected_value) in row.iter().zip(expected_row.iter()) {
                assert!((value - expected_value).abs() < 1e-12);
            }
        }
    }

    #[test]
    fn adapting_to_the_same_white_is_the_identity() {
        let white = Illuminant::D65.xyz();
        let m = adaptation_matrix(Cat::Cat16, white, white);
        for (row, identity_row) in m.0.iter().zip(Mat3::IDENTITY.0.iter()) {
            for (value, identity) in row.iter().zip(identity_row.iter()) {
                assert!((value - identity).abs() < 1e-15);
            }
        }
    }

    #[test]
    fn adapting_a_white_lands_on_the_destination_white() {
        let adapted = adapt(
            Illuminant::D65.xyz(),
            Cat::Cat02,
            Illuminant::D65.xyz(),
            Illuminant::D50.xyz(),
        );
        let expected = Illuminant::D50.xyz();
        assert!((adapted.x - expected.x).abs() < 1e-12);
        assert!((adapted.y - expected.y).abs() < 1e-12);
        assert!((adapted.z - expected.z).abs() < 1e-12);
    }
}
