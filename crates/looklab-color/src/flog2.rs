// SPDX-License-Identifier: AGPL-3.0-or-later
//! Fujifilm F-Log2 log encoding and its inverse, from *F-Log2 Data Sheet
//! Ver.1.1* §2-3 (<https://dl.fujifilm-x.com/technical-data/F-Log2_DataSheet_E_Ver.1.1.pdf>).

pub const FLOG2_CUT1: f64 = 0.000_889;
pub const FLOG2_CUT2: f64 = 0.100_686_685_370_811;

const A: f64 = 5.555_556;
const B: f64 = 0.064_829;
const C: f64 = 0.245_281;
const D: f64 = 0.384_316;
const E: f64 = 8.799_461;
const F: f64 = 0.092_864;

/// Scene-linear reflection to normalised F-Log2 code value.
pub fn encode(reflection: f64) -> f64 {
    if reflection < FLOG2_CUT1 {
        E * reflection + F
    } else {
        C * libm::log10(A * reflection + B) + D
    }
}

/// Normalised F-Log2 code value to scene-linear reflection.
pub fn decode(code: f64) -> f64 {
    if code < FLOG2_CUT2 {
        (code - F) / E
    } else {
        (libm::pow(10.0, (code - D) / C) - B) / A
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_code_values() {
        // Data sheet §2-2: 0%, 18% and 90% reflection are 95, 400 and 570
        // on the 10-bit scale, i.e. these normalised code values after rounding.
        let code = |reflection: f64| (encode(reflection) * 1023.0).round() as i64;
        assert_eq!(code(0.0), 95);
        assert_eq!(code(0.18), 400);
        assert_eq!(code(0.90), 570);
    }

    #[test]
    fn decode_inverts_encode() {
        for step in 0..=1000 {
            let reflection = step as f64 / 1000.0 * 60.0;
            let round_trip = decode(encode(reflection));
            assert!(
                (round_trip - reflection).abs() <= 1e-10 * reflection.abs().max(1.0),
                "{reflection}: {round_trip}"
            );
        }
    }

    #[test]
    fn segments_are_continuous() {
        let below = E * FLOG2_CUT1 + F;
        let above = C * libm::log10(A * FLOG2_CUT1 + B) + D;
        assert!((below - above).abs() < 1e-7);
    }
}
