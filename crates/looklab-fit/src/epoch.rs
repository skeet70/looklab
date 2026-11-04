// SPDX-License-Identifier: AGPL-3.0-or-later
//! Firmware-epoch and look split detection (docs/02 §4.5): permutation Hotelling T² over image-level residual projections.

/// One image's group, residual projection onto the offset basis, and the median ΔE its shift produces on supported probes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplitSample {
    pub group: u32,
    pub projection: [f64; 2],
    pub effect_de: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplitOptions {
    pub permutations: usize,
    pub alpha: f64,
    pub min_effect_de: f64,
    pub seed: u64,
}

impl Default for SplitOptions {
    fn default() -> Self {
        Self {
            permutations: 10_000,
            alpha: 0.001,
            min_effect_de: 1.0,
            seed: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplitDecision {
    /// The two groups compared, in ascending order.
    pub groups: (u32, u32),
    pub p_value: f64,
    /// Median effect difference between the groups, in ΔE.
    pub effect_de: f64,
    pub t_squared: f64,
}

/// Two-sample permutation test on Hotelling's T², gated by effect size.
pub fn detect_split(samples: &[SplitSample], options: &SplitOptions) -> Option<SplitDecision> {
    let mut groups: Vec<u32> = samples.iter().map(|sample| sample.group).collect();
    groups.sort_unstable();
    groups.dedup();
    if groups.len() < 2 {
        return None;
    }
    let mut best: Option<SplitDecision> = None;
    for pair in 0..groups.len() {
        for other in pair + 1..groups.len() {
            let (left, right) = (groups[pair], groups[other]);
            let Some(decision) = compare(samples, left, right, options) else {
                continue;
            };
            let better = match &best {
                None => true,
                Some(current) => decision.p_value < current.p_value,
            };
            if better {
                best = Some(decision);
            }
        }
    }
    let decision = best?;
    if decision.p_value < options.alpha && decision.effect_de >= options.min_effect_de {
        Some(decision)
    } else {
        None
    }
}

fn compare(
    samples: &[SplitSample],
    left: u32,
    right: u32,
    options: &SplitOptions,
) -> Option<SplitDecision> {
    let a: Vec<&SplitSample> = samples.iter().filter(|s| s.group == left).collect();
    let b: Vec<&SplitSample> = samples.iter().filter(|s| s.group == right).collect();
    if a.len() < 3 || b.len() < 3 {
        return None;
    }
    let mut labels: Vec<bool> = Vec::with_capacity(a.len() + b.len());
    let mut rows: Vec<[f64; 2]> = Vec::with_capacity(a.len() + b.len());
    for sample in &a {
        labels.push(true);
        rows.push(sample.projection);
    }
    for sample in &b {
        labels.push(false);
        rows.push(sample.projection);
    }
    let observed = hotelling(&rows, &labels)?;
    let mut rng = Xorshift::new(options.seed ^ ((left as u64) << 32) ^ right as u64);
    let mut exceed = 0usize;
    let permutations = options.permutations.max(1);
    for _ in 0..permutations {
        // Fisher-Yates over the label vector.
        for index in (1..labels.len()).rev() {
            let swap = rng.below(index + 1);
            labels.swap(index, swap);
        }
        if let Some(statistic) = hotelling(&rows, &labels)
            && statistic >= observed
        {
            exceed += 1;
        }
    }
    let p_value = (exceed as f64 + 1.0) / (permutations as f64 + 1.0);
    let median = |values: &mut Vec<f64>| {
        values.sort_by(f64::total_cmp);
        if values.is_empty() {
            0.0
        } else {
            values[values.len() / 2]
        }
    };
    let effect = (median(&mut a.iter().map(|s| s.effect_de).collect())
        - median(&mut b.iter().map(|s| s.effect_de).collect()))
    .abs();
    Some(SplitDecision {
        groups: (left, right),
        p_value,
        effect_de: effect,
        t_squared: observed,
    })
}

/// Hotelling's T² for the two labelled groups; `None` when the pooled scatter is singular.
fn hotelling(rows: &[[f64; 2]], label: &[bool]) -> Option<f64> {
    let mut count = [0.0_f64; 2];
    let mut mean = [[0.0_f64; 2]; 2];
    for (row, left) in rows.iter().zip(label.iter()) {
        let index = usize::from(!*left);
        count[index] += 1.0;
        for axis in 0..2 {
            mean[index][axis] += row[axis];
        }
    }
    if count[0] < 2.0 || count[1] < 2.0 {
        return None;
    }
    for (index, row) in mean.iter_mut().enumerate() {
        for value in row.iter_mut() {
            *value /= count[index].max(1.0);
        }
    }
    let mut scatter = [[0.0_f64; 2]; 2];
    for (row, left) in rows.iter().zip(label.iter()) {
        let index = usize::from(!*left);
        let delta = [row[0] - mean[index][0], row[1] - mean[index][1]];
        for i in 0..2 {
            for j in 0..2 {
                scatter[i][j] += delta[i] * delta[j];
            }
        }
    }
    let degrees = count[0] + count[1] - 2.0;
    let scale = count[0] * count[1] / (count[0] + count[1]);
    let difference = [mean[0][0] - mean[1][0], mean[0][1] - mean[1][1]];
    let t_squared = solve_2x2(scatter, difference)?;
    Some(scale * t_squared / degrees)
}

/// A tiny ridge keeps a rank-deficient scatter matrix invertible.
fn solve_2x2(scatter: [[f64; 2]; 2], d: [f64; 2]) -> Option<f64> {
    let ridge = 1e-12 * (scatter[0][0].abs() + scatter[1][1].abs() + 1e-30);
    let a = scatter[0][0] + ridge;
    let b = scatter[0][1];
    let c = scatter[1][0];
    let e = scatter[1][1] + ridge;
    let determinant = a * e - b * c;
    if !(determinant.is_finite() && determinant > 0.0) {
        return None;
    }
    let inverse_d = [
        (e * d[0] - b * d[1]) / determinant,
        (a * d[1] - c * d[0]) / determinant,
    ];
    let value = d[0] * inverse_d[0] + d[1] * inverse_d[1];
    value.is_finite().then_some(value)
}

struct Xorshift {
    state: u64,
}

impl Xorshift {
    fn new(seed: u64) -> Self {
        Self {
            state: seed ^ 0x9e37_79b9_7f4a_7c15 | 1,
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut state = self.state;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.state = state;
        state
    }

    /// Uniform in `0..bound`.
    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next_u64() % bound as u64) as usize
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: u64) -> Xorshift {
        Xorshift::new(seed)
    }

    fn gaussian(rng: &mut Xorshift) -> f64 {
        let u1 = (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        let u2 = (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        (-2.0 * libm::log(u1.max(1e-12))).sqrt() * libm::cos(2.0 * core::f64::consts::PI * u2)
    }

    #[test]
    fn a_silent_firmware_change_is_detected() {
        let mut rng = rng(7);
        let mut samples = Vec::new();
        for index in 0..24 {
            let shift = if index % 2 == 0 { 0.0 } else { 0.8 };
            samples.push(SplitSample {
                group: (index % 2) as u32,
                projection: [shift + 0.2 * gaussian(&mut rng), 0.2 * gaussian(&mut rng)],
                effect_de: if index % 2 == 0 { 0.4 } else { 1.6 },
            });
        }
        let options = SplitOptions {
            seed: 11,
            ..SplitOptions::default()
        };
        let decision = detect_split(&samples, &options).expect("split detected");
        assert!(decision.p_value < 0.001, "p {}", decision.p_value);
        assert!(decision.effect_de >= 1.0, "effect {}", decision.effect_de);
    }

    #[test]
    fn same_version_data_does_not_split() {
        let mut rng = rng(9);
        let mut samples = Vec::new();
        for index in 0..200 {
            samples.push(SplitSample {
                group: (index % 2) as u32,
                projection: [0.3 * gaussian(&mut rng), 0.3 * gaussian(&mut rng)],
                effect_de: 0.4,
            });
        }
        let options = SplitOptions {
            seed: 3,
            ..SplitOptions::default()
        };
        assert!(detect_split(&samples, &options).is_none());
    }

    #[test]
    fn a_significant_p_with_a_small_effect_is_not_a_split() {
        let mut rng = rng(13);
        let mut samples = Vec::new();
        for index in 0..40 {
            let shift = if index % 2 == 0 { 0.0 } else { 0.6 };
            samples.push(SplitSample {
                group: (index % 2) as u32,
                projection: [shift + 0.05 * gaussian(&mut rng), 0.05 * gaussian(&mut rng)],
                effect_de: if index % 2 == 0 { 0.4 } else { 0.8 },
            });
        }
        let options = SplitOptions {
            seed: 5,
            ..SplitOptions::default()
        };
        assert!(detect_split(&samples, &options).is_none());
    }

    #[test]
    fn detection_is_deterministic() {
        let mut rng = rng(17);
        let samples: Vec<SplitSample> = (0..30)
            .map(|index| SplitSample {
                group: (index % 3) as u32,
                projection: [0.4 * gaussian(&mut rng), 0.4 * gaussian(&mut rng)],
                effect_de: 0.5,
            })
            .collect();
        let options = SplitOptions {
            seed: 21,
            ..SplitOptions::default()
        };
        let first = detect_split(&samples, &options);
        let second = detect_split(&samples, &options);
        assert_eq!(first.is_none(), second.is_none());
    }
}
