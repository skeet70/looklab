// SPDX-License-Identifier: AGPL-3.0-or-later
//! The exact `Y_ref` solve of docs/02 §3.3 step 2.

use looklab_color::cam16::{Cam16Model, ViewingConditions};
use looklab_color::{Xyz, ucs};
use std::sync::LazyLock;
use thiserror::Error;

/// Largest `J'` the CAM16-UCS formula approaches as `J` goes to infinity:
/// `(1 + 100·c1)/c1` with `c1 = 0.007`. The neutral model cannot reach it.
pub const J_PRIME_MAX: f64 = 1.7 / 0.007;

/// Failure to invert the neutral `J'(Y)` curve.
#[derive(Clone, Copy, Debug, Error, PartialEq)]
pub enum SolveError {
    #[error("target J' {target} is outside the attainable range [0, {max}]")]
    Unattainable { target: f64, max: f64 },
    #[error("Y_ref solve did not converge")]
    Convergence,
}

static REFERENCE_MODEL: LazyLock<Cam16Model> =
    LazyLock::new(|| Cam16Model::new(&ViewingConditions::reference()));

pub fn reference_model() -> &'static Cam16Model {
    &REFERENCE_MODEL
}

pub fn reference_white() -> Xyz {
    ViewingConditions::reference().white
}

/// CAM16-UCS `J'` of a D65 neutral stimulus at luminance `y`.
pub fn neutral_j_prime(y: f64) -> f64 {
    crate::deriv::reference_cam16().neutral_j_and_dj_dy(y).0
}

/// Largest neutral `J'` at finite luminance, approached as `Y -> infinity`.
pub fn neutral_j_prime_max() -> f64 {
    static MAXIMUM: LazyLock<f64> =
        LazyLock::new(|| crate::deriv::reference_cam16().neutral_j_prime_max());
    *MAXIMUM
}

/// `Y` whose D65 neutral `J'` equals `target`, with an explicit error for
/// targets outside the attainable range.
pub fn try_solve_y_ref(target: f64, guess: f64) -> Result<f64, SolveError> {
    let max = neutral_j_prime_max();
    if target == 0.0 {
        return Ok(0.0);
    }
    if !(target > 0.0 && target < max) {
        return Err(SolveError::Unattainable { target, max });
    }
    let model = crate::deriv::reference_cam16();
    let value = |y: f64| model.neutral_j_and_dj_dy(y).0;

    let fallback = if guess > 0.0 && guess.is_finite() {
        guess
    } else {
        0.18
    };
    let starting = model.neutral_y_for_j_prime(target).unwrap_or(fallback);
    let starting = if starting > 0.0 && starting.is_finite() {
        starting
    } else {
        fallback
    };
    let mut lo = 0.0_f64;
    let mut hi = (starting * 2.0).min(f64::MAX / 4.0);
    let mut guard = 0;
    while value(hi) < target {
        hi *= 2.0;
        guard += 1;
        if !hi.is_finite() || guard > 1024 {
            return Err(SolveError::Unattainable { target, max });
        }
    }

    let mut y = starting.clamp(f64::MIN_POSITIVE, hi);
    let mut converged = false;
    for _ in 0..256 {
        let (current, derivative) = model.neutral_j_and_dj_dy(y);
        let residual = current - target;
        if residual.abs() < 1e-10 {
            converged = true;
            break;
        }
        if residual < 0.0 {
            lo = y;
        } else {
            hi = y;
        }
        let newton = if derivative.is_finite() && derivative > 0.0 {
            y - residual / derivative
        } else {
            f64::NAN
        };
        y = if newton.is_finite() && newton > lo && newton < hi {
            newton
        } else {
            0.5 * (lo + hi)
        };
    }
    if converged {
        Ok(y)
    } else {
        Err(SolveError::Convergence)
    }
}

/// `Y` whose D65 neutral `J'` equals `target`. Targets outside the attainable
/// range saturate at `neutral_j_prime_max() * (1 - 1e-12)`; use
/// [`try_solve_y_ref`] to detect that.
pub fn solve_y_ref(target: f64, guess: f64) -> f64 {
    match try_solve_y_ref(target, guess) {
        Ok(y) => y,
        Err(SolveError::Unattainable { .. }) => {
            let saturated = neutral_j_prime_max() * (1.0 - 1e-12);
            try_solve_y_ref(saturated, guess).unwrap_or_else(|_| {
                let relaxed = neutral_j_prime_max() * (1.0 - 1e-9);
                try_solve_y_ref(relaxed, guess).unwrap_or(0.18)
            })
        }
        Err(SolveError::Convergence) => {
            let relaxed = neutral_j_prime_max() * (1.0 - 1e-9);
            try_solve_y_ref(relaxed, guess).unwrap_or(0.18)
        }
    }
}

/// CAM16-UCS of `xyz` under the reference viewing conditions.
pub fn ucs_of_xyz(xyz: Xyz) -> ucs::Ucs {
    let cam = reference_model().xyz_to_cam16(xyz);
    ucs::jmh_to_ucs(
        looklab_color::cam16::Jmh {
            j: cam.j,
            m: cam.m,
            h: cam.h,
        },
        ucs::UcsSpace::Ucs,
    )
}
