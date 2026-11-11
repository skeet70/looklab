// SPDX-License-Identifier: AGPL-3.0-or-later
//! The exact `Y_ref` solve of docs/02 §3.3 step 2.

use looklab_color::cam16::{Cam16Model, ViewingConditions};
use looklab_color::{WhiteRelativeXyz, Xyz, ucs};
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
    match model.neutral_y_for_j_prime(target) {
        Some(y) if y.is_finite() && y > 0.0 => Ok(y),
        _ => {
            let _ = guess;
            Err(SolveError::Unattainable { target, max })
        }
    }
}

/// `Y` whose D65 neutral `J'` equals `target`; out-of-range targets saturate
/// at `neutral_j_prime_max() * (1 - 1e-12)`. Use [`try_solve_y_ref`] to detect.
pub fn solve_y_ref(target: f64, guess: f64) -> f64 {
    if target <= 0.0 {
        return 0.0;
    }
    if let Ok(y) = try_solve_y_ref(target, guess) {
        return y;
    }
    let saturated = neutral_j_prime_max() * (1.0 - 1e-12);
    crate::deriv::reference_cam16()
        .neutral_y_for_j_prime(saturated)
        .unwrap_or(if guess > 0.0 { guess } else { 0.18 })
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

/// CAM16-UCS of `white + departure` under the reference viewing conditions;
/// `departure` is relative to the reference white.
pub fn ucs_of_xyz_departure(departure: WhiteRelativeXyz) -> ucs::Ucs {
    let cam = reference_model().xyz_departure_to_cam16(departure);
    ucs::jmh_to_ucs(
        looklab_color::cam16::Jmh {
            j: cam.j,
            m: cam.m,
            h: cam.h,
        },
        ucs::UcsSpace::Ucs,
    )
}
