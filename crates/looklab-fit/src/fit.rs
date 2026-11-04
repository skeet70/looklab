// SPDX-License-Identifier: AGPL-3.0-or-later
//! Staged Gauss-Newton/Levenberg-Marquardt with block solves and per-stage GCV.

use looklab_model::basis::{RESIDUAL_COEFS, S_COEFS, TONE_COEFS};

use crate::basis;
use crate::density;
use crate::layout::{BlockKind, Layout, Params};
use crate::outlier;
use crate::problem::{ActiveColumns, BlockAcc, PassOutput, Problem, ResidualSample};
use crate::solve::{factor_in_place, select_lambda, select_lambda_diagonal, solve_in_place};
use crate::types::{
    Diagnostics, ExposureOffset, FitImage, FitOptions, FitResult, ImageNuisances, Realization,
    Result, StageLambdas, Verdict,
};
use crate::uncertainty;

const RESIDUAL_NODES: usize = RESIDUAL_COEFS / 3;
const RESIDUAL_GRID: usize = 17;

struct Stage {
    blocks: &'static [BlockKind],
    active: ActiveColumns,
    neutral: bool,
    sweeps: usize,
    gcv: bool,
}

const STAGES: [Stage; 5] = [
    Stage {
        blocks: &[BlockKind::Tone],
        active: ActiveColumns {
            image: true,
            mode: true,
            wb: true,
            clip: false,
        },
        neutral: true,
        sweeps: 6,
        gcv: true,
    },
    Stage {
        blocks: &[BlockKind::Tone, BlockKind::Tint],
        active: ActiveColumns::ALL,
        neutral: false,
        sweeps: 2,
        gcv: true,
    },
    Stage {
        blocks: &[
            BlockKind::Tone,
            BlockKind::Tint,
            BlockKind::G,
            BlockKind::R,
            BlockKind::Lambda,
        ],
        active: ActiveColumns::ALL,
        neutral: false,
        sweeps: 4,
        gcv: true,
    },
    Stage {
        blocks: &[BlockKind::Tone, BlockKind::Residual],
        active: ActiveColumns::ALL,
        neutral: false,
        sweeps: 3,
        gcv: true,
    },
    Stage {
        blocks: &[
            BlockKind::Tone,
            BlockKind::Tint,
            BlockKind::G,
            BlockKind::R,
            BlockKind::Lambda,
            BlockKind::Residual,
        ],
        active: ActiveColumns::ALL,
        neutral: false,
        sweeps: 2,
        gcv: false,
    },
];

pub(crate) fn run(images: &[FitImage], options: &FitOptions) -> Result<FitResult> {
    let problem = Problem::new(images, options)?;
    let layout = problem.layout.clone();
    let mut params = initial_params(&problem);
    let mut lambdas = StageLambdas::default();
    let mut evaluations = 0usize;

    for stage in &STAGES {
        let kinds: Vec<BlockKind> = stage
            .blocks
            .iter()
            .copied()
            .filter(|kind| options.fit_residual || *kind != BlockKind::Residual)
            .collect();
        let mut blocks: Vec<BlockAcc> = kinds
            .iter()
            .map(|kind| make_block(*kind, &layout, stage.active))
            .collect();
        let penalties: Vec<Vec<f64>> = kinds
            .iter()
            .zip(blocks.iter())
            .map(|(kind, block)| embed(penalty_for(*kind), block.len()))
            .collect();
        let mut damping: f64 = 1e-3;
        let mut previous: Option<(f64, Params)> = None;
        let mut gcv_done = false;
        for _ in 0..stage.sweeps.min(options.max_sweeps.max(1)) {
            for block in &mut blocks {
                block.reset();
            }
            let pass = problem.pass(
                &params,
                stage.active,
                &mut blocks,
                false,
                false,
                stage.neutral,
            );
            evaluations += 1;
            let cost =
                pass.cost + total_cost(&problem, &params, &blocks, &penalties, &lambdas, options);
            if let Some((previous_cost, previous_params)) = &previous
                && cost > previous_cost * (1.0 + 1e-12)
            {
                params = previous_params.clone();
                damping = (damping * 10.0).min(1e6_f64);
                if damping >= 1e6 {
                    break;
                }
                continue;
            }
            previous = Some((cost, params.clone()));

            if stage.gcv && !gcv_done && options.run_gcv {
                choose_lambdas(&problem, &pass, &blocks, &penalties, &mut lambdas, options);
                gcv_done = true;
            }

            let mut step = vec![0.0_f64; layout.total];
            for (index, block) in blocks.iter().enumerate() {
                if block.kind == BlockKind::Residual {
                    continue;
                }
                solve_block(
                    block,
                    &mut step,
                    &penalties[index],
                    &params,
                    &lambdas,
                    damping,
                );
            }
            if kinds.contains(&BlockKind::Residual) {
                let lambda = lam(&lambdas, BlockKind::Residual);
                let delta = residual_step(
                    &pass.residual_samples,
                    &params.theta[layout.residual.clone()],
                    lambda,
                    damping,
                );
                for (index, value) in delta.iter().enumerate() {
                    step[layout.residual.start + index] = *value;
                }
            }
            apply_step(&mut params, &step);
            if kinds.contains(&BlockKind::Residual) {
                pin_residual(&mut params, &layout, &pass.residual_samples);
            }
            update_sigma(&problem, &mut params, &pass);
            damping = (damping * 0.3).max(1e-6);
        }
    }

    let mut blocks: Vec<BlockAcc> = STAGES[4]
        .blocks
        .iter()
        .map(|kind| make_block(*kind, &layout, ActiveColumns::ALL))
        .collect();
    for block in &mut blocks {
        block.reset();
    }
    let penalties: Vec<Vec<f64>> = STAGES[4]
        .blocks
        .iter()
        .zip(blocks.iter())
        .map(|(kind, block)| embed(penalty_for(*kind), block.len()))
        .collect();
    let final_pass = problem.pass(&params, ActiveColumns::ALL, &mut blocks, true, true, false);
    evaluations += 1;
    // The exposure/body gauge is a definition (docs/02 §2.1), not an estimate, so
    // the Look's covariance is conditional on the exposure nuisances.
    let uncertainty_columns = ActiveColumns {
        image: false,
        mode: false,
        wb: true,
        clip: true,
    };
    let mut uncertainty_blocks = vec![make_look_block(&layout, uncertainty_columns)];
    let uncertainty_pass = problem.pass(
        &params,
        uncertainty_columns,
        &mut uncertainty_blocks,
        false,
        true,
        false,
    );
    evaluations += 1;
    let _ = &final_pass.per_image_gradient;
    let (prior_cost, dense_reg) =
        prior_and_reg(&problem, &params, &blocks, &penalties, &lambdas, options);
    let _ = dense_reg;

    let look = params.look();
    let clip_k = params.clip_k();
    let mut images_out = Vec::with_capacity(problem.images.len());
    for (index, image) in images.iter().enumerate() {
        let nuisance = params.image(index);
        images_out.push(ImageNuisances {
            exposure: nuisance[0],
            wb_log_r: nuisance[1],
            wb_log_b: nuisance[2],
            exposure_mode: image.exposure_mode,
            contributor: image.contributor,
            weight: problem.images[index].trust,
        });
    }
    let sigma_v = problem
        .contributors
        .iter()
        .enumerate()
        .map(|(index, contributor)| (*contributor, params.sigma(index)))
        .collect::<Vec<_>>();
    let exposure_offsets = problem
        .modes
        .iter()
        .enumerate()
        .map(|(index, mode)| ExposureOffset {
            mode: *mode,
            offset: params.mode_offset(index),
        })
        .collect::<Vec<_>>();

    let density = density::build(&problem, &params, options.density_floor_fraction);
    let image_stats = outlier::score(&problem, &final_pass, options);
    let uncertainty_penalty = combined_look_penalty(&layout, &lambdas, uncertainty_blocks[0].len());
    let probe_result = uncertainty::probe_uncertainty(
        &problem,
        &params,
        &look,
        &uncertainty_blocks[0],
        &uncertainty_penalty,
        &lambdas,
        &uncertainty_pass.per_image_gradient,
        &density,
        &uncertainty_pass.residual_samples,
        options,
    )?;

    let mut diagnostics = Diagnostics {
        sweeps: 0,
        evaluations,
        final_cost: final_pass.cost + prior_cost,
        sigma_v: sigma_v.clone(),
        gcv: lambdas,
        hessian_samples: problem.hessian.len(),
        objective_samples: problem.samples.len(),
        quarantined: image_stats
            .iter()
            .filter(|stat| stat.verdict == Verdict::Quarantine)
            .count(),
        down_weighted: image_stats
            .iter()
            .filter(|stat| stat.verdict == Verdict::DownWeight)
            .count(),
    };
    diagnostics.sweeps = evaluations;

    Ok(FitResult {
        look,
        images: images_out,
        realization: Realization {
            clip_k,
            exposure_offsets,
        },
        lambdas,
        probes: probe_result,
        density,
        image_stats,
        diagnostics,
    })
}

fn initial_params(problem: &Problem) -> Params {
    let mut params = Params::initial(problem.layout.clone(), problem.contributors.len());
    for (index, image) in problem.images.iter().enumerate() {
        let range = params.layout.image(index);
        params.theta[range.start] = image.exposure_hint.clamp(-8.0, 8.0);
    }
    params
}

fn make_block(kind: BlockKind, layout: &Layout, active: ActiveColumns) -> BlockAcc {
    let params: Vec<usize> = match kind {
        BlockKind::Tone => {
            let mut out = vec![layout.tone_c0];
            out.extend(layout.tone_beta.clone());
            for index in 0..problem_image_count(layout) {
                let range = layout.image(index);
                if active.image {
                    out.push(range.start);
                }
                if active.wb {
                    out.push(range.start + 1);
                    out.push(range.start + 2);
                }
            }
            if active.mode {
                out.extend(layout.modes.clone());
            }
            if active.clip {
                out.push(layout.clip);
            }
            out
        }
        BlockKind::Tint => {
            let mut out: Vec<usize> = layout.tint_a.clone().collect();
            out.extend(layout.tint_b.clone());
            out
        }
        BlockKind::G => layout.g.clone().collect(),
        BlockKind::R => layout.r.clone().collect(),
        BlockKind::Lambda => layout.lambda.clone().collect(),
        BlockKind::Residual => Vec::new(),
    };
    BlockAcc::new(kind, params, layout.total)
}

/// One dense block over every look coefficient the covariance needs, plus the WB nuisances and clip.
fn make_look_block(layout: &Layout, active: ActiveColumns) -> BlockAcc {
    let mut params = Vec::with_capacity(2300);
    params.push(layout.tone_c0);
    params.extend(layout.tone_beta.clone());
    params.extend(layout.tint_a.clone());
    params.extend(layout.tint_b.clone());
    params.extend(layout.g.clone());
    params.extend(layout.r.clone());
    params.extend(layout.lambda.clone());
    for index in 0..problem_image_count(layout) {
        let range = layout.image(index);
        if active.image {
            params.push(range.start);
        }
        if active.wb {
            params.push(range.start + 1);
            params.push(range.start + 2);
        }
    }
    if active.clip {
        params.push(layout.clip);
    }
    BlockAcc::new(BlockKind::Tone, params, layout.total)
}

/// Block-diagonal penalty over the joint block, with each section's λ folded in.
fn combined_look_penalty(layout: &Layout, lambdas: &StageLambdas, n: usize) -> Vec<f64> {
    let mut out = vec![0.0_f64; n * n];
    let tone = penalty_for(BlockKind::Tone);
    place(&mut out, n, &tone, 1 + TONE_COEFS, 0, lambdas.tone);
    let tint = penalty_for(BlockKind::Tint);
    place(&mut out, n, &tint, 24, layout.tint_a.start, lambdas.tint);
    let grl = penalty_for(BlockKind::G);
    place(
        &mut out,
        n,
        &grl,
        crate::layout::GRL_COEFS,
        layout.g.start,
        lambdas.grl,
    );
    place(
        &mut out,
        n,
        &grl,
        crate::layout::GRL_COEFS,
        layout.r.start,
        lambdas.grl,
    );
    place(
        &mut out,
        n,
        &grl,
        crate::layout::GRL_COEFS,
        layout.lambda.start,
        lambdas.grl,
    );
    out
}

fn place(target: &mut [f64], n: usize, block: &[f64], m: usize, offset: usize, scale: f64) {
    for i in 0..m {
        for j in 0..m {
            target[(offset + i) * n + offset + j] += scale * block[i * m + j];
        }
    }
}

fn problem_image_count(layout: &Layout) -> usize {
    layout.images.len() / crate::layout::NUISANCE_PER_IMAGE
}

fn penalty_for(kind: BlockKind) -> Vec<f64> {
    match kind {
        BlockKind::Tone => {
            let n = 1 + TONE_COEFS;
            let mut s = vec![0.0_f64; n * n];
            for row in basis::second_difference(TONE_COEFS, false) {
                let indices: Vec<(usize, f64)> = row
                    .iter()
                    .enumerate()
                    .filter(|(_, value)| **value != 0.0)
                    .map(|(index, value)| (1 + index, *value))
                    .collect();
                accumulate_row(&mut s, n, &indices);
            }
            for index in 0..n {
                s[index * n + index] += 1.0;
            }
            s
        }
        BlockKind::Tint => {
            let n = 24;
            let mut s = vec![0.0_f64; n * n];
            for offset in [0usize, 12] {
                for row in basis::second_difference(12, false) {
                    let indices: Vec<(usize, f64)> = row
                        .iter()
                        .enumerate()
                        .filter(|(_, value)| **value != 0.0)
                        .map(|(index, value)| (offset + index, *value))
                        .collect();
                    accumulate_row(&mut s, n, &indices);
                }
            }
            for index in 0..n {
                s[index * n + index] += 1.0;
            }
            s
        }
        BlockKind::G | BlockKind::R | BlockKind::Lambda => {
            let n = crate::layout::GRL_COEFS;
            let mut s = vec![0.0_f64; n * n];
            for row in basis::second_difference(S_COEFS, false) {
                let indices: Vec<(usize, f64)> = row
                    .iter()
                    .enumerate()
                    .filter(|(_, value)| **value != 0.0)
                    .map(|(index, value)| (index, *value))
                    .collect();
                accumulate_row(&mut s, n, &indices);
            }
            for hue_row in basis::second_difference(24, true) {
                for s_index in 0..S_COEFS {
                    for channel in 0..3 {
                        let indices: Vec<(usize, f64)> = hue_row
                            .iter()
                            .enumerate()
                            .filter(|(_, value)| **value != 0.0)
                            .map(|(hue, value)| {
                                (S_COEFS + (hue * S_COEFS + s_index) * 3 + channel, *value)
                            })
                            .collect();
                        accumulate_row(&mut s, n, &indices);
                    }
                }
            }
            for s_row in basis::second_difference(S_COEFS, false) {
                for hue in 0..24 {
                    for channel in 0..3 {
                        let indices: Vec<(usize, f64)> = s_row
                            .iter()
                            .enumerate()
                            .filter(|(_, value)| **value != 0.0)
                            .map(|(s_index, value)| {
                                (S_COEFS + (hue * S_COEFS + s_index) * 3 + channel, *value)
                            })
                            .collect();
                        accumulate_row(&mut s, n, &indices);
                    }
                }
            }
            for c_row in basis::second_difference(3, false) {
                for hue in 0..24 {
                    for s_index in 0..S_COEFS {
                        let indices: Vec<(usize, f64)> = c_row
                            .iter()
                            .enumerate()
                            .filter(|(_, value)| **value != 0.0)
                            .map(|(channel, value)| {
                                (S_COEFS + (hue * S_COEFS + s_index) * 3 + channel, *value)
                            })
                            .collect();
                        accumulate_row(&mut s, n, &indices);
                    }
                }
            }
            for index in 0..n {
                s[index * n + index] += 1.0;
            }
            s
        }
        BlockKind::Residual => Vec::new(),
    }
}

/// Embeds a square penalty into a larger block as its leading submatrix.
fn embed(penalty: Vec<f64>, n: usize) -> Vec<f64> {
    let m = (penalty.len() as f64).sqrt() as usize;
    if m == n {
        return penalty;
    }
    let mut out = vec![0.0_f64; n * n];
    for i in 0..m {
        for j in 0..m {
            out[i * n + j] = penalty[i * m + j];
        }
    }
    out
}

fn accumulate_row(target: &mut [f64], n: usize, indices: &[(usize, f64)]) {
    for (i, a) in indices {
        for (j, b) in indices {
            target[i * n + j] += a * b;
        }
    }
}

fn lam(lambdas: &StageLambdas, kind: BlockKind) -> f64 {
    match kind {
        BlockKind::Tone => lambdas.tone,
        BlockKind::Tint => lambdas.tint,
        BlockKind::G | BlockKind::R | BlockKind::Lambda => lambdas.grl,
        BlockKind::Residual => lambdas.residual,
    }
}

fn set_lam(lambdas: &mut StageLambdas, kind: BlockKind, value: f64) {
    match kind {
        BlockKind::Tone => lambdas.tone = value,
        BlockKind::Tint => lambdas.tint = value,
        BlockKind::G | BlockKind::R | BlockKind::Lambda => lambdas.grl = value,
        BlockKind::Residual => lambdas.residual = value,
    }
}

fn penalty_value(theta: &[f64], params: &[usize], penalty: &[f64]) -> f64 {
    let n = params.len();
    let mut value = 0.0;
    for i in 0..n {
        let a = theta[params[i]];
        if a == 0.0 {
            continue;
        }
        let mut row = 0.0;
        for j in 0..n {
            row += penalty[i * n + j] * theta[params[j]];
        }
        value += a * row;
    }
    value
}

fn prior_cost(problem: &Problem, params: &Params, options: &FitOptions) -> f64 {
    let mut cost = 0.0;
    let exposure = 1.0 / (options.exposure_prior_sd * options.exposure_prior_sd);
    let wb = 1.0 / (options.wb_prior_sd * options.wb_prior_sd);
    for (index, image) in problem.images.iter().enumerate() {
        let nuisance = params.image(index);
        let delta = nuisance[0] - image.exposure_hint;
        cost += 0.5 * delta * delta * exposure;
        cost += 0.5 * nuisance[1] * nuisance[1] * wb;
        cost += 0.5 * nuisance[2] * nuisance[2] * wb;
    }
    let mode = 1.0 / (options.exposure_mode_prior_sd * options.exposure_mode_prior_sd);
    for index in 0..problem.modes.len() {
        let offset = params.mode_offset(index);
        cost += 0.5 * offset * offset * mode;
    }
    let clip = params.clip_k();
    let (mean, sd) = options.clip_prior;
    cost += 0.5 * (clip - mean) * (clip - mean) / (sd * sd);
    for index in 0..problem.contributors.len() {
        let t = libm::log(params.sigma(index).max(1e-6));
        let (a, b) = options.sigma_prior;
        cost += 2.0 * a * t + b * libm::exp(-2.0 * t);
    }
    cost
}

fn total_cost(
    problem: &Problem,
    params: &Params,
    blocks: &[BlockAcc],
    penalties: &[Vec<f64>],
    lambdas: &StageLambdas,
    options: &FitOptions,
) -> f64 {
    let mut cost = prior_cost(problem, params, options);
    for (index, block) in blocks.iter().enumerate() {
        if block.kind == BlockKind::Residual {
            cost += lam(lambdas, BlockKind::Residual)
                * residual_penalty_value(&params.theta[problem.layout.residual.clone()]);
            continue;
        }
        cost += lam(lambdas, block.kind)
            * penalty_value(&params.theta, &block.params, &penalties[index]);
    }
    cost
}

fn prior_and_reg(
    problem: &Problem,
    params: &Params,
    blocks: &[BlockAcc],
    penalties: &[Vec<f64>],
    lambdas: &StageLambdas,
    options: &FitOptions,
) -> (f64, f64) {
    let mut reg = 0.0;
    for (index, block) in blocks.iter().enumerate() {
        if block.kind == BlockKind::Residual {
            continue;
        }
        reg += lam(lambdas, block.kind)
            * penalty_value(&params.theta, &block.params, &penalties[index]);
    }
    (prior_cost(problem, params, options) + reg, reg)
}

fn choose_lambdas(
    problem: &Problem,
    pass: &PassOutput,
    blocks: &[BlockAcc],
    penalties: &[Vec<f64>],
    lambdas: &mut StageLambdas,
    options: &FitOptions,
) {
    let observations = 3.0 * problem.hessian.len().max(1) as f64;
    for (index, block) in blocks.iter().enumerate() {
        if lam(lambdas, block.kind) != 0.0 {
            continue;
        }
        let rhs: Vec<f64> = block.gradient.iter().map(|value| -value).collect();
        let choice = if block.kind == BlockKind::Residual {
            let diagonal = residual_diagonal(&pass.residual_samples);
            let penalty_diagonal = residual_penalty_diagonal();
            let (residual_rhs, rss) = residual_gradient(&pass.residual_samples);
            select_lambda_diagonal(
                &diagonal,
                &penalty_diagonal,
                &residual_rhs,
                rss.max(1e-12),
                observations,
            )
        } else {
            select_lambda(
                &block.hessian,
                &penalties[index],
                &rhs,
                pass.rss.max(1e-12),
                block.len(),
                observations,
                4,
                options.seed,
            )
        };
        set_lam(lambdas, block.kind, choice.lambda.max(1e-12));
    }
}

fn solve_block(
    block: &BlockAcc,
    step: &mut [f64],
    penalty: &[f64],
    params: &Params,
    lambdas: &StageLambdas,
    damping: f64,
) {
    let n = block.len();
    if n == 0 {
        return;
    }
    let lambda = lam(lambdas, block.kind);
    let mut a = block.hessian.clone();
    for entry in 0..n * n {
        a[entry] += lambda * penalty[entry];
    }
    for i in 0..n {
        let diagonal = block.hessian[i * n + i].max(1e-12);
        a[i * n + i] += damping * diagonal;
    }
    let mut rhs: Vec<f64> = block.gradient.iter().map(|value| -value).collect();
    for i in 0..n {
        let theta = params.theta[block.params[i]];
        let mut gradient = 0.0;
        for j in 0..n {
            gradient += penalty[i * n + j] * theta;
        }
        rhs[i] -= 2.0 * lambda * gradient;
    }
    let mut delta = rhs.clone();
    if !factor_in_place(&mut a, n) {
        return;
    }
    solve_in_place(&a, n, &mut delta);
    for i in 0..n {
        step[block.params[i]] = delta[i];
    }
}

fn apply_step(params: &mut Params, step: &[f64]) {
    let largest = step.iter().fold(0.0_f64, |acc, value| {
        if value.is_finite() {
            acc.max(value.abs())
        } else {
            acc
        }
    });
    let factor = if largest > 2.0 { 2.0 / largest } else { 1.0 };
    for (index, value) in step.iter().enumerate() {
        if value.is_finite() {
            params.theta[index] += factor * value;
        }
    }
}

fn update_sigma(problem: &Problem, params: &mut Params, pass: &PassOutput) {
    for index in 0..problem.contributors.len() {
        let weight = pass.contributor_weight[index];
        if weight <= 0.0 {
            continue;
        }
        let variance = (pass.contributor_rss[index] / (3.0 * weight)).max(1e-6);
        params.theta[params.layout.sigma_of(index)] = 0.5 * libm::log(variance);
    }
}

/// Nodes the data does not reach stay at zero (docs/02 §3.5).
fn pin_residual(params: &mut Params, layout: &Layout, samples: &[ResidualSample]) {
    let mut support = vec![0.0_f64; RESIDUAL_COEFS];
    for sample in samples {
        for (m, node) in sample.nodes.iter().enumerate() {
            let weight = sample.weights[m];
            for channel in 0..3 {
                support[*node as usize * 3 + channel] += sample.alpha * weight * weight;
            }
        }
    }
    let peak = support.iter().copied().fold(0.0_f64, f64::max);
    for (index, value) in support.iter().enumerate() {
        if *value < 1e-4 * peak {
            params.theta[layout.residual.start + index] = 0.0;
        }
    }
}

fn residual_penalty_diagonal() -> Vec<f64> {
    let mut diagonal = vec![0.0_f64; RESIDUAL_COEFS];
    for node in 0..RESIDUAL_NODES {
        let mut degree = 0.0;
        for axis in 0..3 {
            let (before, after) = neighbours(node, axis);
            degree += if before.is_some() { 1.0 } else { 0.0 };
            degree += if after.is_some() { 1.0 } else { 0.0 };
        }
        for channel in 0..3 {
            diagonal[node * 3 + channel] = degree + 0.01;
        }
    }
    diagonal
}

fn neighbours(node: usize, axis: usize) -> (Option<usize>, Option<usize>) {
    let stride = match axis {
        0 => RESIDUAL_GRID * RESIDUAL_GRID,
        1 => RESIDUAL_GRID,
        _ => 1,
    };
    let coordinate = match axis {
        0 => node / (RESIDUAL_GRID * RESIDUAL_GRID),
        1 => (node / RESIDUAL_GRID) % RESIDUAL_GRID,
        _ => node % RESIDUAL_GRID,
    };
    let before = if coordinate > 0 {
        Some(node - stride)
    } else {
        None
    };
    let after = if coordinate + 1 < RESIDUAL_GRID {
        Some(node + stride)
    } else {
        None
    };
    (before, after)
}

fn laplacian_at(values: &[f64], index: usize, penalty_diagonal: &[f64]) -> f64 {
    let node = index / 3;
    let channel = index % 3;
    let mut value = penalty_diagonal[index] * values[index];
    for axis in 0..3 {
        let (before, after) = neighbours(node, axis);
        if let Some(neighbour) = before {
            value -= values[neighbour * 3 + channel];
        }
        if let Some(neighbour) = after {
            value -= values[neighbour * 3 + channel];
        }
    }
    value
}

/// Data gradient of the residual block and its weighted residual sum of squares.
fn residual_gradient(samples: &[ResidualSample]) -> (Vec<f64>, f64) {
    let mut rhs = vec![0.0_f64; RESIDUAL_COEFS];
    let mut rss = 0.0;
    for sample in samples {
        let squared = sample.residual[0] * sample.residual[0]
            + sample.residual[1] * sample.residual[1]
            + sample.residual[2] * sample.residual[2];
        rss += sample.alpha * squared;
        for (m, node) in sample.nodes.iter().enumerate() {
            let weight = sample.weights[m];
            for channel in 0..3 {
                let mut gradient = 0.0;
                for row in 0..3 {
                    gradient += sample.clip.0[row][channel] * sample.residual[row];
                }
                rhs[*node as usize * 3 + channel] -= sample.alpha * weight * gradient;
            }
        }
    }
    (rhs, rss)
}

fn residual_penalty_value(theta: &[f64]) -> f64 {
    let diagonal = residual_penalty_diagonal();
    let mut value = 0.0;
    for index in 0..RESIDUAL_COEFS {
        if theta[index] != 0.0 {
            value += theta[index] * laplacian_at(theta, index, &diagonal);
        }
    }
    value
}

fn residual_diagonal(samples: &[ResidualSample]) -> Vec<f64> {
    let mut diagonal = vec![0.0_f64; RESIDUAL_COEFS];
    for sample in samples {
        for (m, node) in sample.nodes.iter().enumerate() {
            let weight = sample.weights[m];
            if weight == 0.0 {
                continue;
            }
            for channel in 0..3 {
                let mut sum = 0.0;
                for row in 0..3 {
                    sum += sample.clip.0[row][channel] * sample.clip.0[row][channel];
                }
                diagonal[*node as usize * 3 + channel] += sample.alpha * weight * weight * sum;
            }
        }
    }
    diagonal
}

fn residual_step(samples: &[ResidualSample], theta: &[f64], lambda: f64, damping: f64) -> Vec<f64> {
    let dim = RESIDUAL_COEFS;
    let penalty_diagonal = residual_penalty_diagonal();
    let data_diagonal = residual_diagonal(samples);
    let (mut rhs, _) = residual_gradient(samples);
    let mut diagonal = vec![0.0_f64; dim];
    for index in 0..dim {
        let base = data_diagonal[index].max(1e-9);
        diagonal[index] = base + lambda * penalty_diagonal[index] + damping * base + 1e-3;
        rhs[index] -= lambda * laplacian_at(theta, index, &penalty_diagonal);
    }
    let mut solution = vec![0.0_f64; dim];
    crate::solve::conjugate_gradient(
        dim,
        |v, product| {
            product.fill(0.0);
            for sample in samples {
                let mut y = [0.0_f64; 3];
                for (m, node) in sample.nodes.iter().enumerate() {
                    let weight = sample.weights[m];
                    for channel in 0..3 {
                        let value = v[*node as usize * 3 + channel];
                        for (row, slot) in y.iter_mut().enumerate() {
                            *slot += sample.clip.0[row][channel] * weight * value;
                        }
                    }
                }
                for (m, node) in sample.nodes.iter().enumerate() {
                    let weight = sample.weights[m];
                    for channel in 0..3 {
                        let mut gradient = 0.0;
                        for (row, value) in y.iter().enumerate() {
                            gradient += sample.clip.0[row][channel] * value;
                        }
                        product[*node as usize * 3 + channel] += sample.alpha * weight * gradient;
                    }
                }
            }
            for index in 0..dim {
                product[index] += lambda * laplacian_at(v, index, &penalty_diagonal);
                product[index] += (damping * data_diagonal[index].max(1e-9) + 1e-3) * v[index];
            }
        },
        &diagonal,
        &rhs,
        &mut solution,
        40,
        1e-6,
    );
    solution
}
