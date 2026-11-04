// SPDX-License-Identifier: AGPL-3.0-or-later
//! The white-preserving six-parameter transform `I + Δ` (docs/02 §2).

use looklab_color::Mat3;
use serde::{Deserialize, Serialize};

use crate::error::{BodyError, Result};

pub const PARAMETERS: usize = 6;

/// Larger deviations stop being a matrix near the identity.
pub const MAX_DELTA: f64 = 0.5;

/// Six free entries of `I + Δ`, ordered `(d00, d01, d10, d11, d20, d21)`.
///
/// Each row's third entry is the negated sum of its first two, so `Δ·1 = 0`
/// and applying the transform to a neutral is the identity.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Delta6([f64; PARAMETERS]);

impl Delta6 {
    pub const ZERO: Self = Self([0.0; PARAMETERS]);

    pub fn new(params: [f64; PARAMETERS]) -> Result<Self> {
        for (index, value) in params.iter().enumerate() {
            if !value.is_finite() {
                return Err(BodyError::NonFinite {
                    what: "delta parameter",
                });
            }
            if value.abs() > MAX_DELTA {
                return Err(BodyError::DeltaOutOfRange {
                    index,
                    value: *value,
                    max: MAX_DELTA,
                });
            }
        }
        Ok(Self(params))
    }

    pub fn params(&self) -> [f64; PARAMETERS] {
        self.0
    }

    /// `Δ` itself.
    pub fn matrix(&self) -> Mat3 {
        let [p0, p1, p2, p3, p4, p5] = self.0;
        Mat3([
            [p0, p1, -(p0 + p1)],
            [p2, p3, -(p2 + p3)],
            [p4, p5, -(p4 + p5)],
        ])
    }

    /// `I + Δ`, for consumers that multiply matrices.
    pub fn transform(&self) -> Mat3 {
        let m = self.matrix();
        Mat3([
            [1.0 + m.0[0][0], m.0[0][1], m.0[0][2]],
            [m.0[1][0], 1.0 + m.0[1][1], m.0[1][2]],
            [m.0[2][0], m.0[2][1], 1.0 + m.0[2][2]],
        ])
    }

    /// `(I + Δ)·rgb`, evaluated as `rgb + Δ·rgb`.
    pub fn apply(&self, rgb: [f64; 3]) -> [f64; 3] {
        let delta = self.matrix().mul_vec(rgb);
        [rgb[0] + delta[0], rgb[1] + delta[1], rgb[2] + delta[2]]
    }

    /// The first two columns of `m - I`; the third is determined by the rows.
    pub fn from_matrix(m: &Mat3) -> Result<Self> {
        Self::new([
            m.0[0][0] - 1.0,
            m.0[0][1],
            m.0[1][0],
            m.0[1][1] - 1.0,
            m.0[2][0],
            m.0[2][1],
        ])
    }

    /// `d(I + Δ)/d(params[index])`.
    pub fn basis(index: usize) -> Mat3 {
        let mut m = [[0.0_f64; 3]; 3];
        m[index / 2][index % 2] = 1.0;
        m[index / 2][2] = -1.0;
        Mat3(m)
    }
}

/// Symmetric 6×6 covariance in [`Delta6`] coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cov6([[f64; PARAMETERS]; PARAMETERS]);

impl Cov6 {
    pub const ZERO: Self = Self([[0.0_f64; PARAMETERS]; PARAMETERS]);

    pub fn new(covariance: [[f64; PARAMETERS]; PARAMETERS]) -> Result<Self> {
        for row in covariance.iter() {
            for value in row.iter() {
                if !value.is_finite() {
                    return Err(BodyError::NonFinite {
                        what: "delta covariance",
                    });
                }
            }
        }
        Ok(Self(covariance))
    }

    pub fn diagonal(variances: [f64; PARAMETERS]) -> Self {
        let mut covariance = [[0.0_f64; PARAMETERS]; PARAMETERS];
        for (index, variance) in variances.iter().enumerate() {
            covariance[index][index] = *variance;
        }
        Self(covariance)
    }

    pub fn isotropic(standard_deviation: f64) -> Self {
        Self::diagonal([standard_deviation * standard_deviation; PARAMETERS])
    }

    pub fn as_array(&self) -> &[[f64; PARAMETERS]; PARAMETERS] {
        &self.0
    }

    pub fn standard_deviations(&self) -> [f64; PARAMETERS] {
        let mut out = [0.0_f64; PARAMETERS];
        for (index, row) in self.0.iter().enumerate() {
            out[index] = row[index].max(0.0).sqrt();
        }
        out
    }

    pub fn precision(&self) -> Result<[[f64; PARAMETERS]; PARAMETERS]> {
        crate::linalg::invert(&self.0).ok_or(BodyError::Singular {
            what: "delta covariance",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLES: [[f64; PARAMETERS]; 5] = [
        [0.0; PARAMETERS],
        [0.001, -0.002, 0.003, -0.004, 0.005, -0.006],
        [0.5, -0.5, 0.5, -0.5, 0.5, -0.5],
        [-0.25, 0.25, 0.1, 0.2, -0.1, -0.2],
        [1e-9, 1e-9, -1e-9, -1e-9, 1e-9, -1e-9],
    ];

    #[test]
    fn delta_leaves_the_neutral_axis_fixed() {
        for params in SAMPLES {
            let delta = Delta6::new(params).expect("parameters are in range");
            assert_eq!(delta.matrix().mul_vec([1.0; 3]), [0.0; 3]);
            assert_eq!(delta.apply([1.0; 3]), [1.0; 3]);
        }
    }

    #[test]
    fn transform_round_trips_through_the_matrix() {
        for params in SAMPLES {
            let delta = Delta6::new(params).expect("parameters are in range");
            let recovered = Delta6::from_matrix(&delta.transform()).expect("near the identity");
            for (recovered, expected) in recovered.params().iter().zip(params) {
                assert!((recovered - expected).abs() < 1e-12);
            }
        }
    }

    #[test]
    fn basis_matrices_are_the_derivatives() {
        let delta = Delta6::new([0.02, -0.03, 0.04, -0.05, 0.06, -0.07]).expect("in range");
        let epsilon = 1e-7;
        for index in 0..PARAMETERS {
            let mut plus = delta.params();
            plus[index] += epsilon;
            let mut minus = delta.params();
            minus[index] -= epsilon;
            let increasing = Delta6::new(plus).expect("in range").transform();
            let decreasing = Delta6::new(minus).expect("in range").transform();
            let basis = Delta6::basis(index);
            for row in 0..3 {
                for column in 0..3 {
                    let finite_difference =
                        (increasing.0[row][column] - decreasing.0[row][column]) / (2.0 * epsilon);
                    assert!((finite_difference - basis.0[row][column]).abs() < 1e-9);
                }
            }
        }
    }

    #[test]
    fn out_of_range_parameters_are_rejected() {
        assert!(matches!(
            Delta6::new([0.75, 0.0, 0.0, 0.0, 0.0, 0.0]),
            Err(BodyError::DeltaOutOfRange { index: 0, .. })
        ));
        assert!(matches!(
            Delta6::new([f64::NAN, 0.0, 0.0, 0.0, 0.0, 0.0]),
            Err(BodyError::NonFinite { .. })
        ));
    }

    #[test]
    fn covariance_round_trips_through_its_precision() {
        let covariance = Cov6::diagonal([0.01, 0.02, 0.03, 0.04, 0.05, 0.06]);
        let precision = covariance.precision().expect("positive diagonal");
        let sigma = covariance.standard_deviations();
        assert!((sigma[0] - 0.1).abs() < 1e-15);
        assert!((precision[0][0] - 100.0).abs() < 1e-12);
    }
}
