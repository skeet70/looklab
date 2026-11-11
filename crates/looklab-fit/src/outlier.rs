// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-image robust outlier scoring and reason heuristics (docs/02 §4.4).

use crate::problem::{PassOutput, Problem};
use crate::types::{FitOptions, ImageFitStats, OutlierReason, Verdict};

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        0.5 * (values[mid - 1] + values[mid])
    } else {
        values[mid]
    }
}

/// Trimmed root mean square of the sample residuals, dropping the worst 5% per image.
fn robust_rms(values: &mut [(f64, f64)]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.1.total_cmp(&b.1));
    let keep = (values.len() * 95 / 100).max(1);
    let (sum, weight) = values[..keep]
        .iter()
        .fold((0.0, 0.0), |acc, (w, r)| (acc.0 + w * r, acc.1 + w));
    if weight > 0.0 {
        (sum / weight).max(0.0).sqrt()
    } else {
        0.0
    }
}

pub(crate) fn score(
    problem: &Problem,
    pass: &PassOutput,
    _options: &FitOptions,
) -> Vec<ImageFitStats> {
    let count = problem.images.len();
    let mut rms = vec![0.0_f64; count];
    let have_samples = pass.residuals.len() == problem.samples.len();
    for index in 0..count {
        let range = problem.images[index].samples.clone();
        if have_samples && !range.is_empty() {
            let mut values: Vec<(f64, f64)> = range
                .map(|sample| {
                    let residual = pass.residuals[sample];
                    let squared = residual[0] * residual[0]
                        + residual[1] * residual[1]
                        + residual[2] * residual[2];
                    (problem.samples[sample].weight, squared)
                })
                .collect();
            rms[index] = robust_rms(&mut values);
        }
    }
    let mut pool = rms.clone();
    let pool_median = median(&mut pool);
    let mut deviations: Vec<f64> = rms
        .iter()
        .map(|value| (value - pool_median).abs())
        .collect();
    let mad = median(&mut deviations).max(1e-6);
    let mut out = Vec::with_capacity(count);
    for (index, value) in rms.iter().enumerate() {
        let z = (value - pool_median) / (1.4826 * mad);
        let verdict = if z > 3.5 {
            Verdict::Quarantine
        } else if z > 2.5 {
            Verdict::DownWeight
        } else {
            Verdict::Accept
        };
        let reason = if verdict == Verdict::Accept {
            None
        } else {
            Some(reason_for(problem, pass, index))
        };
        out.push(ImageFitStats {
            rms_de: *value,
            z,
            verdict,
            reason,
        });
    }
    out
}

fn reason_for(problem: &Problem, pass: &PassOutput, image: usize) -> OutlierReason {
    let range = problem.images[image].samples.clone();
    let mut sum_j = 0.0;
    let mut sum_a = 0.0;
    let mut sum_b = 0.0;
    let mut weight = 0.0;
    let mut high = 0.0;
    let mut high_weight = 0.0;
    let mut low = 0.0;
    let mut low_weight = 0.0;
    if pass.residuals.len() == problem.samples.len() {
        for index in range {
            let residual = pass.residuals[index];
            let w = problem.samples[index].weight;
            sum_j += w * residual[0];
            sum_a += w * residual[1];
            sum_b += w * residual[2];
            weight += w;
            let target = problem.samples[index].target.ucs;
            let squared =
                residual[0] * residual[0] + residual[1] * residual[1] + residual[2] * residual[2];
            if target.j > 80.0 {
                high += w * squared;
                high_weight += w;
            } else {
                low += w * squared;
                low_weight += w;
            }
        }
    }
    if weight <= 0.0 {
        return OutlierReason::ContentCorrelated;
    }
    let mean_j = sum_j / weight;
    let mean_a = sum_a / weight;
    let mean_b = sum_b / weight;
    let chroma = (mean_a * mean_a + mean_b * mean_b).sqrt();
    let rms_high = if high_weight > 0.0 {
        (high / high_weight).sqrt()
    } else {
        0.0
    };
    let rms_low = if low_weight > 0.0 {
        (low / low_weight).sqrt()
    } else {
        0.0
    };
    if rms_high > 2.0 * rms_low.max(0.2) && rms_high > 1.0 {
        OutlierReason::ShoulderOnly
    } else if chroma > 1.5 * mean_j.abs().max(0.2) && chroma > 0.6 {
        OutlierReason::HueUniformCast
    } else if mean_j.abs() > 0.8 && chroma < 0.5 * mean_j.abs() {
        OutlierReason::ExposureLike
    } else {
        OutlierReason::ContentCorrelated
    }
}
