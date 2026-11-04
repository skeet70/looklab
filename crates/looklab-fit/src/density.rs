// SPDX-License-Identifier: AGPL-3.0-or-later
//! Weighted sample density in `(s, h_in, √C_in)` (docs/02 §3.6).

use looklab_model::LookInput;

use crate::layout::Params;
use crate::problem::Problem;
use crate::types::Density;

const S_BINS: usize = 36;
const H_BINS: usize = 24;
const C_BINS: usize = 12;
const S_LO: f64 = -12.0;
const S_HI: f64 = 6.0;
const C_HI: f64 = 120.0;

fn index_of(values: [f64; 3]) -> usize {
    let s = ((values[0] - S_LO) / (S_HI - S_LO) * S_BINS as f64).floor();
    let s = (s.max(0.0) as usize).min(S_BINS - 1);
    let h = (values[1] / 360.0 * H_BINS as f64).floor();
    let h = (h.max(0.0) as usize) % H_BINS;
    let c = (values[2] / C_HI * C_BINS as f64).floor();
    let c = (c.max(0.0) as usize).min(C_BINS - 1);
    (s * H_BINS + h) * C_BINS + c
}

fn wrap_hue(value: f64) -> f64 {
    let wrapped = value - (value / H_BINS as f64).floor() * H_BINS as f64;
    if wrapped < 0.0 {
        wrapped + H_BINS as f64
    } else {
        wrapped
    }
}

fn coordinates(s: f64, hue: f64, c: f64) -> [f64; 3] {
    [
        ((s - S_LO) / (S_HI - S_LO) * S_BINS as f64 - 0.5).clamp(0.0, (S_BINS - 1) as f64),
        wrap_hue((hue / 360.0 * H_BINS as f64) - 0.5),
        ((c / C_HI * C_BINS as f64) - 0.5).clamp(0.0, (C_BINS - 1) as f64),
    ]
}

pub(crate) fn build(problem: &Problem, params: &Params, floor_fraction: f64) -> Density {
    let mut counts = vec![0.0_f64; S_BINS * H_BINS * C_BINS];
    for sample in &problem.samples {
        let image = sample.image as usize;
        let context = problem.image_ctx(params, image);
        let xyz = context.transform.mul_vec([
            sample.rgb[0] * context.wb_r * libm::pow(2.0, context.exposure),
            sample.rgb[1] * libm::pow(2.0, context.exposure),
            sample.rgb[2] * context.wb_b * libm::pow(2.0, context.exposure),
        ]);
        let input = LookInput::new(looklab_color::Xyz::from_array(xyz));
        let weight = sample.density * problem.images[image].trust;
        counts[index_of([input.s, input.h_in, input.c_in])] += weight;
    }
    let total: f64 = counts.iter().sum();
    if total > 0.0 {
        smooth(&mut counts);
    }
    let scale = (S_BINS * H_BINS * C_BINS) as f64 / total.max(1e-12);
    for value in &mut counts {
        *value *= scale;
    }
    let peak = counts.iter().copied().fold(0.0_f64, f64::max);
    Density {
        s_bins: S_BINS,
        hue_bins: H_BINS,
        c_bins: C_BINS,
        counts,
        total,
        floor: floor_fraction * peak,
    }
}

fn smooth(counts: &mut [f64]) {
    let mut scratch = counts.to_vec();
    for _ in 0..2 {
        for s in 0..S_BINS {
            for h in 0..H_BINS {
                for c in 0..C_BINS {
                    let mut sum = 0.0;
                    for (offset, weight) in [(-1i64, 1.0), (0, 2.0), (1, 1.0)] {
                        let index = (s as i64 + offset).clamp(0, (S_BINS - 1) as i64) as usize;
                        sum += weight * scratch[(index * H_BINS + h) * C_BINS + c];
                    }
                    counts[(s * H_BINS + h) * C_BINS + c] = sum * 0.25;
                }
            }
        }
        scratch.copy_from_slice(counts);
        for s in 0..S_BINS {
            for h in 0..H_BINS {
                for c in 0..C_BINS {
                    let mut sum = 0.0;
                    for (offset, weight) in [(-1i64, 1.0), (0, 2.0), (1, 1.0)] {
                        let index = ((h as i64 + offset).rem_euclid(H_BINS as i64)) as usize;
                        sum += weight * scratch[(s * H_BINS + index) * C_BINS + c];
                    }
                    counts[(s * H_BINS + h) * C_BINS + c] = sum * 0.25;
                }
            }
        }
        scratch.copy_from_slice(counts);
        for s in 0..S_BINS {
            for h in 0..H_BINS {
                for c in 0..C_BINS {
                    let mut sum = 0.0;
                    for (offset, weight) in [(-1i64, 1.0), (0, 2.0), (1, 1.0)] {
                        let index = (c as i64 + offset).clamp(0, (C_BINS - 1) as i64) as usize;
                        sum += weight * scratch[(s * H_BINS + h) * C_BINS + index];
                    }
                    counts[(s * H_BINS + h) * C_BINS + c] = sum * 0.25;
                }
            }
        }
        scratch.copy_from_slice(counts);
    }
}

pub(crate) fn lookup(density: &Density, s: f64, hue: f64, c: f64) -> f64 {
    let [u, v, w] = coordinates(s, hue, c);
    let s0 = u.floor().max(0.0) as usize;
    let h0 = v.floor() as usize % H_BINS;
    let c0 = w.floor().max(0.0) as usize;
    let s1 = (s0 + 1).min(S_BINS - 1);
    let h1 = (h0 + 1) % H_BINS;
    let c1 = (c0 + 1).min(C_BINS - 1);
    let fu = u - s0 as f64;
    let fv = v - h0 as f64;
    let fw = w - c0 as f64;
    let at = |si: usize, hi: usize, ci: usize| density.counts[(si * H_BINS + hi) * C_BINS + ci];
    let mix = |a: f64, b: f64, t: f64| a * (1.0 - t) + b * t;
    let low = mix(
        mix(at(s0, h0, c0), at(s0, h1, c0), fv),
        mix(at(s0, h0, c1), at(s0, h1, c1), fv),
        fw,
    );
    let high = mix(
        mix(at(s1, h0, c0), at(s1, h1, c0), fv),
        mix(at(s1, h0, c1), at(s1, h1, c1), fv),
        fw,
    );
    mix(low, high, fu)
}
