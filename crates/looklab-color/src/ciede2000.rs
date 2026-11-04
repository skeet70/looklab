// SPDX-License-Identifier: AGPL-3.0-or-later
//! CIEDE2000 colour difference, ported from colour-science `delta_E_CIE2000`
//! (BSD-3-Clause, Colour Developers; see `NOTICE`). Implements Sharma, Wu and
//! Dalal (2005), *The CIEDE2000 color-difference formula: implementation notes,
//! supplementary test data, and mathematical observations*.

use crate::lab::Lab;
use core::f64::consts::PI;

/// CIEDE2000 reference conditions: `k_L = k_C = k_H = 1`.
pub fn delta_e_ciede2000(lab1: Lab, lab2: Lab) -> f64 {
    let c1 = libm::hypot(lab1.a, lab1.b);
    let c2 = libm::hypot(lab2.a, lab2.b);
    let c_bar = (c1 + c2) / 2.0;
    let c_bar_7 = pow7(c_bar);
    let g = 0.5 * (1.0 - libm::sqrt(c_bar_7 / (c_bar_7 + 6_103_515_625.0)));

    let a1p = (1.0 + g) * lab1.a;
    let a2p = (1.0 + g) * lab2.a;
    let c1p = libm::hypot(a1p, lab1.b);
    let c2p = libm::hypot(a2p, lab2.b);
    let h1p = hue_angle(lab1.b, a1p);
    let h2p = hue_angle(lab2.b, a2p);

    let delta_l = lab2.l - lab1.l;
    let delta_c = c2p - c1p;

    let hue_delta = h2p - h1p;
    let c_prod = c1p * c2p;
    let delta_h = if c_prod == 0.0 || hue_delta.is_nan() {
        0.0
    } else if hue_delta.abs() <= 180.0 {
        hue_delta
    } else if hue_delta > 180.0 {
        hue_delta - 360.0
    } else {
        hue_delta + 360.0
    };
    let delta_big_h = 2.0 * libm::sqrt(c_prod) * libm::sin(to_radians(delta_h / 2.0));

    let hue_abs_delta = (h1p - h2p).abs();
    let hue_sum = h1p + h2p;
    let h_bar = if c_prod == 0.0 {
        hue_sum
    } else if hue_abs_delta <= 180.0 {
        hue_sum / 2.0
    } else if hue_sum < 360.0 {
        (hue_sum + 360.0) / 2.0
    } else {
        (hue_sum - 360.0) / 2.0
    };

    let l_bar = (lab1.l + lab2.l) / 2.0;
    let c_bar_p = (c1p + c2p) / 2.0;
    let t = 1.0 - 0.17 * cos_degrees(h_bar - 30.0)
        + 0.24 * cos_degrees(2.0 * h_bar)
        + 0.32 * cos_degrees(3.0 * h_bar + 6.0)
        - 0.20 * cos_degrees(4.0 * h_bar - 63.0);

    let theta_ratio = (h_bar - 275.0) / 25.0;
    let delta_theta = 30.0 * libm::exp(-(theta_ratio * theta_ratio));

    let c_bar_p_7 = pow7(c_bar_p);
    let r_c = 2.0 * libm::sqrt(c_bar_p_7 / (c_bar_p_7 + 6_103_515_625.0));

    let l_bar_2 = (l_bar - 50.0) * (l_bar - 50.0);
    let s_l = 1.0 + (0.015 * l_bar_2) / libm::sqrt(20.0 + l_bar_2);
    let s_c = 1.0 + 0.045 * c_bar_p;
    let s_h = 1.0 + 0.015 * c_bar_p * t;
    let r_t = -libm::sin(to_radians(2.0 * delta_theta)) * r_c;

    let l_term = delta_l / s_l;
    let c_term = delta_c / s_c;
    let h_term = delta_big_h / s_h;

    libm::sqrt(l_term * l_term + c_term * c_term + h_term * h_term + r_t * c_term * h_term)
}

fn pow7(x: f64) -> f64 {
    let x2 = x * x;
    let x3 = x2 * x;
    x3 * x3 * x
}

fn hue_angle(b: f64, a: f64) -> f64 {
    if b == 0.0 && a == 0.0 {
        0.0
    } else {
        let degrees = libm::atan2(b, a) * (180.0 / PI);
        let remainder = degrees % 360.0;
        if remainder < 0.0 {
            remainder + 360.0
        } else {
            remainder
        }
    }
}

fn to_radians(degrees: f64) -> f64 {
    degrees * (PI / 180.0)
}

fn cos_degrees(degrees: f64) -> f64 {
    libm::cos(to_radians(degrees))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sharma, Wu and Dalal (2005), table 1.
    const SHARMA: [([f64; 3], [f64; 3], f64); 34] = [
        (
            [50.0000, 2.6772, -79.7751],
            [50.0000, 0.0000, -82.7485],
            2.0425,
        ),
        (
            [50.0000, 3.1571, -77.2803],
            [50.0000, 0.0000, -82.7485],
            2.8615,
        ),
        (
            [50.0000, 2.8361, -74.0200],
            [50.0000, 0.0000, -82.7485],
            3.4412,
        ),
        (
            [50.0000, -1.3802, -84.2814],
            [50.0000, 0.0000, -82.7485],
            1.0000,
        ),
        (
            [50.0000, -1.1848, -84.8006],
            [50.0000, 0.0000, -82.7485],
            1.0000,
        ),
        (
            [50.0000, -0.9009, -85.5211],
            [50.0000, 0.0000, -82.7485],
            1.0000,
        ),
        (
            [50.0000, 0.0000, 0.0000],
            [50.0000, -1.0000, 2.0000],
            2.3669,
        ),
        (
            [50.0000, -1.0000, 2.0000],
            [50.0000, 0.0000, 0.0000],
            2.3669,
        ),
        (
            [50.0000, 2.4900, -0.0010],
            [50.0000, -2.4900, 0.0009],
            7.1792,
        ),
        (
            [50.0000, 2.4900, -0.0010],
            [50.0000, -2.4900, 0.0010],
            7.1792,
        ),
        (
            [50.0000, 2.4900, -0.0010],
            [50.0000, -2.4900, 0.0011],
            7.2195,
        ),
        (
            [50.0000, 2.4900, -0.0010],
            [50.0000, -2.4900, 0.0012],
            7.2195,
        ),
        (
            [50.0000, -0.0010, 2.4900],
            [50.0000, 0.0009, -2.4900],
            4.8045,
        ),
        (
            [50.0000, -0.0010, 2.4900],
            [50.0000, 0.0010, -2.4900],
            4.8045,
        ),
        (
            [50.0000, -0.0010, 2.4900],
            [50.0000, 0.0011, -2.4900],
            4.7461,
        ),
        (
            [50.0000, 2.5000, 0.0000],
            [50.0000, 0.0000, -2.5000],
            4.3065,
        ),
        (
            [50.0000, 2.5000, 0.0000],
            [73.0000, 25.0000, -18.0000],
            27.1492,
        ),
        (
            [50.0000, 2.5000, 0.0000],
            [61.0000, -5.0000, 29.0000],
            22.8977,
        ),
        (
            [50.0000, 2.5000, 0.0000],
            [56.0000, -27.0000, -3.0000],
            31.9030,
        ),
        (
            [50.0000, 2.5000, 0.0000],
            [58.0000, 24.0000, 15.0000],
            19.4535,
        ),
        ([50.0000, 2.5000, 0.0000], [50.0000, 3.1736, 0.5854], 1.0000),
        ([50.0000, 2.5000, 0.0000], [50.0000, 3.2972, 0.0000], 1.0000),
        ([50.0000, 2.5000, 0.0000], [50.0000, 1.8634, 0.5757], 1.0000),
        ([50.0000, 2.5000, 0.0000], [50.0000, 3.2592, 0.3350], 1.0000),
        (
            [60.2574, -34.0099, 36.2677],
            [60.4626, -34.1751, 39.4387],
            1.2644,
        ),
        (
            [63.0109, -31.0961, -5.8663],
            [62.8187, -29.7946, -4.0864],
            1.2630,
        ),
        (
            [61.2901, 3.7196, -5.3901],
            [61.4292, 2.2480, -4.9620],
            1.8731,
        ),
        (
            [35.0831, -44.1164, 3.7933],
            [35.0232, -40.0716, 1.5901],
            1.8645,
        ),
        (
            [22.7233, 20.0904, -46.6940],
            [23.0331, 14.9730, -42.5619],
            2.0373,
        ),
        (
            [36.4612, 47.8580, 18.3852],
            [36.2715, 50.5065, 21.2231],
            1.4146,
        ),
        (
            [90.8027, -2.0831, 1.4410],
            [91.1528, -1.6435, 0.0447],
            1.4441,
        ),
        (
            [90.9257, -0.5406, -0.9208],
            [88.6381, -0.8985, -0.7239],
            1.5381,
        ),
        (
            [6.7747, -0.2908, -2.4247],
            [5.8714, -0.0985, -2.2286],
            0.6377,
        ),
        (
            [2.0776, 0.0795, -1.1350],
            [0.9033, -0.0636, -0.5514],
            0.9082,
        ),
    ];

    #[test]
    fn sharma_test_data_matches_to_four_decimals() {
        for (lab1, lab2, expected) in SHARMA {
            let delta = delta_e_ciede2000(Lab::from_array(lab1), Lab::from_array(lab2));
            assert!(
                (delta - expected).abs() < 1e-4,
                "{lab1:?} vs {lab2:?}: {delta} != {expected}"
            );
        }
    }

    #[test]
    fn difference_is_symmetric() {
        let lab1 = Lab::new(50.0, 2.5, 0.0);
        let lab2 = Lab::new(73.0, 25.0, -18.0);
        assert!((delta_e_ciede2000(lab1, lab2) - delta_e_ciede2000(lab2, lab1)).abs() < 1e-12);
    }
}
