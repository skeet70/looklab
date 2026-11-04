// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_model::Tone;
use looklab_model::basis::TONE_COEFS;
use proptest::prelude::*;

proptest! {
    #[test]
    fn non_negative_slopes_give_a_monotone_tone(
        c0 in -5.0_f64..5.0,
        values in proptest::collection::vec(0.0_f64..1.0, TONE_COEFS),
    ) {
        let mut beta = [0.0_f64; TONE_COEFS];
        beta.copy_from_slice(&values);
        let tone = Tone::spline(c0, beta).expect("non-negative betas");
        let mut previous = f64::NEG_INFINITY;
        for step in 0..=500 {
            let s = -12.0 + 18.0 * step as f64 / 500.0;
            let value = tone.eval(s);
            prop_assert!(value >= previous - 1e-12, "s={s} value={value} previous={previous}");
            previous = value;
        }
    }
}
