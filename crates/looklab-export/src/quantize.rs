// SPDX-License-Identifier: AGPL-3.0-or-later
//! Artifact storage precision: round to the grid the firmware sees.

use looklab_schema::Precision;

/// Rounds to the destination precision and back.
pub fn quantize(value: f32, precision: Precision) -> f32 {
    match precision {
        Precision::F32 => value,
        Precision::F16 => f16_value(f16_bits(value)),
        Precision::U16 => quantize_unsigned(value, 65_535),
        Precision::U12 => quantize_unsigned(value, 4_095),
    }
}

fn quantize_unsigned(value: f32, max: u32) -> f32 {
    let clamped = value.clamp(0.0, 1.0);
    (clamped * max as f32).round() / max as f32
}

/// IEEE 754 binary16 bits for `value`, round to nearest, ties to even.
pub fn f16_bits(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x007f_ffff;

    if exponent == 0xff {
        return sign | 0x7c00 | if mantissa != 0 { 0x0200 } else { 0 };
    }
    let unbiased = exponent - 127;
    if unbiased > 15 {
        return sign | 0x7c00;
    }
    if unbiased < -24 {
        return sign;
    }
    if unbiased < -14 {
        let drop = (13 - unbiased) as u32;
        let significand = mantissa | 0x0080_0000;
        let mut rounded = significand >> drop;
        let remainder = significand & ((1 << drop) - 1);
        let halfway = 1 << (drop - 1);
        if remainder > halfway || (remainder == halfway && rounded & 1 == 1) {
            rounded += 1;
        }
        return sign | rounded as u16;
    }
    let mut rounded = mantissa >> 13;
    let remainder = mantissa & 0x1fff;
    if remainder > 0x1000 || (remainder == 0x1000 && rounded & 1 == 1) {
        rounded += 1;
        if rounded == 0x0400 {
            let next = unbiased + 16;
            if next >= 31 {
                return sign | 0x7c00;
            }
            return sign | ((next as u16) << 10);
        }
    }
    sign | (((unbiased + 15) as u16) << 10) | rounded as u16
}

pub fn f16_value(bits: u16) -> f32 {
    let sign = ((bits & 0x8000) as u32) << 16;
    let exponent = ((bits >> 10) & 0x1f) as u32;
    let mantissa = (bits & 0x03ff) as u32;
    if exponent == 0 {
        if mantissa == 0 {
            return f32::from_bits(sign);
        }
        let mut exponent = 127 - 15 + 1;
        let mut mantissa = mantissa;
        while mantissa & 0x0400 == 0 {
            mantissa <<= 1;
            exponent -= 1;
        }
        return f32::from_bits(sign | (exponent << 23) | ((mantissa & 0x03ff) << 13));
    }
    if exponent == 31 {
        return f32::from_bits(sign | 0x7f80_0000 | (mantissa << 13));
    }
    f32::from_bits(sign | ((exponent + 127 - 15) << 23) | (mantissa << 13))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f16_round_trips_exact_values() {
        for value in [0.0_f32, 1.0, 0.5, -2.0, 0.1, 65_504.0, 1.0 / 3.0] {
            let back = f16_value(f16_bits(value));
            assert!(
                (back - value).abs() <= value.abs() * 5.0e-4,
                "{value} -> {back}"
            );
        }
        assert_eq!(f16_value(f16_bits(1.0)), 1.0);
        assert_eq!(f16_value(f16_bits(-1.5)), -1.5);
    }

    #[test]
    fn f16_handles_subnormals() {
        let smallest = f16_value(1);
        assert!((smallest - 5.960_464_5e-8).abs() < 1.0e-14);
        assert_eq!(f16_value(f16_bits(0.0)), 0.0);
    }

    #[test]
    fn unsigned_precisions_stay_in_range() {
        assert_eq!(
            quantize(0.3, Precision::U12),
            (0.3_f32 * 4095.0).round() / 4095.0
        );
        assert_eq!(quantize(-1.0, Precision::U16), 0.0);
        assert_eq!(quantize(2.0, Precision::U16), 1.0);
    }
}
