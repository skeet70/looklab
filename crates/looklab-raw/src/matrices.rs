// SPDX-License-Identifier: AGPL-3.0-or-later
//! Camera colour matrices from rawler and from the file's DNG tags.

use looklab_color::Mat3;
use std::collections::HashMap;

/// DNG calibration illuminant codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Illuminant {
    Unknown,
    Daylight,
    Fluorescent,
    Tungsten,
    Flash,
    FineWeather,
    CloudyWeather,
    Shade,
    DaylightFluorescent,
    DaylightWhiteFluorescent,
    CoolWhiteFluorescent,
    WhiteFluorescent,
    A,
    B,
    C,
    D55,
    D65,
    D75,
    D50,
    IsoStudioTungsten,
}

impl Illuminant {
    pub const fn code(self) -> u16 {
        match self {
            Self::Unknown => 0,
            Self::Daylight => 1,
            Self::Fluorescent => 2,
            Self::Tungsten => 3,
            Self::Flash => 4,
            Self::FineWeather => 9,
            Self::CloudyWeather => 10,
            Self::Shade => 11,
            Self::DaylightFluorescent => 12,
            Self::DaylightWhiteFluorescent => 13,
            Self::CoolWhiteFluorescent => 14,
            Self::WhiteFluorescent => 15,
            Self::A => 17,
            Self::B => 18,
            Self::C => 19,
            Self::D55 => 20,
            Self::D65 => 21,
            Self::D75 => 22,
            Self::D50 => 23,
            Self::IsoStudioTungsten => 24,
        }
    }

    pub const fn from_code(code: u16) -> Option<Self> {
        Some(match code {
            0 => Self::Unknown,
            1 => Self::Daylight,
            2 => Self::Fluorescent,
            3 => Self::Tungsten,
            4 => Self::Flash,
            9 => Self::FineWeather,
            10 => Self::CloudyWeather,
            11 => Self::Shade,
            12 => Self::DaylightFluorescent,
            13 => Self::DaylightWhiteFluorescent,
            14 => Self::CoolWhiteFluorescent,
            15 => Self::WhiteFluorescent,
            17 => Self::A,
            18 => Self::B,
            19 => Self::C,
            20 => Self::D55,
            21 => Self::D65,
            22 => Self::D75,
            23 => Self::D50,
            24 => Self::IsoStudioTungsten,
            _ => return None,
        })
    }
}

/// XYZ(D65-relative) to camera RGB, with the illuminant it was calibrated under.
#[derive(Debug, Clone, PartialEq)]
pub struct CameraMatrix {
    pub illuminant: Illuminant,
    pub xyz_to_camera: Mat3,
}

/// Camera RGB to XYZ under D50, the DNG `ForwardMatrix` direction.
#[derive(Debug, Clone, PartialEq)]
pub struct ForwardMatrix {
    pub illuminant: Illuminant,
    pub camera_to_xyz: Mat3,
}

/// Camera matrices found in the file, ordered by illuminant code.
#[derive(Debug, Clone, PartialEq)]
pub struct ColorMatrices {
    matrices: Vec<CameraMatrix>,
    forward_matrices: Vec<ForwardMatrix>,
}

impl ColorMatrices {
    pub fn matrices(&self) -> &[CameraMatrix] {
        &self.matrices
    }

    pub fn for_illuminant(&self, illuminant: Illuminant) -> Option<&CameraMatrix> {
        self.matrices.iter().find(|m| m.illuminant == illuminant)
    }

    pub fn forward_matrices(&self) -> &[ForwardMatrix] {
        &self.forward_matrices
    }

    pub fn forward_for_illuminant(&self, illuminant: Illuminant) -> Option<&ForwardMatrix> {
        self.forward_matrices
            .iter()
            .find(|m| m.illuminant == illuminant)
    }

    pub(crate) fn new(
        source: &HashMap<rawler::imgop::xyz::Illuminant, Vec<f32>>,
        mut forward_matrices: Vec<ForwardMatrix>,
    ) -> Option<Self> {
        let mut matrices: Vec<CameraMatrix> = source
            .iter()
            .filter_map(|(illuminant, flat)| {
                Some(CameraMatrix {
                    illuminant: Illuminant::from_code(u16::from(*illuminant))?,
                    xyz_to_camera: mat3(flat)?,
                })
            })
            .collect();
        matrices.sort_by_key(|m| m.illuminant);
        forward_matrices.sort_by_key(|m| m.illuminant);
        if matrices.is_empty() && forward_matrices.is_empty() {
            return None;
        }
        Some(Self {
            matrices,
            forward_matrices,
        })
    }
}

/// rawler stores three or four component rows; the extra row is dropped.
fn mat3(flat: &[f32]) -> Option<Mat3> {
    let cell = |index: usize| flat.get(index).map(|v| f64::from(*v));
    Some(Mat3([
        [cell(0)?, cell(1)?, cell(2)?],
        [cell(3)?, cell(4)?, cell(5)?],
        [cell(6)?, cell(7)?, cell(8)?],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn illuminant_codes_round_trip() {
        for illuminant in [
            Illuminant::Daylight,
            Illuminant::Tungsten,
            Illuminant::D55,
            Illuminant::D65,
            Illuminant::IsoStudioTungsten,
        ] {
            assert_eq!(Illuminant::from_code(illuminant.code()), Some(illuminant));
        }
        assert_eq!(Illuminant::from_code(5), None);
    }
}
