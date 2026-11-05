// SPDX-License-Identifier: AGPL-3.0-or-later
//! The fixed `looklab-neutral-srgb-v1` display baseline (docs/02 §3.8).

use looklab_color::{Rgb, trc};

use crate::file::{BaseReference, NEUTRAL_BASELINE_SLUG, NEUTRAL_BASELINE_VERSION};

/// Scene-linear Rec.709 RGB to BT.709-encoded code values, hard-clipped to `[0, 1]`.
pub fn neutral_baseline_srgb_v1(scene_linear: Rgb) -> Rgb {
    let encode = |value: f64| trc::bt709_encode(value.clamp(0.0, 1.0));
    Rgb::new(
        encode(scene_linear.r),
        encode(scene_linear.g),
        encode(scene_linear.b),
    )
}

pub fn is_neutral_baseline(base: &BaseReference) -> bool {
    matches!(
        base,
        BaseReference::Look { slug, version }
            if slug.as_str() == NEUTRAL_BASELINE_SLUG
                && version.as_str() == NEUTRAL_BASELINE_VERSION
    )
}
