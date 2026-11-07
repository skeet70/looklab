// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_model::realization::Settings;
use looklab_schema::{Affects, SimKey};
use rand_chacha::ChaCha12Rng;
use rand_chacha::rand_core::{RngCore, SeedableRng};
use std::collections::BTreeSet;

use crate::error::{MatchError, Result};
use crate::linear::{LinearCache, ProbeSubset};
use crate::model::MatchModel;
use crate::settings::Plan;
use crate::target::PreparedTarget;

pub const OPTIMIZER_VERSION: &str = "looklab-match/1";

#[derive(Clone, Debug, PartialEq)]
pub struct MatchOptions {
    pub seed: u64,
    pub beta: f64,
    pub probe_subset: usize,
    pub beam: usize,
    pub perturbation_rounds: usize,
    pub relinearize_threshold: f64,
    pub max_relinearizations: usize,
    pub top_sims: usize,
    pub full_rescore: usize,
    pub top_results: usize,
    pub coordinate_passes: usize,
    pub warm_restarts: usize,
}

impl Default for MatchOptions {
    fn default() -> Self {
        Self {
            seed: 0x5eed_1234,
            beta: 0.25,
            probe_subset: 500,
            beam: 5,
            perturbation_rounds: 3,
            relinearize_threshold: 0.3,
            max_relinearizations: 1,
            top_sims: 3,
            full_rescore: 5,
            top_results: 3,
            coordinate_passes: 4,
            warm_restarts: 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub sim: SimKey,
    pub settings: Settings,
    pub objective: f64,
    pub median_delta_e: f64,
    pub p95_delta_e: f64,
    pub max_delta_e: f64,
    pub sigma_band: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunStats {
    pub sims_considered: usize,
    pub sims_kept: usize,
    pub supported_probes: usize,
    pub probe_subset: usize,
    pub linear_evaluations: usize,
    pub exact_evaluations: usize,
    pub relinearizations: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MatchOutcome {
    pub results: Vec<Suggestion>,
    pub stats: RunStats,
}

struct Entry {
    sim: usize,
    settings: Settings,
    linear_score: f64,
}

pub fn optimize(
    models: &[MatchModel],
    target: &PreparedTarget<'_>,
    plan: &Plan,
    options: &MatchOptions,
) -> Result<MatchOutcome> {
    if models.is_empty() {
        return Err(MatchError::NoSims);
    }
    let mut seen = BTreeSet::new();
    for model in models {
        if model.crm.epoch != plan.epoch {
            return Err(MatchError::WrongEpoch(
                model.sim().to_string(),
                model.crm.epoch,
                plan.epoch,
            ));
        }
        if !seen.insert(model.sim().as_str().to_string()) {
            return Err(MatchError::Schema(format!(
                "duplicate control-response model for {}",
                model.sim()
            )));
        }
    }

    let subset = target.subset(options.probe_subset);
    let coarse = coarse_subset(target, 64);
    let restart_subset = coarse_subset(target, 48);
    let mut stats = RunStats {
        sims_considered: models.len(),
        supported_probes: target.supported.len(),
        probe_subset: subset.indices.len(),
        ..RunStats::default()
    };

    let mut pruned: Vec<(f64, usize)> = Vec::with_capacity(models.len());
    for (index, model) in models.iter().enumerate() {
        let score = prune_sim(model, plan, target, &subset, &coarse, &mut stats)?;
        pruned.push((score, index));
    }
    pruned.sort_by(|left, right| left.0.total_cmp(&right.0));
    pruned.truncate(options.top_sims.max(1));
    stats.sims_kept = pruned.len();

    let mut pool: Vec<Entry> = Vec::new();
    let mut seeds: Vec<(usize, Settings)> = Vec::new();
    let best_sim = pruned.first().map(|entry| entry.1);
    let mut caches: Vec<Option<LinearCache>> = (0..models.len()).map(|_| None).collect();
    for (position, (_score, index)) in pruned.into_iter().enumerate() {
        let cache = LinearCache::new(
            &models[index],
            plan,
            target,
            &plan.default,
            &subset,
            options.beta,
        )?;
        let mut start = plan.default.clone();
        for control in plan.stage_controls() {
            if affects(plan, control, Affects::Exposure) || affects(plan, control, Affects::Wb) {
                sweep(
                    &cache,
                    plan,
                    &mut start,
                    control,
                    &mut stats.linear_evaluations,
                );
            }
        }
        for start_point in [plan.default.clone(), start.clone()] {
            let greedy = greedy_residual(&cache, plan, &start_point);
            seeds.push((index, greedy.clone()));
            let greedy_descended = descend(
                &cache,
                plan,
                &greedy,
                options,
                &mut stats.linear_evaluations,
            );
            stats.linear_evaluations += 1;
            pool.push(Entry {
                sim: index,
                linear_score: cache.score(plan, &greedy_descended),
                settings: greedy_descended.clone(),
            });
            seeds.push((index, greedy_descended));
        }
        let exact_start = if position == 0 {
            exact_descend(
                &cache,
                plan,
                target,
                &coarse,
                &start,
                options,
                &mut stats.exact_evaluations,
            )?
        } else {
            start.clone()
        };
        seeds.push((index, exact_start.clone()));
        let mut restart_rng = ChaCha12Rng::seed_from_u64(
            options
                .seed
                .wrapping_add(0x51ed_2a6b_7c31_0001 ^ index as u64),
        );
        let restarts = if position == 0 {
            options.warm_restarts
        } else {
            0
        };
        for _ in 0..restarts {
            let random = random_settings(plan, &mut restart_rng);
            let result = exact_descend(
                &cache,
                plan,
                target,
                &restart_subset,
                &random,
                options,
                &mut stats.exact_evaluations,
            )?;
            seeds.push((index, result));
        }
        let exact_cache = LinearCache::new(
            &models[index],
            plan,
            target,
            &exact_start,
            &subset,
            options.beta,
        )?;
        let exact_descended = descend(
            &exact_cache,
            plan,
            &exact_start,
            options,
            &mut stats.linear_evaluations,
        );
        stats.linear_evaluations += 1;
        pool.push(Entry {
            sim: index,
            linear_score: exact_cache.score(plan, &exact_descended),
            settings: exact_descended,
        });
        let mut rng = ChaCha12Rng::seed_from_u64(
            options
                .seed
                .wrapping_add((index as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15)),
        );
        let mut beam = beam_descend(&cache, plan, &start, options, &mut stats.linear_evaluations);
        let descended = descend(&cache, plan, &start, options, &mut stats.linear_evaluations);
        let descended_score = {
            stats.linear_evaluations += 1;
            cache.score(plan, &descended)
        };
        beam.push((descended_score, descended));
        beam.sort_by(|left, right| left.0.total_cmp(&right.0));
        beam.truncate(options.beam);
        for (_, candidate) in &beam {
            seeds.push((index, candidate.clone()));
        }
        for round in 0..options.perturbation_rounds {
            let snapshot = beam.clone();
            for (parent_score, parent) in &snapshot {
                for attempt in 0..options.beam {
                    let mutated = mutate(plan, parent, &mut rng, round, attempt);
                    stats.linear_evaluations += 1;
                    let candidate = cache.score(plan, &mutated);
                    if candidate < *parent_score {
                        beam.push((candidate, mutated));
                    }
                }
            }
            beam.sort_by(|left, right| left.0.total_cmp(&right.0));
            beam.dedup_by(|left, right| left.1 == right.1);
            beam.truncate(options.beam);
        }
        for (linear_score, settings) in beam {
            pool.push(Entry {
                sim: index,
                settings,
                linear_score,
            });
        }
        caches[index] = Some(cache);
    }

    let mut relinearized: Vec<(usize, Settings)> = Vec::new();
    for _ in 0..options.max_relinearizations {
        let shortlist = shortlist(&pool, options.full_rescore);
        let mut worst: Option<(f64, usize, Settings)> = None;
        for entry in shortlist {
            if Some(entry.sim) != best_sim {
                continue;
            }
            let Some(cache) = caches[entry.sim].as_ref() else {
                continue;
            };
            let exact = exact_score(cache, target, plan, &subset, &entry.settings, options.beta)?;
            stats.exact_evaluations += 1;
            let linear = cache.predicted_output(plan, &entry.settings);
            let median = median_prediction_difference(&linear, &exact.predictions);
            let exact_median = quantile(&exact.deltas, 0.5);
            if exact_median <= 0.3 {
                continue;
            }
            if median > options.relinearize_threshold
                && !relinearized
                    .iter()
                    .any(|(sim, settings)| *sim == entry.sim && settings == &entry.settings)
                && worst.as_ref().is_none_or(|(value, _, _)| median > *value)
            {
                worst = Some((median, entry.sim, entry.settings.clone()));
            }
        }
        let Some((_, sim, settings)) = worst else {
            break;
        };
        relinearized.push((sim, settings.clone()));
        let cache = LinearCache::new(&models[sim], plan, target, &settings, &subset, options.beta)?;
        let descended = descend(
            &cache,
            plan,
            &settings,
            options,
            &mut stats.linear_evaluations,
        );
        stats.linear_evaluations += 1;
        let score = cache.score(plan, &descended);
        pool.push(Entry {
            sim,
            settings: descended,
            linear_score: score,
        });
        caches[sim] = Some(cache);
        stats.relinearizations += 1;
    }

    let mut candidates: Vec<(usize, Settings)> = shortlist(&pool, options.full_rescore)
        .into_iter()
        .map(|entry| (entry.sim, entry.settings.clone()))
        .collect();
    for seed in seeds {
        if !candidates
            .iter()
            .any(|(sim, settings)| *sim == seed.0 && settings == &seed.1)
        {
            candidates.push(seed);
        }
    }
    let full = target.full_subset();
    let mut scored: Vec<(f64, Suggestion)> = Vec::with_capacity(candidates.len());
    for (sim, settings) in candidates {
        let Some(cache) = caches[sim].as_ref() else {
            continue;
        };
        let exact = exact_score(cache, target, plan, &full, &settings, options.beta)?;
        stats.exact_evaluations += 1;
        let suggestion = Suggestion {
            sim: models[sim].sim().clone(),
            settings,
            objective: exact.objective,
            median_delta_e: quantile(&exact.deltas, 0.5),
            p95_delta_e: quantile(&exact.deltas, 0.95),
            max_delta_e: exact.deltas.last().copied().unwrap_or(0.0),
            sigma_band: exact.band(),
        };
        scored.push((suggestion.objective, suggestion));
    }
    scored.sort_by(|left, right| left.0.total_cmp(&right.0));
    scored.truncate(options.top_results.max(1));
    Ok(MatchOutcome {
        results: scored
            .into_iter()
            .map(|(_, suggestion)| suggestion)
            .collect(),
        stats,
    })
}

fn affects(plan: &Plan, control: usize, affect: Affects) -> bool {
    plan.controls[control].affects.contains(&affect)
}

fn exact_probe_score(
    model: &MatchModel,
    target: &PreparedTarget<'_>,
    settings: &Settings,
    subset: &ProbeSubset,
) -> Result<f64> {
    let realization = model.realize(settings)?;
    let mut total = 0.0_f64;
    for (index, weight) in subset.indices.iter().zip(&subset.weights) {
        let probe = &target.probes.probes[*index];
        let prediction = model
            .input_ucs(&realization, probe.xyz, [0.0, 0.0], 0.0)
            .to_array();
        let expected = target.target[*index].to_array();
        let delta = [
            prediction[0] - expected[0],
            prediction[1] - expected[1],
            prediction[2] - expected[2],
        ];
        total += weight * (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]);
    }
    Ok(total)
}

/// The prune stage of docs/04 §3 step 1: every sim at its defaults, with the
/// best exposure and white-balance values found by 1-D sweeps on a coarse subset.
fn prune_sim(
    model: &MatchModel,
    plan: &Plan,
    target: &PreparedTarget<'_>,
    subset: &ProbeSubset,
    coarse: &ProbeSubset,
    stats: &mut RunStats,
) -> Result<f64> {
    let mut settings = plan.default.clone();
    for control in plan.stage_controls() {
        if !(affects(plan, control, Affects::Exposure) || affects(plan, control, Affects::Wb)) {
            continue;
        }
        let mut best_value = plan.value_index(control, &settings);
        stats.exact_evaluations += 1;
        let mut best_score = exact_probe_score(model, target, &settings, coarse)?;
        for value in 0..plan.domains[control].values.len() {
            if value == best_value {
                continue;
            }
            let candidate = plan.candidate(&settings, control, value);
            stats.exact_evaluations += 1;
            let score = exact_probe_score(model, target, &candidate, coarse)?;
            if score < best_score {
                best_score = score;
                best_value = value;
            }
        }
        plan.set(control, best_value, &mut settings);
    }
    stats.exact_evaluations += 1;
    exact_probe_score(model, target, &settings, subset)
}

fn sweep(
    cache: &LinearCache,
    plan: &Plan,
    settings: &mut Settings,
    control: usize,
    evaluations: &mut usize,
) {
    let current = plan.value_index(control, settings);
    let mut best_value = current;
    let mut best_score = {
        *evaluations += 1;
        cache.score(plan, settings)
    };
    for value in 0..plan.domains[control].values.len() {
        if value == current {
            continue;
        }
        let candidate = plan.candidate(settings, control, value);
        *evaluations += 1;
        let score = cache.score(plan, &candidate);
        if score < best_score {
            best_score = score;
            best_value = value;
        }
    }
    plan.set(control, best_value, settings);
}

fn random_settings(plan: &Plan, rng: &mut ChaCha12Rng) -> Settings {
    let mut settings = plan.default.clone();
    for control in plan.stage_controls() {
        let size = plan.domains[control].values.len();
        if size > 1 && !rng.next_u32().is_multiple_of(4) {
            let value = (rng.next_u32() as usize) % size;
            plan.set(control, value, &mut settings);
        }
    }
    settings
}

fn greedy_residual(cache: &LinearCache, plan: &Plan, start: &Settings) -> Settings {
    let mut current = start.clone();
    let mut unlocked: Vec<usize> = plan.plan_controls.clone();
    loop {
        let predicted = cache.predicted_output(plan, &current);
        let mut current_score = 0.0_f64;
        for (probe, prediction) in predicted.iter().enumerate() {
            let mut delta = [0.0_f64; 3];
            for (channel, value) in delta.iter_mut().enumerate() {
                *value = cache.target[probe][channel] - prediction[channel];
            }
            current_score += cache.weights[probe]
                * (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]);
        }
        let mut best: Option<(f64, usize, usize)> = None;
        for &control in &unlocked {
            let current_value = plan.value_index(control, &current);
            for value in 0..plan.domains[control].values.len() {
                if value == current_value {
                    continue;
                }
                let mut score = 0.0_f64;
                for (probe, prediction) in predicted.iter().enumerate() {
                    let old = cache.contributions[probe][control][current_value];
                    let new = cache.contributions[probe][control][value];
                    let mut delta = [0.0_f64; 3];
                    for (channel, value) in delta.iter_mut().enumerate() {
                        let updated = prediction[channel] - old[channel] + new[channel];
                        *value = cache.target[probe][channel] - updated;
                    }
                    score += cache.weights[probe]
                        * (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]);
                }
                if best.is_none_or(|(value, _, _)| score < value) {
                    best = Some((score, control, value));
                }
            }
        }
        let Some((score, control, value)) = best else {
            break;
        };
        if score + 1e-12 >= current_score {
            break;
        }
        plan.set(control, value, &mut current);
        unlocked.retain(|entry| *entry != control);
        if unlocked.is_empty() {
            break;
        }
    }
    current
}

fn beam_descend(
    cache: &LinearCache,
    plan: &Plan,
    start: &Settings,
    options: &MatchOptions,
    evaluations: &mut usize,
) -> Vec<(f64, Settings)> {
    let width = options.beam.max(1);
    let mut beam = vec![(
        {
            *evaluations += 1;
            cache.score(plan, start)
        },
        start.clone(),
    )];
    for stage in &plan.stages {
        for _ in 0..options.coordinate_passes {
            let mut progressed = false;
            for &control in stage {
                let mut next: Vec<(f64, Settings)> = Vec::new();
                for (_, base) in &beam {
                    for value in 0..plan.domains[control].values.len() {
                        let candidate = plan.candidate(base, control, value);
                        *evaluations += 1;
                        next.push((cache.score(plan, &candidate), candidate));
                    }
                }
                next.sort_by(|left, right| left.0.total_cmp(&right.0));
                next.dedup_by(|left, right| left.1 == right.1);
                next.truncate(width);
                if next.first().map(|entry| entry.0) < beam.first().map(|entry| entry.0) {
                    progressed = true;
                }
                beam = next;
            }
            if !progressed {
                break;
            }
        }
    }
    beam
}

fn descend(
    cache: &LinearCache,
    plan: &Plan,
    start: &Settings,
    options: &MatchOptions,
    evaluations: &mut usize,
) -> Settings {
    let mut current = start.clone();
    let mut current_score = {
        *evaluations += 1;
        cache.score(plan, &current)
    };
    for stage in &plan.stages {
        for _ in 0..options.coordinate_passes {
            let mut improved = false;
            for &control in stage {
                let current_value = plan.value_index(control, &current);
                let mut best_value = current_value;
                let mut best_score = current_score;
                for value in 0..plan.domains[control].values.len() {
                    if value == current_value {
                        continue;
                    }
                    let candidate = plan.candidate(&current, control, value);
                    *evaluations += 1;
                    let score = cache.score(plan, &candidate);
                    if score + 1e-12 < best_score {
                        best_score = score;
                        best_value = value;
                    }
                }
                if best_value != current_value {
                    plan.set(control, best_value, &mut current);
                    current_score = best_score;
                    improved = true;
                }
            }
            if !improved {
                break;
            }
        }
    }
    current
}

fn coarse_subset(target: &PreparedTarget<'_>, count: usize) -> ProbeSubset {
    let count = count.max(1).min(target.supported.len());
    let stride = target.supported.len() as f64 / count as f64;
    let mut indices = Vec::with_capacity(count);
    for slot in 0..count {
        let position = (((slot as f64 + 0.5) * stride) as usize).min(target.supported.len() - 1);
        let index = target.supported[position];
        if !indices.contains(&index) {
            indices.push(index);
        }
    }
    let weights = indices.iter().map(|index| target.weight[*index]).collect();
    let covariance_weights = indices
        .iter()
        .map(|index| target.covariance_weight[*index])
        .collect();
    ProbeSubset {
        indices,
        weights,
        covariance_weights,
    }
}

fn exact_objective(
    cache: &LinearCache,
    plan: &Plan,
    target: &PreparedTarget<'_>,
    subset: &ProbeSubset,
    settings: &Settings,
    beta: f64,
    scratch: &mut crate::linear::RealizationScratch,
) -> Result<f64> {
    if cache.model.groups.is_empty() {
        let (gains, exposure) = cache.realize_into(plan, settings, scratch)?;
        let mut total = 0.0_f64;
        for (slot, index) in subset.indices.iter().enumerate() {
            let probe = &target.probes.probes[*index];
            let input = cache.input_xyz(probe.xyz, gains, exposure);
            let prediction = scratch.look.eval(input);
            let expected = target.target[*index];
            let delta = [
                prediction.j - expected.j,
                prediction.a - expected.a,
                prediction.b - expected.b,
            ];
            total += subset.weights[slot]
                * (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]);
        }
        return Ok(total);
    }
    let realization = cache.realization(plan, settings)?;
    let mut total = 0.0_f64;
    for (slot, index) in subset.indices.iter().enumerate() {
        let probe = &target.probes.probes[*index];
        let prediction = cache
            .model
            .input_ucs(&realization, probe.xyz, [0.0, 0.0], 0.0);
        let expected = target.target[*index];
        let delta = [
            prediction.j - expected.j,
            prediction.a - expected.a,
            prediction.b - expected.b,
        ];
        let squared = delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2];
        let trace = crate::covariance::trace_with(&cache.model, &realization, settings, probe.xyz)?;
        total += subset.weights[slot] * squared + beta * subset.covariance_weights[slot] * trace;
    }
    Ok(total)
}

fn exact_descend(
    cache: &LinearCache,
    plan: &Plan,
    target: &PreparedTarget<'_>,
    subset: &ProbeSubset,
    start: &Settings,
    options: &MatchOptions,
    evaluations: &mut usize,
) -> Result<Settings> {
    let mut scratch = cache.scratch();
    let mut current = start.clone();
    let mut current_score = {
        *evaluations += 1;
        exact_objective(
            cache,
            plan,
            target,
            subset,
            &current,
            options.beta,
            &mut scratch,
        )?
    };
    for stage in &plan.stages {
        for _ in 0..options.coordinate_passes.min(1) {
            let mut improved = false;
            for &control in stage {
                let current_value = plan.value_index(control, &current);
                let mut best_value = current_value;
                let mut best_score = current_score;
                for value in 0..plan.domains[control].values.len() {
                    if value == current_value {
                        continue;
                    }
                    let candidate = plan.candidate(&current, control, value);
                    *evaluations += 1;
                    let score = exact_objective(
                        cache,
                        plan,
                        target,
                        subset,
                        &candidate,
                        options.beta,
                        &mut scratch,
                    )?;
                    if score + 1e-12 < best_score {
                        best_score = score;
                        best_value = value;
                    }
                }
                if best_value != current_value {
                    plan.set(control, best_value, &mut current);
                    current_score = best_score;
                    improved = true;
                }
            }
            if !improved {
                break;
            }
        }
    }
    Ok(current)
}

fn mutate(
    plan: &Plan,
    settings: &Settings,
    rng: &mut ChaCha12Rng,
    round: usize,
    attempt: usize,
) -> Settings {
    let mut controls: Vec<usize> = plan
        .stage_controls()
        .filter(|control| plan.domains[*control].values.len() > 1)
        .collect();
    for index in (1..controls.len()).rev() {
        let swap = (rng.next_u32() as usize) % (index + 1);
        controls.swap(index, swap);
    }
    let count = 2 + ((rng.next_u32() as usize + round + attempt) % 2);
    let mut out = settings.clone();
    for &control in controls.iter().take(count.min(controls.len())) {
        let current = plan.value_index(control, &out);
        let size = plan.domains[control].values.len();
        let magnitude = 1 + (rng.next_u32() as usize % 2);
        let sign = if rng.next_u32() & 1 == 0 { 1 } else { -1 };
        let target = current as isize + sign * magnitude as isize;
        let target = target.clamp(0, size as isize - 1) as usize;
        let target = if target == current {
            if sign > 0 {
                current.saturating_sub(magnitude)
            } else {
                (current + magnitude).min(size - 1)
            }
        } else {
            target
        };
        plan.set(control, target, &mut out);
    }
    out
}

fn shortlist(pool: &[Entry], count: usize) -> Vec<&Entry> {
    let mut sorted: Vec<&Entry> = pool.iter().collect();
    sorted.sort_by(|left, right| left.linear_score.total_cmp(&right.linear_score));
    let mut seen: Vec<(usize, &Settings)> = Vec::new();
    let mut out = Vec::with_capacity(count);
    for entry in sorted {
        if seen
            .iter()
            .any(|(sim, settings)| *sim == entry.sim && *settings == &entry.settings)
        {
            continue;
        }
        seen.push((entry.sim, &entry.settings));
        out.push(entry);
        if out.len() == count {
            break;
        }
    }
    out
}

struct ExactScore {
    objective: f64,
    deltas: Vec<f64>,
    predictions: Vec<[f64; 3]>,
    traces: Vec<f64>,
}

impl ExactScore {
    fn band(&self) -> f64 {
        if self.traces.is_empty() {
            0.0
        } else {
            self.traces
                .iter()
                .map(|trace| trace.max(0.0).sqrt())
                .sum::<f64>()
                / self.traces.len() as f64
        }
    }
}

fn exact_score(
    cache: &LinearCache,
    target: &PreparedTarget<'_>,
    plan: &Plan,
    subset: &ProbeSubset,
    settings: &Settings,
    beta: f64,
) -> Result<ExactScore> {
    let mut scratch = cache.scratch();
    let mut objective = 0.0_f64;
    let mut deltas = Vec::with_capacity(subset.indices.len());
    let mut predictions = Vec::with_capacity(subset.indices.len());
    let mut traces = Vec::with_capacity(subset.indices.len());
    let use_scratch = cache.model.groups.is_empty();
    let realization = if use_scratch {
        None
    } else {
        Some(cache.realization(plan, settings)?)
    };
    let (gains, exposure) = if use_scratch {
        cache.realize_into(plan, settings, &mut scratch)?
    } else {
        ([0.0; 3], 0.0)
    };
    for (slot, index) in subset.indices.iter().enumerate() {
        let probe = &target.probes.probes[*index];
        let prediction = if use_scratch {
            scratch
                .look
                .eval(cache.input_xyz(probe.xyz, gains, exposure))
        } else {
            cache.model.input_ucs(
                realization.as_ref().expect("realization"),
                probe.xyz,
                [0.0, 0.0],
                0.0,
            )
        };
        let expected = target.target[*index];
        let delta = [
            prediction.j - expected.j,
            prediction.a - expected.a,
            prediction.b - expected.b,
        ];
        let delta_e = (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]).sqrt();
        let trace = if use_scratch {
            0.0
        } else {
            crate::covariance::trace_with(
                &cache.model,
                realization.as_ref().expect("realization"),
                settings,
                probe.xyz,
            )?
        };
        objective += subset.weights[slot] * delta_e * delta_e
            + beta * subset.covariance_weights[slot] * trace;
        deltas.push(delta_e);
        predictions.push(prediction.to_array());
        traces.push(trace);
    }
    deltas.sort_by(f64::total_cmp);
    Ok(ExactScore {
        objective,
        deltas,
        predictions,
        traces,
    })
}

fn median_prediction_difference(linear: &[[f64; 3]], exact: &[[f64; 3]]) -> f64 {
    let mut differences: Vec<f64> = linear
        .iter()
        .zip(exact.iter())
        .map(|(a, b)| {
            let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
            (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
        })
        .collect();
    differences.sort_by(f64::total_cmp);
    quantile(&differences, 0.5)
}

pub(crate) fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let position = q.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    if lower == upper {
        sorted[lower]
    } else {
        let fraction = position - lower as f64;
        sorted[lower] * (1.0 - fraction) + sorted[upper] * fraction
    }
}
