// SPDX-License-Identifier: AGPL-3.0-or-later
//! Dense Cholesky, preconditioned conjugate gradients, and the GCV search for one block.

/// In-place Cholesky of a row-major symmetric matrix; `false` if not positive definite.
pub(crate) fn factor_in_place(a: &mut [f64], n: usize) -> bool {
    let a = &mut a[..n * n];
    for j in 0..n {
        let (_, remaining) = a.split_at_mut(j * n);
        let (pivot, following) = remaining.split_at_mut(n);
        let mut diagonal = pivot[j];
        for &value in &pivot[..j] {
            diagonal -= value * value;
        }
        if !diagonal.is_finite() || diagonal <= 0.0 {
            return false;
        }
        let root = diagonal.sqrt();
        pivot[j] = root;
        let mut groups = following.chunks_exact_mut(4 * n);
        // Each row retains increasing-k subtraction order.
        for group in groups.by_ref() {
            let (row0, rest) = group.split_at_mut(n);
            let (row1, rest) = rest.split_at_mut(n);
            let (row2, row3) = rest.split_at_mut(n);
            let mut value0 = row0[j];
            let mut value1 = row1[j];
            let mut value2 = row2[j];
            let mut value3 = row3[j];
            for (k, &right) in pivot[..j].iter().enumerate() {
                value0 -= row0[k] * right;
                value1 -= row1[k] * right;
                value2 -= row2[k] * right;
                value3 -= row3[k] * right;
            }
            row0[j] = value0 / root;
            row1[j] = value1 / root;
            row2[j] = value2 / root;
            row3[j] = value3 / root;
        }
        for row in groups.into_remainder().chunks_exact_mut(n) {
            let mut value = row[j];
            for (&left, &right) in row[..j].iter().zip(&pivot[..j]) {
                value -= left * right;
            }
            row[j] = value / root;
        }
    }
    true
}

pub(crate) fn solve_in_place(l: &[f64], n: usize, b: &mut [f64]) {
    for i in 0..n {
        let mut value = b[i];
        for k in 0..i {
            value -= l[i * n + k] * b[k];
        }
        b[i] = value / l[i * n + i];
    }
    for i in (0..n).rev() {
        let mut value = b[i];
        for k in i + 1..n {
            value -= l[k * n + i] * b[k];
        }
        b[i] = value / l[i * n + i];
    }
}

pub(crate) fn dense_mat_vec(a: &[f64], n: usize, v: &[f64]) -> Vec<f64> {
    let mut out = vec![0.0_f64; n];
    for i in 0..n {
        let mut sum = 0.0;
        for j in 0..n {
            sum += a[i * n + j] * v[j];
        }
        out[i] = sum;
    }
    out
}

pub(crate) fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

pub(crate) fn scaled_add(target: &mut [f64], source: &[f64], scale: f64) {
    for (slot, value) in target.iter_mut().zip(source.iter()) {
        *slot += scale * value;
    }
}

/// Jacobi-preconditioned conjugate gradients, deterministic and allocation-free per iteration.
pub(crate) fn conjugate_gradient(
    dim: usize,
    apply: impl Fn(&[f64], &mut [f64]),
    diagonal: &[f64],
    rhs: &[f64],
    x: &mut [f64],
    max_iterations: usize,
    tolerance: f64,
) {
    x.fill(0.0);
    let mut residual = rhs.to_vec();
    let mut z = vec![0.0_f64; dim];
    let mut direction = vec![0.0_f64; dim];
    let mut product = vec![0.0_f64; dim];
    for i in 0..dim {
        z[i] = residual[i] / diagonal[i].max(1e-12);
        direction[i] = z[i];
    }
    let mut rz = dot(&residual, &z);
    let target = rz * tolerance * tolerance;
    for _ in 0..max_iterations {
        if rz <= target || rz <= 0.0 {
            break;
        }
        apply(&direction, &mut product);
        let curvature = dot(&direction, &product);
        if curvature <= 0.0 {
            break;
        }
        let step = rz / curvature;
        scaled_add(x, &direction, step);
        for i in 0..dim {
            residual[i] -= step * product[i];
        }
        for i in 0..dim {
            z[i] = residual[i] / diagonal[i].max(1e-12);
        }
        let next = dot(&residual, &z);
        let beta = next / rz;
        for i in 0..dim {
            direction[i] = z[i] + beta * direction[i];
        }
        rz = next;
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct GcvChoice {
    pub lambda: f64,
}

/// Generalized cross-validation over eight log-spaced `λ`, with a Hutchinson trace estimate.
#[allow(clippy::too_many_arguments)]
pub(crate) fn select_lambda(
    hessian: &[f64],
    penalty: &[f64],
    gradient: &[f64],
    theta0: &[f64],
    rss: f64,
    n: usize,
    observations: f64,
    probes: usize,
    seed: u64,
) -> GcvChoice {
    let trace_h: f64 = (0..n).map(|i| hessian[i * n + i]).sum();
    let trace_s: f64 = (0..n).map(|i| penalty[i * n + i]).sum();
    if !(trace_h.is_finite() && trace_h > 0.0) || !(trace_s.is_finite() && trace_s > 0.0) {
        return GcvChoice { lambda: 0.0 };
    }
    let scale = trace_h / trace_s;
    let mut best = GcvChoice { lambda: 0.0 };
    let mut best_score = f64::INFINITY;
    let mut rng = crate::problem::Xorshift::new(seed ^ 0x9e37_79b9_7f4a_7c15);
    let probe_vectors: Vec<Vec<f64>> = (0..probes)
        .map(|_| {
            (0..n)
                .map(|_| if rng.unit() < 0.5 { -1.0 } else { 1.0 })
                .collect()
        })
        .collect();
    for step in 0..8 {
        let lambda = scale * libm::pow(10.0, -4.0 + step as f64);
        let mut a = hessian.to_vec();
        for index in 0..n * n {
            a[index] += lambda * penalty[index];
        }
        if !factor_in_place(&mut a, n) {
            continue;
        }
        let mut delta = vec![0.0_f64; n];
        for i in 0..n {
            let mut offset = 0.0;
            for j in 0..n {
                offset += penalty[i * n + j] * theta0[j];
            }
            delta[i] = -gradient[i] - lambda * offset;
        }
        solve_in_place(&a, n, &mut delta);
        let h_delta = dense_mat_vec(hessian, n, &delta);
        let fitted = rss + 2.0 * dot(&delta, gradient) + dot(&delta, &h_delta);
        let mut dof = 0.0;
        for probe in &probe_vectors {
            let mut y = probe.clone();
            solve_in_place(&a, n, &mut y);
            let h_y = dense_mat_vec(hessian, n, &y);
            dof += dot(probe, &h_y);
        }
        dof /= probes as f64;
        let dof = dof.min(observations * 0.999).max(0.0);
        let denominator = 1.0 - dof / observations;
        if denominator <= 0.0 {
            continue;
        }
        let score = (fitted.max(1e-12) / observations) / (denominator * denominator);
        if score.is_finite() && score < best_score {
            best_score = score;
            let _ = dof;
            best = GcvChoice { lambda };
        }
    }
    best
}

/// Diagonal-only GCV, for the matrix-free residual block.
pub(crate) fn select_lambda_diagonal(
    hessian_diagonal: &[f64],
    penalty_diagonal: &[f64],
    gradient: &[f64],
    theta0: &[f64],
    rss: f64,
    observations: f64,
) -> GcvChoice {
    let trace_h: f64 = hessian_diagonal.iter().sum();
    let trace_s: f64 = penalty_diagonal.iter().sum();
    if !(trace_h.is_finite() && trace_h > 0.0) || !(trace_s.is_finite() && trace_s > 0.0) {
        return GcvChoice { lambda: 0.0 };
    }
    let scale = trace_h / trace_s;
    let mut best = GcvChoice { lambda: 0.0 };
    let mut best_score = f64::INFINITY;
    for step in 0..8 {
        let lambda = scale * libm::pow(10.0, -4.0 + step as f64);
        let mut fitted = rss;
        let mut dof = 0.0;
        for index in 0..hessian_diagonal.len() {
            let h = hessian_diagonal[index];
            let a = h + lambda * penalty_diagonal[index];
            if a <= 0.0 {
                continue;
            }
            dof += h / a;
            let delta = (-gradient[index] - lambda * penalty_diagonal[index] * theta0[index]) / a;
            fitted += 2.0 * delta * gradient[index] + h * delta * delta;
        }
        let dof = dof.min(observations * 0.999).max(0.0);
        let denominator = 1.0 - dof / observations;
        if denominator <= 0.0 {
            continue;
        }
        let score = (fitted.max(1e-12) / observations) / (denominator * denominator);
        if score.is_finite() && score < best_score {
            best_score = score;
            let _ = dof;
            best = GcvChoice { lambda };
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cholesky_matches_a_direct_solution() {
        for n in [1, 2, 3, 4, 5, 6, 7, 8, 9, 17, 32, 33] {
            let mut a = vec![0.0; n * n];
            for i in 0..n {
                a[i * n + i] = 4.0;
                if i + 1 < n {
                    a[i * n + i + 1] = 1.0;
                    a[(i + 1) * n + i] = 1.0;
                }
            }
            let mut x = vec![6.0; n];
            x[0] -= 1.0;
            x[n - 1] -= 1.0;
            assert!(factor_in_place(&mut a, n));
            solve_in_place(&a, n, &mut x);
            for value in x {
                assert!((value - 1.0).abs() < 1e-12, "n={n}");
            }
        }
    }

    #[test]
    fn conjugate_gradient_solves_a_spd_system() {
        let n = 4;
        let mut a = vec![0.0_f64; n * n];
        for i in 0..n {
            a[i * n + i] = 2.0 + i as f64;
            if i + 1 < n {
                a[i * n + i + 1] = 0.5;
                a[(i + 1) * n + i] = 0.5;
            }
        }
        let rhs = vec![1.0, -2.0, 0.5, 3.0];
        let diagonal: Vec<f64> = (0..n).map(|i| a[i * n + i]).collect();
        let mut x = vec![0.0; n];
        conjugate_gradient(
            n,
            |v, out| {
                for i in 0..n {
                    let mut sum = 0.0;
                    for j in 0..n {
                        sum += a[i * n + j] * v[j];
                    }
                    out[i] = sum;
                }
            },
            &diagonal,
            &rhs,
            &mut x,
            50,
            1e-12,
        );
        let check = dense_mat_vec(&a, n, &x);
        for i in 0..n {
            assert!((check[i] - rhs[i]).abs() < 1e-9, "{:?}", check);
        }
    }
}
