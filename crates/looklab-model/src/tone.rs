// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::basis::{TONE_COEFS, TONE_KNOTS, ToneBasis};
use crate::error::{ModelError, Result};
use crate::solve::neutral_j_prime;

const FIT_SAMPLES: usize = 401;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tone {
    Colorimetric,
    Spline { c0: f64, beta: [f64; TONE_COEFS] },
}

impl Tone {
    pub fn spline(c0: f64, beta: [f64; TONE_COEFS]) -> Result<Self> {
        if beta.iter().any(|value| !value.is_finite() || *value < 0.0) || !c0.is_finite() {
            return Err(ModelError::NegativeToneBeta);
        }
        Ok(Self::Spline { c0, beta })
    }

    /// Least squares on a dense grid of neutral `J'(s)` with non-negative slopes.
    pub fn colorimetric(knots: &[f64]) -> Result<Self> {
        if knots != TONE_KNOTS {
            return Err(ModelError::Knots);
        }
        let basis = ToneBasis::new();
        let mut s_values = [0.0_f64; FIT_SAMPLES];
        let mut target = [0.0_f64; FIT_SAMPLES];
        for i in 0..FIT_SAMPLES {
            let s = -12.0 + 18.0 * i as f64 / (FIT_SAMPLES - 1) as f64;
            s_values[i] = s;
            target[i] = neutral_j_prime(0.18 * libm::pow(2.0, s));
        }
        let c0 = target[0];
        let mut matrix = [[0.0_f64; TONE_COEFS]; FIT_SAMPLES];
        let mut residual = [0.0_f64; FIT_SAMPLES];
        for i in 0..FIT_SAMPLES {
            matrix[i].copy_from_slice(&basis.values(s_values[i]));
            residual[i] = target[i] - c0;
        }
        let beta = non_negative_least_squares(&matrix, &residual);
        Self::spline(c0, beta)
    }

    /// `s` is physical exposure; the spline is evaluated at [`exposure_lookup`](crate::basis::exposure_lookup).
    pub fn eval(&self, s: f64) -> f64 {
        match self {
            Self::Colorimetric => neutral_j_prime(0.18 * libm::pow(2.0, s.clamp(-12.0, 6.0))),
            Self::Spline { c0, beta } => {
                let values = ToneBasis::new().values(crate::basis::exposure_lookup(s));
                let mut t = *c0;
                for k in 0..TONE_COEFS {
                    t += beta[k] * values[k];
                }
                t
            }
        }
    }

    pub fn is_colorimetric(&self) -> bool {
        matches!(self, Self::Colorimetric)
    }

    pub fn betas(&self) -> Option<&[f64; TONE_COEFS]> {
        match self {
            Self::Spline { beta, .. } => Some(beta),
            Self::Colorimetric => None,
        }
    }
}

fn non_negative_least_squares(
    matrix: &[[f64; TONE_COEFS]; FIT_SAMPLES],
    target: &[f64; FIT_SAMPLES],
) -> [f64; TONE_COEFS] {
    let mut x = [0.0_f64; TONE_COEFS];
    let mut passive: Vec<usize> = Vec::new();
    for _ in 0..4 * TONE_COEFS {
        let gradient = normal_gradient(matrix, target, &x);
        let mut candidate = None;
        let mut best = 1e-12;
        for (j, value) in gradient.iter().enumerate() {
            if !passive.contains(&j) && *value > best {
                best = *value;
                candidate = Some(j);
            }
        }
        let Some(j) = candidate else {
            break;
        };
        passive.push(j);
        for _ in 0..4 * TONE_COEFS {
            let z = solve_passive(matrix, target, &passive);
            if z.iter().all(|value| *value > 0.0) {
                for (position, index) in passive.iter().enumerate() {
                    x[*index] = z[position];
                }
                break;
            }
            let mut alpha = f64::INFINITY;
            for (position, index) in passive.iter().enumerate() {
                if z[position] <= 0.0 {
                    let denominator = x[*index] - z[position];
                    if denominator > 0.0 {
                        alpha = alpha.min(x[*index] / denominator);
                    }
                }
            }
            if !alpha.is_finite() {
                alpha = 0.0;
            }
            for (position, index) in passive.iter().enumerate() {
                x[*index] += alpha * (z[position] - x[*index]);
            }
            passive.retain(|index| x[*index] > 1e-14);
            if passive.is_empty() {
                break;
            }
        }
    }
    for value in x.iter_mut() {
        if *value < 0.0 {
            *value = 0.0;
        }
    }
    x
}

fn normal_gradient(
    matrix: &[[f64; TONE_COEFS]; FIT_SAMPLES],
    target: &[f64; FIT_SAMPLES],
    x: &[f64; TONE_COEFS],
) -> [f64; TONE_COEFS] {
    let mut gradient = [0.0_f64; TONE_COEFS];
    for (row, value) in matrix.iter().zip(target.iter()) {
        let mut fitted = 0.0;
        for (a, b) in row.iter().zip(x.iter()) {
            fitted += a * b;
        }
        let residual = value - fitted;
        for (slot, a) in gradient.iter_mut().zip(row.iter()) {
            *slot += a * residual;
        }
    }
    gradient
}

fn solve_passive(
    matrix: &[[f64; TONE_COEFS]; FIT_SAMPLES],
    target: &[f64; FIT_SAMPLES],
    passive: &[usize],
) -> Vec<f64> {
    let size = passive.len();
    let mut augmented = vec![vec![0.0_f64; size + 1]; size];
    for (row, value) in matrix.iter().zip(target.iter()) {
        for (a, index_a) in passive.iter().enumerate() {
            let coefficient = row[*index_a];
            augmented[a][size] += coefficient * value;
            for (b, index_b) in passive.iter().enumerate() {
                augmented[a][b] += coefficient * row[*index_b];
            }
        }
    }
    for (index, row) in augmented.iter_mut().enumerate() {
        row[index] += 1e-12;
    }
    for column in 0..size {
        let mut pivot = column;
        for row in column + 1..size {
            if augmented[row][column].abs() > augmented[pivot][column].abs() {
                pivot = row;
            }
        }
        augmented.swap(column, pivot);
        let diagonal = augmented[column][column];
        if diagonal.abs() < 1e-18 {
            continue;
        }
        for row in column + 1..size {
            let factor = augmented[row][column] / diagonal;
            let (pivot_rows, later_rows) = augmented.split_at_mut(row);
            let pivot = &pivot_rows[column];
            let target = &mut later_rows[0];
            for (entry, value) in target[column..=size]
                .iter_mut()
                .zip(pivot[column..=size].iter())
            {
                *entry -= factor * value;
            }
        }
    }
    let mut solution = vec![0.0_f64; size];
    for row in (0..size).rev() {
        let mut value = augmented[row][size];
        for (column, coefficient) in solution.iter().enumerate().skip(row + 1) {
            value -= augmented[row][column] * coefficient;
        }
        solution[row] = if augmented[row][row].abs() < 1e-18 {
            0.0
        } else {
            value / augmented[row][row]
        };
    }
    solution
}
