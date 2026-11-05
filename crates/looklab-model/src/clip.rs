// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_color::cam16::Chroma;
use looklab_color::ucs::{Ucs, UcsSpace, ucs_to_jmh};
use looklab_color::{RgbSpace, Xyz};
use serde::{Deserialize, Serialize};

use crate::solve;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipFunction {
    TanhSoftV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClipSettings {
    pub function: ClipFunction,
    pub k: f64,
}

impl ClipSettings {
    pub fn default_v1() -> Self {
        Self {
            function: ClipFunction::TanhSoftV1,
            k: 0.03,
        }
    }

    pub fn validate(&self) -> bool {
        (0.005..=0.1).contains(&self.k)
    }
}

/// `c(x)` of docs/02 §3.4: identity on `[k, 1-k]`, `tanh` shoulders outside.
pub fn soft_clip(x: f64, k: f64) -> f64 {
    if x > 1.0 - k {
        (1.0 - k) + k * libm::tanh((x - (1.0 - k)) / k)
    } else if x < k {
        k - k * libm::tanh((k - x) / k)
    } else {
        x
    }
}

/// `J'a'b'` through linear output RGB, the per-channel soft clip, and back.
pub fn output_encode_clip(ucs: Ucs, output_space: RgbSpace, k: f64) -> Ucs {
    if let Some(encoded) = encode_once(ucs, output_space, k) {
        return encoded;
    }
    // Contract to the neutral at the same lightness, bounded to CAM16's inverse domain.
    let neutral = if ucs.j.is_finite() {
        Ucs::new(ucs.j.clamp(0.0, solve::neutral_j_prime_max()), 0.0, 0.0)
    } else {
        Ucs::new(0.0, 0.0, 0.0)
    };
    encode_once(neutral, output_space, k)
        .or_else(|| encode_once(Ucs::new(0.0, 0.0, 0.0), output_space, k))
        .unwrap_or(Ucs::new(0.0, 0.0, 0.0))
}

/// [`output_encode_clip`] for a point CAM16's inverse model is defined on.
fn encode_once(ucs: Ucs, output_space: RgbSpace, k: f64) -> Option<Ucs> {
    let model = solve::reference_model();
    let jmh = ucs_to_jmh(ucs, UcsSpace::Ucs);
    if !(jmh.j.is_finite() && jmh.m.is_finite() && jmh.h.is_finite()) || jmh.j < 0.0 {
        return None;
    }
    let xyz = model.cam16_to_xyz(jmh.j, Chroma::M(jmh.m), jmh.h);
    let rgb = output_space.xyz_to_rgb_linear(xyz);
    let clipped = looklab_color::Rgb::new(
        soft_clip(rgb.r, k),
        soft_clip(rgb.g, k),
        soft_clip(rgb.b, k),
    );
    if !(clipped.r.is_finite() && clipped.g.is_finite() && clipped.b.is_finite()) {
        return None;
    }
    let encoded: Xyz = output_space.rgb_linear_to_xyz(clipped);
    let out = solve::ucs_of_xyz(encoded);
    out.to_array()
        .iter()
        .all(|value| value.is_finite())
        .then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_clip_is_c1_at_the_knots() {
        for k in [0.005, 0.03, 0.1] {
            for knot in [k, 1.0 - k] {
                let h = 1e-7;
                let left = (soft_clip(knot, k) - soft_clip(knot - h, k)) / h;
                let right = (soft_clip(knot + h, k) - soft_clip(knot, k)) / h;
                assert!((left - 1.0).abs() < 1e-5, "k={k} left={left}");
                assert!((right - 1.0).abs() < 1e-5, "k={k} right={right}");
            }
        }
    }

    #[test]
    fn negative_lightness_contracts_to_the_neutral() {
        let neutral = output_encode_clip(Ucs::new(0.0, 0.0, 0.0), RgbSpace::Srgb, 0.03);
        let negative = output_encode_clip(Ucs::new(-0.16, 12.0, -8.0), RgbSpace::Srgb, 0.03);
        assert!(
            (negative.j - neutral.j).abs() < 1e-9,
            "{negative:?} vs {neutral:?}"
        );
        assert!((negative.a - neutral.a).abs() < 1e-9);
        assert!((negative.b - neutral.b).abs() < 1e-9);
    }

    #[test]
    fn soft_clip_limits_and_monotonicity() {
        let k = 0.03;
        assert!(soft_clip(1e6, k) <= 1.0);
        assert!(soft_clip(1e6, k) >= 1.0 - 1e-9);
        assert!(soft_clip(-1e6, k) >= 0.0);
        assert!(soft_clip(-1e6, k) <= 1e-9);
        let mut previous = f64::NEG_INFINITY;
        for step in 0..=2000 {
            let x = -2.0 + 4.0 * step as f64 / 2000.0;
            let y = soft_clip(x, k);
            assert!(y >= previous);
            previous = y;
        }
    }

    #[test]
    fn identity_on_the_interior() {
        let k = 0.03;
        for x in [0.03, 0.5, 0.97] {
            assert_eq!(soft_clip(x, k), x);
        }
    }
}
