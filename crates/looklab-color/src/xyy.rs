// SPDX-License-Identifier: AGPL-3.0-or-later
//! CIE xyY and chromaticity conversions, and standard illuminants, ported from
//! colour-science (`colour.XYZ_to_xyY`, `colour.xyY_to_XYZ`, `colour.XYZ_to_xy`,
//! `colour.xy_to_XYZ`; BSD-3-Clause, Colour Developers; see `NOTICE`).

use crate::types::{Xyy, Xyz};
use serde::{Deserialize, Serialize};

/// Division that returns 0 for a zero denominator, matching colour-science's
/// `sdiv` default mode.
pub(crate) fn safe_div(a: f64, b: f64) -> f64 {
    if b == 0.0 { 0.0 } else { a / b }
}

/// `colour.XYZ_to_xyY`.
pub fn xyz_to_xyy(xyz: Xyz) -> Xyy {
    let sum = xyz.x + xyz.y + xyz.z;
    Xyy {
        x: safe_div(xyz.x, sum),
        y: safe_div(xyz.y, sum),
        big_y: xyz.y,
    }
}

/// `colour.xyY_to_XYZ`.
pub fn xyy_to_xyz(xyy: Xyy) -> Xyz {
    let scale = safe_div(xyy.big_y, xyy.y);
    Xyz::new(xyy.x * scale, xyy.big_y, (1.0 - (xyy.x + xyy.y)) * scale)
}

/// `colour.XYZ_to_xy`.
pub fn xyz_to_xy(xyz: Xyz) -> (f64, f64) {
    let xyy = xyz_to_xyy(xyz);
    (xyy.x, xyy.y)
}

/// `colour.xy_to_xyY` with an explicit luminance.
pub const fn xy_to_xyy(xy: (f64, f64), big_y: f64) -> Xyy {
    Xyy {
        x: xy.0,
        y: xy.1,
        big_y,
    }
}

/// `colour.xy_to_XYZ`; the returned tristimulus values have Y = 1.
pub fn xy_to_xyz(xy: (f64, f64)) -> Xyz {
    xyy_to_xyz(xy_to_xyy(xy, 1.0))
}

/// CIE standard illuminants, with the chromaticity coordinates colour-science
/// stores in `CCS_ILLUMINANTS` for the CIE 1931 2 degree standard observer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Illuminant {
    A,
    D50,
    D65,
    E,
}

impl Illuminant {
    /// `CCS_ILLUMINANTS["CIE 1931 2 Degree Standard Observer"]`.
    pub const fn xy(self) -> (f64, f64) {
        match self {
            Self::A => (0.447_58, 0.407_45),
            Self::D50 => (0.345_7, 0.358_5),
            Self::D65 => (0.312_7, 0.329),
            Self::E => (1.0 / 3.0, 1.0 / 3.0),
        }
    }

    /// `colour.xy_to_XYZ`; tristimulus values normalised to Y = 1.
    pub const fn xyz(self) -> Xyz {
        let (x, y) = self.xy();
        let scale = 1.0 / y;
        Xyz::new(x * scale, 1.0, (1.0 - (x + y)) * scale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d65_matches_colour_science() {
        let xyz = Illuminant::D65.xyz();
        assert!((xyz.x - 0.950_455_927_051_671_6).abs() < 1e-15);
        assert_eq!(xyz.y, 1.0);
        assert!((xyz.z - 1.089_057_750_759_878_4).abs() < 1e-15);
    }

    #[test]
    fn e_is_equal_energy() {
        let xyz = Illuminant::E.xyz();
        assert_eq!(xyz, Xyz::new(1.0, 1.0, 1.0));
    }

    #[test]
    fn zero_xyz_has_zero_chromaticity() {
        assert_eq!(
            xyz_to_xyy(Xyz::new(0.0, 0.0, 0.0)),
            Xyy {
                x: 0.0,
                y: 0.0,
                big_y: 0.0
            }
        );
    }

    #[test]
    fn zero_y_gives_zero_xyz() {
        assert_eq!(
            xyy_to_xyz(Xyy {
                x: 0.3,
                y: 0.0,
                big_y: 0.5
            }),
            Xyz::new(0.0, 0.5, 0.0)
        );
    }
}
