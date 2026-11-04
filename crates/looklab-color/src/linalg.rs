// SPDX-License-Identifier: AGPL-3.0-or-later
//! Fixed-order 3×3 matrix arithmetic. Every reduction sums in index order, so results are reproducible across targets.

use serde::{Deserialize, Serialize};

/// Row-major 3×3 matrix.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mat3(pub [[f64; 3]; 3]);

impl Mat3 {
    pub const IDENTITY: Self = Self([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);

    pub const fn diag(d: [f64; 3]) -> Self {
        Self([[d[0], 0.0, 0.0], [0.0, d[1], 0.0], [0.0, 0.0, d[2]]])
    }

    pub fn mul_vec(&self, v: [f64; 3]) -> [f64; 3] {
        let m = &self.0;
        [
            m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
            m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
            m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
        ]
    }

    pub fn mul(&self, other: &Self) -> Self {
        let (a, b) = (&self.0, &other.0);
        let cell = |i: usize, j: usize| a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j];
        Self([
            [cell(0, 0), cell(0, 1), cell(0, 2)],
            [cell(1, 0), cell(1, 1), cell(1, 2)],
            [cell(2, 0), cell(2, 1), cell(2, 2)],
        ])
    }

    pub fn transpose(&self) -> Self {
        let m = &self.0;
        Self([
            [m[0][0], m[1][0], m[2][0]],
            [m[0][1], m[1][1], m[2][1]],
            [m[0][2], m[1][2], m[2][2]],
        ])
    }

    pub fn det(&self) -> f64 {
        let m = &self.0;
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    }

    /// Inverse by adjugate; `None` when the matrix is singular.
    pub fn inverse(&self) -> Option<Self> {
        let d = self.det();
        if d == 0.0 || !d.is_finite() {
            return None;
        }
        let m = &self.0;
        let inv = 1.0 / d;
        Some(Self([
            [
                (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * inv,
                (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * inv,
                (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * inv,
            ],
            [
                (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * inv,
                (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * inv,
                (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * inv,
            ],
            [
                (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * inv,
                (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * inv,
                (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * inv,
            ],
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_times_matrix_is_identity() {
        let m = Mat3([[0.4124, 0.3576, 0.1805], [0.2126, 0.7152, 0.0722], [0.0193, 0.1192, 0.9505]]);
        let p = m.mul(&m.inverse().unwrap());
        for (i, row) in p.0.iter().enumerate() {
            for (j, v) in row.iter().enumerate() {
                let want = if i == j { 1.0 } else { 0.0 };
                assert!((v - want).abs() < 1e-12);
            }
        }
    }

    #[test]
    fn singular_matrix_has_no_inverse() {
        let m = Mat3([[1.0, 2.0, 3.0], [2.0, 4.0, 6.0], [0.0, 1.0, 0.0]]);
        assert!(m.inverse().is_none());
    }
}
