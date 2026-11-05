// SPDX-License-Identifier: AGPL-3.0-or-later
//! RGB colour spaces: primaries, derived RGB/XYZ matrices and transfer
//! functions. Ported from colour-science `RGB_COLOURSPACES`,
//! `normalised_primary_matrix`, `RGB_to_XYZ` and `XYZ_to_RGB` (BSD-3-Clause,
//! Colour Developers; see `NOTICE`).

use std::sync::LazyLock;

use crate::linalg::Mat3;
use crate::trc;
use crate::types::{Rgb, Xyz};
use crate::xyy::{Illuminant, xy_to_xyz};
use serde::{Deserialize, Serialize};

/// CIE xy chromaticity coordinates of an RGB space's three primaries.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Primaries {
    pub red: (f64, f64),
    pub green: (f64, f64),
    pub blue: (f64, f64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RgbSpace {
    Srgb,
    AdobeRgb,
    Rec2020,
    DisplayP3,
    #[serde(rename = "prophoto")]
    ProPhoto,
}

impl RgbSpace {
    pub const ALL: [Self; 5] = [
        Self::Srgb,
        Self::AdobeRgb,
        Self::Rec2020,
        Self::DisplayP3,
        Self::ProPhoto,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Srgb => "sRGB",
            Self::AdobeRgb => "Adobe RGB (1998)",
            Self::Rec2020 => "ITU-R BT.2020",
            Self::DisplayP3 => "Display P3",
            Self::ProPhoto => "ProPhoto RGB",
        }
    }

    /// `primaries` in colour-science's `RGB_COLOURSPACES`.
    pub const fn primaries(self) -> Primaries {
        match self {
            Self::Srgb => Primaries {
                red: (0.640, 0.330),
                green: (0.300, 0.600),
                blue: (0.150, 0.060),
            },
            Self::AdobeRgb => Primaries {
                red: (0.640, 0.330),
                green: (0.210, 0.710),
                blue: (0.150, 0.060),
            },
            Self::Rec2020 => Primaries {
                red: (0.708, 0.292),
                green: (0.170, 0.797),
                blue: (0.131, 0.046),
            },
            Self::DisplayP3 => Primaries {
                red: (0.680, 0.320),
                green: (0.265, 0.690),
                blue: (0.150, 0.060),
            },
            Self::ProPhoto => Primaries {
                red: (0.734_7, 0.265_3),
                green: (0.159_6, 0.840_4),
                blue: (0.036_6, 0.000_1),
            },
        }
    }

    /// `whitepoint` chromaticity in colour-science's `RGB_COLOURSPACES`.
    pub const fn white_xy(self) -> (f64, f64) {
        match self {
            Self::ProPhoto => Illuminant::D50.xy(),
            Self::Srgb | Self::AdobeRgb | Self::Rec2020 | Self::DisplayP3 => Illuminant::D65.xy(),
        }
    }

    /// `whitepoint` tristimulus values, normalised to Y = 1.
    pub fn white(self) -> Xyz {
        xy_to_xyz(self.white_xy())
    }

    /// `matrix_RGB_to_XYZ`, derived with `normalised_primary_matrix`.
    pub fn to_xyz_matrix(self) -> Mat3 {
        MATRICES[self.index()].to_xyz
    }

    /// `matrix_XYZ_to_RGB`.
    pub fn from_xyz_matrix(self) -> Mat3 {
        MATRICES[self.index()].from_xyz
    }

    /// The space's encoding colour component transfer function.
    pub fn encode(self, linear: f64) -> f64 {
        match self {
            Self::Srgb | Self::DisplayP3 => trc::srgb_encode(linear),
            Self::AdobeRgb => trc::adobe_rgb_encode(linear),
            Self::Rec2020 => trc::rec2020_encode(linear),
            Self::ProPhoto => trc::prophoto_encode(linear),
        }
    }

    /// The space's decoding colour component transfer function.
    pub fn decode(self, encoded: f64) -> f64 {
        match self {
            Self::Srgb | Self::DisplayP3 => trc::srgb_decode(encoded),
            Self::AdobeRgb => trc::adobe_rgb_decode(encoded),
            Self::Rec2020 => trc::rec2020_decode(encoded),
            Self::ProPhoto => trc::prophoto_decode(encoded),
        }
    }

    /// colour-science `RGB_to_XYZ` without transfer function decoding.
    pub fn rgb_linear_to_xyz(self, rgb: Rgb) -> Xyz {
        Xyz::from_array(self.to_xyz_matrix().mul_vec(rgb.to_array()))
    }

    /// colour-science `XYZ_to_RGB` without transfer function encoding.
    pub fn xyz_to_rgb_linear(self, xyz: Xyz) -> Rgb {
        Rgb::from_array(self.from_xyz_matrix().mul_vec(xyz.to_array()))
    }

    /// colour-science `RGB_to_XYZ` with transfer function decoding.
    pub fn rgb_encoded_to_xyz(self, rgb: Rgb) -> Xyz {
        self.rgb_linear_to_xyz(Rgb::new(
            self.decode(rgb.r),
            self.decode(rgb.g),
            self.decode(rgb.b),
        ))
    }

    /// colour-science `XYZ_to_RGB` with transfer function encoding.
    pub fn xyz_to_rgb_encoded(self, xyz: Xyz) -> Rgb {
        let linear = self.xyz_to_rgb_linear(xyz);
        Rgb::new(
            self.encode(linear.r),
            self.encode(linear.g),
            self.encode(linear.b),
        )
    }

    const fn index(self) -> usize {
        match self {
            Self::Srgb => 0,
            Self::AdobeRgb => 1,
            Self::Rec2020 => 2,
            Self::DisplayP3 => 3,
            Self::ProPhoto => 4,
        }
    }
}

struct SpaceMatrices {
    to_xyz: Mat3,
    from_xyz: Mat3,
}

static MATRICES: LazyLock<[SpaceMatrices; 5]> = LazyLock::new(|| {
    RgbSpace::ALL.map(|space| {
        let to_xyz = normalised_primary_matrix(space.primaries(), space.white_xy());
        let from_xyz = to_xyz.inverse().expect("primary matrix is non-singular");
        SpaceMatrices { to_xyz, from_xyz }
    })
});

/// colour-science `normalised_primary_matrix`.
pub fn normalised_primary_matrix(primaries: Primaries, white: (f64, f64)) -> Mat3 {
    let (xr, yr) = primaries.red;
    let (xg, yg) = primaries.green;
    let (xb, yb) = primaries.blue;
    let primary_matrix = Mat3([
        [xr / yr, xg / yg, xb / yb],
        [1.0, 1.0, 1.0],
        [
            (1.0 - (xr + yr)) / yr,
            (1.0 - (xg + yg)) / yg,
            (1.0 - (xb + yb)) / yb,
        ],
    ]);
    let scale = primary_matrix
        .inverse()
        .expect("primary matrix is non-singular")
        .mul_vec(xy_to_xyz(white).to_array());
    primary_matrix.mul(&Mat3::diag(scale))
}

/// Fujifilm F-Gamut primaries and D65 white from *F-Log2 Data Sheet Ver.1.1* §3.
pub const FGAMUT_PRIMARIES: Primaries = Primaries {
    red: (0.708, 0.292),
    green: (0.170, 0.797),
    blue: (0.131, 0.046),
};

static FGAMUT_MATRICES: LazyLock<SpaceMatrices> = LazyLock::new(|| {
    let to_xyz = normalised_primary_matrix(FGAMUT_PRIMARIES, Illuminant::D65.xy());
    let from_xyz = to_xyz.inverse().expect("primary matrix is non-singular");
    SpaceMatrices { to_xyz, from_xyz }
});

/// `normalised_primary_matrix` of [`FGAMUT_PRIMARIES`].
pub fn fgamut_to_xyz_matrix() -> Mat3 {
    FGAMUT_MATRICES.to_xyz
}

pub fn fgamut_from_xyz_matrix() -> Mat3 {
    FGAMUT_MATRICES.from_xyz
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn white_rgb_maps_to_the_white_point() {
        for space in RgbSpace::ALL {
            let xyz = space.rgb_linear_to_xyz(Rgb::new(1.0, 1.0, 1.0));
            let white = space.white();
            assert!((xyz.x - white.x).abs() < 1e-12, "{}", space.name());
            assert!((xyz.y - white.y).abs() < 1e-12, "{}", space.name());
            assert!((xyz.z - white.z).abs() < 1e-12, "{}", space.name());
        }
    }

    #[test]
    fn display_p3_shares_the_srgb_transfer_function() {
        for value in [-0.2, 0.0, 0.04045, 0.5, 1.5] {
            assert_eq!(
                RgbSpace::DisplayP3.encode(value),
                RgbSpace::Srgb.encode(value)
            );
            assert_eq!(
                RgbSpace::DisplayP3.decode(value),
                RgbSpace::Srgb.decode(value)
            );
        }
    }

    #[test]
    fn matrices_are_inverse() {
        for space in RgbSpace::ALL {
            let product = space.to_xyz_matrix().mul(&space.from_xyz_matrix());
            for (row, identity_row) in product.0.iter().zip(Mat3::IDENTITY.0.iter()) {
                for (value, identity) in row.iter().zip(identity_row.iter()) {
                    assert!((value - identity).abs() < 1e-12, "{}", space.name());
                }
            }
        }
    }
}
