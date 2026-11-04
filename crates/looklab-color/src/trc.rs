// SPDX-License-Identifier: AGPL-3.0-or-later
//! RGB transfer functions (encoding and decoding), ported from colour-science
//! `colour.models.rgb.transfer_functions` (BSD-3-Clause, Colour Developers);
//! see `NOTICE`.
//!
//! Values outside `[0, 1]` take the same formula; Adobe RGB (1998) is a pure
//! power law, so negative linear light is NaN there.

/// IEC 61966-2-1:1999; colour-science `eotf_inverse_sRGB`.
pub fn srgb_encode(linear: f64) -> f64 {
    if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * libm::pow(linear, 1.0 / 2.4) - 0.055
    }
}

/// `eotf_inverse_sRGB(0.003_130_8)`, the encoded value where the sRGB segments meet.
const SRGB_DECODE_THRESHOLD: f64 = 0.040_449_936;

/// IEC 61966-2-1:1999; colour-science `eotf_sRGB`.
pub fn srgb_decode(encoded: f64) -> f64 {
    if encoded <= SRGB_DECODE_THRESHOLD {
        encoded / 12.92
    } else {
        libm::pow((encoded + 0.055) / 1.055, 2.4)
    }
}

/// Adobe RGB (1998) gamma, 563/256.
const ADOBE_RGB_GAMMA: f64 = 563.0 / 256.0;

/// colour-science `gamma_function` with exponent `1 / 2.19921875`.
pub fn adobe_rgb_encode(linear: f64) -> f64 {
    libm::pow(linear, 1.0 / ADOBE_RGB_GAMMA)
}

/// colour-science `gamma_function` with exponent `2.19921875`.
pub fn adobe_rgb_decode(encoded: f64) -> f64 {
    libm::pow(encoded, ADOBE_RGB_GAMMA)
}

const REC2020_ALPHA: f64 = 1.099;
const REC2020_BETA: f64 = 0.018;

/// `oetf_BT2020(0.018)`, the encoded value where the Rec. 2020 segments meet.
const REC2020_DECODE_THRESHOLD: f64 = 0.081_247_944_035_140_49;

/// Rec. 2020 opto-electronic transfer function, ITU-R BT.2020.
/// colour-science `oetf_BT2020` with its default constants.
pub fn rec2020_encode(linear: f64) -> f64 {
    if linear < REC2020_BETA {
        4.5 * linear
    } else {
        REC2020_ALPHA * libm::pow(linear, 0.45) - (REC2020_ALPHA - 1.0)
    }
}

/// Rec. 2020 inverse opto-electronic transfer function, ITU-R BT.2020.
/// colour-science `oetf_inverse_BT2020` with its default constants.
pub fn rec2020_decode(encoded: f64) -> f64 {
    if encoded < REC2020_DECODE_THRESHOLD {
        encoded / 4.5
    } else {
        libm::pow(
            (encoded + (REC2020_ALPHA - 1.0)) / REC2020_ALPHA,
            1.0 / 0.45,
        )
    }
}

/// ROMM RGB (ProPhoto) encoding, ANSI/I3A IT10.7666:2006; RGB values in `[0, 1]`.
/// colour-science `cctf_encoding_ROMMRGB`.
pub fn prophoto_encode(linear: f64) -> f64 {
    if linear < 1.0 / 512.0 {
        linear * 16.0
    } else {
        libm::pow(linear, 1.0 / 1.8)
    }
}

/// ROMM RGB (ProPhoto) decoding, ANSI/I3A IT10.7666:2006.
/// colour-science `cctf_decoding_ROMMRGB`.
pub fn prophoto_decode(encoded: f64) -> f64 {
    if encoded < 1.0 / 32.0 {
        encoded / 16.0
    } else {
        libm::pow(encoded, 1.8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_is_continuous_at_its_segment_boundary() {
        let below = srgb_encode(0.003_130_8);
        let above = srgb_encode(0.003_130_8 + 1e-9);
        assert!((above - below).abs() < 1e-7);
    }

    #[test]
    fn rec2020_decode_threshold_matches_encode() {
        assert!((rec2020_encode(REC2020_BETA) - REC2020_DECODE_THRESHOLD).abs() < 1e-15);
    }

    #[test]
    fn prophoto_is_continuous_at_its_segment_boundary() {
        let below = prophoto_encode(1.0 / 512.0 - 1e-12);
        let above = prophoto_encode(1.0 / 512.0 + 1e-12);
        assert!((above - below).abs() < 1e-9);
    }

    #[test]
    fn adobe_rgb_negative_light_is_nan() {
        assert!(adobe_rgb_encode(-0.5).is_nan());
        assert!(adobe_rgb_decode(-0.5).is_nan());
    }

    #[test]
    fn encodings_extend_above_one() {
        assert!(srgb_encode(1.5) > 1.0);
        assert!(adobe_rgb_encode(1.5) > 1.0);
        assert!(rec2020_encode(1.5) > 1.0);
        assert!(prophoto_encode(1.5) > 1.0);
    }
}
