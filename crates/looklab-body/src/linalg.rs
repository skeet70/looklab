// SPDX-License-Identifier: AGPL-3.0-or-later
//! Dense fixed-size linear algebra: Gaussian elimination with partial pivoting.

/// Solve `a·x = b`. Returns `None` when a pivot is zero.
pub(crate) fn solve<const N: usize>(a: &mut [[f64; N]; N], b: &mut [f64; N]) -> Option<[f64; N]> {
    for column in 0..N {
        let mut pivot = column;
        for row in column + 1..N {
            if a[row][column].abs() > a[pivot][column].abs() {
                pivot = row;
            }
        }
        if a[pivot][column] == 0.0 {
            return None;
        }
        a.swap(column, pivot);
        b.swap(column, pivot);
        for row in column + 1..N {
            let factor = a[row][column] / a[column][column];
            if factor == 0.0 {
                continue;
            }
            for cell in column..N {
                a[row][cell] -= factor * a[column][cell];
            }
            b[row] -= factor * b[column];
        }
    }
    let mut x = [0.0_f64; N];
    for row in (0..N).rev() {
        let mut sum = b[row];
        for column in row + 1..N {
            sum -= a[row][column] * x[column];
        }
        x[row] = sum / a[row][row];
    }
    Some(x)
}

/// Inverse by solving `N` unit right-hand sides.
pub(crate) fn invert<const N: usize>(m: &[[f64; N]; N]) -> Option<[[f64; N]; N]> {
    let mut inverse = [[0.0_f64; N]; N];
    for column in 0..N {
        let mut a = *m;
        let mut b = [0.0_f64; N];
        b[column] = 1.0;
        let solution = solve(&mut a, &mut b)?;
        for row in 0..N {
            inverse[row][column] = solution[row];
        }
    }
    Some(inverse)
}

#[cfg(test)]
pub(crate) fn matmul<const N: usize>(a: &[[f64; N]; N], b: &[[f64; N]; N]) -> [[f64; N]; N] {
    let mut out = [[0.0_f64; N]; N];
    for row in 0..N {
        for column in 0..N {
            let mut sum = 0.0;
            for index in 0..N {
                sum += a[row][index] * b[index][column];
            }
            out[row][column] = sum;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solve_recovers_a_known_solution() {
        let mut a = [[3.0, 1.0], [1.0, 2.0]];
        let mut b = [9.0, 8.0];
        let x = solve(&mut a, &mut b).expect("nonsingular");
        assert!((x[0] - 2.0).abs() < 1e-12);
        assert!((x[1] - 3.0).abs() < 1e-12);
    }

    #[test]
    fn singular_matrix_has_no_solution() {
        let mut a = [[1.0, 2.0], [2.0, 4.0]];
        let mut b = [1.0, 2.0];
        assert!(solve(&mut a, &mut b).is_none());
    }

    #[test]
    fn invert_times_matrix_is_identity() {
        let m = [[4.0, 7.0, 2.0], [3.0, 6.0, 1.0], [2.0, 5.0, 3.0]];
        let inverse = invert(&m).expect("nonsingular");
        let product = matmul(&m, &inverse);
        for row in 0..3 {
            for column in 0..3 {
                let expected = if row == column { 1.0 } else { 0.0 };
                assert!((product[row][column] - expected).abs() < 1e-12);
            }
        }
    }
}
