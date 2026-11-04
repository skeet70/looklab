// SPDX-License-Identifier: AGPL-3.0-or-later
//! DNG camera colour: correlated colour temperature, dual-illuminant matrix
//! interpolation and the camera-to-XYZ(D65) transform.
//!
//! Matrix selection, 1/T interpolation and the iterative neutral solve follow
//! the DNG specification 1.6 chapter 6 ("Mapping Camera Color Space to CIE XYZ
//! Space") and `dng_color_spec::NeutralToXY` in the Adobe DNG SDK. The
//! isotemperature-line search is Robertson (1968) as implemented by
//! colour-science `colour.temperature.uv_to_CCT_Robertson1968` and
//! `CCT_to_uv_Robertson1968` (BSD-3-Clause, Colour Developers; see `NOTICE`).
//! xy/uv conversion is `colour.models.xy_to_UCS_uv` and `UCS_uv_to_xy`.

use std::fmt;

use crate::cat::{Cat, adaptation_matrix};
use crate::linalg::Mat3;
use crate::types::Xyz;
use crate::xyy::{Illuminant, xy_to_xyz, xyz_to_xy};

/// Robertson (1968) isotemperature lines: mired, u, v, slope.
const ISOTEMPERATURE_LINES: [(f64, f64, f64, f64); 31] = [
    (0.0, 0.18006, 0.26352, -0.24341),
    (10.0, 0.18066, 0.26589, -0.25479),
    (20.0, 0.18133, 0.26846, -0.26876),
    (30.0, 0.18208, 0.27119, -0.28539),
    (40.0, 0.18293, 0.27407, -0.3047),
    (50.0, 0.18388, 0.27709, -0.32675),
    (60.0, 0.18494, 0.28021, -0.35156),
    (70.0, 0.18611, 0.28342, -0.37915),
    (80.0, 0.1874, 0.28668, -0.40955),
    (90.0, 0.1888, 0.28997, -0.44278),
    (100.0, 0.19032, 0.29326, -0.47888),
    (125.0, 0.19462, 0.30141, -0.58204),
    (150.0, 0.19962, 0.30921, -0.70471),
    (175.0, 0.20525, 0.31647, -0.84901),
    (200.0, 0.21142, 0.32312, -1.0182),
    (225.0, 0.21807, 0.32909, -1.2168),
    (250.0, 0.22511, 0.33439, -1.4512),
    (275.0, 0.23247, 0.33904, -1.7298),
    (300.0, 0.2401, 0.34308, -2.0637),
    (325.0, 0.24792, 0.34655, -2.4681),
    (350.0, 0.25591, 0.34951, -2.9641),
    (375.0, 0.264, 0.352, -3.5814),
    (400.0, 0.27218, 0.35407, -4.3633),
    (425.0, 0.28039, 0.35577, -5.3762),
    (450.0, 0.28863, 0.35714, -6.7262),
    (475.0, 0.29685, 0.35823, -8.5955),
    (500.0, 0.30505, 0.35907, -11.324),
    (525.0, 0.3132, 0.35968, -15.628),
    (550.0, 0.32129, 0.36011, -23.325),
    (575.0, 0.32931, 0.36038, -40.77),
    (600.0, 0.33724, 0.36051, -116.45),
];

/// CIE 1960 UCS uv from CIE 1931 xy.
pub fn xy_to_uv(x: f64, y: f64) -> (f64, f64) {
    let denominator = -2.0 * x + 12.0 * y + 3.0;
    if denominator == 0.0 {
        return (0.0, 0.0);
    }
    (4.0 * x / denominator, 6.0 * y / denominator)
}

/// CIE 1931 xy from CIE 1960 UCS uv.
pub fn uv_to_xy(u: f64, v: f64) -> (f64, f64) {
    let denominator = 2.0 * u - 8.0 * v + 4.0;
    if denominator == 0.0 {
        return (0.0, 0.0);
    }
    (3.0 * u / denominator, 2.0 * v / denominator)
}

/// Robertson (1968) isotemperature-line search; matches
/// `colour.temperature.uv_to_CCT_Robertson1968` for the temperature.
pub fn xy_to_cct_robertson(x: f64, y: f64) -> f64 {
    let (u, v) = xy_to_uv(x, y);
    let mut last_dt = 0.0;
    for index in 1..ISOTEMPERATURE_LINES.len() {
        let (_, line_u, line_v, slope) = ISOTEMPERATURE_LINES[index];
        let length = (1.0 + slope * slope).sqrt();
        let du = 1.0 / length;
        let dv = slope / length;
        let uu = u - line_u;
        let vv = v - line_v;
        let dt = -uu * dv + vv * du;
        if dt <= 0.0 || index == ISOTEMPERATURE_LINES.len() - 1 {
            let dt = if dt > 0.0 { 0.0 } else { -dt };
            let fraction = if index == 1 { 0.0 } else { dt / (last_dt + dt) };
            let (r0, _, _, _) = ISOTEMPERATURE_LINES[index - 1];
            let (r1, _, _, _) = ISOTEMPERATURE_LINES[index];
            let mired = r0 * fraction + r1 * (1.0 - fraction);
            return if mired > 0.0 {
                1.0e6 / mired
            } else {
                f64::INFINITY
            };
        }
        last_dt = dt;
    }
    f64::INFINITY
}

/// xy on the Planckian locus at `cct`, with zero tint, Robertson (1968).
pub fn cct_to_xy_robertson(cct: f64) -> (f64, f64) {
    let cct = cct.clamp(1_000.0, 100_000.0);
    let mired = 1.0e6 / cct;
    let mut index = ISOTEMPERATURE_LINES.len() - 2;
    for candidate in 0..ISOTEMPERATURE_LINES.len() - 1 {
        if mired < ISOTEMPERATURE_LINES[candidate + 1].0 {
            index = candidate;
            break;
        }
    }
    let (r0, u0, v0, _) = ISOTEMPERATURE_LINES[index];
    let (r1, u1, v1, _) = ISOTEMPERATURE_LINES[index + 1];
    let fraction = if r1 != r0 {
        (r1 - mired) / (r1 - r0)
    } else {
        0.0
    };
    uv_to_xy(
        u0 * fraction + u1 * (1.0 - fraction),
        v0 * fraction + v1 * (1.0 - fraction),
    )
}

/// DNG calibration illuminant temperature, from
/// `dng_camera_profile::IlluminantToTemperature`.
pub fn calibration_temperature(code: u16) -> Option<f64> {
    Some(match code {
        3 | 17 => 2850.0,
        24 => 3200.0,
        23 => 5000.0,
        1 | 4 | 9 | 18 | 20 => 5500.0,
        10 | 19 | 21 => 6500.0,
        11 | 22 => 7500.0,
        12 => (5700.0 + 7100.0) * 0.5,
        13 => (4600.0 + 5500.0) * 0.5,
        2 | 14 => (3800.0 + 4500.0) * 0.5,
        15 => (3250.0 + 3800.0) * 0.5,
        _ => return None,
    })
}

/// One ColorMatrix or ForwardMatrix with the calibration illuminant's
/// temperature. `xyz_to_camera` is DNG's ColorMatrix layout; forward matrices
/// use the same `Mat3` layout in the camera-to-XYZ direction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CalibrationMatrix {
    pub cct: f64,
    pub matrix: Mat3,
}

impl CalibrationMatrix {
    pub fn new(cct: f64, matrix: Mat3) -> Option<Self> {
        if cct > 0.0 && cct.is_finite() {
            Some(Self { cct, matrix })
        } else {
            None
        }
    }
}

/// Camera-to-XYZ(D65) at an estimated scene CCT.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraToXyzD65 {
    pub cct: f64,
    pub xy: (f64, f64),
    pub matrix: Mat3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DngError {
    NoMatrices,
    TooManyMatrices,
    MatrixMismatch,
    InvalidTemperature,
    SingularMatrix,
}

impl fmt::Display for DngError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NoMatrices => "no color matrices",
            Self::TooManyMatrices => "more than two color matrices",
            Self::MatrixMismatch => "forward and color matrix counts differ",
            Self::InvalidTemperature => "invalid calibration temperature",
            Self::SingularMatrix => "singular color matrix",
        })
    }
}

impl std::error::Error for DngError {}

/// A DNG colour profile: ColorMatrix1/2 and optional ForwardMatrix1/2.
#[derive(Clone, Debug, PartialEq)]
pub struct DngColorSpec {
    color_matrices: Vec<CalibrationMatrix>,
    forward_matrices: Vec<CalibrationMatrix>,
}

impl DngColorSpec {
    pub fn new(
        mut color_matrices: Vec<CalibrationMatrix>,
        mut forward_matrices: Vec<CalibrationMatrix>,
    ) -> Result<Self, DngError> {
        if color_matrices.is_empty() {
            return Err(DngError::NoMatrices);
        }
        if color_matrices.len() > 2 || forward_matrices.len() > 2 {
            return Err(DngError::TooManyMatrices);
        }
        if !forward_matrices.is_empty() && forward_matrices.len() != color_matrices.len() {
            return Err(DngError::MatrixMismatch);
        }
        color_matrices.sort_by(|a, b| a.cct.total_cmp(&b.cct));
        forward_matrices.sort_by(|a, b| a.cct.total_cmp(&b.cct));
        if color_matrices
            .iter()
            .any(|matrix| !matrix.cct.is_finite() || matrix.cct <= 0.0)
        {
            return Err(DngError::InvalidTemperature);
        }
        Ok(Self {
            color_matrices,
            forward_matrices,
        })
    }

    pub fn color_matrices(&self) -> &[CalibrationMatrix] {
        &self.color_matrices
    }

    pub fn forward_matrices(&self) -> &[CalibrationMatrix] {
        &self.forward_matrices
    }

    /// Weight of the first (cooler) calibration, 1 at its temperature and 0 at
    /// the second's.
    pub fn interpolation_fraction(&self, cct: f64) -> f64 {
        if self.color_matrices.len() == 1 {
            return 1.0;
        }
        match self.color_matrices.as_slice() {
            [first, second] => {
                if cct <= first.cct {
                    1.0
                } else if cct >= second.cct {
                    0.0
                } else {
                    let inverse = 1.0 / cct;
                    (inverse - 1.0 / second.cct) / (1.0 / first.cct - 1.0 / second.cct)
                }
            }
            _ => 1.0,
        }
    }

    fn matrix_at(matrices: &[CalibrationMatrix], fraction: f64) -> Option<Mat3> {
        match matrices {
            [] => None,
            [single] => Some(single.matrix),
            [first, second] => {
                if fraction >= 1.0 {
                    Some(first.matrix)
                } else if fraction <= 0.0 {
                    Some(second.matrix)
                } else {
                    Some(add_scaled(
                        first.matrix,
                        second.matrix,
                        fraction,
                        1.0 - fraction,
                    ))
                }
            }
            _ => None,
        }
    }

    /// Interpolated XYZ(calibration illuminant) to camera matrix.
    pub fn xyz_to_camera(&self, cct: f64) -> Option<Mat3> {
        Self::matrix_at(&self.color_matrices, self.interpolation_fraction(cct))
    }

    /// Interpolated camera to XYZ(D50) forward matrix, when the profile has one.
    pub fn forward_matrix(&self, cct: f64) -> Option<Mat3> {
        Self::matrix_at(&self.forward_matrices, self.interpolation_fraction(cct))
    }

    /// Camera RGB to XYZ adapted to D65 at `cct`.
    ///
    /// A single-matrix profile uses its own calibration temperature, because it
    /// carries no second illuminant to interpolate towards.
    pub fn camera_to_xyz_d65(&self, cct: f64) -> Result<Mat3, DngError> {
        let effective_cct = if self.color_matrices.len() == 1 {
            self.color_matrices[0].cct
        } else {
            cct
        };
        if !effective_cct.is_finite() || effective_cct <= 0.0 {
            return Err(DngError::InvalidTemperature);
        }
        let color_matrix = self
            .xyz_to_camera(effective_cct)
            .ok_or(DngError::NoMatrices)?;
        let source_white = xy_to_xyz(cct_to_xy_robertson(effective_cct));
        if let Some(forward) = self.forward_matrix(effective_cct) {
            let reference = color_matrix.mul_vec(source_white.to_array());
            let mut scale = [1.0_f64; 3];
            for index in 0..3 {
                if reference[index].abs() > 1e-12 {
                    scale[index] = 1.0 / reference[index];
                }
            }
            let pcs = forward.mul(&Mat3::diag(scale));
            let adaptation =
                adaptation_matrix(Cat::Bradford, Illuminant::D50.xyz(), Illuminant::D65.xyz());
            return Ok(adaptation.mul(&pcs));
        }
        let camera_to_xyz = color_matrix.inverse().ok_or(DngError::SingularMatrix)?;
        let adaptation = adaptation_matrix(Cat::Bradford, source_white, Illuminant::D65.xyz());
        Ok(adaptation.mul(&camera_to_xyz))
    }

    /// Scene xy from an as-shot neutral in camera RGB, by the DNG SDK's
    /// fixed-point iteration (`dng_color_spec::NeutralToXY`).
    pub fn neutral_to_xy(&self, neutral: [f64; 3]) -> Result<(f64, f64), DngError> {
        if !neutral
            .iter()
            .all(|value| value.is_finite() && *value > 0.0)
        {
            return Err(DngError::InvalidTemperature);
        }
        let mut last = Illuminant::D50.xy();
        for pass in 0..30 {
            let cct = xy_to_cct_robertson(last.0, last.1);
            let xyz_to_camera = self.xyz_to_camera(cct).ok_or(DngError::NoMatrices)?;
            let camera_to_xyz = xyz_to_camera.inverse().ok_or(DngError::SingularMatrix)?;
            let xyz = Xyz::from_array(camera_to_xyz.mul_vec(neutral));
            let next = xyz_to_xy(xyz);
            if (next.0 - last.0).abs() + (next.1 - last.1).abs() < 1.0e-7 {
                return Ok(next);
            }
            if pass == 29 {
                return Ok(((last.0 + next.0) * 0.5, (last.1 + next.1) * 0.5));
            }
            last = next;
        }
        Ok(last)
    }

    /// Neutral solve followed by the camera-to-XYZ(D65) transform at the
    /// solved CCT.
    pub fn from_neutral(&self, neutral: [f64; 3]) -> Result<CameraToXyzD65, DngError> {
        let xy = self.neutral_to_xy(neutral)?;
        let cct = xy_to_cct_robertson(xy.0, xy.1);
        let matrix = self.camera_to_xyz_d65(cct)?;
        Ok(CameraToXyzD65 { cct, xy, matrix })
    }
}

fn add_scaled(a: Mat3, b: Mat3, wa: f64, wb: f64) -> Mat3 {
    let mut out = [[0.0_f64; 3]; 3];
    for (row, out_row) in out.iter_mut().enumerate() {
        for (column, value) in out_row.iter_mut().enumerate() {
            *value = wa * a.0[row][column] + wb * b.0[row][column];
        }
    }
    Mat3(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn x_s10() -> DngColorSpec {
        DngColorSpec::new(
            vec![
                CalibrationMatrix::new(
                    2850.0,
                    Mat3([
                        [1.6064, -0.9942, 0.112],
                        [-0.376, 1.2096, 0.1853],
                        [-0.0007, 0.0545, 0.6654],
                    ]),
                )
                .expect("temperature"),
                CalibrationMatrix::new(
                    6500.0,
                    Mat3([
                        [1.3426, -0.6334, -0.1177],
                        [-0.4244, 1.2136, 0.2371],
                        [-0.058, 0.1303, 0.598],
                    ]),
                )
                .expect("temperature"),
            ],
            Vec::new(),
        )
        .expect("profile")
    }

    #[test]
    fn interpolates_in_inverse_temperature() {
        let spec = x_s10();
        assert_eq!(spec.interpolation_fraction(2000.0), 1.0);
        assert_eq!(spec.interpolation_fraction(10_000.0), 0.0);
        let midpoint = spec.interpolation_fraction(3_963.0);
        assert!((midpoint - 0.5).abs() < 0.03);
    }

    #[test]
    fn neutral_solve_round_trips_a_known_illuminant() {
        let spec = x_s10();
        let xy = cct_to_xy_robertson(4_500.0);
        let cct = xy_to_cct_robertson(xy.0, xy.1);
        let matrix = spec.xyz_to_camera(cct).expect("matrix");
        let neutral = matrix.mul_vec(xy_to_xyz(xy).to_array());
        let solved = spec.neutral_to_xy(neutral).expect("neutral");
        assert!((solved.0 - xy.0).abs() < 2.0e-4, "{solved:?} vs {xy:?}");
        assert!((solved.1 - xy.1).abs() < 2.0e-4, "{solved:?} vs {xy:?}");
    }

    #[test]
    fn camera_to_xyz_is_invertible_and_d65_relative() {
        let spec = x_s10();
        let matrix = spec.camera_to_xyz_d65(6_500.0).expect("matrix");
        let inverse = matrix.inverse().expect("inverse");
        let product = matrix.mul(&inverse);
        for row in 0..3 {
            for column in 0..3 {
                let expected = if row == column { 1.0 } else { 0.0 };
                assert!((product.0[row][column] - expected).abs() < 1.0e-12);
            }
        }
        let source = xy_to_xyz(cct_to_xy_robertson(6_500.0));
        let color = spec.xyz_to_camera(6_500.0).expect("color matrix");
        let white = matrix.mul_vec(color.mul_vec(source.to_array()));
        let xy = xyz_to_xy(Xyz::from_array(white));
        assert!(
            (xy.0 - Illuminant::D65.xy().0).abs() < 1.0e-3,
            "{xy:?} from {source:?}"
        );
        assert!((xy.1 - Illuminant::D65.xy().1).abs() < 1.0e-3);
    }

    #[test]
    fn single_matrix_falls_back_to_its_calibration_white() {
        let matrix = Mat3([[1.4, -0.6, -0.1], [-0.4, 1.2, 0.2], [-0.05, 0.13, 0.6]]);
        let spec = DngColorSpec::new(
            vec![CalibrationMatrix::new(6_500.0, matrix).expect("temperature")],
            Vec::new(),
        )
        .expect("profile");
        assert!(spec.camera_to_xyz_d65(3_000.0).is_ok());
    }
}
