// SPDX-License-Identifier: AGPL-3.0-or-later
//! The exact `Y_ref` solve of docs/02 §3.3 step 2.

use looklab_color::cam16::{Cam16Model, ViewingConditions};
use looklab_color::{Xyz, ucs};
use std::sync::LazyLock;

/// Largest `J'` a neutral stimulus approaches as `Y` goes to infinity:
/// `(1 + 100·c1)/c1` with `c1 = 0.007`.
pub const J_PRIME_MAX: f64 = 1.7 / 0.007;

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

/// `Y` whose D65 neutral `J'` equals `target`. Safeguarded Newton with a
/// bisection fallback; the closed-form neutral inverse starts the iteration.
pub fn solve_y_ref(target: f64, guess: f64) -> f64 {
    if target <= 0.0 {
        return 0.0;
    }
    let target = target.min(J_PRIME_MAX - 1e-6);
    let model = crate::deriv::reference_cam16();
    let value = |y: f64| model.neutral_j_and_dj_dy(y).0;

    let inverted = model.neutral_y_for_j_prime(target);
    let starting = inverted.unwrap_or(if guess > 0.0 && guess.is_finite() {
        guess
    } else {
        0.18
    });
    let mut lo = 0.0_f64;
    let mut hi = (starting * 2.0).clamp(1e-9, 1e8);
    let mut guard = 0;
    while value(hi) < target {
        hi *= 2.0;
        guard += 1;
        if guard > 64 || hi >= 1e8 {
            hi = 1e8;
            break;
        }
    }

    let mut y = starting.clamp(1e-12, hi);
    for _ in 0..160 {
        let (current, derivative) = model.neutral_j_and_dj_dy(y);
        let residual = current - target;
        if residual.abs() < 1e-10 {
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
    y
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
