// SPDX-License-Identifier: AGPL-3.0-or-later
//! Correlated colour temperature from white-balance multipliers, and the
//! camera colour profile the DNG solve runs on (docs/02 §2).

use looklab_color::Mat3;
use looklab_color::dng::{
    CalibrationMatrix, DngColorSpec, cct_to_xy_robertson, xy_to_cct_robertson,
};
use looklab_color::xyy::Illuminant;
use looklab_raw::ColorMatrices;
use serde::{Deserialize, Serialize};

use crate::error::{BodyError, Result};

/// DNG's standard-light-A temperature (docs/02 §2).
pub const CCT_A: f64 = 2856.0;
/// DNG's D65 temperature (docs/02 §2).
pub const CCT_D65: f64 = 6504.0;

pub const MIN_CCT: f64 = 1_000.0;
pub const MAX_CCT: f64 = 100_000.0;

/// A correlated colour temperature with the chromaticity it was solved from.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cct {
    kelvin: f64,
    xy: (f64, f64),
}

impl Cct {
    pub const A: Self = Self {
        kelvin: CCT_A,
        xy: Illuminant::A.xy(),
    };

    pub const D65: Self = Self {
        kelvin: CCT_D65,
        xy: Illuminant::D65.xy(),
    };

    pub fn from_kelvin(kelvin: f64) -> Result<Self> {
        if !kelvin.is_finite() || !(MIN_CCT..=MAX_CCT).contains(&kelvin) {
            return Err(BodyError::CctOutOfRange {
                kelvin,
                min: MIN_CCT,
                max: MAX_CCT,
            });
        }
        Ok(Self {
            kelvin,
            xy: cct_to_xy_robertson(kelvin),
        })
    }

    pub fn from_xy(x: f64, y: f64) -> Result<Self> {
        if !(x.is_finite() && y.is_finite()) || y <= 0.0 {
            return Err(BodyError::CctOutOfRange {
                kelvin: f64::NAN,
                min: MIN_CCT,
                max: MAX_CCT,
            });
        }
        let kelvin = xy_to_cct_robertson(x, y);
        if !kelvin.is_finite() || !(MIN_CCT..=MAX_CCT).contains(&kelvin) {
            return Err(BodyError::CctOutOfRange {
                kelvin,
                min: MIN_CCT,
                max: MAX_CCT,
            });
        }
        Ok(Self { kelvin, xy: (x, y) })
    }

    pub fn kelvin(&self) -> f64 {
        self.kelvin
    }

    pub fn xy(&self) -> (f64, f64) {
        self.xy
    }
}

/// A calibration illuminant with a tabulated standard white.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CalibrationIlluminant {
    A,
    D50,
    D55,
    D65,
    D75,
    Daylight,
}

impl CalibrationIlluminant {
    pub fn cct(self) -> f64 {
        match self {
            Self::A => CCT_A,
            Self::D50 => 5000.0,
            Self::D55 | Self::Daylight => 5500.0,
            Self::D65 => CCT_D65,
            Self::D75 => 7500.0,
        }
    }

    /// `CCS_ILLUMINANTS["CIE 1931 2 Degree Standard Observer"]`.
    pub fn xy(self) -> (f64, f64) {
        match self {
            Self::A => (0.447_58, 0.407_45),
            Self::D50 => (0.345_7, 0.358_5),
            Self::D55 | Self::Daylight => (0.332_43, 0.347_44),
            Self::D65 => (0.312_7, 0.329),
            Self::D75 => (0.299_03, 0.314_88),
        }
    }

    pub fn from_raw(illuminant: looklab_raw::Illuminant) -> Option<Self> {
        Some(match illuminant {
            looklab_raw::Illuminant::A => Self::A,
            looklab_raw::Illuminant::D50 => Self::D50,
            looklab_raw::Illuminant::D55 => Self::D55,
            looklab_raw::Illuminant::D65 => Self::D65,
            looklab_raw::Illuminant::D75 => Self::D75,
            looklab_raw::Illuminant::Daylight => Self::Daylight,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
struct ProfileEntry {
    illuminant: CalibrationIlluminant,
    matrix: Mat3,
}

/// A camera's DNG colour profile: ColorMatrix entries, and ForwardMatrix
/// entries when the file carries them.
#[derive(Clone, Debug, PartialEq)]
pub struct CameraProfile {
    spec: DngColorSpec,
    color: Vec<ProfileEntry>,
}

impl CameraProfile {
    pub fn new(
        color: Vec<(CalibrationIlluminant, Mat3)>,
        forward: Vec<(CalibrationIlluminant, Mat3)>,
    ) -> Result<Self> {
        if color.is_empty() {
            return Err(BodyError::NoMatrices);
        }
        let mut color: Vec<ProfileEntry> = color
            .into_iter()
            .map(|(illuminant, matrix)| ProfileEntry { illuminant, matrix })
            .collect();
        color.sort_by(|a, b| a.illuminant.cct().total_cmp(&b.illuminant.cct()));
        let mut forward: Vec<ProfileEntry> = forward
            .into_iter()
            .map(|(illuminant, matrix)| ProfileEntry { illuminant, matrix })
            .collect();
        forward.sort_by(|a, b| a.illuminant.cct().total_cmp(&b.illuminant.cct()));
        if !forward.is_empty() && forward.len() != color.len() {
            return Err(BodyError::Dng(looklab_color::dng::DngError::MatrixMismatch));
        }
        let spec = DngColorSpec::new(
            color
                .iter()
                .map(|entry| {
                    CalibrationMatrix::new(entry.illuminant.cct(), entry.matrix)
                        .ok_or(BodyError::NoMatrices)
                })
                .collect::<Result<Vec<_>>>()?,
            forward
                .iter()
                .map(|entry| {
                    CalibrationMatrix::new(entry.illuminant.cct(), entry.matrix)
                        .ok_or(BodyError::NoMatrices)
                })
                .collect::<Result<Vec<_>>>()?,
        )?;
        Ok(Self { spec, color })
    }

    pub fn from_raw(matrices: &ColorMatrices) -> Result<Self> {
        let color = matrices
            .matrices()
            .iter()
            .filter_map(|entry| {
                CalibrationIlluminant::from_raw(entry.illuminant)
                    .map(|illuminant| (illuminant, entry.xyz_to_camera))
            })
            .collect();
        let forward = matrices
            .forward_matrices()
            .iter()
            .filter_map(|entry| {
                CalibrationIlluminant::from_raw(entry.illuminant)
                    .map(|illuminant| (illuminant, entry.camera_to_xyz))
            })
            .collect();
        Self::new(color, forward)
    }

    pub fn spec(&self) -> &DngColorSpec {
        &self.spec
    }

    pub fn color_illuminants(&self) -> impl Iterator<Item = CalibrationIlluminant> + '_ {
        self.color.iter().map(|entry| entry.illuminant)
    }

    /// Camera-to-XYZ(D65) at `target`'s white, adapted from its calibration
    /// illuminant. Without a `target` entry the lowest-temperature entry
    /// serves both endpoints.
    pub fn matrix_at(&self, target: CalibrationIlluminant) -> Result<Mat3> {
        let entry = self
            .color
            .iter()
            .find(|entry| entry.illuminant == target)
            .or_else(|| self.color.first())
            .ok_or(BodyError::NoMatrices)?;
        Ok(self
            .spec
            .camera_to_xyz_d65_at_white(entry.illuminant.xy())?)
    }

    /// The profile's matrices for the two transformer endpoints.
    pub fn endpoints(&self) -> Result<(Mat3, Mat3)> {
        Ok((
            self.matrix_at(CalibrationIlluminant::A)?,
            self.matrix_at(CalibrationIlluminant::D65)?,
        ))
    }
}

/// Camera-neutral tristimulus values from as-shot multipliers.
pub fn neutral_from_wb(multipliers: [f64; 3]) -> Result<[f64; 3]> {
    if !multipliers
        .iter()
        .all(|value| value.is_finite() && *value > 0.0)
    {
        return Err(BodyError::InvalidWbMultipliers);
    }
    Ok([
        1.0 / multipliers[0],
        1.0 / multipliers[1],
        1.0 / multipliers[2],
    ])
}

/// The as-shot neutral's CCT through the DNG `NeutralToXY` iteration.
pub fn estimate_cct(profile: &CameraProfile, multipliers: [f64; 3]) -> Result<Cct> {
    let neutral = neutral_from_wb(multipliers)?;
    let xy = profile.spec.neutral_to_xy(neutral)?;
    Cct::from_xy(xy.0, xy.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kelvin_round_trips_through_the_locus() {
        for kelvin in [2856.0, 4000.0, 5000.0, 6504.0, 9000.0] {
            let cct = Cct::from_kelvin(kelvin).expect("in range");
            assert!((cct.kelvin() - kelvin).abs() < 2.0);
        }
    }

    #[test]
    fn standard_whites_have_their_tabulated_temperatures() {
        let a = Cct::from_xy(
            CalibrationIlluminant::A.xy().0,
            CalibrationIlluminant::A.xy().1,
        )
        .expect("standard A");
        assert!((a.kelvin() - CCT_A).abs() < 5.0);
        let d65 = Cct::from_xy(
            CalibrationIlluminant::D65.xy().0,
            CalibrationIlluminant::D65.xy().1,
        )
        .expect("D65");
        assert!((d65.kelvin() - CCT_D65).abs() < 5.0);
    }

    #[test]
    fn out_of_range_temperatures_are_rejected() {
        assert!(matches!(
            Cct::from_kelvin(500.0),
            Err(BodyError::CctOutOfRange { .. })
        ));
        assert!(matches!(
            Cct::from_xy(0.0, 0.0),
            Err(BodyError::CctOutOfRange { .. })
        ));
    }

    #[test]
    fn white_balance_multipliers_must_be_positive() {
        assert_eq!(
            neutral_from_wb([2.0, 1.0, 1.5]).expect("valid"),
            [0.5, 1.0, 2.0 / 3.0]
        );
        assert!(matches!(
            neutral_from_wb([0.0, 1.0, 1.5]),
            Err(BodyError::InvalidWbMultipliers)
        ));
    }

    #[test]
    fn a_single_illuminant_profile_repeats_its_matrix() {
        let matrix = Mat3([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
        let profile = CameraProfile::new(vec![(CalibrationIlluminant::D65, matrix)], Vec::new())
            .expect("one colour matrix");
        let (a, d65) = profile.endpoints().expect("endpoints");
        assert_eq!(a, d65);
        assert_eq!(profile.color_illuminants().count(), 1);
    }
}
