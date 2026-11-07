// SPDX-License-Identifier: AGPL-3.0-or-later
//! Staged Gauss-Newton/Levenberg-Marquardt with block solves and per-stage GCV.

use looklab_model::basis::{CORE_TERMS, RESIDUAL_COEFS, S_COEFS, TONE_COEFS, VOLUME_COEFS};
use looklab_model::realization::{
    ControlResponseModel, ControlValue, LayerKind, ResponseComponent, ResponseParamId,
};
use looklab_schema::{Control, ControlDefault, ControlSchema, ScalarValue, Tolerated};

use crate::basis;
use crate::density;
use crate::layout::{BlockKind, Layout, NUISANCE_PER_IMAGE, Params, zeta_from_clip};
use crate::model_error::{apply_model_error, estimate_model_error};
use crate::outlier;
use crate::predict::Layer;
use crate::problem::{ActiveColumns, BlockAcc, IntentFilter, PassOutput, Problem, ResidualSample};
use crate::solve::{factor_in_place, select_lambda, select_lambda_diagonal, solve_in_place};
use crate::types::{
    ControlResponseFit, Diagnostics, ExcludeReason, ExcludedImage, ExposureOffset, FitImage,
    FitOptions, FitResult, FitState, ImageIntent, ImageNuisances, PreviousSupport,
    ProbeUncertainty, Realization, Result, SampleEvaluations, StageLambdas, Verdict,
};
use crate::uncertainty;

const RESIDUAL_NODES: usize = RESIDUAL_COEFS / 3;
const RESIDUAL_GRID: usize = 17;
const STEP_RADIUS: f64 = 2.0;
const STATIONARY_GRADIENT: f64 = 1e-3;
const STATIONARY_DECREMENT_NATS: f64 = 1e-3;
const INEXACT_DESCENT_RESIDUAL: f64 = 0.1;

struct Stage {
    blocks: &'static [BlockKind],
    active: ActiveColumns,
    neutral: bool,
    sweeps: usize,
    gcv: bool,
    /// Stage runs to convergence up to `FitOptions::max_sweeps` instead of a fixed count.
    converge: bool,
}

const STAGES: [Stage; 5] = [
    Stage {
        blocks: &[BlockKind::Tone],
        active: ActiveColumns {
            image: true,
            mode: true,
            wb: true,
            clip: false,
            response: false,
        },
        neutral: true,
        sweeps: 6,
        converge: false,
        gcv: true,
    },
    Stage {
        blocks: &[
            BlockKind::Tone,
            BlockKind::Tint,
            BlockKind::ResponseTone,
            BlockKind::ResponseTint,
        ],
        active: ActiveColumns::ALL,
        neutral: false,
        sweeps: 4,
        converge: false,
        gcv: true,
    },
    Stage {
        blocks: &[
            BlockKind::Tone,
            BlockKind::Tint,
            BlockKind::G,
            BlockKind::R,
            BlockKind::Lambda,
            BlockKind::ResponseTone,
            BlockKind::ResponseTint,
            BlockKind::ResponseChroma,
            BlockKind::ResponseHue,
            BlockKind::ResponseLightness,
            BlockKind::ResponseWb,
        ],
        active: ActiveColumns::ALL,
        neutral: false,
        sweeps: 2,
        converge: false,
        gcv: true,
    },
    Stage {
        blocks: &[
            BlockKind::Tone,
            BlockKind::Residual,
            BlockKind::ResponseTone,
            BlockKind::ResponseTint,
            BlockKind::ResponseChroma,
            BlockKind::ResponseHue,
            BlockKind::ResponseLightness,
            BlockKind::ResponseWb,
        ],
        active: ActiveColumns::ALL,
        neutral: false,
        sweeps: 3,
        converge: false,
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
            BlockKind::ResponseTone,
            BlockKind::ResponseTint,
            BlockKind::ResponseChroma,
            BlockKind::ResponseHue,
            BlockKind::ResponseLightness,
            BlockKind::ResponseWb,
        ],
        active: ActiveColumns::ALL,
        neutral: false,
        sweeps: 3,
        converge: true,
        gcv: false,
    },
];

fn subsample_images(images: &[FitImage], limit: usize) -> Vec<FitImage> {
    images
        .iter()
        .map(|image| {
            let n = image.samples.n().min(limit);
            if n == image.samples.n() {
                return image.clone();
            }
            let mut header = image.samples.header().clone();
            header.n = n as u32;
            let samples = looklab_samples::SampleSet::new(
                header,
                image.samples.in_rgb()[..n].to_vec(),
                image.samples.out_rgb()[..n].to_vec(),
                image.samples.density_w()[..n].to_vec(),
                image.samples.flags()[..n].to_vec(),
            )
            .unwrap_or_else(|_| unreachable!("a subset of a valid sample set is valid"));
            let mut out = image.clone();
            out.samples = samples;
            if let Some(trust) = out.sample_trust.as_mut() {
                trust.truncate(n);
            }
            out
        })
        .collect()
}

fn accepted(previous: f64, cost: f64) -> bool {
    cost.is_finite() && cost <= previous + 1e-12 * previous.abs().max(1.0)
}

fn converged(recent: &[f64]) -> bool {
    if recent.len() < 5 {
        return false;
    }
    let oldest = recent[0];
    let newest = recent[recent.len() - 1];
    (oldest - newest).abs() <= 1e-3 * oldest.abs().max(1.0)
}

pub(crate) fn fit_diag() -> bool {
    std::env::var_os("LOOKLAB_FIT_DIAG").is_some()
}

pub(crate) fn filter_spatial(
    images: &[FitImage],
    schema: &ControlSchema,
    epoch: u32,
) -> Result<(Vec<FitImage>, Vec<ExcludedImage>)> {
    let epoch_schema = schema
        .epochs
        .iter()
        .find(|entry| entry.id == epoch)
        .ok_or_else(|| crate::types::FitError::Schema(format!("epoch {epoch} is not declared")))?;
    let spatial: Vec<&Control> = epoch_schema
        .controls
        .iter()
        .filter(|control| control.spatial)
        .collect();
    let mut kept = Vec::with_capacity(images.len());
    let mut excluded = Vec::new();
    for (index, image) in images.iter().enumerate() {
        let mut reason = None;
        for control in &spatial {
            let value = image
                .settings
                .get(&control.key)
                .cloned()
                .unwrap_or_else(|| control_value(control));
            if !tolerated(control, &value) {
                reason = Some(ExcludeReason::SpatialControl {
                    key: control.key.clone(),
                });
                break;
            }
        }
        match reason {
            Some(reason) => excluded.push(ExcludedImage { index, reason }),
            None => kept.push(image.clone()),
        }
    }
    Ok((kept, excluded))
}

fn control_value(control: &Control) -> looklab_model::realization::ControlValue {
    use looklab_model::realization::ControlValue;
    match &control.default {
        ControlDefault::Number(number) => ControlValue::Step(
            control
                .step
                .as_ref()
                .and_then(|step| number.div_exact(step))
                .unwrap_or(0),
        ),
        ControlDefault::Enum(value) => ControlValue::Enum(value.clone()),
        ControlDefault::Bool(value) => ControlValue::Bool(*value),
    }
}

fn tolerated(control: &Control, value: &looklab_model::realization::ControlValue) -> bool {
    use looklab_model::realization::ControlValue;
    let default = control_value(control);
    let allowed = match &control.tolerated {
        Some(Tolerated::Any) => return true,
        Some(Tolerated::Values(values)) => values.clone(),
        None => Vec::new(),
    };
    let scalar = match value {
        ControlValue::Step(index) => {
            let number = control
                .step
                .as_ref()
                .and_then(|step| step.checked_mul_i64(*index));
            match number {
                Some(number) => ScalarValue::Number(number),
                None => return false,
            }
        }
        ControlValue::Enum(name) => ScalarValue::Text(name.as_str().to_string()),
        ControlValue::Bool(value) => ScalarValue::Bool(*value),
    };
    if value == &default {
        return true;
    }
    allowed.contains(&scalar)
}

pub(crate) fn run(
    images: &[FitImage],
    options: &FitOptions,
    model: Option<ControlResponseModel>,
) -> Result<FitResult> {
    run_with(images, options, model, Vec::new())
}

pub(crate) fn run_with(
    images: &[FitImage],
    options: &FitOptions,
    model: Option<ControlResponseModel>,
    excluded: Vec<ExcludedImage>,
) -> Result<FitResult> {
    run_inner(
        images,
        options,
        model,
        excluded,
        None,
        true,
        None,
        None,
        FitMode::Main,
    )
    .map(|(result, _)| result)
}

pub(crate) fn run_with_history(
    images: &[FitImage],
    options: &FitOptions,
    model: Option<ControlResponseModel>,
    excluded: Vec<ExcludedImage>,
    previous: Option<&PreviousSupport>,
) -> Result<FitResult> {
    run_inner(
        images,
        options,
        model,
        excluded,
        None,
        true,
        previous,
        None,
        FitMode::Main,
    )
    .map(|(result, _)| result)
}

pub(crate) fn run_with_history_resumable(
    images: &[FitImage],
    options: &FitOptions,
    model: Option<ControlResponseModel>,
    excluded: Vec<ExcludedImage>,
    previous: Option<&PreviousSupport>,
    resume: Option<&FitState>,
) -> Result<(FitResult, Option<FitState>)> {
    run_inner(
        images,
        options,
        model,
        excluded,
        None,
        true,
        previous,
        resume,
        FitMode::Main,
    )
}

pub(crate) fn run_scoring(
    images: &[FitImage],
    options: &FitOptions,
    model: ControlResponseModel,
    excluded: Vec<ExcludedImage>,
) -> Result<FitResult> {
    run_inner(
        images,
        options,
        Some(model),
        excluded,
        None,
        false,
        None,
        None,
        FitMode::Main,
    )
    .map(|(result, _)| result)
}

fn run_reference(
    images: &[FitImage],
    options: &FitOptions,
    model: Option<&ControlResponseModel>,
) -> Result<FitResult> {
    let exposure_model = model.map(|model| {
        let mut model = model.clone();
        model.controls.clear();
        model.interactions.clear();
        model.wb.controls.clear();
        model
    });
    run_inner(
        images,
        options,
        exposure_model,
        Vec::new(),
        None,
        false,
        None,
        None,
        FitMode::Reference,
    )
    .map(|(result, _)| result)
}

/// Result of a conditional Look-only solve.
#[derive(Clone)]
pub(crate) struct LookOnlyResult {
    pub look: looklab_model::Look,
    pub probes: ProbeUncertainty,
    pub density: crate::types::Density,
    pub clip_k: f64,
    pub output_space: looklab_color::RgbSpace,
    pub evaluations: usize,
}

/// Optimize only θ_L, with per-image nuisances, clip, scales and control
/// responses frozen at the body-block values.
pub(crate) fn run_look_only(
    images: &[FitImage],
    options: &FitOptions,
    initial: Option<&looklab_model::Look>,
    clip_k: f64,
    model: Option<&ControlResponseModel>,
    previous: Option<&PreviousSupport>,
) -> Result<LookOnlyResult> {
    let problem = Problem::new_with(images, model, options)?;
    let layout = problem.layout.clone();
    let mut params = match initial {
        Some(look) => params_from_look(&problem, look),
        None => initial_params(&problem, options),
    };
    for index in 0..problem.images.len() {
        let range = layout.image(index);
        params.theta[range.start] = problem.images[index].exposure_hint.clamp(-8.0, 8.0);
        params.theta[range.start + 1] = 0.0;
        params.theta[range.start + 2] = 0.0;
    }
    params.theta[layout.clip] = crate::layout::zeta_from_clip(clip_k);
    if let Some(model) = model {
        seed_response_params(&problem, &mut params, model);
    }
    let active = ActiveColumns {
        image: false,
        mode: false,
        wb: false,
        clip: false,
        response: false,
    };
    let mut lambdas = options.initial_lambdas;
    let roles = prior_roles(&problem, &layout);
    let mut evaluations = 0usize;
    for stage in &STAGES {
        let kinds: Vec<BlockKind> = stage
            .blocks
            .iter()
            .copied()
            .filter(|kind| !is_response_kind(*kind))
            .filter(|kind| options.fit_residual || *kind != BlockKind::Residual)
            .collect();
        let mut blocks: Vec<BlockAcc> = kinds
            .iter()
            .map(|kind| make_block(&problem, *kind, active))
            .collect();
        let penalties: Vec<Vec<f64>> = blocks
            .iter()
            .map(|block| block_penalty(&problem, block))
            .collect();
        let mut damping: f64 = 1e-3;
        let mut previous: Option<(f64, Params)> = None;
        let mut gcv_done = false;
        let mut recent: Vec<f64> = Vec::new();
        let stage_sweeps = if stage.converge {
            options.max_sweeps.max(1)
        } else {
            stage.sweeps.min(options.max_sweeps.max(1))
        };
        let mut sample_evaluations = SampleEvaluations::default();
        for _ in 0..stage_sweeps {
            if sample_evaluations.get() as f64 + problem.samples.len() as f64 > options.sweep_budget
            {
                break;
            }
            sample_evaluations += problem.samples.len();
            for block in &mut blocks {
                block.reset();
            }
            let pass = problem.pass(
                &params,
                active,
                &mut blocks,
                false,
                false,
                stage.neutral,
                IntentFilter::NativeOnly,
            );
            evaluations += 1;
            let hessian_scale = pass.active_samples.max(1) as f64 / pass.hessian_rows.max(1) as f64;
            let cost =
                pass.cost + total_cost(&problem, &params, &blocks, &penalties, &lambdas, options);
            if let Some((previous_cost, previous_params)) = &previous
                && !accepted(*previous_cost, cost)
            {
                params = previous_params.clone();
                damping = (damping * 10.0).min(1e6);
                if damping >= 1e6 {
                    break;
                }
                continue;
            }
            if previous
                .as_ref()
                .is_none_or(|(previous_cost, _)| cost < *previous_cost)
            {
                recent.push(cost);
                if recent.len() > 5 {
                    recent.remove(0);
                }
                if converged(&recent) {
                    break;
                }
            }
            previous = Some((cost, params.clone()));
            if stage.gcv && !gcv_done && options.run_gcv {
                choose_lambdas(
                    &problem,
                    &params,
                    &pass,
                    &blocks,
                    &penalties,
                    &mut lambdas,
                    options,
                );
                gcv_done = true;
                previous = Some((
                    pass.cost
                        + total_cost(&problem, &params, &blocks, &penalties, &lambdas, options),
                    params.clone(),
                ));
            }

            let mut step = vec![0.0_f64; layout.total];
            for (index, block) in blocks.iter().enumerate() {
                if block.kind == BlockKind::Residual {
                    continue;
                }
                let (prior_gradient, prior_curvature) =
                    prior_terms(&problem, &roles, &params, options, block);
                solve_block(
                    &layout,
                    block,
                    &mut step,
                    &penalties[index],
                    &prior_gradient,
                    &prior_curvature,
                    &params,
                    &lambdas,
                    damping,
                    hessian_scale,
                    StepLimit::WholeStep,
                );
            }
            if kinds.contains(&BlockKind::Residual) {
                let lambda = lam(&lambdas, BlockKind::Residual);
                let delta = residual_step(
                    &pass.residual_samples,
                    &pass.residual_gradient,
                    &params.theta[layout.residual.clone()],
                    lambda,
                    damping,
                    hessian_scale,
                    StepLimit::WholeStep,
                );
                for (index, value) in delta.iter().enumerate() {
                    step[layout.residual.start + index] = *value;
                }
            }
            apply_step(&mut params, &step);
            if kinds.contains(&BlockKind::Residual) {
                pin_residual(&mut params, &layout, &pass.residual_samples);
            }
            damping = (damping * 0.3).max(1e-6);
        }
    }
    let look = params.look();
    let density = density::build(&problem, &params, options.density_floor);
    let frozen = ActiveColumns {
        image: false,
        mode: false,
        wb: false,
        clip: false,
        response: false,
    };
    let mut uncertainty_blocks = vec![
        make_look_block(&layout, frozen),
        make_block(&problem, BlockKind::Residual, frozen),
    ];
    let uncertainty_pass = problem.pass(
        &params,
        frozen,
        &mut uncertainty_blocks,
        false,
        true,
        false,
        IntentFilter::All,
    );
    evaluations += 1;
    let penalty = double(combined_look_penalty(
        &problem,
        &layout,
        &lambdas,
        uncertainty_blocks[0].len(),
        false,
    ));
    let probes = uncertainty::probe_uncertainty(
        &problem,
        &params,
        &look,
        &uncertainty_blocks[0],
        &penalty,
        uncertainty_pass.active_samples.max(1) as f64 / uncertainty_pass.hessian_rows.max(1) as f64,
        &lambdas,
        &uncertainty_pass.per_image_gradient,
        &density,
        &uncertainty_pass.residual_samples,
        options,
        previous,
    )?;
    Ok(LookOnlyResult {
        look,
        probes,
        density,
        clip_k,
        output_space: problem
            .images
            .first()
            .map_or(looklab_color::RgbSpace::Srgb, |image| image.space),
        evaluations,
    })
}

pub(crate) struct ConditionedNuisances {
    pub images: Vec<ImageNuisances>,
    pub converged: bool,
    pub accepted: usize,
    pub attempts: usize,
    pub damping: f64,
    pub last_step: f64,
    pub gradient_max: f64,
}

/// Per-image exposure and WB offsets with the Look fixed, fitted on `images`.
pub(crate) fn condition_nuisances(
    images: &[FitImage],
    options: &FitOptions,
    look: &looklab_model::Look,
    model: Option<&ControlResponseModel>,
    clip_k: f64,
) -> Result<ConditionedNuisances> {
    condition_nuisances_inner(images, options, look, model, clip_k)
}

fn condition_nuisances_inner(
    images: &[FitImage],
    options: &FitOptions,
    look: &looklab_model::Look,
    model: Option<&ControlResponseModel>,
    clip_k: f64,
) -> Result<ConditionedNuisances> {
    let problem = Problem::new_with(images, model, options)?;
    let layout = problem.layout.clone();
    let mut params = params_from_look(&problem, look);
    if let Some(model) = model {
        seed_response_params(&problem, &mut params, model);
        seed_mode_params(&problem, &mut params, model);
    }
    for index in 0..problem.images.len() {
        let range = layout.image(index);
        params.theta[range.start] = problem.images[index].exposure_hint.clamp(-8.0, 8.0);
        params.theta[range.start + 1] = 0.0;
        params.theta[range.start + 2] = 0.0;
    }
    params.theta[layout.clip] = crate::layout::zeta_from_clip(clip_k);
    let active = ActiveColumns {
        image: true,
        mode: false,
        wb: true,
        clip: false,
        response: false,
    };
    let mut blocks: Vec<BlockAcc> = (0..problem.images.len())
        .map(|index| make_image_block(&layout, index, active))
        .collect();
    let penalties: Vec<Vec<f64>> = blocks
        .iter()
        .map(|block| block_penalty(&problem, block))
        .collect();
    let roles = prior_roles(&problem, &layout);
    let lambdas = options.initial_lambdas;
    let mut damping = 1e-3;
    let mut recent: Vec<f64> = Vec::new();
    let mut converged_fit = false;
    let mut accepted_count = 0usize;
    let mut attempts = 0usize;
    let mut last_step = 0.0_f64;
    let mut last_gradient = 0.0_f64;
    let max_attempts = options.max_sweeps.clamp(1, 8).max(5) * 3;
    for block in &mut blocks {
        block.reset();
    }
    let mut pass = problem.pass(
        &params,
        active,
        &mut blocks,
        false,
        false,
        false,
        IntentFilter::All,
    );
    let mut cost =
        pass.cost + total_cost(&problem, &params, &blocks, &penalties, &lambdas, options);
    recent.push(cost);
    for _ in 0..max_attempts {
        attempts += 1;
        if fit_diag() {
            eprintln!(
                "nuisance attempt {attempts} start cost {cost:.12e} damping {damping:.3e} history {}",
                recent.len()
            );
        }
        let hessian_scale = pass.active_samples.max(1) as f64 / pass.hessian_rows.max(1) as f64;
        let mut step = vec![0.0_f64; layout.total];
        let mut failed: Option<usize> = None;
        for (index, block) in blocks.iter().enumerate() {
            let (prior_gradient, prior_curvature) =
                prior_terms(&problem, &roles, &params, options, block);
            let outcome = solve_block(
                &layout,
                block,
                &mut step,
                &penalties[index],
                &prior_gradient,
                &prior_curvature,
                &params,
                &lambdas,
                damping,
                hessian_scale,
                StepLimit::WholeStep,
            );
            if outcome == BlockStep::Failed && failed.is_none() {
                failed = Some(index);
            }
        }
        if let Some(index) = failed {
            // An unsolved image block leaves its nuisances unmoved; accepting the trial
            // would count a no-op as progress and lower the damping.
            damping = (damping * 10.0).min(1e6);
            if fit_diag() {
                eprintln!(
                    "nuisance attempt {attempts} block {index} factorization failed damping {damping:.3e}"
                );
            }
            if damping >= 1e6 {
                break;
            }
            continue;
        }
        let mut trial = params.clone();
        apply_step(&mut trial, &step);
        last_step = step.iter().fold(0.0_f64, |acc, value| {
            if value.is_finite() {
                acc.max(value.abs())
            } else {
                f64::INFINITY
            }
        });
        for block in &mut blocks {
            block.reset();
        }
        let trial_pass = problem.pass(
            &trial,
            active,
            &mut blocks,
            false,
            false,
            false,
            IntentFilter::All,
        );
        let trial_cost =
            trial_pass.cost + total_cost(&problem, &trial, &blocks, &penalties, &lambdas, options);
        if accepted(cost, trial_cost) {
            accepted_count += 1;
            params = trial;
            pass = trial_pass;
            cost = trial_cost;
            let gradient = gradient_max(GradientArgs {
                layout: &layout,
                problem: &problem,
                params: &params,
                options,
                blocks: &blocks,
                penalties: &penalties,
                lambdas: &lambdas,
                roles: &roles,
            });
            let accepted_damping = damping;
            damping = (damping * 0.3).max(1e-6);
            recent.push(cost);
            if recent.len() > 5 {
                recent.remove(0);
            }
            let stationary = gradient <= STATIONARY_GRADIENT;
            last_gradient = gradient;
            let stable_step = accepted_damping <= 1e-2 && last_step <= 1e-6 && stationary;
            let stable_cost = accepted_damping <= 1e-2 && stationary && recent.len() >= 3 && {
                let last = recent[recent.len() - 1];
                let previous = recent[recent.len() - 2];
                (last - previous).abs() <= 1e-4 * last.abs().max(1.0)
            };
            if fit_diag() {
                eprintln!(
                    "nuisance attempt {attempts} accepted cost {cost:.12e} gradient {gradient:.9e} damping {accepted_damping:.3e} step {last_step:.9e} history {}",
                    recent.len()
                );
                print_block_gradients(
                    "nuisance", &layout, &problem, &params, options, &blocks, &penalties, &lambdas,
                    &roles,
                );
            }
            if gradient <= 1e-5
                || stable_step
                || stable_cost
                || (accepted_damping <= 1e-2 && stationary && converged(&recent))
            {
                converged_fit = true;
                break;
            }
        } else {
            damping = (damping * 10.0).min(1e6);
            if damping >= 1e6 {
                if fit_diag() {
                    eprintln!(
                        "nuisance attempt {attempts} rejected damping cap cost {cost:.12e} history {}",
                        recent.len()
                    );
                }
                break;
            }
            for block in &mut blocks {
                block.reset();
            }
            pass = problem.pass(
                &params,
                active,
                &mut blocks,
                false,
                false,
                false,
                IntentFilter::All,
            );
            cost =
                pass.cost + total_cost(&problem, &params, &blocks, &penalties, &lambdas, options);
            recent.push(cost);
            if recent.len() > 5 {
                recent.remove(0);
            }
            let gradient = gradient_max(GradientArgs {
                layout: &layout,
                problem: &problem,
                params: &params,
                options,
                blocks: &blocks,
                penalties: &penalties,
                lambdas: &lambdas,
                roles: &roles,
            });
            last_gradient = gradient;
            if fit_diag() {
                eprintln!(
                    "nuisance attempt {attempts} rejected cost {cost:.12e} gradient {gradient:.9e} damping {damping:.3e} step {last_step:.9e} history {}",
                    recent.len()
                );
                print_block_gradients(
                    "nuisance", &layout, &problem, &params, options, &blocks, &penalties, &lambdas,
                    &roles,
                );
            }
            if gradient <= 1e-5 || (gradient <= STATIONARY_GRADIENT && converged(&recent)) {
                converged_fit = true;
                break;
            }
        }
    }
    let mut out = Vec::with_capacity(problem.images.len());
    for index in 0..problem.images.len() {
        let nuisance = params.image(index);
        out.push(ImageNuisances {
            exposure: nuisance[0],
            wb_log_r: nuisance[1],
            wb_log_b: nuisance[2],
            exposure_mode: images[index].exposure_mode,
            contributor: images[index].contributor,
            weight: problem.images[index].trust,
        });
    }
    if fit_diag() {
        eprintln!(
            "nuisance done converged {converged_fit} accepted {accepted_count} attempts {attempts} damping {damping:.3e} step {last_step:.9e} gradient {last_gradient:.9e}"
        );
    }
    Ok(ConditionedNuisances {
        images: out,
        converged: converged_fit,
        accepted: accepted_count,
        attempts,
        damping,
        last_step,
        gradient_max: last_gradient,
    })
}

/// Fit without the probe/response uncertainty.
pub(crate) fn run_quick(
    images: &[FitImage],
    options: &FitOptions,
    initial: Option<&looklab_model::Look>,
    model: Option<&ControlResponseModel>,
) -> Result<FitResult> {
    run_inner(
        images,
        options,
        model.cloned(),
        Vec::new(),
        initial,
        false,
        None,
        None,
        FitMode::Main,
    )
    .map(|(result, _)| result)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FitMode {
    Main,
    Reference,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StepLimit {
    WholeStep,
    PerBlock,
}

#[allow(clippy::too_many_arguments)]
fn run_inner(
    images: &[FitImage],
    options: &FitOptions,
    model: Option<ControlResponseModel>,
    excluded: Vec<ExcludedImage>,
    initial: Option<&looklab_model::Look>,
    want_uncertainty: bool,
    previous: Option<&PreviousSupport>,
    resume: Option<&FitState>,
    mode: FitMode,
) -> Result<(FitResult, Option<FitState>)> {
    let reference_mode = mode == FitMode::Reference;
    let mut native_positions: Vec<Option<usize>> = vec![None; images.len()];
    let mut native = Vec::new();
    for (index, image) in images.iter().enumerate() {
        if image.intent == ImageIntent::Native {
            native_positions[index] = Some(native.len());
            native.push(image.clone());
        }
    }
    if native.is_empty() {
        return Err(crate::types::FitError::NoNativeImages);
    }
    let mut problem = Problem::new_with(&native, model.as_ref(), options)?;
    if fit_diag() {
        eprintln!(
            "fit start reference {reference_mode} resume {} images {} samples {} hessian {} max_sweeps {} budget {:.3e}",
            resume.is_some(),
            problem.images.len(),
            problem.samples.len(),
            problem.hessian.len(),
            options.max_sweeps,
            options.sweep_budget
        );
    }
    // Warm-up subsampling only saves time on large pools; small many-image pools pay
    // little for the full set and start the final stage from a converged point.
    let subsample_warmup = model.is_some()
        || problem.images.len() < 24
        || problem.samples.len() as f64 > options.sweep_budget / 64.0;
    let early_problem = if subsample_warmup
        && options.early_samples_per_image > 0
        && options.early_samples_per_image < options.max_samples_per_image
    {
        let early = subsample_images(&native, options.early_samples_per_image);
        Some(Problem::new_with(&early, model.as_ref(), options)?)
    } else {
        None
    };
    let mut layout = problem.layout.clone();
    let mut params = initial_params(&problem, options);
    if let Some(look) = initial {
        apply_look(&problem, &mut params, look);
    }
    let mut roles = prior_roles(&problem, &layout);
    let mut lambdas = options.initial_lambdas;
    let mut evaluations = 0usize;
    if let Some(state) = resume {
        if state.theta.len() == params.theta.len() {
            params.theta.copy_from_slice(&state.theta);
        }
        lambdas = state.lambdas;
    }

    let mut sample_evaluations = SampleEvaluations::default();
    if !reference_mode && model.is_some() && resume.is_none() {
        let reference: Vec<FitImage> = native
            .iter()
            .enumerate()
            .filter(|(index, _)| problem.terms[*index].is_empty())
            .map(|(_, image)| image.clone())
            .collect();
        if reference.len() >= 4 && reference.len() < native.len() {
            let reference_fit = run_reference(&reference, options, model.as_ref())?;
            evaluations += reference_fit.diagnostics.evaluations;
            sample_evaluations += reference_fit.diagnostics.sample_evaluations;
            apply_look(&problem, &mut params, &reference_fit.look);
        }
        if !problem.layout.response.is_empty() {
            run_frozen_responses(
                &problem,
                &mut params,
                options,
                &mut lambdas,
                &mut evaluations,
                IntentFilter::NativeOnly,
            )?;
        }
    }

    let warmup_allowed =
        (options.sweep_budget / problem.samples.len().max(1) as f64).clamp(8.0, 64.0) as usize;
    let mut final_converged = false;
    let mut operator_rows = 0usize;
    let mut resume_state: Option<FitState> = None;
    let mut step_warm: Option<Vec<f64>> = None;
    let mut cert_warm: Option<Vec<f64>> = None;
    let start_stage = resume.map_or(0, |state| state.stage.min(STAGES.len()));
    for (stage_index, stage) in STAGES.iter().enumerate() {
        if stage_index < start_stage {
            continue;
        }
        let stage_sweeps = if stage.converge {
            options.max_sweeps.max(1)
        } else {
            stage
                .sweeps
                .min(options.max_sweeps.max(1))
                .min(warmup_allowed)
        };
        let stage_problem = if stage.converge {
            &problem
        } else {
            early_problem.as_ref().unwrap_or(&problem)
        };
        let kinds: Vec<BlockKind> = stage
            .blocks
            .iter()
            .copied()
            .filter(|kind| options.fit_residual || *kind != BlockKind::Residual)
            .filter(|kind| !is_response_kind(*kind))
            .collect();
        let split_nuisances = stage_problem.images.len() > 64;
        let mut blocks: Vec<BlockAcc> = kinds
            .iter()
            .map(|kind| {
                if *kind == BlockKind::Tone && !split_nuisances {
                    make_joint_tone_block(stage_problem, stage.active)
                } else {
                    make_block(stage_problem, *kind, stage.active)
                }
            })
            .collect();
        if split_nuisances && (stage.active.image || stage.active.wb) {
            for index in 0..problem_image_count(&layout) {
                blocks.push(make_image_block(&layout, index, stage.active));
            }
        }
        if split_nuisances && (stage.active.mode || stage.active.clip) {
            blocks.push(make_shared_nuisance_block(
                stage_problem,
                &layout,
                stage.active,
            ));
        }
        let penalties: Vec<Vec<f64>> = blocks
            .iter()
            .map(|block| block_penalty(stage_problem, block))
            .collect();
        let resuming = resume.is_some_and(|state| state.stage == stage_index);
        let start_sweep = if resuming {
            resume.map_or(0, |state| state.sweep).min(stage_sweeps)
        } else {
            0
        };
        let mut damping: f64 = if resuming {
            resume.map_or(1e-3, |state| state.damping)
        } else {
            1e-3
        };
        let mut last_cost: Option<f64> = None;
        let mut gcv_done = false;
        let mut accepted_state = false;
        let mut gradient_scale: Option<f64> = None;
        let mut recent: Vec<f64> = Vec::new();
        if fit_diag() && resuming {
            eprintln!(
                "fit resume stage {stage_index} start_sweep {start_sweep} of {stage_sweeps} empty {} damping {damping:.3e}",
                start_sweep >= stage_sweeps
            );
        }
        for sweep_index in start_sweep..stage_sweeps {
            let samples_per_pass = stage_problem.samples.len();
            if sample_evaluations.get() as f64 + 2.0 * samples_per_pass as f64
                > options.sweep_budget
            {
                if fit_diag() {
                    eprintln!(
                        "fit checkpoint stage {stage_index} sweep {sweep_index} reason budget damping {damping:.3e}"
                    );
                }
                resume_state = Some(FitState {
                    theta: params.theta.clone(),
                    lambdas,
                    stage: stage_index,
                    sweep: sweep_index,
                    damping,
                    converged: false,
                });
                break;
            }
            sample_evaluations += samples_per_pass;
            for block in &mut blocks {
                block.reset();
            }
            let pass = stage_problem.pass_with_rows(
                &params,
                stage.active,
                &mut blocks,
                false,
                false,
                stage.neutral,
                IntentFilter::NativeOnly,
                stage.converge,
                stage.converge && stage_problem.samples.len() <= FULL_ROW_LIMIT,
            );
            evaluations += 1;
            if stage.converge {
                operator_rows = pass.hessian_rows;
            }
            let hessian_scale = pass.active_samples.max(1) as f64 / pass.hessian_rows.max(1) as f64;
            let mut cost =
                pass.cost + total_cost(&problem, &params, &blocks, &penalties, &lambdas, options);
            if stage.converge {
                if accepted_state {
                    recent.push(cost);
                    if recent.len() > 5 {
                        recent.remove(0);
                    }
                    accepted_state = false;
                }
            } else if last_cost.is_none_or(|previous| cost < previous) {
                recent.push(cost);
                if recent.len() > 5 {
                    recent.remove(0);
                }
            } else {
                recent.clear();
            }
            last_cost = Some(cost);

            if fit_diag() && stage.converge {
                eprintln!(
                    "fit stage {stage_index} sweep {sweep_index} cost {cost:.12e} history {} damping {damping:.3e} active {} hessian {} scale {hessian_scale:.6}",
                    recent.len(),
                    pass.active_samples,
                    pass.hessian_rows
                );
                print_cost_breakdown(
                    &problem, &params, &blocks, &penalties, &lambdas, options, &pass,
                );
                print_block_gradients(
                    "current", &layout, &problem, &params, options, &blocks, &penalties, &lambdas,
                    &roles,
                );
            }

            if stage.gcv && !gcv_done && options.run_gcv {
                choose_lambdas(
                    &problem,
                    &params,
                    &pass,
                    &blocks,
                    &penalties,
                    &mut lambdas,
                    options,
                );
                gcv_done = true;
                cost = pass.cost
                    + total_cost(&problem, &params, &blocks, &penalties, &lambdas, options);
                last_cost = Some(cost);
                recent.clear();
            }

            if converged(&recent) {
                if !stage.converge {
                    break;
                }
                let decrement = stationarity_decrement(
                    &layout,
                    &problem,
                    &params,
                    options,
                    &blocks,
                    &penalties,
                    &lambdas,
                    &roles,
                    &pass,
                    hessian_scale,
                    &mut cert_warm,
                );
                if fit_diag() {
                    eprintln!(
                        "fit stage {stage_index} sweep {sweep_index} plateau decrement {decrement:.9e} nats threshold {STATIONARY_DECREMENT_NATS:.3e}"
                    );
                    print_block_gradients(
                        "plateau", &layout, &problem, &params, options, &blocks, &penalties,
                        &lambdas, &roles,
                    );
                }
                if decrement <= STATIONARY_DECREMENT_NATS {
                    final_converged = true;
                    break;
                }
            }

            if stage.converge {
                match final_stage_step(
                    &layout,
                    &problem,
                    stage_problem,
                    stage,
                    options,
                    &mut params,
                    &blocks,
                    &penalties,
                    &lambdas,
                    &roles,
                    &pass,
                    hessian_scale,
                    cost,
                    &mut damping,
                    &mut gradient_scale,
                    &mut evaluations,
                    &mut sample_evaluations,
                    samples_per_pass,
                    stage_index,
                    sweep_index,
                    recent.len(),
                    &mut step_warm,
                ) {
                    FinalOutcome::Accepted(trial) => {
                        update_sigma(stage_problem, &mut params, &trial, options);
                        accepted_state = true;
                        continue;
                    }
                    FinalOutcome::Budget | FinalOutcome::Stalled => {
                        if fit_diag() {
                            eprintln!(
                                "fit checkpoint stage {stage_index} sweep {sweep_index} reason joint damping {damping:.3e} history {}",
                                recent.len()
                            );
                        }
                        resume_state = Some(FitState {
                            theta: params.theta.clone(),
                            lambdas,
                            stage: stage_index,
                            sweep: sweep_index,
                            damping,
                            converged: false,
                        });
                        break;
                    }
                }
            }
            let step_params = params.clone();
            let mut step = vec![0.0_f64; layout.total];
            for (index, block) in blocks.iter().enumerate() {
                if block.kind == BlockKind::Residual {
                    continue;
                }
                let (prior_gradient, prior_curvature) =
                    prior_terms(&problem, &roles, &params, options, block);
                solve_block(
                    &layout,
                    block,
                    &mut step,
                    &penalties[index],
                    &prior_gradient,
                    &prior_curvature,
                    &params,
                    &lambdas,
                    damping,
                    hessian_scale,
                    StepLimit::PerBlock,
                );
            }
            if kinds.contains(&BlockKind::Residual) {
                let delta = residual_step(
                    &pass.residual_samples,
                    &pass.residual_gradient,
                    &params.theta[layout.residual.clone()],
                    lam(&lambdas, BlockKind::Residual),
                    damping,
                    hessian_scale,
                    StepLimit::PerBlock,
                );
                for (index, value) in delta.iter().enumerate() {
                    step[layout.residual.start + index] = *value;
                }
            }
            apply_step(&mut params, &step);
            if kinds.contains(&BlockKind::Residual) {
                pin_residual(&mut params, &layout, &pass.residual_samples);
            }
            let trial = stage_problem.pass(
                &params,
                stage.active,
                &mut [],
                false,
                false,
                stage.neutral,
                IntentFilter::NativeOnly,
            );
            evaluations += 1;
            sample_evaluations += samples_per_pass;
            let trial_cost =
                trial.cost + total_cost(&problem, &params, &blocks, &penalties, &lambdas, options);
            if trial.infeasible || !accepted(cost, trial_cost) {
                params = step_params;
                damping = (damping * 10.0).min(1e6_f64);
                if fit_diag() {
                    eprintln!(
                        "fit stage {stage_index} sweep {sweep_index} rejected trial {trial_cost:.12e} current {cost:.12e} damping {damping:.3e} history {}",
                        recent.len()
                    );
                }
                if damping >= 1e6 {
                    resume_state = Some(FitState {
                        theta: params.theta.clone(),
                        lambdas,
                        stage: stage_index,
                        sweep: sweep_index,
                        damping,
                        converged: false,
                    });
                    break;
                }
                continue;
            }
            update_sigma(stage_problem, &mut params, &trial, options);
            accepted_state = true;
        }
        if resume_state.is_some() {
            break;
        }
        if stage.converge && !final_converged {
            if fit_diag() {
                eprintln!(
                    "fit stage {stage_index} not converged after {stage_sweeps} sweeps: history {:?} last_cost {last_cost:?} damping {damping:.3e}",
                    recent
                );
                print_block_gradients(
                    "final", &layout, &problem, &params, options, &blocks, &penalties, &lambdas,
                    &roles,
                );
            }
            resume_state = Some(FitState {
                theta: params.theta.clone(),
                lambdas,
                stage: stage_index,
                sweep: 0,
                damping,
                converged: false,
            });
            break;
        }
    }
    let want_uncertainty = want_uncertainty && final_converged;

    if !reference_mode && model.is_some() && !problem.layout.response.is_empty() {
        run_frozen_responses(
            &problem,
            &mut params,
            options,
            &mut lambdas,
            &mut evaluations,
            IntentFilter::NativeOnly,
        )?;
    }

    if images
        .iter()
        .any(|image| image.intent == ImageIntent::Recipe)
    {
        let all_problem = Problem::new_with(images, model.as_ref(), options)?;
        let mut all_params =
            carry_params(&problem, &params, &all_problem, &native_positions, images);
        layout = all_problem.layout.clone();
        roles = prior_roles(&all_problem, &layout);
        let kinds = [
            BlockKind::Nuisance,
            BlockKind::ResponseTone,
            BlockKind::ResponseTint,
            BlockKind::ResponseChroma,
            BlockKind::ResponseHue,
            BlockKind::ResponseLightness,
            BlockKind::ResponseWb,
        ];
        let mut blocks: Vec<BlockAcc> = kinds
            .iter()
            .map(|kind| make_block(&all_problem, *kind, ActiveColumns::ALL))
            .collect();
        let penalties: Vec<Vec<f64>> = blocks
            .iter()
            .map(|block| block_penalty(&all_problem, block))
            .collect();
        let mut damping: f64 = 1e-3;
        for _ in 0..options.recipe_sweeps {
            for block in &mut blocks {
                block.reset();
            }
            let pass = all_problem.pass(
                &all_params,
                ActiveColumns::ALL,
                &mut blocks,
                false,
                false,
                false,
                IntentFilter::All,
            );
            evaluations += 1;
            let hessian_scale = pass.active_samples.max(1) as f64 / pass.hessian_rows.max(1) as f64;
            let mut step = vec![0.0_f64; layout.total];
            for (index, block) in blocks.iter().enumerate() {
                let (prior_gradient, prior_curvature) =
                    prior_terms(&all_problem, &roles, &all_params, options, block);
                solve_block(
                    &layout,
                    block,
                    &mut step,
                    &penalties[index],
                    &prior_gradient,
                    &prior_curvature,
                    &all_params,
                    &lambdas,
                    damping,
                    hessian_scale,
                    StepLimit::WholeStep,
                );
            }
            apply_step(&mut all_params, &step);
            update_sigma(&all_problem, &mut all_params, &pass, options);
            damping = (damping * 0.3).max(1e-6);
        }
        problem = all_problem;
        params = all_params;
    }

    let mut blocks: Vec<BlockAcc> = STAGES[4]
        .blocks
        .iter()
        .map(|kind| make_block(&problem, *kind, ActiveColumns::ALL))
        .collect();
    for block in &mut blocks {
        block.reset();
    }
    let penalties: Vec<Vec<f64>> = blocks
        .iter()
        .map(|block| block_penalty(&problem, block))
        .collect();
    let final_pass = problem.pass(
        &params,
        ActiveColumns::ALL,
        &mut blocks,
        !reference_mode,
        want_uncertainty,
        false,
        IntentFilter::All,
    );
    evaluations += 1;
    let uncertainty_columns = ActiveColumns {
        response: problem.terms.iter().any(|terms| !terms.is_empty()),
        ..ActiveColumns::ALL
    };
    let mut uncertainty_blocks = want_uncertainty.then(|| {
        vec![
            make_look_block(&layout, uncertainty_columns),
            make_block(&problem, BlockKind::Residual, uncertainty_columns),
        ]
    });
    let uncertainty_pass = uncertainty_blocks.as_mut().map(|blocks| {
        problem.pass(
            &params,
            uncertainty_columns,
            blocks,
            false,
            true,
            false,
            IntentFilter::All,
        )
    });
    if uncertainty_pass.is_some() {
        evaluations += 1;
    }
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

    let density = density::build(&problem, &params, options.density_floor);
    let image_stats = if reference_mode {
        Vec::new()
    } else {
        outlier::score(&problem, &final_pass, options)
    };
    let mut grl_precision_ratio = Vec::new();
    let (mut probe_result, parameter_ids, covariance) =
        match (uncertainty_blocks.as_ref(), uncertainty_pass.as_ref()) {
            (Some(uncertainty_blocks), Some(uncertainty_pass)) => {
                let uncertainty_penalty = double(combined_look_penalty(
                    &problem,
                    &layout,
                    &lambdas,
                    uncertainty_blocks[0].len(),
                    uncertainty_columns.response,
                ));
                let block = &uncertainty_blocks[0];
                let scale = uncertainty_pass.active_samples.max(1) as f64
                    / uncertainty_pass.hessian_rows.max(1) as f64;
                let identity = identity_curvature(&layout, block);
                let block_len = block.len();
                for (local, global) in block.params.iter().enumerate() {
                    if layout.g.contains(global)
                        || layout.r.contains(global)
                        || layout.lambda.contains(global)
                    {
                        let data = block.hessian[local * block_len + local] * scale;
                        let prior = identity[local]
                            + lambdas.grl * uncertainty_penalty[local * block_len + local];
                        grl_precision_ratio.push(data / prior.max(1e-12));
                    }
                }
                let joint = uncertainty::probe_uncertainty(
                    &problem,
                    &params,
                    &look,
                    &uncertainty_blocks[0],
                    &uncertainty_penalty,
                    uncertainty_pass.active_samples.max(1) as f64
                        / uncertainty_pass.hessian_rows.max(1) as f64,
                    &lambdas,
                    &uncertainty_pass.per_image_gradient,
                    &density,
                    &uncertainty_pass.residual_samples,
                    options,
                    previous,
                );
                let probes = match joint {
                    Ok(result) => result,
                    Err(crate::types::FitError::SingularRegularization) => {
                        let look_params: Vec<usize> = uncertainty_blocks[0]
                            .params
                            .iter()
                            .copied()
                            .filter(|index| !layout.response.contains(index))
                            .collect();
                        let mut fallback = vec![
                            BlockAcc::new(BlockKind::Tone, look_params, layout.total),
                            make_block(&problem, BlockKind::Residual, ActiveColumns::ALL),
                        ];
                        let fallback_pass = problem.pass(
                            &params,
                            ActiveColumns::ALL,
                            &mut fallback,
                            false,
                            true,
                            false,
                            IntentFilter::All,
                        );
                        let penalty = double(combined_look_penalty(
                            &problem,
                            &layout,
                            &lambdas,
                            fallback[0].len(),
                            false,
                        ));
                        uncertainty::probe_uncertainty(
                            &problem,
                            &params,
                            &look,
                            &fallback[0],
                            &penalty,
                            fallback_pass.active_samples.max(1) as f64
                                / fallback_pass.hessian_rows.max(1) as f64,
                            &lambdas,
                            &fallback_pass.per_image_gradient,
                            &density,
                            &fallback_pass.residual_samples,
                            options,
                            previous,
                        )?
                    }
                    Err(error) => return Err(error),
                };
                let hessian_scale =
                    final_pass.active_samples.max(1) as f64 / final_pass.hessian_rows.max(1) as f64;
                let (parameter_ids, covariance) = uncertainty::response_covariance(
                    &problem,
                    &blocks,
                    &penalties,
                    &lambdas,
                    hessian_scale,
                    &final_pass.per_image_gradient,
                );
                (probes, parameter_ids, covariance)
            }
            _ => (
                ProbeUncertainty {
                    probes: Vec::new(),
                    ids: Vec::new(),
                    supported_fraction: 0.0,
                },
                Vec::new(),
                Vec::new(),
            ),
        };
    if want_uncertainty && (4..=64).contains(&native.len()) && !probe_result.probes.is_empty() {
        // The published σ includes the same body/image K-fold model-error term the
        // bootstrap path uses, so systematic basis misfit widens support probes.
        let clip_k = params.clip_k();
        let field = estimate_model_error(&native, &[], model.as_ref(), options, clip_k)?;
        let space = problem
            .images
            .first()
            .map_or(looklab_color::RgbSpace::Srgb, |image| image.space);
        apply_model_error(
            &mut probe_result,
            &field,
            &look,
            &density,
            space,
            options,
            clip_k,
            previous,
        );
    }
    let control_response = if reference_mode {
        None
    } else {
        model.map(|template| {
            let (ids, covariance) = if want_uncertainty {
                (parameter_ids, covariance)
            } else {
                (problem.parameter_ids.clone(), Vec::new())
            };
            build_control_response(template, &problem, &params, ids, covariance)
        })
    };

    let mut diagnostics = Diagnostics {
        sweeps: 0,
        evaluations,
        sample_evaluations,
        final_cost: final_pass.cost + prior_cost,
        sigma_v: sigma_v.clone(),
        gcv: lambdas,
        hessian_samples: problem.hessian.len(),
        excluded_samples: problem.excluded_samples,
        excluded_regions: problem.excluded_regions.clone(),
        operator_rows,
        objective_samples: problem.samples.len(),
        quarantined: image_stats
            .iter()
            .filter(|stat| stat.verdict == Verdict::Quarantine)
            .count(),
        down_weighted: image_stats
            .iter()
            .filter(|stat| stat.verdict == Verdict::DownWeight)
            .count(),
        grl_precision_ratio,
        converged: final_converged,
    };
    diagnostics.sweeps = evaluations;

    Ok((
        FitResult {
            look,
            output_space: problem
                .images
                .first()
                .map_or(looklab_color::RgbSpace::Srgb, |image| image.space),
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
            control_response,
            excluded,
        },
        resume_state,
    ))
}

enum FinalOutcome {
    Accepted(Box<PassOutput>),
    Budget,
    Stalled,
}

/// One damped joint step at the frozen linearization, retried at higher damping until the objective decreases or damping caps.
#[allow(clippy::too_many_arguments)]
fn final_stage_step(
    layout: &Layout,
    problem: &Problem,
    stage_problem: &Problem,
    stage: &Stage,
    options: &FitOptions,
    params: &mut Params,
    blocks: &[BlockAcc],
    penalties: &[Vec<f64>],
    lambdas: &StageLambdas,
    roles: &[Role],
    pass: &PassOutput,
    data_scale: f64,
    cost: f64,
    damping: &mut f64,
    gradient_scale: &mut Option<f64>,
    evaluations: &mut usize,
    sample_evaluations: &mut SampleEvaluations,
    samples_per_pass: usize,
    stage_index: usize,
    sweep_index: usize,
    history: usize,
    warm: &mut Option<Vec<f64>>,
) -> FinalOutcome {
    let entry_damping = *damping;
    let model = match JointModel::new(
        layout, problem, params, options, blocks, penalties, lambdas, roles, pass, data_scale,
        true, false,
    ) {
        Ok(model) => model,
        Err(failure) => {
            if fit_diag() {
                eprintln!(
                    "fit stage {stage_index} sweep {sweep_index} joint model failed: {}",
                    failure.label()
                );
            }
            return FinalOutcome::Stalled;
        }
    };
    let outcome = joint_attempts(
        &model,
        layout,
        problem,
        stage_problem,
        stage,
        options,
        params,
        blocks,
        penalties,
        lambdas,
        pass,
        data_scale,
        cost,
        damping,
        gradient_scale,
        evaluations,
        sample_evaluations,
        samples_per_pass,
        stage_index,
        sweep_index,
        history,
        warm,
    );
    if !matches!(outcome, FinalOutcome::Stalled) {
        return outcome;
    }
    if fit_diag() {
        eprintln!(
            "fit stage {stage_index} sweep {sweep_index} falling back to the GN curvature model"
        );
    }
    *damping = entry_damping;
    match JointModel::new(
        layout, problem, params, options, blocks, penalties, lambdas, roles, pass, data_scale,
        false, false,
    ) {
        Ok(model) => joint_attempts(
            &model,
            layout,
            problem,
            stage_problem,
            stage,
            options,
            params,
            blocks,
            penalties,
            lambdas,
            pass,
            data_scale,
            cost,
            damping,
            gradient_scale,
            evaluations,
            sample_evaluations,
            samples_per_pass,
            stage_index,
            sweep_index,
            history,
            warm,
        ),
        Err(failure) => {
            if fit_diag() {
                eprintln!(
                    "fit stage {stage_index} sweep {sweep_index} GN model failed: {}",
                    failure.label()
                );
            }
            outcome
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn joint_attempts(
    model: &JointModel,
    layout: &Layout,
    problem: &Problem,
    stage_problem: &Problem,
    stage: &Stage,
    options: &FitOptions,
    params: &mut Params,
    blocks: &[BlockAcc],
    penalties: &[Vec<f64>],
    lambdas: &StageLambdas,
    pass: &PassOutput,
    data_scale: f64,
    cost: f64,
    damping: &mut f64,
    gradient_scale: &mut Option<f64>,
    evaluations: &mut usize,
    sample_evaluations: &mut SampleEvaluations,
    samples_per_pass: usize,
    stage_index: usize,
    sweep_index: usize,
    history: usize,
    warm: &mut Option<Vec<f64>>,
) -> FinalOutcome {
    let norm = crate::solve::dot(&model.rhs, &model.rhs).sqrt();
    let reference = *gradient_scale.get_or_insert(norm.max(1e-300));
    let tolerance = (0.02 * norm / reference).clamp(1e-3, 0.02);
    let residual_active = blocks.iter().any(|block| block.kind == BlockKind::Residual);
    let mut attempts = 0usize;
    loop {
        if sample_evaluations.get() as f64 + samples_per_pass as f64 > options.sweep_budget {
            return FinalOutcome::Budget;
        }
        attempts += 1;
        let outcome = JointSolve::new(model, *damping)
            .and_then(|solve| solve.solve(model, tolerance, Some(STEP_RADIUS), warm.as_deref()));
        let solution = match outcome {
            Ok(solution) if solution.relative_residual <= tolerance => solution,
            Ok(solution) if solution.relative_residual <= INEXACT_DESCENT_RESIDUAL => {
                // Ordinary descent takes the inexact step; the trial's gain ratio decides it.
                if fit_diag() {
                    eprintln!(
                        "fit stage {stage_index} sweep {sweep_index} inexact step ({}) residual {:.3e} tolerance {:.3e} damping {:.3e} attempt {attempts}",
                        solution.failure.map_or("residual", SolveFailure::label),
                        solution.relative_residual,
                        tolerance,
                        *damping
                    );
                }
                solution
            }
            Ok(solution) => {
                if fit_diag() {
                    eprintln!(
                        "fit stage {stage_index} sweep {sweep_index} joint solve failed ({}) residual {:.3e} tolerance {:.3e} damping {:.3e} attempt {attempts}",
                        solution.failure.map_or("residual", SolveFailure::label),
                        solution.relative_residual,
                        tolerance,
                        *damping
                    );
                }
                *damping = (*damping * 10.0).min(1e6);
                if *damping >= 1e6 || attempts >= 12 {
                    return FinalOutcome::Stalled;
                }
                continue;
            }
            Err(failure) => {
                if fit_diag() {
                    eprintln!(
                        "fit stage {stage_index} sweep {sweep_index} joint solve failed ({}) damping {:.3e} attempt {attempts}",
                        failure.label(),
                        *damping
                    );
                }
                *damping = (*damping * 10.0).min(1e6);
                if *damping >= 1e6 || attempts >= 12 {
                    return FinalOutcome::Stalled;
                }
                continue;
            }
        };
        *warm = Some(solution.delta[..model.outer_count].to_vec());
        let mut step = vec![0.0_f64; layout.total];
        for (index, value) in solution.delta.iter().enumerate() {
            step[model.globals[index]] = *value;
        }
        let residual_delta = if residual_active {
            residual_step(
                &pass.residual_samples,
                &pass.residual_gradient,
                &params.theta[layout.residual.clone()],
                lam(lambdas, BlockKind::Residual),
                *damping,
                data_scale,
                StepLimit::PerBlock,
            )
        } else {
            Vec::new()
        };
        let step_max = step
            .iter()
            .chain(residual_delta.iter())
            .filter(|value| value.is_finite())
            .fold(0.0_f64, |largest, value| largest.max(value.abs()));
        let mut trial_params = params.clone();
        apply_step_unclamped(&mut trial_params, &step);
        for (index, value) in residual_delta.iter().enumerate() {
            trial_params.theta[layout.residual.start + index] += value;
        }
        let mut prediction = solution.prediction;
        let mut residual_part = 0.0_f64;
        let mut cross_part = 0.0_f64;
        if residual_active {
            pin_residual(&mut trial_params, layout, &pass.residual_samples);
            let effective: Vec<f64> = (0..RESIDUAL_COEFS)
                .map(|index| {
                    trial_params.theta[layout.residual.start + index]
                        - params.theta[layout.residual.start + index]
                })
                .collect();
            residual_part =
                residual_prediction(layout, pass, params, &effective, lambdas, data_scale);
            let cross = residual_cross(
                &pass.coupled_rows,
                &layout.residual,
                model.robust,
                &step,
                &effective,
                data_scale,
            );
            cross_part = cross;
            prediction += residual_part - cross;
        }
        let step_worst = step
            .iter()
            .chain(residual_delta.iter())
            .enumerate()
            .filter(|(_, value)| value.is_finite())
            .fold((0usize, 0.0_f64), |(index, largest), (slot, value)| {
                if value.abs() > largest {
                    (slot, value.abs())
                } else {
                    (index, largest)
                }
            });
        let trial = stage_problem.pass(
            &trial_params,
            stage.active,
            &mut [],
            false,
            false,
            stage.neutral,
            IntentFilter::NativeOnly,
        );
        *evaluations += 1;
        *sample_evaluations += samples_per_pass;
        let trial_cost =
            trial.cost + total_cost(problem, &trial_params, blocks, penalties, lambdas, options);
        if trial.infeasible {
            if fit_diag() {
                eprintln!(
                    "fit stage {stage_index} sweep {sweep_index} infeasible trial damping {:.3e} attempt {attempts}",
                    *damping
                );
            }
            *damping = (*damping * 10.0).min(1e6);
            if *damping >= 1e6 || attempts >= 12 {
                return FinalOutcome::Stalled;
            }
            continue;
        }
        let decrease = cost - trial_cost;
        let ratio = if prediction.is_finite() && prediction > 0.0 {
            decrease / prediction
        } else {
            f64::NAN
        };
        let improves = if cost.is_finite() {
            decrease > 1e-12 * cost.abs().max(1.0) && ratio >= 1e-4
        } else {
            true
        };
        if trial_cost.is_finite() && improves {
            *params = trial_params;
            if ratio > 0.75 {
                *damping = (*damping * 0.3).max(1e-6);
            } else if ratio < 0.25 {
                *damping = (*damping * 2.0).min(1e6);
            }
            if fit_diag() {
                eprintln!(
                    "fit stage {stage_index} sweep {sweep_index} accepted trial {trial_cost:.12e} ratio {ratio:.6} prediction {prediction:.9e} joint prediction {:.9e} residual prediction {residual_part:.9e} cross {cross_part:.9e} step {step_max:.9e} at {} damping {:.3e} iterations {} residual {:.3e} history {history}",
                    solution.prediction,
                    step_worst.0,
                    *damping,
                    solution.iterations,
                    solution.relative_residual
                );
            }
            return FinalOutcome::Accepted(Box::new(trial));
        }
        if fit_diag() {
            eprintln!(
                "fit stage {stage_index} sweep {sweep_index} rejected trial {trial_cost:.12e} current {cost:.12e} ratio {ratio:.6} prediction {prediction:.9e} step {step_max:.9e} damping {:.3e} attempt {attempts}",
                *damping
            );
        }
        *damping = (*damping * 10.0).min(1e6);
        if *damping >= 1e6 || attempts >= 12 {
            return FinalOutcome::Stalled;
        }
    }
}

/// Residual-grid share of the model prediction for the applied residual delta.
#[allow(clippy::too_many_arguments)]
fn residual_prediction(
    layout: &Layout,
    pass: &PassOutput,
    params: &Params,
    delta: &[f64],
    lambdas: &StageLambdas,
    data_scale: f64,
) -> f64 {
    let ridge = 2.0 * lam(lambdas, BlockKind::Residual);
    let identity = identity_weight(BlockKind::Residual);
    let penalty = residual_penalty_diagonal();
    let before = &params.theta[layout.residual.clone()];
    let mut rhs = vec![0.0_f64; RESIDUAL_COEFS];
    for index in 0..RESIDUAL_COEFS {
        rhs[index] = pass.residual_gradient[index]
            - ridge * laplacian_at(before, index, &penalty)
            - identity * before[index];
    }
    let mut curvature = vec![0.0_f64; RESIDUAL_COEFS];
    residual_undamped_apply(
        &pass.residual_samples,
        delta,
        &mut curvature,
        &penalty,
        ridge,
        identity,
        data_scale,
    );
    crate::solve::dot(&rhs, delta) - 0.5 * crate::solve::dot(delta, &curvature)
}

/// `δ_lᵀ B_lr δ_r` over the retained rows: the GN, Student-t and barrier cross terms between
/// the joint step and the residual step, projected from the same rows.
fn residual_cross(
    rows: &[crate::problem::HessianRow],
    residual: &core::ops::Range<usize>,
    robust: bool,
    step: &[f64],
    residual_step: &[f64],
    data_scale: f64,
) -> f64 {
    let mut total = 0.0_f64;
    for row in rows {
        if row.residual_columns.is_empty() {
            continue;
        }
        let mut projected = [0.0_f64; 3];
        let mut robust_projection = 0.0_f64;
        let mut margin_projection = 0.0_f64;
        for (position, (column, derivative)) in row.columns.iter().enumerate() {
            let value = step[*column as usize];
            projected[0] += derivative[0] * value;
            projected[1] += derivative[1] * value;
            projected[2] += derivative[2] * value;
            robust_projection += row.robust_terms[position] * value;
            margin_projection += row.margin_terms[position] * value;
        }
        let mut residual_projected = [0.0_f64; 3];
        let mut residual_robust = 0.0_f64;
        let mut residual_margin = 0.0_f64;
        for (column, derivative, robust_term, margin_term) in &row.residual_columns {
            let value = residual_step[*column as usize - residual.start];
            residual_projected[0] += derivative[0] * value;
            residual_projected[1] += derivative[1] * value;
            residual_projected[2] += derivative[2] * value;
            residual_robust += robust_term * value;
            residual_margin += margin_term * value;
        }
        total += row.weight * dot3(projected, residual_projected)
            + if robust {
                row.robust * robust_projection * residual_robust
            } else {
                0.0
            }
            + row.barrier * margin_projection * residual_margin;
    }
    data_scale * total
}

fn carry_params(
    native: &Problem,
    native_params: &Params,
    all: &Problem,
    positions: &[Option<usize>],
    images: &[FitImage],
) -> Params {
    let mut params = Params::initial(all.layout.clone(), all.contributors.len());
    let copy =
        |source: core::ops::Range<usize>, target: core::ops::Range<usize>, params: &mut Params| {
            params.theta[target].copy_from_slice(&native_params.theta[source]);
        };
    copy(
        native.layout.tone_beta.clone(),
        all.layout.tone_beta.clone(),
        &mut params,
    );
    params.theta[all.layout.tone_c0] = native_params.theta[native.layout.tone_c0];
    copy(
        native.layout.tint_a.clone(),
        all.layout.tint_a.clone(),
        &mut params,
    );
    copy(
        native.layout.tint_b.clone(),
        all.layout.tint_b.clone(),
        &mut params,
    );
    copy(native.layout.g.clone(), all.layout.g.clone(), &mut params);
    copy(native.layout.r.clone(), all.layout.r.clone(), &mut params);
    copy(
        native.layout.lambda.clone(),
        all.layout.lambda.clone(),
        &mut params,
    );
    copy(
        native.layout.residual.clone(),
        all.layout.residual.clone(),
        &mut params,
    );
    params.theta[all.layout.clip] = native_params.theta[native.layout.clip];
    if !native.layout.response.is_empty() {
        copy(
            native.layout.response.clone(),
            all.layout.response.clone(),
            &mut params,
        );
    }
    for (index, contributor) in all.contributors.iter().enumerate() {
        if let Some(position) = native
            .contributors
            .iter()
            .position(|entry| entry == contributor)
        {
            params.theta[all.layout.sigma_of(index)] =
                native_params.theta[native.layout.sigma_of(position)];
        }
    }
    for (index, mode) in all.modes.iter().enumerate() {
        if let Some(position) = native.modes.iter().position(|entry| entry == mode) {
            params.theta[all.layout.mode_of(index)] =
                native_params.theta[native.layout.mode_of(position)];
        }
    }
    for (index, image) in images.iter().enumerate() {
        let target = all.layout.image(index);
        match positions[index] {
            Some(position) => {
                let source = native.layout.image(position);
                params.theta[target.start..target.end]
                    .copy_from_slice(&native_params.theta[source.start..source.end]);
            }
            None => {
                params.theta[target.start] = image.exposure_hint.clamp(-8.0, 8.0);
            }
        }
    }
    params
}

fn run_frozen_responses(
    problem: &Problem,
    params: &mut Params,
    options: &FitOptions,
    lambdas: &mut StageLambdas,
    evaluations: &mut usize,
    intents: IntentFilter,
) -> Result<()> {
    let kinds = [
        BlockKind::Nuisance,
        BlockKind::ResponseTone,
        BlockKind::ResponseTint,
        BlockKind::ResponseChroma,
        BlockKind::ResponseHue,
        BlockKind::ResponseLightness,
        BlockKind::ResponseWb,
    ];
    for kind in &kinds[1..] {
        let value = lam(lambdas, *kind).max(RESPONSE_LAMBDA_FLOOR);
        set_lam(lambdas, *kind, value);
    }
    let mut blocks: Vec<BlockAcc> = kinds
        .iter()
        .map(|kind| make_block(problem, *kind, ActiveColumns::ALL))
        .collect();
    let penalties: Vec<Vec<f64>> = blocks
        .iter()
        .map(|block| block_penalty(problem, block))
        .collect();
    let layout = problem.layout.clone();
    let roles = prior_roles(problem, &layout);
    let mut damping = 1e-3;
    for _ in 0..options.recipe_sweeps.max(1) {
        for block in &mut blocks {
            block.reset();
        }
        let pass = problem.pass(
            params,
            ActiveColumns::ALL,
            &mut blocks,
            false,
            false,
            false,
            intents,
        );
        *evaluations += 1;
        let hessian_scale = pass.active_samples.max(1) as f64 / pass.hessian_rows.max(1) as f64;
        let mut step = vec![0.0_f64; layout.total];
        for (index, block) in blocks.iter().enumerate() {
            let (prior_gradient, prior_curvature) =
                prior_terms(problem, &roles, params, options, block);
            solve_block(
                &layout,
                block,
                &mut step,
                &penalties[index],
                &prior_gradient,
                &prior_curvature,
                params,
                lambdas,
                damping,
                hessian_scale,
                StepLimit::WholeStep,
            );
        }
        apply_step(params, &step);
        update_sigma(problem, params, &pass, options);
        damping = (damping * 0.3).max(1e-6);
    }
    Ok(())
}

pub(crate) fn run_responses(
    look: &looklab_model::Look,
    images: &[FitImage],
    schema: &ControlSchema,
    epoch: u32,
    base_sim: &looklab_schema::SimKey,
    model: &ControlResponseModel,
    options: &FitOptions,
) -> Result<ControlResponseFit> {
    let template = ControlResponseModel::zeroed(schema, epoch, base_sim, model.version)?;
    let problem = Problem::new_with(images, Some(&template), options)?;
    let layout = problem.layout.clone();
    let mut params = params_from_look(&problem, look);
    seed_response_params(&problem, &mut params, model);
    let kinds = [
        BlockKind::Nuisance,
        BlockKind::ResponseTone,
        BlockKind::ResponseTint,
        BlockKind::ResponseChroma,
        BlockKind::ResponseHue,
        BlockKind::ResponseLightness,
        BlockKind::ResponseWb,
    ];
    let mut blocks: Vec<BlockAcc> = kinds
        .iter()
        .map(|kind| make_block(&problem, *kind, ActiveColumns::ALL))
        .collect();
    let penalties: Vec<Vec<f64>> = blocks
        .iter()
        .map(|block| block_penalty(&problem, block))
        .collect();
    let lambdas = StageLambdas::default();
    let roles = prior_roles(&problem, &layout);
    let mut damping = 1e-3;
    for _ in 0..options.recipe_sweeps.max(1) {
        for block in &mut blocks {
            block.reset();
        }
        let pass = problem.pass(
            &params,
            ActiveColumns::ALL,
            &mut blocks,
            false,
            false,
            false,
            IntentFilter::All,
        );
        let hessian_scale = pass.active_samples.max(1) as f64 / pass.hessian_rows.max(1) as f64;
        let mut step = vec![0.0_f64; layout.total];
        for (index, block) in blocks.iter().enumerate() {
            let (prior_gradient, prior_curvature) =
                prior_terms(&problem, &roles, &params, options, block);
            solve_block(
                &layout,
                block,
                &mut step,
                &penalties[index],
                &prior_gradient,
                &prior_curvature,
                &params,
                &lambdas,
                damping,
                hessian_scale,
                StepLimit::WholeStep,
            );
        }
        apply_step(&mut params, &step);
        update_sigma(&problem, &mut params, &pass, options);
        damping = (damping * 0.3).max(1e-6);
    }
    let mut blocks: Vec<BlockAcc> = kinds
        .iter()
        .map(|kind| make_block(&problem, *kind, ActiveColumns::ALL))
        .collect();
    let penalties: Vec<Vec<f64>> = blocks
        .iter()
        .map(|block| block_penalty(&problem, block))
        .collect();
    let final_pass = problem.pass(
        &params,
        ActiveColumns::ALL,
        &mut blocks,
        false,
        true,
        false,
        IntentFilter::All,
    );
    let hessian_scale =
        final_pass.active_samples.max(1) as f64 / final_pass.hessian_rows.max(1) as f64;
    let (parameter_ids, covariance) = uncertainty::response_covariance(
        &problem,
        &blocks,
        &penalties,
        &lambdas,
        hessian_scale,
        &final_pass.per_image_gradient,
    );
    Ok(build_control_response(
        template,
        &problem,
        &params,
        parameter_ids,
        covariance,
    ))
}

pub(crate) fn params_from_look(problem: &Problem, look: &looklab_model::Look) -> Params {
    let mut params = Params::initial(problem.layout.clone(), problem.contributors.len());
    apply_look(problem, &mut params, look);
    params
}

fn apply_look(problem: &Problem, params: &mut Params, look: &looklab_model::Look) {
    let layout = look.layout();
    let coef = look.coef_vector();
    if let looklab_model::Tone::Spline { c0, beta } = look.tone {
        params.theta[problem.layout.tone_c0] = c0;
        for (k, value) in beta.iter().enumerate() {
            params.theta[problem.layout.tone_beta.start + k] =
                crate::layout::softplus_inverse(*value);
        }
    }
    params.theta[problem.layout.tint_a.clone()].copy_from_slice(&coef[layout.tint_a.clone()]);
    params.theta[problem.layout.tint_b.clone()].copy_from_slice(&coef[layout.tint_b.clone()]);
    params.theta[problem.layout.g.clone()].copy_from_slice(&coef[layout.g.clone()]);
    params.theta[problem.layout.r.clone()].copy_from_slice(&coef[layout.r.clone()]);
    params.theta[problem.layout.lambda.clone()].copy_from_slice(&coef[layout.lambda.clone()]);
    params.theta[problem.layout.residual.clone()].copy_from_slice(&coef[layout.residual.clone()]);
    params.theta[problem.layout.clip] = crate::layout::zeta_from_clip(look.clip.k);
}

pub(crate) fn seed_mode_params(
    problem: &Problem,
    params: &mut Params,
    model: &ControlResponseModel,
) {
    for (index, mode) in model.exposure.modes.iter().enumerate() {
        if let Some(position) = problem
            .modes
            .iter()
            .position(|candidate| candidate.0 as usize == index)
        {
            params.theta[problem.layout.mode_of(position)] = mode.ev;
        }
    }
}

pub(crate) fn seed_response_params(
    problem: &Problem,
    params: &mut Params,
    model: &ControlResponseModel,
) {
    for (index, id) in problem.parameter_ids.iter().enumerate() {
        let value = match id.component {
            ResponseComponent::WbR | ResponseComponent::WbB => model
                .wb
                .controls
                .iter()
                .find(|control| control.key == id.control)
                .and_then(|control| control.values.iter().find(|value| value.value == id.value))
                .map(|value| {
                    if id.component == ResponseComponent::WbR {
                        value.log_gain[0]
                    } else {
                        value.log_gain[1]
                    }
                })
                .unwrap_or(0.0),
            _ => {
                let layer = match id.component {
                    ResponseComponent::Tone => LayerKind::Tone,
                    ResponseComponent::Tint => LayerKind::Tint,
                    ResponseComponent::Chroma => LayerKind::Chroma,
                    ResponseComponent::Hue => LayerKind::Hue,
                    ResponseComponent::Lightness => LayerKind::Lightness,
                    ResponseComponent::WbR | ResponseComponent::WbB => unreachable!(),
                };
                model
                    .controls
                    .iter()
                    .find(|control| control.key == id.control)
                    .and_then(|control| control.values.iter().find(|value| value.value == id.value))
                    .and_then(|value| value.delta.layers.iter().find(|delta| delta.layer == layer))
                    .and_then(|delta| {
                        delta
                            .entries
                            .iter()
                            .find(|(feature, _)| *feature == id.feature)
                    })
                    .map(|(_, value)| *value)
                    .unwrap_or(0.0)
            }
        };
        params.theta[problem.layout.response.start + index] = value;
    }
}

pub(crate) fn build_control_response(
    mut model: ControlResponseModel,
    problem: &Problem,
    params: &Params,
    parameter_ids: Vec<ResponseParamId>,
    covariance: Vec<f64>,
) -> ControlResponseFit {
    let index_of = |control: &looklab_schema::ControlKey,
                    value: &looklab_model::realization::ControlValue,
                    component: ResponseComponent,
                    feature: u32| {
        parameter_ids.iter().position(|id| {
            &id.control == control
                && &id.value == value
                && id.component == component
                && id.feature == feature
        })
    };
    let coefficient = |index: Option<usize>| match index {
        Some(index) => params.theta[problem.layout.response.start + index],
        None => 0.0,
    };
    for control in &mut model.controls {
        let layers = control.layers();
        for value in &mut control.values {
            let mut delta = looklab_model::realization::Delta::default();
            for layer in &layers {
                let entries: Vec<(u32, f64)> = (0..layer.feature_count())
                    .filter_map(|feature| {
                        let coefficient = coefficient(index_of(
                            &control.key,
                            &value.value,
                            (*layer).into(),
                            feature,
                        ));
                        (coefficient != 0.0).then_some((feature, coefficient))
                    })
                    .collect();
                if !entries.is_empty() {
                    delta.layers.push(looklab_model::realization::LayerDelta {
                        layer: *layer,
                        entries,
                    });
                }
            }
            value.delta = delta;
        }
    }
    for control in &mut model.wb.controls {
        for value in &mut control.values {
            value.log_gain = [
                coefficient(index_of(
                    &control.key,
                    &value.value,
                    ResponseComponent::WbR,
                    0,
                )),
                coefficient(index_of(
                    &control.key,
                    &value.value,
                    ResponseComponent::WbB,
                    0,
                )),
            ];
        }
    }
    for (index, mode) in model.exposure.modes.iter_mut().enumerate() {
        mode.ev = if index == 0 {
            0.0
        } else {
            problem
                .modes
                .iter()
                .position(|mode_id| mode_id.0 as usize == index)
                .map(|position| params.mode_offset(position))
                .unwrap_or(0.0)
        };
    }
    model.support = build_support(&model, problem);
    ControlResponseFit {
        model,
        parameter_ids,
        covariance,
    }
}

fn build_support(
    model: &ControlResponseModel,
    problem: &Problem,
) -> looklab_model::realization::Support {
    use looklab_model::realization::{ControlSupport, Support};
    let mut controls = Vec::new();
    let mut add = |key: &looklab_schema::ControlKey,
                   default: &looklab_model::realization::ControlValue| {
        let mut observed = vec![default.clone()];
        for settings in &problem.settings {
            let value = settings.get(key).unwrap_or(default).clone();
            if !observed.contains(&value) {
                observed.push(value);
            }
        }
        controls.push(ControlSupport {
            key: key.clone(),
            default: default.clone(),
            observed,
        });
    };
    for control in &model.controls {
        add(&control.key, &control.default);
    }
    for control in &model.wb.controls {
        add(&control.key, &control.default);
    }
    for step in &model.exposure.steps {
        let default = looklab_model::realization::ControlValue::Step(step.default);
        add(&step.key, &default);
    }
    Support { controls }
}

fn initial_params(problem: &Problem, options: &FitOptions) -> Params {
    let mut params = Params::initial(problem.layout.clone(), problem.contributors.len());
    for (index, image) in problem.images.iter().enumerate() {
        let range = params.layout.image(index);
        params.theta[range.start] = image.exposure_hint.clamp(-8.0, 8.0);
    }
    // The clip prior mean is the declared clip.
    params.theta[problem.layout.clip] = zeta_from_clip(options.clip_prior.0);
    params
}

fn make_block(problem: &Problem, kind: BlockKind, active: ActiveColumns) -> BlockAcc {
    let layout = &problem.layout;
    let params: Vec<usize> = match kind {
        BlockKind::Tone => {
            let mut out = vec![layout.tone_c0];
            out.extend(layout.tone_beta.clone());
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
        BlockKind::Nuisance => {
            let mut out = Vec::new();
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
        BlockKind::ResponseTone
        | BlockKind::ResponseTint
        | BlockKind::ResponseChroma
        | BlockKind::ResponseHue
        | BlockKind::ResponseLightness
        | BlockKind::ResponseWb => {
            if !active.response {
                Vec::new()
            } else {
                let component = response_component(kind);
                problem
                    .parameter_ids
                    .iter()
                    .enumerate()
                    .filter(|(_, id)| match kind {
                        BlockKind::ResponseWb => matches!(
                            id.component,
                            ResponseComponent::WbR | ResponseComponent::WbB
                        ),
                        _ => Some(id.component) == component,
                    })
                    .map(|(index, _)| layout.response.start + index)
                    .collect()
            }
        }
    };
    BlockAcc::new(kind, params, layout.total)
}

/// Tone block with the concatenated nuisances, for pools below the split
/// threshold where the joint conditioning is better.
fn make_joint_tone_block(problem: &Problem, active: ActiveColumns) -> BlockAcc {
    let layout = &problem.layout;
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
    BlockAcc::new(BlockKind::Tone, out, layout.total)
}

/// Nuisance block for one image: exposure, `log w_R`, `log w_B`.
fn make_image_block(layout: &Layout, index: usize, active: ActiveColumns) -> BlockAcc {
    let range = layout.image(index);
    let mut params = Vec::with_capacity(3);
    if active.image {
        params.push(range.start);
    }
    if active.wb {
        params.push(range.start + 1);
        params.push(range.start + 2);
    }
    let mut block = BlockAcc::new(BlockKind::Nuisance, params, layout.total);
    block.image = Some(index);
    block
}

/// Mode offsets and the clip parameter.
fn make_shared_nuisance_block(
    problem: &Problem,
    layout: &Layout,
    active: ActiveColumns,
) -> BlockAcc {
    let mut params = Vec::new();
    if active.mode {
        params.extend(layout.modes.clone());
    }
    if active.clip {
        params.push(layout.clip);
    }
    let _ = problem;
    BlockAcc::new(BlockKind::Nuisance, params, layout.total)
}

fn is_response_kind(kind: BlockKind) -> bool {
    matches!(
        kind,
        BlockKind::ResponseTone
            | BlockKind::ResponseTint
            | BlockKind::ResponseChroma
            | BlockKind::ResponseHue
            | BlockKind::ResponseLightness
            | BlockKind::ResponseWb
    )
}

fn response_component(kind: BlockKind) -> Option<ResponseComponent> {
    match kind {
        BlockKind::ResponseTone => Some(ResponseComponent::Tone),
        BlockKind::ResponseTint => Some(ResponseComponent::Tint),
        BlockKind::ResponseChroma => Some(ResponseComponent::Chroma),
        BlockKind::ResponseHue => Some(ResponseComponent::Hue),
        BlockKind::ResponseLightness => Some(ResponseComponent::Lightness),
        _ => None,
    }
}

/// One dense block over the look coefficients plus the active image nuisances, modes and clip.
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
    if active.mode {
        params.extend(layout.modes.clone());
    }
    if active.clip {
        params.push(layout.clip);
    }
    if active.response {
        params.extend(layout.response.clone());
    }
    BlockAcc::new(BlockKind::Tone, params, layout.total)
}

/// Block-diagonal penalty over the joint block, with each section's λ folded in.
fn combined_look_penalty(
    problem: &Problem,
    layout: &Layout,
    lambdas: &StageLambdas,
    n: usize,
    include_response: bool,
) -> Vec<f64> {
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
    if include_response && !layout.response.is_empty() {
        let response = response_penalty_all(problem, lambdas, layout.response.len());
        place(
            &mut out,
            n,
            &response,
            layout.response.len(),
            n - layout.response.len(),
            1.0,
        );
    }
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
            for term in 0..CORE_TERMS {
                for row in basis::second_difference(S_COEFS, false) {
                    let indices: Vec<(usize, f64)> = row
                        .iter()
                        .enumerate()
                        .filter(|(_, value)| **value != 0.0)
                        .map(|(s_index, value)| {
                            (S_COEFS + VOLUME_COEFS + s_index * CORE_TERMS + term, *value)
                        })
                        .collect();
                    accumulate_row(&mut s, n, &indices);
                }
            }
            s
        }
        BlockKind::Residual
        | BlockKind::Nuisance
        | BlockKind::ResponseTone
        | BlockKind::ResponseTint
        | BlockKind::ResponseChroma
        | BlockKind::ResponseHue
        | BlockKind::ResponseLightness
        | BlockKind::ResponseWb => Vec::new(),
    }
}

type ResponseGroup = (
    looklab_schema::ControlKey,
    ResponseComponent,
    Vec<(u32, i64, usize)>,
);

const RESPONSE_LAMBDA_FLOOR: f64 = 1.0;

/// Shrinkage plus second differences over adjacent steps, per control, component and feature.
fn response_penalty(
    problem: &Problem,
    params: &[usize],
    wb_only: bool,
    component: Option<ResponseComponent>,
) -> Vec<f64> {
    let n = params.len();
    let mut out = vec![0.0_f64; n * n];
    for local in 0..n {
        out[local * n + local] += 1.0;
    }
    let mut groups: Vec<ResponseGroup> = Vec::new();
    for (local, global) in params.iter().enumerate() {
        let Some(id) = problem
            .parameter_ids
            .get(*global - problem.layout.response.start)
        else {
            continue;
        };
        let is_wb = matches!(
            id.component,
            ResponseComponent::WbR | ResponseComponent::WbB
        );
        if is_wb != wb_only || component.is_some_and(|value| value != id.component) {
            continue;
        }
        let Some(offset) = offset_from_default(problem, id) else {
            continue;
        };
        match groups
            .iter_mut()
            .find(|(key, entry, _)| key == &id.control && *entry == id.component)
        {
            Some((_, _, entries)) => entries.push((id.feature, offset, local)),
            None => groups.push((
                id.control.clone(),
                id.component,
                vec![(id.feature, offset, local)],
            )),
        }
    }
    for (_, _, entries) in groups {
        let mut features: Vec<u32> = entries.iter().map(|(feature, _, _)| *feature).collect();
        features.sort_unstable();
        features.dedup();
        for feature in features {
            let mut points: Vec<(i64, Option<usize>)> = vec![(0, None)];
            points.extend(
                entries
                    .iter()
                    .filter(|(candidate, _, _)| *candidate == feature)
                    .map(|(_, offset, local)| (*offset, Some(*local))),
            );
            points.sort_by_key(|(offset, _)| *offset);
            points.dedup_by_key(|(offset, _)| *offset);
            for window in points.windows(3) {
                let mut indices = Vec::new();
                for (position, (_, local)) in window.iter().enumerate() {
                    if let Some(local) = local {
                        let weight = match position {
                            0 => 1.0,
                            1 => -2.0,
                            _ => 1.0,
                        };
                        indices.push((*local, weight));
                    }
                }
                accumulate_row(&mut out, n, &indices);
            }
        }
    }
    out
}

fn offset_from_default(problem: &Problem, id: &ResponseParamId) -> Option<i64> {
    let ControlValue::Step(step) = &id.value else {
        return None;
    };
    let (_, default) = problem
        .defaults
        .iter()
        .find(|(key, _)| key == &id.control)?;
    let ControlValue::Step(default) = default else {
        return None;
    };
    Some(step - default)
}

fn block_penalty(problem: &Problem, block: &BlockAcc) -> Vec<f64> {
    match block.kind {
        BlockKind::Nuisance => vec![0.0; block.len() * block.len()],
        BlockKind::ResponseTone
        | BlockKind::ResponseTint
        | BlockKind::ResponseChroma
        | BlockKind::ResponseHue
        | BlockKind::ResponseLightness
        | BlockKind::ResponseWb => {
            let component = response_component(block.kind);
            double(response_penalty(
                problem,
                &block.params,
                block.kind == BlockKind::ResponseWb,
                component,
            ))
        }
        kind => double(embed(penalty_for(kind), block.len())),
    }
}

fn response_penalty_all(problem: &Problem, lambdas: &StageLambdas, n: usize) -> Vec<f64> {
    let mut out = vec![0.0_f64; n * n];
    let all: Vec<usize> = problem.layout.response.clone().collect();
    let sections = [
        (Some(ResponseComponent::Tone), lambdas.response_tone, false),
        (Some(ResponseComponent::Tint), lambdas.response_tint, false),
        (
            Some(ResponseComponent::Chroma),
            lambdas.response_chroma,
            false,
        ),
        (Some(ResponseComponent::Hue), lambdas.response_hue, false),
        (
            Some(ResponseComponent::Lightness),
            lambdas.response_lightness,
            false,
        ),
        (None, lambdas.response_wb, true),
    ];
    for (component, lambda, wb_only) in sections {
        let params: Vec<usize> = all
            .iter()
            .copied()
            .filter(|global| {
                problem
                    .parameter_ids
                    .get(*global - problem.layout.response.start)
                    .is_some_and(|id| {
                        let is_wb = matches!(
                            id.component,
                            ResponseComponent::WbR | ResponseComponent::WbB
                        );
                        is_wb == wb_only && component.is_none_or(|value| value == id.component)
                    })
            })
            .collect();
        if params.is_empty() {
            continue;
        }
        let section = response_penalty(problem, &params, wb_only, component);
        for (a, global_a) in params.iter().enumerate() {
            for (b, global_b) in params.iter().enumerate() {
                out[(*global_a - problem.layout.response.start) * n
                    + (*global_b - problem.layout.response.start)] +=
                    lambda * section[a * params.len() + b];
            }
        }
    }
    out
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

/// Fixed prior precision (1/sd²) for shrinkage toward the identity look.
///
/// SDs: tone 1 J' per stop, tint 2 J'a'b', log G 0.3, hue 15°, lightness 8 J',
/// residual 0.5 ΔE (docs/02 §4.1).
pub(crate) fn identity_weight(kind: BlockKind) -> f64 {
    match kind {
        BlockKind::Tone => 1.0,
        BlockKind::Tint => 1.0 / (2.0 * 2.0),
        BlockKind::G => 1.0 / (0.3 * 0.3),
        BlockKind::R => 1.0 / (15.0 * 15.0),
        BlockKind::Lambda => 1.0 / (8.0 * 8.0),
        BlockKind::Residual => 1.0 / (0.5 * 0.5),
        _ => 0.0,
    }
}

fn kind_of(layout: &Layout, global: usize) -> Option<BlockKind> {
    if layout.tone_beta.contains(&global) || global == layout.tone_c0 {
        Some(BlockKind::Tone)
    } else if layout.tint_a.contains(&global) || layout.tint_b.contains(&global) {
        Some(BlockKind::Tint)
    } else if layout.g.contains(&global) {
        Some(BlockKind::G)
    } else if layout.r.contains(&global) {
        Some(BlockKind::R)
    } else if layout.lambda.contains(&global) {
        Some(BlockKind::Lambda)
    } else if layout.residual.contains(&global) {
        Some(BlockKind::Residual)
    } else {
        None
    }
}

/// Identity-prior curvature matrix over a block.
pub(crate) fn identity_matrix(layout: &Layout, block: &BlockAcc) -> Vec<f64> {
    let n = block.len();
    let mut out = vec![0.0_f64; n * n];
    for (local, global) in block.params.iter().enumerate() {
        out[local * n + local] =
            layout.identity_scale * kind_of(layout, *global).map_or(0.0, identity_weight);
    }
    out
}

/// `Λ_id · x` restricted to the block's coordinates.
pub(crate) fn identity_apply(layout: &Layout, block: &BlockAcc, x: &[f64]) -> Vec<f64> {
    let mut out = vec![0.0_f64; block.len()];
    for (local, global) in block.params.iter().enumerate() {
        out[local] = layout.identity_scale
            * kind_of(layout, *global).map_or(0.0, identity_weight)
            * x[*global];
    }
    out
}

/// Diagonal of the identity-prior curvature, for GCV's diagonal approximation.
pub(crate) fn identity_curvature(layout: &Layout, block: &BlockAcc) -> Vec<f64> {
    let mut diagonal = vec![0.0_f64; block.len()];
    for (local, global) in block.params.iter().enumerate() {
        diagonal[local] =
            layout.identity_scale * kind_of(layout, *global).map_or(0.0, identity_weight);
    }
    diagonal
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
        BlockKind::Nuisance => 0.0,
        BlockKind::ResponseTone => lambdas.response_tone,
        BlockKind::ResponseTint => lambdas.response_tint,
        BlockKind::ResponseChroma => lambdas.response_chroma,
        BlockKind::ResponseHue => lambdas.response_hue,
        BlockKind::ResponseLightness => lambdas.response_lightness,
        BlockKind::ResponseWb => lambdas.response_wb,
    }
}

fn block_lambda(block: &BlockAcc, lambdas: &StageLambdas) -> f64 {
    lam(lambdas, block.kind)
}

fn set_lam(lambdas: &mut StageLambdas, kind: BlockKind, value: f64) {
    match kind {
        BlockKind::Tone => lambdas.tone = value,
        BlockKind::Tint => lambdas.tint = value,
        BlockKind::G | BlockKind::R | BlockKind::Lambda => lambdas.grl = value,
        BlockKind::Residual => lambdas.residual = value,
        BlockKind::Nuisance => {}
        BlockKind::ResponseTone => lambdas.response_tone = value,
        BlockKind::ResponseTint => lambdas.response_tint = value,
        BlockKind::ResponseChroma => lambdas.response_chroma = value,
        BlockKind::ResponseHue => lambdas.response_hue = value,
        BlockKind::ResponseLightness => lambdas.response_lightness = value,
        BlockKind::ResponseWb => lambdas.response_wb = value,
    }
}

/// Energy of the fixed identity prior over a block.
fn identity_cost(layout: &Layout, block: &BlockAcc, theta: &[f64]) -> f64 {
    let applied = identity_apply(layout, block, theta);
    let mut value = 0.0;
    for (local, global) in block.params.iter().enumerate() {
        value += 0.5 * theta[*global] * applied[local];
    }
    value
}

fn penalty_value(theta: &[f64], params: &[usize], penalty: &[f64]) -> f64 {
    let n = params.len();
    let mut value = 0.0;
    // `penalty` holds 2P.
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
    0.5 * value
}

fn double(mut matrix: Vec<f64>) -> Vec<f64> {
    for value in &mut matrix {
        *value *= 2.0;
    }
    matrix
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
            cost += residual_cost(
                &problem.layout,
                &params.theta,
                lam(lambdas, BlockKind::Residual),
            );
            continue;
        }
        cost += block_lambda(block, lambdas)
            * penalty_value(&params.theta, &block.params, &penalties[index]);
        cost += identity_cost(&problem.layout, block, &params.theta);
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
            reg += residual_cost(
                &problem.layout,
                &params.theta,
                lam(lambdas, BlockKind::Residual),
            );
            continue;
        }
        reg += block_lambda(block, lambdas)
            * penalty_value(&params.theta, &block.params, &penalties[index]);
        reg += identity_cost(&problem.layout, block, &params.theta);
    }
    (prior_cost(problem, params, options) + reg, reg)
}

fn choose_lambdas(
    problem: &Problem,
    params: &Params,
    pass: &PassOutput,
    blocks: &[BlockAcc],
    penalties: &[Vec<f64>],
    lambdas: &mut StageLambdas,
    options: &FitOptions,
) {
    // The objective is tempered by n_eff per image, so the effective observation count is
    // `3 * n_eff * n_images`, not the raw sample count (docs/02 §4.1).
    let observations = 3.0 * options.n_eff * problem.active_images.max(1) as f64;
    let hessian_scale = pass.active_samples.max(1) as f64 / pass.hessian_rows.max(1) as f64;
    let roles = prior_roles(problem, &problem.layout);
    for (index, block) in blocks.iter().enumerate() {
        if block.kind == BlockKind::Nuisance || lam(lambdas, block.kind) != 0.0 {
            continue;
        }
        if fit_diag() {
            eprintln!("fit gcv search block {:?}", block.kind);
        }
        let choice = if block.kind == BlockKind::Residual {
            let identity = identity_weight(BlockKind::Residual);
            let diagonal: Vec<f64> = residual_diagonal(&pass.residual_samples)
                .into_iter()
                .map(|value| value * hessian_scale)
                .collect();
            let fixed_diagonal = vec![identity; diagonal.len()];
            let penalty_diagonal: Vec<f64> = residual_penalty_diagonal()
                .into_iter()
                .map(|v| 2.0 * v)
                .collect();
            let (residual_rhs, rss) = residual_gradient(&pass.residual_samples);
            let gradient: Vec<f64> = residual_rhs
                .into_iter()
                .map(|value| -value * hessian_scale)
                .collect();
            let theta = &params.theta[problem.layout.residual.clone()];
            let fixed_gradient: Vec<f64> = theta.iter().map(|value| identity * value).collect();
            select_lambda_diagonal(
                &diagonal,
                &fixed_diagonal,
                &penalty_diagonal,
                &gradient,
                &fixed_gradient,
                theta,
                (rss * hessian_scale).max(1e-12),
                observations,
            )
        } else {
            let theta0: Vec<f64> = block
                .params
                .iter()
                .map(|index| params.theta[*index])
                .collect();
            let (prior_gradient, prior_curvature) =
                prior_terms(problem, &roles, params, options, block);
            let identity = identity_curvature(&problem.layout, block);
            let fixed_curvature: Vec<f64> = identity
                .iter()
                .zip(prior_curvature.iter())
                .map(|(left, right)| left + right)
                .collect();
            let fixed_gradient: Vec<f64> = prior_gradient
                .iter()
                .zip(identity.iter())
                .zip(theta0.iter())
                .map(|((prior, curvature), theta)| prior + curvature * theta)
                .collect();
            let hessian: Vec<f64> = block
                .gn_hessian
                .iter()
                .map(|value| value * hessian_scale)
                .collect();
            let gradient: Vec<f64> = block
                .hessian_gradient
                .iter()
                .map(|value| value * hessian_scale)
                .collect();
            select_lambda(
                &hessian,
                &fixed_curvature,
                &penalties[index],
                &gradient,
                &fixed_gradient,
                &theta0,
                (pass.hessian_rss * hessian_scale).max(1e-12),
                block.len(),
                observations,
                4,
                options.seed,
            )
        };
        let mut value = choice.lambda.max(1e-12);
        if matches!(
            block.kind,
            BlockKind::ResponseTone
                | BlockKind::ResponseTint
                | BlockKind::ResponseChroma
                | BlockKind::ResponseHue
                | BlockKind::ResponseLightness
                | BlockKind::ResponseWb
        ) {
            value = value.max(RESPONSE_LAMBDA_FLOOR);
        }
        set_lam(lambdas, block.kind, value);
        if fit_diag() {
            eprintln!(
                "fit gcv block {:?} choice {:.6e} used {value:.6e} admissible {} scale {:.6e}",
                block.kind, choice.lambda, choice.admissible, choice.scale
            );
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Exposure(usize),
    Wb(usize),
    Mode(usize),
    Clip,
    None,
}

fn prior_roles(problem: &Problem, layout: &Layout) -> Vec<Role> {
    let mut roles = vec![Role::None; layout.total];
    for index in 0..problem_image_count(layout) {
        let range = layout.image(index);
        roles[range.start] = Role::Exposure(index);
        roles[range.start + 1] = Role::Wb(index);
        roles[range.start + 2] = Role::Wb(index);
    }
    for index in 0..problem.modes.len() {
        roles[layout.mode_of(index)] = Role::Mode(index);
    }
    roles[layout.clip] = Role::Clip;
    roles
}

/// Prior gradient and curvature in the fitted (unconstrained) coordinates.
fn prior_terms(
    problem: &Problem,
    roles: &[Role],
    params: &Params,
    options: &FitOptions,
    block: &BlockAcc,
) -> (Vec<f64>, Vec<f64>) {
    let n = block.len();
    let mut gradient = vec![0.0_f64; n];
    let mut curvature = vec![0.0_f64; n];
    let exposure = 1.0 / (options.exposure_prior_sd * options.exposure_prior_sd);
    let wb = 1.0 / (options.wb_prior_sd * options.wb_prior_sd);
    let mode = 1.0 / (options.exposure_mode_prior_sd * options.exposure_mode_prior_sd);
    let (mean, sd) = options.clip_prior;
    for (local, global) in block.params.iter().enumerate() {
        match roles[*global] {
            Role::Exposure(image) => {
                let hint = problem.images[image].exposure_hint;
                gradient[local] = (params.theta[*global] - hint) * exposure;
                curvature[local] = exposure;
            }
            Role::Wb(_) => {
                gradient[local] = params.theta[*global] * wb;
                curvature[local] = wb;
            }
            Role::Mode(_) => {
                gradient[local] = params.theta[*global] * mode;
                curvature[local] = mode;
            }
            Role::Clip => {
                let slope = params.clip_slope();
                gradient[local] = (params.clip_k() - mean) / (sd * sd) * slope;
                curvature[local] = slope * slope / (sd * sd);
            }
            Role::None => {}
        }
    }
    (gradient, curvature)
}

#[allow(clippy::too_many_arguments)]
fn block_gradients(
    layout: &Layout,
    problem: &Problem,
    params: &Params,
    options: &FitOptions,
    block: &BlockAcc,
    penalty: &[f64],
    lambda: f64,
    roles: &[Role],
) -> Vec<f64> {
    if block.params.is_empty() {
        return Vec::new();
    }
    let (prior_gradient, _) = prior_terms(problem, roles, params, options, block);
    let identity = identity_apply(layout, block, &params.theta);
    let n = block.len();
    (0..n)
        .map(|local| {
            let mut regularizer = 0.0_f64;
            for other in 0..n {
                regularizer += penalty[local * n + other] * params.theta[block.params[other]];
            }
            block.gradient[local] + prior_gradient[local] + identity[local] + lambda * regularizer
        })
        .collect()
}

/// Largest penalized-objective derivative over one block, with the parameter carrying it.
#[allow(clippy::too_many_arguments)]
fn block_gradient_peak(
    layout: &Layout,
    problem: &Problem,
    params: &Params,
    options: &FitOptions,
    block: &BlockAcc,
    penalty: &[f64],
    lambda: f64,
    roles: &[Role],
) -> (f64, Option<usize>) {
    let gradients = block_gradients(
        layout, problem, params, options, block, penalty, lambda, roles,
    );
    let mut largest = 0.0_f64;
    let mut worst = None;
    for (local, gradient) in gradients.iter().enumerate() {
        if gradient.abs() > largest {
            largest = gradient.abs();
            worst = Some(block.params[local]);
        }
    }
    (largest, worst)
}

/// Inputs shared by [`gradient_max`] and its block scans.
struct GradientArgs<'a> {
    layout: &'a Layout,
    problem: &'a Problem,
    params: &'a Params,
    options: &'a FitOptions,
    blocks: &'a [BlockAcc],
    penalties: &'a [Vec<f64>],
    lambdas: &'a StageLambdas,
    roles: &'a [Role],
}

/// Largest absolute derivative of the penalized objective over the active blocks.
fn gradient_max(args: GradientArgs<'_>) -> f64 {
    args.blocks
        .iter()
        .enumerate()
        .filter(|(_, block)| block.kind != BlockKind::Residual && !block.params.is_empty())
        .map(|(index, block)| {
            block_gradient_peak(
                args.layout,
                args.problem,
                args.params,
                args.options,
                block,
                &args.penalties[index],
                block_lambda(block, args.lambdas),
                args.roles,
            )
            .0
        })
        .fold(0.0_f64, f64::max)
}

/// `E = ½gᵀB⁻¹g` on the joint metric, certified by `½zᵀBz + zᵀr + ½rᵀP₀⁻¹r` for `r = g − Bz` and diagonal floor `P₀`.
#[allow(clippy::too_many_arguments)]
fn stationarity_decrement(
    layout: &Layout,
    problem: &Problem,
    params: &Params,
    options: &FitOptions,
    blocks: &[BlockAcc],
    penalties: &[Vec<f64>],
    lambdas: &StageLambdas,
    roles: &[Role],
    pass: &PassOutput,
    data_scale: f64,
    warm: &mut Option<Vec<f64>>,
) -> f64 {
    stationarity_bound(
        layout, problem, params, options, blocks, penalties, lambdas, roles, pass, data_scale, warm,
    )
}

#[allow(clippy::too_many_arguments)]
fn stationarity_bound(
    layout: &Layout,
    problem: &Problem,
    params: &Params,
    options: &FitOptions,
    blocks: &[BlockAcc],
    penalties: &[Vec<f64>],
    lambdas: &StageLambdas,
    roles: &[Role],
    pass: &PassOutput,
    data_scale: f64,
    warm: &mut Option<Vec<f64>>,
) -> f64 {
    let model = match JointModel::new(
        layout, problem, params, options, blocks, penalties, lambdas, roles, pass, data_scale,
        false, false,
    ) {
        Ok(model) => model,
        Err(failure) => {
            if fit_diag() {
                eprintln!("fit certificate model failed: {}", failure.label());
            }
            return f64::INFINITY;
        }
    };
    let solve = match JointSolve::new(&model, 0.0) {
        Ok(solve) => solve,
        Err(failure) => {
            if fit_diag() {
                eprintln!("fit certificate factor failed: {}", failure.label());
            }
            return f64::INFINITY;
        }
    };
    let lower_bound = solve.energy_lower_bound(&model);
    if lower_bound > STATIONARY_DECREMENT_NATS {
        if fit_diag() {
            eprintln!("fit certificate lower bound {lower_bound:.9e} nats");
        }
        return lower_bound;
    }
    let solution = match solve.solve(&model, CERTIFICATE_TOLERANCE, None, warm.as_deref()) {
        Ok(solution) => solution,
        Err(failure) => {
            if fit_diag() {
                eprintln!("fit certificate solve failed: {}", failure.label());
            }
            return f64::INFINITY;
        }
    };
    *warm = Some(solution.delta[..model.outer_count].to_vec());
    let mut quadratic = 0.0_f64;
    let mut first = 0.0_f64;
    let mut residual_correction = 0.0_f64;
    for index in 0..model.dim {
        let residual = model.rhs[index] - solution.curvature[index];
        // `λP` is only positive semidefinite, so `diag(λP)` is not a lower bound of `λP`
        // in Loewner order; only the fixed prior floor is.
        let floor = model.fixed[index];
        if floor <= 0.0 || !floor.is_finite() {
            if fit_diag() {
                eprintln!("fit certificate has no SPD prior floor at joint coordinate {index}");
            }
            return f64::INFINITY;
        }
        quadratic += solution.delta[index] * solution.curvature[index];
        first += solution.delta[index] * residual;
        residual_correction += residual * residual / floor;
    }
    let bound = 0.5 * quadratic + first + 0.5 * residual_correction;
    if fit_diag() {
        eprintln!(
            "fit certificate bound {bound:.9e} nats quadratic {quadratic:.9e} first {first:.9e} residual {residual_correction:.9e} iterations {} relative {:.3e} stalled {} joint {} outer {} border {}",
            solution.iterations,
            solution.relative_residual,
            solution.failure.is_some(),
            model.dim,
            model.outer_count,
            model.border_count
        );
    }
    if !bound.is_finite() || bound < 0.0 {
        return f64::INFINITY;
    }
    if blocks.iter().any(|block| block.kind == BlockKind::Residual) {
        return bound + residual_decrement(pass, layout, params, lambdas, data_scale);
    }
    bound
}

#[cfg(test)]
fn undamped_decrement(mut curvature: Vec<f64>, rhs: &[f64]) -> f64 {
    let n = rhs.len();
    if !factor_in_place(&mut curvature, n) {
        return f64::INFINITY;
    }
    let mut solution = rhs.to_vec();
    solve_in_place(&curvature, n, &mut solution);
    let energy = 0.5 * rhs.iter().zip(solution).map(|(g, z)| g * z).sum::<f64>();
    if energy.is_finite() && energy >= 0.0 {
        energy
    } else {
        f64::INFINITY
    }
}

/// Retained-row bound for the final-stage operator; a row costs about 270 entries, so this
/// is roughly 70 MB. Above it the operator keeps the scaled `hessian_samples` subset.
const FULL_ROW_LIMIT: usize = 8192;

/// Outer conjugate-gradient iterations between explicit-residual checks.
const JOINT_CG_CHUNK: usize = 64;

/// Hard limit on outer conjugate-gradient iterations; the stall detector ends a solve that
/// stops improving well before it.
const JOINT_CG_LIMIT: usize = 4096;

/// Relative residual the final stationarity certificate must reach.
const CERTIFICATE_TOLERANCE: f64 = 1e-10;

/// Why a joint step was not returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SolveFailure {
    /// The iteration limit or the stall detector stopped the solve.
    WorkLimit,
    /// The operator lost positive curvature.
    NonpositiveCurvature,
    /// An image or border factorization failed.
    Factorization(&'static str),
    /// Non-finite arithmetic.
    Nonfinite,
}

impl SolveFailure {
    fn label(self) -> &'static str {
        match self {
            Self::WorkLimit => "work limit",
            Self::NonpositiveCurvature => "nonpositive curvature",
            Self::Factorization("image") => "factorization (image)",
            Self::Factorization("border") => "factorization (border)",
            Self::Factorization(_) => "factorization",
            Self::Nonfinite => "nonfinite arithmetic",
        }
    }
}

fn dot3(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

struct JointRow {
    columns: Vec<(u32, [f64; 3])>,
    weight: f64,
    /// `Jᵀr` per column.
    robust_terms: Vec<f64>,
    /// `−2(ν+3)/q²` times the sample weight and the data scale.
    robust: f64,
    /// `d margin/dθ` per column.
    margin_terms: Vec<f64>,
    /// `barrier''(margin)` times the sample weight and the data scale.
    barrier: f64,
    /// Layer value each column moves, `None` when it mixes layers.
    layers: Vec<Option<Layer>>,
    /// `∂(t, n_a, n_b)/∂θ` in the first three slots and `∂layer/∂θ` of `layers` in
    /// the fourth.
    layer_weights: Vec<[f64; 4]>,
    /// `∂²layer/∂θ²` of a tone column, `∂w_m/∂t` of a tint column, zero otherwise.
    layer_slopes: Vec<f64>,
    /// `α w r·∂p/∂l_k + barrier' ∇margin·∂(J'a'b')/∂l_k` for `k = 0, 1, 2`, scaled.
    motion: [f64; 3],
    /// `Σ_m w''_m a_m`, `Σ_m w''_m b_m`, `Σ_m w'_m a_m`, `Σ_m w'_m b_m`.
    tint: [f64; 4],
    /// `α w Σ_c r_c ∂²p_c/∂l_k∂l_l + barrier' ∂²margin/∂l_k∂l_l`, scaled by `data_scale`.
    second: Box<[f64; 81]>,
}

struct JointGroup {
    /// First outer index of the group.
    start: usize,
    /// Sampled data curvature plus `λ·P` over the group's coordinates.
    matrix: Vec<f64>,
}

/// Frozen final-stage linearization: full-data penalized gradient and undamped joint
/// curvature. Image blocks and the border are eliminated, so the solve runs over Look coefficients.
struct JointModel {
    /// Per image: global indices of exposure, `log w_R` and `log w_B`.
    images: Vec<Vec<usize>>,
    /// Global parameter of each joint coordinate.
    globals: Vec<usize>,
    /// Retained rows, columns already mapped to joint coordinates.
    rows: Vec<JointRow>,
    /// `-g` over the joint space, priors and penalties included.
    rhs: Vec<f64>,
    /// Prior and identity curvature diagonal.
    fixed: Vec<f64>,
    /// `λ·P` curvature entries.
    penalty: Vec<(u32, u32, f64)>,
    /// `λ·P` diagonal, for the preconditioner's prior fallback.
    penalty_diagonal: Vec<f64>,
    /// Full-data curvature diagonal, for LM damping.
    data_diagonal: Vec<f64>,
    /// Data coupling of each image block to the outer coordinates, `m × q`.
    image_outer: Vec<Vec<f64>>,
    /// Data coupling of each image block to the border, `m × k`.
    image_border: Vec<Vec<f64>>,
    /// Data curvature of each image block, `m × m`.
    image_data: Vec<Vec<f64>>,
    /// Data coupling of the outer coordinates to the border, `q × k`.
    outer_border: Vec<f64>,
    /// Data curvature of the border block, `k × k`.
    border_data: Vec<f64>,
    /// Joint range of each image block.
    image_range: Vec<(usize, usize)>,
    /// Preconditioner blocks over the outer coordinates.
    groups: Vec<JointGroup>,
    outer_count: usize,
    border_count: usize,
    /// Largest image block width, for scratch sizing.
    image_width: usize,
    dim: usize,
    /// Whether the rows carry the Student-t robust-weight correction.
    robust: bool,
    /// Whether the rows carry the prediction's second derivative.
    second: bool,
}

impl JointModel {
    #[allow(clippy::too_many_arguments)]
    fn new(
        layout: &Layout,
        problem: &Problem,
        params: &Params,
        options: &FitOptions,
        blocks: &[BlockAcc],
        penalties: &[Vec<f64>],
        lambdas: &StageLambdas,
        roles: &[Role],
        pass: &PassOutput,
        data_scale: f64,
        robust: bool,
        second: bool,
    ) -> Result<Self, SolveFailure> {
        let mut seen = vec![false; layout.total];
        let mut outer: Vec<usize> = Vec::new();
        let mut border: Vec<usize> = Vec::new();
        let mut images: Vec<Vec<usize>> = vec![Vec::new(); problem.images.len()];
        for block in blocks {
            if block.kind == BlockKind::Residual {
                continue;
            }
            for &global in &block.params {
                if seen[global] {
                    continue;
                }
                seen[global] = true;
                match roles[global] {
                    Role::Exposure(image) | Role::Wb(image) => images[image].push(global),
                    Role::Mode(_) | Role::Clip => border.push(global),
                    Role::None => outer.push(global),
                }
            }
        }
        let outer_count = outer.len();
        let border_count = border.len();
        let mut joint_of = vec![u32::MAX; layout.total];
        let mut globals: Vec<usize> = Vec::new();
        for &global in outer.iter().chain(&border) {
            joint_of[global] = globals.len() as u32;
            globals.push(global);
        }
        let mut image_range = Vec::with_capacity(images.len());
        let mut image_width = 0usize;
        for globals_of_image in &images {
            let start = globals.len();
            for &global in globals_of_image {
                joint_of[global] = globals.len() as u32;
                globals.push(global);
            }
            image_range.push((start, globals_of_image.len()));
            image_width = image_width.max(globals_of_image.len());
        }
        let dim = globals.len();

        let mut fixed = vec![0.0_f64; dim];
        let mut rhs = vec![0.0_f64; dim];
        let mut data_diagonal = vec![0.0_f64; dim];
        let mut penalty_diagonal = vec![0.0_f64; dim];
        let mut penalty: Vec<(u32, u32, f64)> = Vec::new();
        let mut groups: Vec<JointGroup> = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            if block.kind == BlockKind::Residual {
                continue;
            }
            let n = block.len();
            let lambda = block_lambda(block, lambdas);
            let (prior_gradient, prior_curvature) =
                prior_terms(problem, roles, params, options, block);
            let identity = identity_curvature(layout, block);
            for (local, &global) in block.params.iter().enumerate() {
                let joint = joint_of[global] as usize;
                fixed[joint] = prior_curvature[local] + identity[local];
                data_diagonal[joint] = block.full_diagonal[local].max(0.0);
                let mut regularizer = 0.0_f64;
                for other in 0..n {
                    regularizer +=
                        penalties[index][local * n + other] * params.theta[block.params[other]];
                }
                rhs[joint] = -(block.gradient[local]
                    + prior_gradient[local]
                    + identity[local] * params.theta[global]
                    + lambda * regularizer);
                penalty_diagonal[joint] = lambda * penalties[index][local * n + local];
                for other in 0..n {
                    let value = lambda * penalties[index][local * n + other];
                    if value != 0.0 {
                        penalty.push((joint as u32, joint_of[block.params[other]], value));
                    }
                }
            }
            let locals: Vec<usize> = (0..n)
                .filter(|local| (joint_of[block.params[*local]] as usize) < outer_count)
                .collect();
            if locals.is_empty() {
                continue;
            }
            let start = joint_of[block.params[locals[0]]] as usize;
            for (offset, local) in locals.iter().enumerate() {
                debug_assert_eq!(joint_of[block.params[*local]] as usize, start + offset);
            }
            let width = locals.len();
            let mut matrix = vec![0.0_f64; width * width];
            for (row, &left) in locals.iter().enumerate() {
                for (column, &right) in locals.iter().enumerate() {
                    matrix[row * width + column] = data_scale * block.hessian[left * n + right]
                        + lambda * penalties[index][left * n + right];
                }
            }
            groups.push(JointGroup { start, matrix });
        }
        if fixed
            .iter()
            .chain(rhs.iter())
            .chain(data_diagonal.iter())
            .any(|value| !value.is_finite())
        {
            if fit_diag() {
                for (label, values) in [
                    ("fixed", &fixed),
                    ("rhs", &rhs),
                    ("data diagonal", &data_diagonal),
                ] {
                    if let Some((index, value)) = values
                        .iter()
                        .enumerate()
                        .find(|(_, value)| !value.is_finite())
                    {
                        eprintln!(
                            "fit joint model nonfinite {label} joint {index} global {} value {value}",
                            globals[index]
                        );
                    }
                }
            }
            return Err(SolveFailure::Nonfinite);
        }

        let mut rows = Vec::with_capacity(pass.coupled_rows.len());
        for row in &pass.coupled_rows {
            let mut columns = Vec::with_capacity(row.columns.len());
            let mut robust_terms = Vec::with_capacity(row.columns.len());
            let mut margin_terms = Vec::with_capacity(row.columns.len());
            let mut layers = Vec::with_capacity(row.columns.len());
            let mut layer_weights = Vec::with_capacity(row.columns.len());
            let mut layer_slopes = Vec::with_capacity(row.columns.len());
            for (index, (global, derivative)) in row.columns.iter().enumerate() {
                let joint = joint_of[*global as usize];
                if joint != u32::MAX {
                    columns.push((joint, *derivative));
                    robust_terms.push(row.robust_terms[index]);
                    margin_terms.push(row.margin_terms[index]);
                    layers.push(row.layers[index]);
                    layer_weights.push(row.layer_weights[index]);
                    layer_slopes.push(row.layer_slopes[index]);
                }
            }
            if columns.is_empty() {
                continue;
            }
            let mut scaled = Box::new([0.0_f64; 81]);
            for (value, source) in scaled.iter_mut().zip(row.second.iter()) {
                *value = source * data_scale;
            }
            let motion = row.motion.map(|value| value * data_scale);
            rows.push(JointRow {
                columns,
                weight: row.weight * data_scale,
                robust_terms,
                robust: row.robust * data_scale,
                margin_terms,
                barrier: row.barrier * data_scale,
                layers,
                layer_weights,
                layer_slopes,
                motion,
                tint: row.tint,
                second: scaled,
            });
        }

        let mut image_outer: Vec<Vec<f64>> = images
            .iter()
            .map(|globals_of_image| vec![0.0_f64; globals_of_image.len() * outer_count])
            .collect();
        let mut image_border: Vec<Vec<f64>> = images
            .iter()
            .map(|globals_of_image| vec![0.0_f64; globals_of_image.len() * border_count])
            .collect();
        let mut image_data: Vec<Vec<f64>> = images
            .iter()
            .map(|globals_of_image| vec![0.0_f64; globals_of_image.len().pow(2)])
            .collect();
        let mut outer_border = vec![0.0_f64; outer_count * border_count];
        let mut border_data = vec![0.0_f64; border_count * border_count];
        let mut joint_image = vec![u32::MAX; dim];
        let mut joint_local = vec![0u8; dim];
        for (slot, globals_of_image) in images.iter().enumerate() {
            for (local, &global) in globals_of_image.iter().enumerate() {
                let joint = joint_of[global] as usize;
                joint_image[joint] = slot as u32;
                joint_local[joint] = local as u8;
            }
        }
        let mut row_outer: Vec<(usize, [f64; 3], f64, f64)> = Vec::new();
        let mut row_border: Vec<(usize, [f64; 3], f64, f64)> = Vec::new();
        let mut row_image: Vec<(usize, [f64; 3], f64, f64)> = Vec::new();
        for row in &rows {
            row_outer.clear();
            row_border.clear();
            row_image.clear();
            let mut slot = u32::MAX;
            for (position, (joint, derivative)) in row.columns.iter().enumerate() {
                let index = *joint as usize;
                let robust_term = row.robust_terms[position];
                let margin_term = row.margin_terms[position];
                if index < outer_count {
                    row_outer.push((index, *derivative, robust_term, margin_term));
                } else if index < outer_count + border_count {
                    row_border.push((index - outer_count, *derivative, robust_term, margin_term));
                } else {
                    slot = joint_image[index];
                    row_image.push((
                        joint_local[index] as usize,
                        *derivative,
                        robust_term,
                        margin_term,
                    ));
                }
            }
            let weight = row.weight;
            let robust = if robust { row.robust } else { 0.0 };
            let barrier = row.barrier;
            let pair =
                |left: &(usize, [f64; 3], f64, f64), right: &(usize, [f64; 3], f64, f64)| -> f64 {
                    weight * dot3(left.1, right.1)
                        + robust * left.2 * right.2
                        + barrier * left.3 * right.3
                };
            if slot != u32::MAX {
                let slot = slot as usize;
                let width = images[slot].len();
                for left in &row_image {
                    for right in &row_image {
                        image_data[slot][left.0 * width + right.0] += pair(left, right);
                    }
                    for right in &row_outer {
                        image_outer[slot][left.0 * outer_count + right.0] += pair(left, right);
                    }
                    for right in &row_border {
                        image_border[slot][left.0 * border_count + right.0] += pair(left, right);
                    }
                }
            }
            for left in &row_outer {
                for right in &row_border {
                    outer_border[left.0 * border_count + right.0] += pair(left, right);
                }
            }
            for left in &row_border {
                for right in &row_border {
                    border_data[left.0 * border_count + right.0] += pair(left, right);
                }
            }
        }

        Ok(Self {
            images,
            globals,
            rows,
            rhs,
            fixed,
            penalty,
            penalty_diagonal,
            data_diagonal,
            image_outer,
            image_border,
            image_data,
            outer_border,
            border_data,
            image_range,
            groups,
            outer_count,
            border_count,
            image_width,
            dim,
            robust,
            second,
        })
    }

    /// Retained-row curvature: GN, the Student-t correction when `self.robust`, the
    /// barrier, and the prediction's second derivative when `self.second`.
    fn apply_rows(&self, v: &[f64], out: &mut [f64]) {
        self.apply_rows_with(v, out, true);
    }

    /// [`Self::apply_rows`] with the second-order term forced on or off.
    fn apply_rows_with(&self, v: &[f64], out: &mut [f64], with_second: bool) {
        let second = self.second && with_second;
        for row in &self.rows {
            let mut projected = [0.0_f64; 3];
            let mut robust_projection = 0.0_f64;
            let mut margin_projection = 0.0_f64;
            let mut layer_direction = [0.0_f64; 9];
            let mut tint_a_direction = 0.0_f64;
            let mut tint_b_direction = 0.0_f64;
            for (position, (column, derivative)) in row.columns.iter().enumerate() {
                let value = v[*column as usize];
                projected[0] += derivative[0] * value;
                projected[1] += derivative[1] * value;
                projected[2] += derivative[2] * value;
                robust_projection += row.robust_terms[position] * value;
                margin_projection += row.margin_terms[position] * value;
                if second {
                    let weights = row.layer_weights[position];
                    for (k, direction) in layer_direction.iter_mut().enumerate().take(3) {
                        *direction += weights[k] * value;
                    }
                    match row.layers[position] {
                        Some(Layer::TintA) => {
                            tint_a_direction += row.layer_slopes[position] * value;
                        }
                        Some(Layer::TintB) => {
                            tint_b_direction += row.layer_slopes[position] * value;
                        }
                        Some(layer) if layer.index() >= 3 => {
                            layer_direction[layer.index()] += weights[3] * value;
                        }
                        _ => {}
                    }
                }
            }
            let weight = row.weight;
            let robust = if self.robust { row.robust } else { 0.0 };
            let barrier = row.barrier;
            let mut layer_curvature = [0.0_f64; 9];
            if second {
                for (k, value) in layer_curvature.iter_mut().enumerate() {
                    let mut total = 0.0;
                    for (l, direction) in layer_direction.iter().enumerate() {
                        total += row.second[k * 9 + l] * direction;
                    }
                    *value = total;
                }
            }
            for (position, (column, derivative)) in row.columns.iter().enumerate() {
                let mut total = weight * dot3(*derivative, projected)
                    + robust * row.robust_terms[position] * robust_projection
                    + barrier * row.margin_terms[position] * margin_projection;
                if second {
                    let layer = row.layers[position];
                    let slope = row.layer_slopes[position];
                    let value = v[*column as usize];
                    let weights = row.layer_weights[position];
                    total += weights[0] * layer_curvature[0]
                        + weights[1] * layer_curvature[1]
                        + weights[2] * layer_curvature[2];
                    if let Some(layer) = layer
                        && layer.index() >= 3
                    {
                        total += weights[3] * layer_curvature[layer.index()];
                    }
                    total += match layer {
                        Some(Layer::Tone) => {
                            let channel = weights[0];
                            row.motion[0] * slope * value
                                + row.motion[1]
                                    * (row.tint[0] * channel * layer_direction[0]
                                        + row.tint[2] * slope * value
                                        + channel * tint_a_direction)
                                + row.motion[2]
                                    * (row.tint[1] * channel * layer_direction[0]
                                        + row.tint[3] * slope * value
                                        + channel * tint_b_direction)
                        }
                        Some(Layer::TintA) => row.motion[1] * slope * layer_direction[0],
                        Some(Layer::TintB) => row.motion[2] * slope * layer_direction[0],
                        _ => 0.0,
                    };
                }
                out[*column as usize] += total;
            }
        }
    }

    /// Undamped joint curvature: retained rows, the fixed diagonal and `λ·P`.
    fn apply_undamped(&self, v: &[f64], out: &mut [f64]) {
        self.apply_undamped_with(v, out, true);
    }

    /// [`Self::apply_undamped`] with the second-order term forced on or off.
    fn apply_undamped_with(&self, v: &[f64], out: &mut [f64], with_second: bool) {
        out.fill(0.0);
        self.apply_rows_with(v, out, with_second);
        for (index, slot) in out.iter_mut().enumerate() {
            *slot += self.fixed[index] * v[index];
        }
        for &(row, column, value) in &self.penalty {
            out[row as usize] += value * v[column as usize];
        }
    }
}

/// Buffers shared by the reduced operator and its preconditioner.
struct JointScratch<'a> {
    joint: &'a mut [f64],
    joint_out: &'a mut [f64],
    image_rhs: &'a mut [f64],
    border: &'a mut [f64],
}

fn scratch_buffer(model: &JointModel) -> Vec<f64> {
    vec![0.0_f64; 2 * model.dim + model.image_width.max(1) + model.border_count.max(1)]
}

fn split_scratch<'a>(buffer: &'a mut [f64], model: &JointModel) -> JointScratch<'a> {
    let dim = model.dim;
    let border_count = model.border_count;
    let (first, rest) = buffer.split_at_mut(dim);
    let (second, rest) = rest.split_at_mut(dim);
    let (image_rhs, border) = rest.split_at_mut(model.image_width.max(1));
    JointScratch {
        joint: first,
        joint_out: second,
        image_rhs,
        border: &mut border[..border_count.max(1)],
    }
}

/// Symmetric Jacobi scaling `D⁻¹ᐟ² A D⁻¹ᐟ²` before Cholesky, with `D = diag(A)`.
fn scaled_factor(matrix: &[f64], width: usize) -> Option<(Vec<f64>, Vec<f64>)> {
    let mut scale = vec![1.0_f64; width];
    for row in 0..width {
        let diagonal = matrix[row * width + row];
        if !(diagonal.is_finite() && diagonal > 0.0) {
            return None;
        }
        scale[row] = libm::sqrt(diagonal);
    }
    let mut factor = matrix.to_vec();
    for row in 0..width {
        for column in 0..width {
            factor[row * width + column] /= scale[row] * scale[column];
        }
    }
    factor_in_place(&mut factor, width).then_some((factor, scale))
}

/// The damped joint system with the per-image blocks and the border eliminated.
struct JointSolve {
    image_factor: Vec<Vec<f64>>,
    border_factor: Vec<f64>,
    /// Eliminated outer/border coupling, `q × k`.
    outer_border: Vec<f64>,
    /// Reduced right-hand side over the outer coordinates.
    outer_rhs: Vec<f64>,
    /// Reduced right-hand side over the border, before the `S_ss` solve.
    border_rhs: Vec<f64>,
    /// `(start, width, scaled factor, Jacobi scale)` of each outer preconditioner block.
    groups: Vec<(usize, usize, Vec<f64>, Vec<f64>)>,
    damping: f64,
    zero_border: Vec<f64>,
}

impl JointSolve {
    fn new(model: &JointModel, damping: f64) -> Result<Self, SolveFailure> {
        Self::build(model, damping)
    }

    fn build(model: &JointModel, damping: f64) -> Result<Self, SolveFailure> {
        let q = model.outer_count;
        let k = model.border_count;
        let mut image_factor = Vec::with_capacity(model.images.len());
        let mut image_border_solved = Vec::with_capacity(model.images.len());
        for (index, globals_of_image) in model.images.iter().enumerate() {
            let m = globals_of_image.len();
            if m == 0 {
                image_factor.push(Vec::new());
                image_border_solved.push(Vec::new());
                continue;
            }
            let (start, _) = model.image_range[index];
            let mut factor = model.image_data[index].clone();
            for row in 0..m {
                factor[row * m + row] += model.fixed[start + row]
                    + damping * model.data_diagonal[start + row].max(1e-12);
            }
            if !factor_in_place(&mut factor, m) {
                return Err(SolveFailure::Factorization("image"));
            }
            let mut solved = vec![0.0_f64; m * k];
            for column in 0..k {
                let mut values = vec![0.0_f64; m];
                for row in 0..m {
                    values[row] = model.image_border[index][row * k + column];
                }
                solve_in_place(&factor, m, &mut values);
                for row in 0..m {
                    solved[row * k + column] = values[row];
                }
            }
            image_factor.push(factor);
            image_border_solved.push(solved);
        }

        let mut border = model.border_data.clone();
        for row in 0..k {
            border[row * k + row] +=
                model.fixed[q + row] + damping * model.data_diagonal[q + row].max(1e-12);
        }
        for (index, globals_of_image) in model.images.iter().enumerate() {
            let m = globals_of_image.len();
            if m == 0 {
                continue;
            }
            let solved = &image_border_solved[index];
            for row in 0..m {
                for left in 0..k {
                    let value = model.image_border[index][row * k + left];
                    for right in 0..k {
                        border[left * k + right] -= value * solved[row * k + right];
                    }
                }
            }
        }
        let mut border_factor = border;
        if k > 0 && !factor_in_place(&mut border_factor, k) {
            return Err(SolveFailure::Factorization("border"));
        }

        let mut outer_border = model.outer_border.clone();
        for (index, globals_of_image) in model.images.iter().enumerate() {
            let m = globals_of_image.len();
            if m == 0 {
                continue;
            }
            let solved = &image_border_solved[index];
            for row in 0..m {
                for left in 0..q {
                    let value = model.image_outer[index][row * q + left];
                    for right in 0..k {
                        outer_border[left * k + right] -= value * solved[row * k + right];
                    }
                }
            }
        }

        let mut outer_rhs = model.rhs[..q].to_vec();
        let mut border_rhs = model.rhs[q..q + k].to_vec();
        for (index, globals_of_image) in model.images.iter().enumerate() {
            let m = globals_of_image.len();
            if m == 0 {
                continue;
            }
            let (start, _) = model.image_range[index];
            let mut values = model.rhs[start..start + m].to_vec();
            solve_in_place(&image_factor[index], m, &mut values);
            for row in 0..m {
                for left in 0..q {
                    outer_rhs[left] -= model.image_outer[index][row * q + left] * values[row];
                }
                for right in 0..k {
                    border_rhs[right] -= model.image_border[index][row * k + right] * values[row];
                }
            }
        }
        let reduced_border = border_rhs.clone();
        if k > 0 {
            solve_in_place(&border_factor, k, &mut border_rhs);
            for left in 0..q {
                let mut sum = 0.0_f64;
                for right in 0..k {
                    sum += outer_border[left * k + right] * border_rhs[right];
                }
                outer_rhs[left] -= sum;
            }
        }

        let mut schur_diagonal = vec![0.0_f64; q];
        let mut image_outer_solved: Vec<Vec<f64>> = Vec::with_capacity(model.images.len());
        let mut column = vec![0.0_f64; model.image_width.max(1)];
        for (index, globals_of_image) in model.images.iter().enumerate() {
            let m = globals_of_image.len();
            if m == 0 {
                image_outer_solved.push(Vec::new());
                continue;
            }
            let factor = &image_factor[index];
            let mut solved = vec![0.0_f64; m * q];
            for left in 0..q {
                for row in 0..m {
                    column[row] = model.image_outer[index][row * q + left];
                }
                solve_in_place(factor, m, &mut column[..m]);
                let mut sum = 0.0_f64;
                for row in 0..m {
                    sum += column[row] * column[row];
                    solved[row * q + left] = column[row];
                }
                schur_diagonal[left] += sum;
            }
            image_outer_solved.push(solved);
        }

        let mut groups = Vec::with_capacity(model.groups.len());
        for group in &model.groups {
            let width = (group.matrix.len() as f64).sqrt() as usize;
            let start = group.start;
            // `group.matrix` already carries the exact `λP` block.
            let mut matrix = group.matrix.clone();
            for row in 0..width {
                let joint = start + row;
                matrix[row * width + row] +=
                    model.fixed[joint] + damping * model.data_diagonal[joint].max(1e-12);
            }
            // Principal block of the reduced operator.
            let unreduced = matrix.clone();
            let mut correction = vec![0.0_f64; width * width];
            for (index, globals_of_image) in model.images.iter().enumerate() {
                let m = globals_of_image.len();
                if m == 0 {
                    continue;
                }
                let solved = &image_outer_solved[index];
                for left in 0..width {
                    for right in 0..width {
                        let mut sum = 0.0_f64;
                        for row in 0..m {
                            sum += model.image_outer[index][row * q + start + left]
                                * solved[row * q + start + right];
                        }
                        correction[left * width + right] += sum;
                    }
                }
            }
            if k > 0 {
                let mut border_solved = vec![0.0_f64; width * k];
                for outer_index in 0..width {
                    let mut values = vec![0.0_f64; k];
                    for row in 0..k {
                        values[row] = outer_border[(start + outer_index) * k + row];
                    }
                    solve_in_place(&border_factor, k, &mut values);
                    for row in 0..k {
                        border_solved[outer_index * k + row] = values[row];
                    }
                }
                for left in 0..width {
                    for right in 0..width {
                        let mut sum = 0.0_f64;
                        for row in 0..k {
                            sum += outer_border[(start + left) * k + row]
                                * border_solved[right * k + row];
                        }
                        correction[left * width + right] += sum;
                    }
                }
            }
            for (slot, value) in matrix.iter_mut().zip(correction.iter()) {
                *slot -= value;
            }
            let mut choice = "reduced";
            let mut scaled = scaled_factor(&matrix, width);
            if scaled.is_none() {
                // A principal block of the Schur complement is positive definite only when
                // the full reduced operator is.
                choice = "block";
                scaled = scaled_factor(&unreduced, width);
            }
            if scaled.is_none() {
                choice = "diagonal";
                let mut diagonal = group.matrix.clone();
                for row in 0..width {
                    let joint = start + row;
                    let total = diagonal[row * width + row].max(0.0)
                        + model.fixed[joint]
                        + damping * model.data_diagonal[joint].max(1e-12);
                    diagonal[row * width + row] = total - schur_diagonal[joint].min(0.5 * total);
                }
                scaled = scaled_factor(&diagonal, width);
            }
            if scaled.is_none() {
                choice = "prior";
                let mut prior = vec![0.0_f64; width * width];
                for row in 0..width {
                    let joint = start + row;
                    prior[row * width + row] =
                        (model.fixed[joint] + model.penalty_diagonal[joint]).max(1e-12);
                }
                scaled = scaled_factor(&prior, width);
            }
            let Some((factor, scale)) = scaled else {
                return Err(SolveFailure::Factorization("preconditioner"));
            };
            if fit_diag() && choice != "reduced" {
                eprintln!("fit preconditioner group {start}+{width} falls back to {choice}");
            }
            groups.push((start, width, factor, scale));
        }

        Ok(Self {
            image_factor,
            border_factor,
            outer_border,
            outer_rhs,
            border_rhs: reduced_border,
            groups,
            damping,
            zero_border: vec![0.0_f64; k],
        })
    }

    /// `[[S_yy, S_ys], [S_sy, S_ss]]` applied to `(v_outer, v_border)`, images eliminated.
    fn eliminated(
        &self,
        model: &JointModel,
        v_outer: &[f64],
        v_border: &[f64],
        out: &mut [f64],
        scratch: &mut JointScratch<'_>,
    ) {
        let q = model.outer_count;
        let k = model.border_count;
        scratch.joint[..q].copy_from_slice(v_outer);
        scratch.joint[q..q + k].copy_from_slice(v_border);
        for slot in scratch.joint[q + k..].iter_mut() {
            *slot = 0.0;
        }
        scratch.joint_out.fill(0.0);
        model.apply_rows(scratch.joint, scratch.joint_out);
        for index in 0..q + k {
            out[index] = scratch.joint_out[index]
                + (model.fixed[index] + self.damping * model.data_diagonal[index].max(1e-12))
                    * scratch.joint[index];
        }
        for &(row, column, value) in &model.penalty {
            out[row as usize] += value * scratch.joint[column as usize];
        }
        for (index, globals_of_image) in model.images.iter().enumerate() {
            let m = globals_of_image.len();
            if m == 0 {
                continue;
            }
            for row in 0..m {
                let mut value = 0.0_f64;
                for left in 0..q {
                    value += model.image_outer[index][row * q + left] * v_outer[left];
                }
                for right in 0..k {
                    value += model.image_border[index][row * k + right] * v_border[right];
                }
                scratch.image_rhs[row] = value;
            }
            solve_in_place(&self.image_factor[index], m, &mut scratch.image_rhs[..m]);
            for row in 0..m {
                let weight = scratch.image_rhs[row];
                for left in 0..q {
                    out[left] -= model.image_outer[index][row * q + left] * weight;
                }
                for right in 0..k {
                    out[q + right] -= model.image_border[index][row * k + right] * weight;
                }
            }
        }
    }

    /// The reduced Schur operator over the outer coordinates.
    fn apply_reduced(
        &self,
        model: &JointModel,
        v: &[f64],
        out: &mut [f64],
        eliminated: &mut [f64],
        scratch: &mut JointScratch<'_>,
    ) {
        let q = model.outer_count;
        let k = model.border_count;
        self.eliminated(model, v, &self.zero_border, eliminated, scratch);
        if k == 0 {
            out.copy_from_slice(&eliminated[..q]);
            return;
        }
        scratch.border.copy_from_slice(&eliminated[q..q + k]);
        solve_in_place(&self.border_factor, k, scratch.border);
        for left in 0..q {
            let mut sum = 0.0_f64;
            for right in 0..k {
                sum += self.outer_border[left * k + right] * scratch.border[right];
            }
            out[left] = eliminated[left] - sum;
        }
    }

    /// `(gᵀd)² / (2 dᵀBd)` for the preconditioned direction, a lower bound on `E`.
    fn energy_lower_bound(&self, model: &JointModel) -> f64 {
        let q = model.outer_count;
        let k = model.border_count;
        let mut buffer = scratch_buffer(model);
        let mut scratch = split_scratch(&mut buffer, model);
        let mut direction = vec![0.0_f64; q];
        let mut product = vec![0.0_f64; q];
        let mut eliminated = vec![0.0_f64; q + k];
        self.precondition(&self.outer_rhs, &mut direction);
        self.apply_reduced(
            model,
            &direction,
            &mut product,
            &mut eliminated,
            &mut scratch,
        );
        let first = crate::solve::dot(&self.outer_rhs, &direction);
        let curvature = crate::solve::dot(&direction, &product);
        if first.is_finite() && curvature.is_finite() && curvature > 0.0 {
            first * first / (2.0 * curvature)
        } else {
            f64::INFINITY
        }
    }

    fn precondition(&self, residual: &[f64], z: &mut [f64]) {
        for (start, width, factor, scale) in &self.groups {
            if *width == 0 {
                continue;
            }
            for index in 0..*width {
                z[*start + index] = residual[*start + index] / scale[index];
            }
            solve_in_place(factor, *width, &mut z[*start..*start + *width]);
            for index in 0..*width {
                z[*start + index] /= scale[index];
            }
        }
    }

    /// Solve the damped joint system, recover the eliminated coordinates and report the
    /// explicit residual of the original joint system.
    fn solve(
        &self,
        model: &JointModel,
        tolerance: f64,
        radius: Option<f64>,
        warm: Option<&[f64]>,
    ) -> Result<JointSolution, SolveFailure> {
        self.solve_inner(model, tolerance, radius, warm)
    }

    fn solve_inner(
        &self,
        model: &JointModel,
        tolerance: f64,
        radius: Option<f64>,
        warm: Option<&[f64]>,
    ) -> Result<JointSolution, SolveFailure> {
        let q = model.outer_count;
        let k = model.border_count;
        let dim = model.dim;
        let mut x = vec![0.0_f64; q];
        let mut residual = self.outer_rhs.clone();
        let mut z = vec![0.0_f64; q];
        let mut direction = vec![0.0_f64; q];
        let mut product = vec![0.0_f64; q];
        let mut eliminated = vec![0.0_f64; q + k];
        let mut buffer = scratch_buffer(model);
        let mut scratch = split_scratch(&mut buffer, model);

        let norm: f64 = crate::solve::dot(&self.outer_rhs, &self.outer_rhs);
        if !norm.is_finite() {
            return Err(SolveFailure::Nonfinite);
        }
        let mut warmed = false;
        if let Some(values) = warm.filter(|values| values.len() == q) {
            x.copy_from_slice(values);
            self.apply_reduced(model, &x, &mut product, &mut eliminated, &mut scratch);
            let mut error = 0.0_f64;
            for index in 0..q {
                residual[index] = self.outer_rhs[index] - product[index];
                error += residual[index] * residual[index];
            }
            if error.is_finite() && error < norm {
                warmed = true;
            } else {
                x.fill(0.0);
                residual.copy_from_slice(&self.outer_rhs);
            }
        }
        self.precondition(&residual, &mut z);
        direction.copy_from_slice(&z);
        let mut rz = crate::solve::dot(&residual, &z);
        if !(rz.is_finite() && rz > 0.0) {
            return Err(SolveFailure::Nonfinite);
        }
        let mut target = rz * tolerance * tolerance;
        let mut iterations = 0usize;
        let mut checked = 0usize;
        let mut stalled = false;
        let mut stagnant = 0usize;
        let mut previous = f64::INFINITY;
        loop {
            if iterations >= JOINT_CG_LIMIT {
                stalled = true;
                break;
            }
            let recursive = rz <= target;
            let chunk =
                (warmed && iterations == 0) || iterations.saturating_sub(checked) >= JOINT_CG_CHUNK;
            if recursive || chunk {
                self.apply_reduced(model, &x, &mut product, &mut eliminated, &mut scratch);
                let mut error = 0.0_f64;
                for index in 0..q {
                    residual[index] = self.outer_rhs[index] - product[index];
                    error += residual[index] * residual[index];
                }
                let relative = (error / norm.max(1e-300)).sqrt();
                if !relative.is_finite() {
                    return Err(SolveFailure::Nonfinite);
                }
                if relative <= tolerance {
                    break;
                }
                if chunk {
                    stagnant = if relative > 0.999 * previous {
                        stagnant + 1
                    } else {
                        0
                    };
                    if stagnant >= 3 {
                        stalled = true;
                        break;
                    }
                    previous = relative;
                }
                checked = iterations;
                self.precondition(&residual, &mut z);
                let reset = crate::solve::dot(&residual, &z);
                if !(reset.is_finite() && reset > 0.0) {
                    return Err(SolveFailure::Nonfinite);
                }
                direction.copy_from_slice(&z);
                rz = reset;
                target = rz * tolerance * tolerance;
                if recursive {
                    continue;
                }
            }
            self.apply_reduced(
                model,
                &direction,
                &mut product,
                &mut eliminated,
                &mut scratch,
            );
            let curvature = crate::solve::dot(&direction, &product);
            if !curvature.is_finite() {
                return Err(SolveFailure::Nonfinite);
            }
            if curvature <= 0.0 {
                return Err(SolveFailure::NonpositiveCurvature);
            }
            iterations += 1;
            let step = rz / curvature;
            crate::solve::scaled_add(&mut x, &direction, step);
            for index in 0..q {
                residual[index] -= step * product[index];
            }
            self.precondition(&residual, &mut z);
            let next = crate::solve::dot(&residual, &z);
            if !next.is_finite() {
                return Err(SolveFailure::Nonfinite);
            }
            let beta = next / rz;
            for index in 0..q {
                direction[index] = z[index] + beta * direction[index];
            }
            rz = next;
        }

        let mut delta = vec![0.0_f64; dim];
        delta[..q].copy_from_slice(&x);
        self.eliminated(model, &x, &self.zero_border, &mut eliminated, &mut scratch);
        let mut border_delta = vec![0.0_f64; k];
        for index in 0..k {
            border_delta[index] = self.border_rhs[index] - eliminated[q + index];
        }
        if k > 0 {
            solve_in_place(&self.border_factor, k, &mut border_delta);
        }
        delta[q..q + k].copy_from_slice(&border_delta);
        for (index, globals_of_image) in model.images.iter().enumerate() {
            let m = globals_of_image.len();
            if m == 0 {
                continue;
            }
            let (start, _) = model.image_range[index];
            for row in 0..m {
                let mut value = model.rhs[start + row];
                for left in 0..q {
                    value -= model.image_outer[index][row * q + left] * x[left];
                }
                for right in 0..k {
                    value -= model.image_border[index][row * k + right] * border_delta[right];
                }
                scratch.image_rhs[row] = value;
            }
            solve_in_place(&self.image_factor[index], m, &mut scratch.image_rhs[..m]);
            delta[start..start + m].copy_from_slice(&scratch.image_rhs[..m]);
        }

        if delta.iter().any(|value| !value.is_finite()) {
            return Err(SolveFailure::Nonfinite);
        }
        let mut curvature = vec![0.0_f64; dim];
        model.apply_undamped(&delta, &mut curvature);
        for (index, value) in curvature.iter_mut().enumerate() {
            *value += self.damping * model.data_diagonal[index].max(1e-12) * delta[index];
        }
        let mut error = 0.0_f64;
        let mut scale = 0.0_f64;
        for index in 0..dim {
            let value = curvature[index] - model.rhs[index];
            error += value * value;
            scale += model.rhs[index] * model.rhs[index];
        }
        let relative_residual = (error / scale.max(1e-300)).sqrt();
        if !relative_residual.is_finite() {
            return Err(SolveFailure::Nonfinite);
        }
        if fit_diag() && (stalled || relative_residual > tolerance) {
            let mut parts: Vec<(String, f64, f64)> = Vec::new();
            let mut push = |label: String, from: usize, to: usize| {
                let mut squared = 0.0_f64;
                let mut rhs_squared = 0.0_f64;
                let mut peak = 0.0_f64;
                for index in from..to {
                    let value = curvature[index] - model.rhs[index];
                    squared += value * value;
                    rhs_squared += model.rhs[index] * model.rhs[index];
                    peak = peak.max(value.abs());
                }
                parts.push((label, squared.sqrt(), rhs_squared.sqrt().max(peak)));
            };
            let mut cursor = 0usize;
            for (start, width, _, _) in &self.groups {
                if *start > cursor {
                    push(format!("gap {cursor}"), cursor, *start);
                }
                push(format!("group {start}+{width}"), *start, *start + *width);
                cursor = *start + *width;
            }
            if cursor < q {
                push(format!("outer tail {cursor}"), cursor, q);
            }
            if k > 0 {
                push("border".to_string(), q, q + k);
            }
            for (index, (start, width)) in model.image_range.iter().enumerate() {
                if *width > 0 {
                    push(format!("image {index}"), *start, *start + *width);
                }
            }
            eprintln!(
                "fit joint residual {relative_residual:.3e} tolerance {tolerance:.3e} iterations {iterations} stalled {stalled} damping {:.3e} q {q} k {k}",
                self.damping
            );
            for (label, residual_norm, rhs_norm) in parts {
                if residual_norm > 1e-9 * rhs_norm.max(1e-300) {
                    eprintln!(
                        "fit joint residual {label} {residual_norm:.6e} of rhs {rhs_norm:.6e} ratio {:.3e}",
                        residual_norm / rhs_norm.max(1e-300)
                    );
                }
            }
        }

        if let Some(radius) = radius {
            clamp_values(&mut delta, radius);
            model.apply_undamped(&delta, &mut curvature);
        } else {
            for (index, value) in curvature.iter_mut().enumerate() {
                *value -= self.damping * model.data_diagonal[index].max(1e-12) * delta[index];
            }
        }
        let mut prediction = crate::solve::dot(&model.rhs, &delta);
        prediction -= 0.5 * crate::solve::dot(&delta, &curvature);
        Ok(JointSolution {
            delta,
            curvature,
            prediction,
            iterations,
            relative_residual,
            failure: stalled.then_some(SolveFailure::WorkLimit),
        })
    }
}

struct JointSolution {
    /// Joint-coordinate step.
    delta: Vec<f64>,
    /// `B δ` under the undamped joint model.
    curvature: Vec<f64>,
    /// `-g·δ - ½ δᵀBδ`.
    prediction: f64,
    iterations: usize,
    /// Relative residual of the damped joint system after nuisance recovery.
    relative_residual: f64,
    /// Set when the iteration limit or the stall detector stopped the solve early.
    failure: Option<SolveFailure>,
}

#[allow(clippy::too_many_arguments)]
fn print_cost_breakdown(
    problem: &Problem,
    params: &Params,
    blocks: &[BlockAcc],
    penalties: &[Vec<f64>],
    lambdas: &StageLambdas,
    options: &FitOptions,
    pass: &PassOutput,
) {
    let prior = prior_cost(problem, params, options);
    let mut penalty_total = 0.0_f64;
    let mut identity_total = 0.0_f64;
    let mut residual_total = 0.0_f64;
    for (index, block) in blocks.iter().enumerate() {
        if block.kind == BlockKind::Residual {
            residual_total += residual_cost(
                &problem.layout,
                &params.theta,
                lam(lambdas, BlockKind::Residual),
            );
            continue;
        }
        let penalty = block_lambda(block, lambdas)
            * penalty_value(&params.theta, &block.params, &penalties[index]);
        let identity = identity_cost(&problem.layout, block, &params.theta);
        penalty_total += penalty;
        identity_total += identity;
        if penalty.abs() + identity.abs() > 1e-6 * prior.abs().max(1.0) {
            eprintln!(
                "fit cost block {:?} penalty {penalty:.9e} identity {identity:.9e}",
                block.kind
            );
        }
    }
    eprintln!(
        "fit cost likelihood {:.12e} prior {prior:.9e} penalty {penalty_total:.9e} identity {identity_total:.9e} residual {residual_total:.9e}",
        pass.cost
    );
    for (index, image) in problem.images.iter().enumerate() {
        let nuisance = params.image(index);
        let mode = image.mode.map_or(0.0, |slot| params.mode_offset(slot));
        if nuisance[0].abs() + nuisance[1].abs() + nuisance[2].abs() + mode.abs() > 5.0 {
            eprintln!(
                "fit cost image {index} exposure {:.6e} wb {:.6e} {:.6e} mode {mode:.6e} hint {:.6e}",
                nuisance[0], nuisance[1], nuisance[2], image.exposure_hint
            );
        }
    }
    let sigma: Vec<f64> = (0..problem.contributors.len())
        .map(|index| params.sigma(index))
        .collect();
    eprintln!("fit cost sigma {sigma:?} clip k {:.6e}", params.clip_k());
}

#[allow(clippy::too_many_arguments)]
fn print_block_gradients(
    label: &str,
    layout: &Layout,
    problem: &Problem,
    params: &Params,
    options: &FitOptions,
    blocks: &[BlockAcc],
    penalties: &[Vec<f64>],
    lambdas: &StageLambdas,
    roles: &[Role],
) {
    for (index, block) in blocks.iter().enumerate() {
        if block.kind == BlockKind::Residual || block.params.is_empty() {
            continue;
        }
        let lambda = block_lambda(block, lambdas);
        let gradients = block_gradients(
            layout,
            problem,
            params,
            options,
            block,
            &penalties[index],
            lambda,
            roles,
        );
        let (peak, worst) = gradients.iter().enumerate().fold(
            (0.0_f64, None),
            |(largest, worst), (local, gradient)| {
                if gradient.abs() > largest {
                    (gradient.abs(), Some(block.params[local]))
                } else {
                    (largest, worst)
                }
            },
        );
        let theta = worst.map_or(f64::NAN, |global| params.theta[global]);
        eprintln!(
            "fit {label} block {:?} gradient {peak:.9e} param {worst:?} theta {theta:.9e}",
            block.kind
        );
    }
}

/// Outcome of one block's damped Newton solve.
#[derive(Clone, Copy, Debug, PartialEq)]
enum BlockStep {
    /// The block system factored; the value is the predicted reduction.
    Solved(f64),
    /// The block system could not be factorized; `step` was left untouched.
    Failed,
}

#[allow(clippy::too_many_arguments)]
fn solve_block(
    layout: &Layout,
    block: &BlockAcc,
    step: &mut [f64],
    penalty: &[f64],
    prior_gradient: &[f64],
    prior_curvature: &[f64],
    params: &Params,
    lambdas: &StageLambdas,
    damping: f64,
    data_scale: f64,
    step_limit: StepLimit,
) -> BlockStep {
    let n = block.len();
    if n == 0 {
        return BlockStep::Solved(0.0);
    }
    let lambda = block_lambda(block, lambdas);
    let (a, rhs) = block_system(
        layout,
        block,
        penalty,
        prior_gradient,
        prior_curvature,
        params,
        lambda,
        damping,
        data_scale,
        step_limit,
    );
    let damping_diag: Vec<f64> = data_diagonal(block, data_scale, step_limit)
        .into_iter()
        .map(|value| damping * value.max(1e-12))
        .collect();
    if block.kind == BlockKind::Nuisance && solve_nuisance_schur(layout, block, &a, &rhs, step) {
        let mut delta: Vec<f64> = block.params.iter().map(|global| step[*global]).collect();
        if step_limit == StepLimit::PerBlock {
            clamp_block_step(step, &block.params, STEP_RADIUS);
            delta = block.params.iter().map(|global| step[*global]).collect();
        }
        return BlockStep::Solved(predicted_reduction(&a, &rhs, &delta, &damping_diag));
    }
    let mut delta = rhs.clone();
    let mut factor = a.clone();
    if !factor_in_place(&mut factor, n) {
        return BlockStep::Failed;
    }
    solve_in_place(&factor, n, &mut delta);
    if step_limit == StepLimit::PerBlock {
        clamp_values(&mut delta, STEP_RADIUS);
    }
    for i in 0..n {
        step[block.params[i]] = delta[i];
    }
    BlockStep::Solved(predicted_reduction(&a, &rhs, &delta, &damping_diag))
}

#[allow(clippy::too_many_arguments)]
fn block_system(
    layout: &Layout,
    block: &BlockAcc,
    penalty: &[f64],
    prior_gradient: &[f64],
    prior_curvature: &[f64],
    params: &Params,
    lambda: f64,
    damping: f64,
    data_scale: f64,
    step_limit: StepLimit,
) -> (Vec<f64>, Vec<f64>) {
    let n = block.len();
    let mut a: Vec<f64> = block
        .hessian
        .iter()
        .map(|value| value * data_scale)
        .collect();
    for entry in 0..n * n {
        a[entry] += lambda * penalty[entry];
    }
    let diagonal = data_diagonal(block, data_scale, step_limit);
    for i in 0..n {
        a[i * n + i] = diagonal[i]
            + lambda * penalty[i * n + i]
            + damping * diagonal[i].max(1e-12)
            + prior_curvature[i];
    }
    let mut rhs: Vec<f64> = block.gradient.iter().map(|value| -value).collect();
    for i in 0..n {
        let mut gradient = 0.0;
        for j in 0..n {
            gradient += penalty[i * n + j] * params.theta[block.params[j]];
        }
        rhs[i] -= lambda * gradient + prior_gradient[i];
    }
    let identity = identity_matrix(layout, block);
    for entry in 0..n * n {
        a[entry] += identity[entry];
    }
    for i in 0..n {
        let mut sum = 0.0;
        for j in 0..n {
            sum += identity[i * n + j] * params.theta[block.params[j]];
        }
        rhs[i] -= sum;
    }
    (a, rhs)
}

/// Eliminate each image's independent nuisance block, leaving one small dense solve
/// for the shared mode offsets and clip.
fn solve_nuisance_schur(
    layout: &Layout,
    block: &BlockAcc,
    a: &[f64],
    rhs: &[f64],
    step: &mut [f64],
) -> bool {
    let n = block.len();
    let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
    let mut global: Vec<usize> = Vec::new();
    for (local, global_index) in block.params.iter().enumerate() {
        if layout.images.contains(global_index) {
            let image = (global_index - layout.images.start) / NUISANCE_PER_IMAGE;
            match groups.last_mut() {
                Some((owner, entries)) if *owner == image => entries.push(local),
                _ => groups.push((image, vec![local])),
            }
        } else {
            global.push(local);
        }
    }
    if groups.is_empty() {
        return false;
    }
    let mut owner: Vec<Option<usize>> = vec![None; n];
    for (group, (_, entries)) in groups.iter().enumerate() {
        for &entry in entries {
            owner[entry] = Some(group);
        }
    }
    for (i, first) in owner.iter().enumerate() {
        if first.is_none() {
            continue;
        }
        for (j, second) in owner.iter().enumerate() {
            if second.is_none() || first == second {
                continue;
            }
            if a[i * n + j] != 0.0 {
                return false;
            }
        }
    }
    let g = global.len();
    if g == 0 {
        return false;
    }
    let mut schur = vec![0.0_f64; g * g];
    let mut target = vec![0.0_f64; g];
    for (row, &i) in global.iter().enumerate() {
        target[row] = rhs[i];
        for (column, &j) in global.iter().enumerate() {
            schur[row * g + column] = a[i * n + j];
        }
    }
    let mut solved_groups: Vec<Vec<f64>> = Vec::with_capacity(groups.len());
    let mut couplings: Vec<Vec<f64>> = Vec::with_capacity(groups.len());
    for (_, entries) in &groups {
        let m = entries.len();
        let mut factor = vec![0.0_f64; m * m];
        for (row, &i) in entries.iter().enumerate() {
            for (column, &j) in entries.iter().enumerate() {
                factor[row * m + column] = a[i * n + j];
            }
        }
        if !factor_in_place(&mut factor, m) {
            return false;
        }
        let mut solved_rhs = vec![0.0_f64; m];
        for (row, &i) in entries.iter().enumerate() {
            solved_rhs[row] = rhs[i];
        }
        solve_in_place(&factor, m, &mut solved_rhs);
        let mut coupling = vec![0.0_f64; m * g];
        for (column, &j) in global.iter().enumerate() {
            for (row, &i) in entries.iter().enumerate() {
                coupling[row * g + column] = a[i * n + j];
            }
            let mut column_values = vec![0.0_f64; m];
            for row in 0..m {
                column_values[row] = coupling[row * g + column];
            }
            solve_in_place(&factor, m, &mut column_values);
            for row in 0..m {
                coupling[row * g + column] = column_values[row];
            }
        }
        for (r, &i) in entries.iter().enumerate() {
            for column in 0..g {
                target[column] -= a[i * n + global[column]] * solved_rhs[r];
            }
        }
        for (row, &i) in entries.iter().enumerate() {
            for left in 0..g {
                for right in 0..g {
                    schur[left * g + right] -= a[i * n + global[left]] * coupling[row * g + right];
                }
            }
        }
        solved_groups.push(solved_rhs);
        couplings.push(coupling);
    }
    let mut x0 = target;
    if !factor_in_place(&mut schur, g) {
        return false;
    }
    solve_in_place(&schur, g, &mut x0);
    for (index, &i) in global.iter().enumerate() {
        step[block.params[i]] = x0[index];
    }
    for (entry_index, (_, entries)) in groups.iter().enumerate() {
        let m = entries.len();
        let solved = &solved_groups[entry_index];
        let coupling = &couplings[entry_index];
        for row in 0..m {
            let mut value = solved[row];
            for column in 0..g {
                value -= coupling[row * g + column] * x0[column];
            }
            step[block.params[entries[row]]] = value;
        }
    }
    true
}

fn data_diagonal(block: &BlockAcc, data_scale: f64, step_limit: StepLimit) -> Vec<f64> {
    let n = block.len();
    if step_limit == StepLimit::PerBlock && block.full_diagonal.len() == n {
        return block.full_diagonal.clone();
    }
    (0..n)
        .map(|index| block.hessian[index * n + index] * data_scale)
        .collect()
}

fn clamp_values(values: &mut [f64], radius: f64) {
    let largest = values
        .iter()
        .filter(|value| value.is_finite())
        .fold(0.0_f64, |largest, value| largest.max(value.abs()));
    if largest > radius {
        let factor = radius / largest;
        for value in values {
            *value *= factor;
        }
    }
}

/// `-g·δ - 0.5 δᵀBδ` for the undamped model `B = A − damping·diag`.
fn predicted_reduction(a: &[f64], rhs: &[f64], delta: &[f64], damping_diag: &[f64]) -> f64 {
    let n = delta.len();
    let first: f64 = rhs.iter().zip(delta.iter()).map(|(r, d)| r * d).sum();
    let mut quadratic = 0.0_f64;
    for i in 0..n {
        let mut row = 0.0_f64;
        for j in 0..n {
            row += a[i * n + j] * delta[j];
        }
        quadratic += delta[i] * row;
    }
    let damped: f64 = (0..n).map(|i| damping_diag[i] * delta[i] * delta[i]).sum();
    first - 0.5 * (quadratic - damped)
}

fn clamp_block_step(step: &mut [f64], params: &[usize], radius: f64) {
    let largest = params
        .iter()
        .filter_map(|global| step.get(*global))
        .fold(0.0_f64, |largest, value| largest.max(value.abs()));
    if largest > radius {
        let factor = radius / largest;
        for global in params {
            if let Some(value) = step.get_mut(*global) {
                *value *= factor;
            }
        }
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

fn apply_step_unclamped(params: &mut Params, step: &[f64]) {
    for (index, value) in step.iter().enumerate() {
        if value.is_finite() {
            params.theta[index] += value;
        }
    }
}

fn update_sigma(problem: &Problem, params: &mut Params, pass: &PassOutput, options: &FitOptions) {
    let (a, b) = options.sigma_prior;
    let nu = problem.nu;
    for index in 0..problem.contributors.len() {
        let pairs = &pass.contributor_pairs[index];
        if pairs.is_empty() {
            continue;
        }
        let mut low = -6.0_f64;
        let mut high = 6.0_f64;
        for _ in 0..48 {
            let mid = 0.5 * (low + high);
            let variance = libm::exp(2.0 * mid);
            let mut derivative = 2.0 * a - 2.0 * b * libm::exp(-2.0 * mid);
            for (weight, squared) in pairs {
                let u = squared / (nu * variance);
                derivative += weight * (3.0 - (nu + 3.0) * u / (1.0 + u));
            }
            if derivative > 0.0 {
                high = mid;
            } else {
                low = mid;
            }
        }
        params.theta[params.layout.sigma_of(index)] = 0.5 * (low + high);
    }
}

fn residual_support(samples: &[ResidualSample]) -> Vec<f64> {
    let mut support = vec![0.0_f64; RESIDUAL_COEFS];
    for sample in samples {
        for (m, node) in sample.nodes.iter().enumerate() {
            let weight = sample.weights[m];
            for channel in 0..3 {
                support[*node as usize * 3 + channel] += sample.alpha * weight * weight;
            }
        }
    }
    support
}

/// Nodes the data does not reach stay at zero (docs/02 §3.5).
fn pin_residual(params: &mut Params, layout: &Layout, samples: &[ResidualSample]) {
    let support = residual_support(samples);
    let peak = support.iter().copied().fold(0.0_f64, f64::max);
    for (index, &value) in support.iter().enumerate() {
        if value < 1e-4 * peak {
            params.theta[layout.residual.start + index] = 0.0;
        }
    }
}

pub(crate) fn residual_penalty_diagonal() -> Vec<f64> {
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

/// Residual smoothness and identity energy matching the residual step's ridge and identity.
fn residual_cost(layout: &Layout, theta: &[f64], lambda: f64) -> f64 {
    let residual = &theta[layout.residual.clone()];
    let smoothness = 2.0 * lambda * residual_penalty_value(residual);
    let identity = 0.5
        * identity_weight(BlockKind::Residual)
        * residual.iter().map(|value| value * value).sum::<f64>();
    smoothness + identity
}

fn residual_penalty_value(theta: &[f64]) -> f64 {
    let diagonal = residual_penalty_diagonal();
    let mut value = 0.0;
    for index in 0..RESIDUAL_COEFS {
        if theta[index] != 0.0 {
            value += theta[index] * laplacian_at(theta, index, &diagonal);
        }
    }
    0.5 * value
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

fn residual_undamped_apply(
    samples: &[ResidualSample],
    v: &[f64],
    product: &mut [f64],
    penalty_diagonal: &[f64],
    ridge: f64,
    identity: f64,
    hessian_scale: f64,
) {
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
                product[*node as usize * 3 + channel] +=
                    hessian_scale * sample.alpha * weight * gradient;
            }
        }
    }
    for index in 0..RESIDUAL_COEFS {
        product[index] += ridge * laplacian_at(v, index, penalty_diagonal);
        product[index] += (1e-3 + identity) * v[index];
    }
}

fn residual_decrement(
    pass: &PassOutput,
    layout: &Layout,
    params: &Params,
    lambdas: &StageLambdas,
    data_scale: f64,
) -> f64 {
    let theta = &params.theta[layout.residual.clone()];
    let penalty = residual_penalty_diagonal();
    let ridge = 2.0 * lam(lambdas, BlockKind::Residual);
    let identity = identity_weight(BlockKind::Residual);
    let support = residual_support(&pass.residual_samples);
    let peak = support.iter().copied().fold(0.0_f64, f64::max);
    let free: Vec<bool> = support
        .iter()
        .map(|s| peak > 0.0 && *s >= 1e-4 * peak)
        .collect();
    let mut rhs = vec![0.0; RESIDUAL_COEFS];
    let mut diagonal = residual_diagonal(&pass.residual_samples);
    for i in 0..RESIDUAL_COEFS {
        if free[i] {
            rhs[i] = pass.residual_gradient[i]
                - ridge * laplacian_at(theta, i, &penalty)
                - identity * theta[i];
        }
        diagonal[i] = if free[i] {
            data_scale * diagonal[i] + ridge * penalty[i] + identity
        } else {
            1.0
        };
    }
    let apply = |v: &[f64], product: &mut [f64]| {
        residual_undamped_apply(
            &pass.residual_samples,
            v,
            product,
            &penalty,
            ridge,
            identity,
            data_scale,
        );
        for i in 0..RESIDUAL_COEFS {
            product[i] = if free[i] {
                product[i] - 1e-3 * v[i]
            } else {
                v[i]
            };
        }
    };
    let mut solution = vec![0.0; RESIDUAL_COEFS];
    crate::solve::conjugate_gradient(
        RESIDUAL_COEFS,
        apply,
        &diagonal,
        &rhs,
        &mut solution,
        40,
        1e-6,
    );
    let mut product = vec![0.0; RESIDUAL_COEFS];
    apply(&solution, &mut product);
    let error: f64 = product
        .iter()
        .zip(&rhs)
        .map(|(a, b)| (a - b) * (a - b))
        .sum();
    let norm: f64 = rhs.iter().map(|v| v * v).sum();
    let energy = 0.5 * rhs.iter().zip(solution).map(|(g, z)| g * z).sum::<f64>();
    if error.is_finite() && error <= 1e-12 * norm.max(1.0) && energy.is_finite() && energy >= 0.0 {
        energy
    } else {
        f64::INFINITY
    }
}

fn residual_step(
    samples: &[ResidualSample],
    full_gradient: &[f64],
    theta: &[f64],
    lambda: f64,
    damping: f64,
    hessian_scale: f64,
    step_limit: StepLimit,
) -> Vec<f64> {
    let dim = RESIDUAL_COEFS;
    let penalty_diagonal = residual_penalty_diagonal();
    let data_diagonal: Vec<f64> = residual_diagonal(samples)
        .into_iter()
        .map(|value| value * hessian_scale)
        .collect();
    let mut rhs = full_gradient.to_vec();
    // The objective is `lambda * theta' P theta`, so the ridge matrix is 2 * lambda * P.
    let ridge = 2.0 * lambda;
    let identity = identity_weight(BlockKind::Residual);
    let mut diagonal = vec![0.0_f64; dim];
    for index in 0..dim {
        let base = data_diagonal[index].max(1e-9);
        diagonal[index] = base + ridge * penalty_diagonal[index] + damping * base + 1e-3 + identity;
        rhs[index] -=
            ridge * laplacian_at(theta, index, &penalty_diagonal) + identity * theta[index];
    }
    let mut solution = vec![0.0_f64; dim];
    crate::solve::conjugate_gradient(
        dim,
        |v, product| {
            residual_undamped_apply(
                samples,
                v,
                product,
                &penalty_diagonal,
                ridge,
                identity,
                hessian_scale,
            );
            for index in 0..dim {
                product[index] += damping * data_diagonal[index].max(1e-9) * v[index];
            }
        },
        &diagonal,
        &rhs,
        &mut solution,
        40,
        1e-6,
    );
    if step_limit == StepLimit::PerBlock {
        clamp_values(&mut solution, STEP_RADIUS);
    }
    solution
}

#[cfg(test)]
mod review_tests {
    use super::*;

    #[test]
    fn acceptance_is_sign_safe() {
        assert!(accepted(-46_079.824, -46_079.824));
        assert!(accepted(-100.0, -100.000_001));
        assert!(!accepted(-100.0, -99.0));
        assert!(accepted(100.0, 100.0));
        assert!(!accepted(100.0, 100.1));
        assert!(!accepted(100.0, f64::NAN));
        assert!(!accepted(100.0, f64::INFINITY));
    }

    #[test]
    fn decrement_is_invariant_to_coordinate_units() {
        let energy = undamped_decrement(vec![4.0], &[2.0]);
        let rescaled = undamped_decrement(vec![4.0 * 57.3 * 57.3], &[2.0 * 57.3]);
        assert!((energy - 0.5).abs() < 1e-12);
        assert!((energy - rescaled).abs() < 1e-12);
        assert!(undamped_decrement(vec![0.0], &[2.0]).is_infinite());
    }

    #[cfg(feature = "synth")]
    #[test]
    fn tiny_damped_steps_do_not_certify_stationarity() {
        let data = crate::synth::dataset(&crate::synth::SynthConfig {
            images: 2,
            samples_per_image: 16,
            ..crate::synth::SynthConfig::default()
        });
        let options = FitOptions::default();
        let problem = Problem::new_with(&data.images, None, &options).expect("problem");
        let layout = &problem.layout;
        let mut params = initial_params(&problem, &options);
        params.theta[layout.tone_c0] = 0.0;

        let mut block = BlockAcc::new(BlockKind::Tone, vec![layout.tone_c0], layout.total);
        block.hessian[0] = 4.0;
        block.full_diagonal[0] = 4.0;
        block.gradient[0] = 2.0;
        let penalty = vec![0.0];
        let roles = prior_roles(&problem, layout);
        let (prior_gradient, prior_curvature) =
            prior_terms(&problem, &roles, &params, &options, &block);
        let mut step = vec![0.0; layout.total];
        solve_block(
            layout,
            &block,
            &mut step,
            &penalty,
            &prior_gradient,
            &prior_curvature,
            &params,
            &options.initial_lambdas,
            1e12,
            1.0,
            StepLimit::PerBlock,
        );
        assert!(step[layout.tone_c0].abs() < 1e-9);
        let pass = problem.pass(
            &params,
            ActiveColumns::ALL,
            &mut [],
            false,
            false,
            false,
            IntentFilter::NativeOnly,
        );
        let decrement = stationarity_decrement(
            layout,
            &problem,
            &params,
            &options,
            &[block],
            &[penalty],
            &options.initial_lambdas,
            &roles,
            &pass,
            1.0,
            &mut None,
        );
        assert!(converged(&[100.0; 5]));
        assert!(decrement > STATIONARY_DECREMENT_NATS);
    }

    #[cfg(feature = "synth")]
    #[test]
    fn coupled_curvature_retains_cross_layer_products() {
        let data = crate::synth::dataset(&crate::synth::SynthConfig {
            images: 2,
            samples_per_image: 16,
            ..crate::synth::SynthConfig::default()
        });
        let options = FitOptions::default();
        let problem = Problem::new_with(&data.images, None, &options).expect("problem");
        let layout = &problem.layout;
        let mut params = initial_params(&problem, &options);
        let globals = [layout.tone_c0, layout.tint_a.start];
        let mut blocks = [
            BlockAcc::new(BlockKind::Tone, vec![globals[0]], layout.total),
            BlockAcc::new(BlockKind::Tint, vec![globals[1]], layout.total),
        ];
        for (i, block) in blocks.iter_mut().enumerate() {
            params.theta[globals[i]] = 0.0;
            let derivative = (i + 2) as f64;
            block.hessian[0] = derivative * derivative;
            block.full_diagonal[0] = derivative * derivative;
            block.gradient[0] = -derivative;
        }
        let mut pass = problem.pass(
            &params,
            ActiveColumns::ALL,
            &mut [],
            false,
            false,
            false,
            IntentFilter::NativeOnly,
        );
        pass.coupled_rows = vec![crate::problem::HessianRow {
            columns: vec![
                (globals[0] as u32, [2.0, 0.0, 0.0]),
                (globals[1] as u32, [3.0, 0.0, 0.0]),
            ],
            weight: 1.0,
            robust_terms: vec![0.0, 0.0],
            robust: 0.0,
            margin_terms: vec![0.0, 0.0],
            barrier: 0.0,
            residual: [0.0; 3],
            residual_columns: Vec::new(),
            layers: vec![None; 2],
            layer_weights: vec![[0.0; 4]; 2],
            layer_slopes: vec![0.0; 2],
            motion: [0.0; 3],
            tint: [0.0; 4],
            second: Box::new([0.0; 81]),
        }];
        let model = JointModel::new(
            layout,
            &problem,
            &params,
            &options,
            &blocks,
            &[vec![0.0], vec![0.0]],
            &options.initial_lambdas,
            &prior_roles(&problem, layout),
            &pass,
            1.0,
            true,
            false,
        )
        .expect("positive definite curvature");
        let solve = JointSolve::new(&model, 0.0).expect("factorization");
        let solution = solve
            .solve(&model, 1e-12, None, None)
            .expect("resolved joint solve");
        assert!((5.0 * solution.delta[0] + 6.0 * solution.delta[1] - 2.0).abs() < 1e-10);
        assert!((6.0 * solution.delta[0] + 9.25 * solution.delta[1] - 3.0).abs() < 1e-10);
        assert!(solution.prediction > STATIONARY_DECREMENT_NATS);
    }

    #[test]
    fn identity_curvature_follows_each_layer() {
        let layout = Layout::with_response(1, 0, 1, 0);
        let block = BlockAcc::new(
            BlockKind::Tone,
            vec![
                layout.tone_c0,
                layout.tint_a.start,
                layout.g.start,
                layout.r.start,
                layout.lambda.start,
                layout.residual.start,
                layout.image(0).start,
                layout.clip,
            ],
            layout.total,
        );
        let curvature = identity_curvature(&layout, &block);
        assert_eq!(curvature[0], identity_weight(BlockKind::Tone));
        assert_eq!(curvature[1], identity_weight(BlockKind::Tint));
        assert_eq!(curvature[2], identity_weight(BlockKind::G));
        assert_eq!(curvature[3], identity_weight(BlockKind::R));
        assert_eq!(curvature[4], identity_weight(BlockKind::Lambda));
        assert_eq!(curvature[5], identity_weight(BlockKind::Residual));
        assert_eq!(curvature[6], 0.0);
        assert_eq!(curvature[7], 0.0);
    }

    #[cfg(feature = "synth")]
    #[test]
    fn truth_parameters_leave_quantization_residuals() {
        let config = crate::synth::SynthConfig {
            seed: 21,
            images: 4,
            samples_per_image: 200,
            noise_ucs: 0.0,
            ..crate::synth::SynthConfig::default()
        };
        let data = crate::synth::dataset(&config);
        let problem =
            Problem::new_with(&data.images, None, &FitOptions::default()).expect("problem");
        let mut params = params_from_look(&problem, &data.truth);
        for index in 0..problem.images.len() {
            let range = problem.layout.image(index);
            let nuisance = data.nuisances[index];
            params.theta[range.start] = nuisance.exposure;
            params.theta[range.start + 1] = nuisance.wb_log_r;
            params.theta[range.start + 2] = nuisance.wb_log_b;
        }
        params.theta[problem.layout.clip] = crate::layout::zeta_from_clip(data.clip_k);
        let mut blocks: Vec<BlockAcc> = Vec::new();
        let pass = problem.pass(
            &params,
            ActiveColumns::ALL,
            &mut blocks,
            true,
            false,
            false,
            IntentFilter::All,
        );
        let mut norms: Vec<f64> = pass
            .residuals
            .iter()
            .map(|residual| {
                (residual[0] * residual[0] + residual[1] * residual[1] + residual[2] * residual[2])
                    .sqrt()
            })
            .collect();
        norms.sort_by(f64::total_cmp);
        let median = norms[norms.len() / 2];
        let maximum = norms[norms.len() - 1];
        assert!(median < 1e-4, "median residual {median}");
        assert!(maximum < 1e-3, "maximum residual {maximum}");
    }

    #[test]
    fn identity_apply_is_diagonal() {
        let layout = Layout::with_response(0, 0, 0, 0);
        let block = BlockAcc::new(BlockKind::G, layout.g.clone().collect(), layout.total);
        let x = vec![1.0_f64; layout.total];
        let applied = identity_apply(&layout, &block, &x);
        for value in &applied {
            assert_eq!(*value, identity_weight(BlockKind::G));
        }
    }

    #[test]
    fn gcv_fixed_priors_resolve_the_exposure_mode_gauge() {
        // Rows for two image exposures plus their shared mode offset, and one tone coefficient.
        // The exposure/mode columns have the null vector [1, 1, -1, 0].
        let n = 4;
        let rows = [
            [1.0, 0.0, 1.0, 0.0],
            [0.0, 1.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 2.0],
        ];
        let mut hessian = vec![0.0_f64; n * n];
        for row in rows {
            for i in 0..n {
                for j in 0..n {
                    hessian[i * n + j] += row[i] * row[j];
                }
            }
        }
        let mut penalty = vec![0.0_f64; n * n];
        penalty[3 * n + 3] = 1.0;
        let gradient = vec![0.2, -0.2, 0.0, 0.3];
        let theta0 = vec![0.3, 0.4, 0.5, 0.2];
        let precision = 1.0 / (0.3 * 0.3);
        let fixed_curvature = vec![precision, precision, precision, 1.0];
        let fixed_gradient: Vec<f64> = fixed_curvature
            .iter()
            .zip(theta0.iter())
            .map(|(curvature, theta)| curvature * theta)
            .collect();
        let choice = select_lambda(
            &hessian,
            &fixed_curvature,
            &penalty,
            &gradient,
            &fixed_gradient,
            &theta0,
            0.5,
            n,
            100.0,
            4,
            7,
        );
        assert!(choice.lambda.is_finite() && choice.lambda > 0.0);
    }
}

#[cfg(test)]
mod schur_tests {
    use super::*;

    struct NuisanceSystem {
        layout: Layout,
        params: Params,
        block: BlockAcc,
        penalty: Vec<f64>,
        prior_gradient: Vec<f64>,
        prior_curvature: Vec<f64>,
        gradient: Vec<f64>,
    }

    fn nuisance_system(cross_image: f64) -> NuisanceSystem {
        let layout = Layout::new(3, 1, 0);
        let mut globals = Vec::new();
        for image in 0..3 {
            globals.extend(layout.image(image));
        }
        globals.push(layout.modes.start);
        globals.push(layout.clip);
        let n = globals.len();
        let mut hessian = vec![0.0_f64; n * n];
        for i in 0..n {
            hessian[i * n + i] = 40.0 + i as f64;
        }
        let mode_local = globals
            .iter()
            .position(|global| *global == layout.modes.start)
            .expect("mode offset is in the block");
        let clip_local = globals
            .iter()
            .position(|global| *global == layout.clip)
            .expect("clip parameter is in the block");
        for image in 0..3 {
            let base = image * 3;
            for a in 0..3 {
                for b in a + 1..3 {
                    let value = 0.05 * (a + b + image + 1) as f64;
                    hessian[(base + a) * n + base + b] = value;
                    hessian[(base + b) * n + base + a] = value;
                }
                for global in [mode_local, clip_local] {
                    let value = 0.12 * (a + 1) as f64 + 0.01 * image as f64;
                    hessian[(base + a) * n + global] = value;
                    hessian[global * n + base + a] = value;
                }
            }
        }
        hessian[mode_local * n + clip_local] = 0.2;
        hessian[clip_local * n + mode_local] = 0.2;
        hessian[3] += cross_image;
        hessian[3 * n] += cross_image;

        let mut params = Params::initial(layout.clone(), 0);
        for (index, global) in globals.iter().enumerate() {
            params.theta[*global] = 0.13 * index as f64 - 0.35;
        }
        let gradient: Vec<f64> = (0..n).map(|i| ((i * 7) % 5) as f64 * 0.1 - 0.2).collect();
        let penalty: Vec<f64> = (0..n * n)
            .map(|index| if index / n == index % n { 0.5 } else { 0.0 })
            .collect();
        let prior_curvature: Vec<f64> = (0..n).map(|i| 0.4 + 0.1 * i as f64).collect();
        let prior_gradient: Vec<f64> = (0..n).map(|i| ((i * 3) % 7) as f64 * 0.03 - 0.05).collect();
        let mut block = BlockAcc::new(BlockKind::Nuisance, globals, layout.total);
        block.hessian = hessian;
        block.gradient = gradient.clone();
        NuisanceSystem {
            layout,
            params,
            block,
            penalty,
            prior_gradient,
            prior_curvature,
            gradient,
        }
    }

    fn dense_solution(system: &NuisanceSystem, lambda: f64) -> Vec<f64> {
        let mut block = BlockAcc::new(
            BlockKind::Tone,
            system.block.params.clone(),
            system.layout.total,
        );
        block.hessian = system.block.hessian.clone();
        block.gradient = system.gradient.clone();
        let (mut a, rhs) = block_system(
            &system.layout,
            &block,
            &system.penalty,
            &system.prior_gradient,
            &system.prior_curvature,
            &system.params,
            lambda,
            1e-3,
            1.0,
            StepLimit::WholeStep,
        );
        let n = block.len();
        assert!(factor_in_place(&mut a, n));
        let mut delta = rhs;
        solve_in_place(&a, n, &mut delta);
        delta
    }

    #[test]
    fn nuisance_schur_matches_dense_solve_with_priors_and_shared_couplings() {
        let system = nuisance_system(0.0);
        let lambda = 0.9;
        let mut dense_block = BlockAcc::new(
            BlockKind::Tone,
            system.block.params.clone(),
            system.layout.total,
        );
        dense_block.hessian = system.block.hessian.clone();
        dense_block.gradient = system.gradient.clone();
        let (a, rhs) = block_system(
            &system.layout,
            &dense_block,
            &system.penalty,
            &system.prior_gradient,
            &system.prior_curvature,
            &system.params,
            lambda,
            1e-3,
            1.0,
            StepLimit::WholeStep,
        );
        let dense = dense_solution(&system, lambda);
        let mut schur = vec![0.0_f64; system.layout.total];
        assert!(solve_nuisance_schur(
            &system.layout,
            &system.block,
            &a,
            &rhs,
            &mut schur
        ));
        for (local, global) in system.block.params.iter().enumerate() {
            assert!(
                (dense[local] - schur[*global]).abs() < 1e-9,
                "index {global}: {} vs {}",
                dense[local],
                schur[*global]
            );
        }
    }

    #[test]
    fn nuisance_schur_dispatches_from_the_block_solve() {
        let system = nuisance_system(0.0);
        let dense = dense_solution(&system, 0.0);
        let mut step = vec![0.0_f64; system.layout.total];
        solve_block(
            &system.layout,
            &system.block,
            &mut step,
            &system.penalty,
            &system.prior_gradient,
            &system.prior_curvature,
            &system.params,
            &StageLambdas::default(),
            1e-3,
            1.0,
            StepLimit::WholeStep,
        );
        for (local, global) in system.block.params.iter().enumerate() {
            assert!(
                (dense[local] - step[*global]).abs() < 1e-9,
                "index {global}: {} vs {}",
                dense[local],
                step[*global]
            );
        }
    }

    #[test]
    fn nuisance_schur_falls_back_without_independent_image_blocks() {
        let system = nuisance_system(0.5);
        let (a, rhs) = block_system(
            &system.layout,
            &system.block,
            &system.penalty,
            &system.prior_gradient,
            &system.prior_curvature,
            &system.params,
            0.9,
            1e-3,
            1.0,
            StepLimit::WholeStep,
        );
        let mut step = vec![0.0_f64; system.layout.total];
        assert!(!solve_nuisance_schur(
            &system.layout,
            &system.block,
            &a,
            &rhs,
            &mut step
        ));
    }

    fn joint_blocks(problem: &Problem, layers: bool) -> Vec<BlockAcc> {
        let layout = &problem.layout;
        let split = problem.images.len() > 64;
        let mut blocks: Vec<BlockAcc> = STAGES[4]
            .blocks
            .iter()
            .filter(|kind| **kind != BlockKind::Residual && !is_response_kind(**kind))
            .filter(|kind| {
                layers || !matches!(kind, BlockKind::G | BlockKind::R | BlockKind::Lambda)
            })
            .map(|kind| {
                if *kind == BlockKind::Tone && !split {
                    make_joint_tone_block(problem, ActiveColumns::ALL)
                } else {
                    make_block(problem, *kind, ActiveColumns::ALL)
                }
            })
            .collect();
        if split {
            for index in 0..problem_image_count(layout) {
                blocks.push(make_image_block(layout, index, ActiveColumns::ALL));
            }
            blocks.push(make_shared_nuisance_block(
                problem,
                layout,
                ActiveColumns::ALL,
            ));
        }
        blocks
    }

    /// The same joint curvature and right-hand side, assembled densely with no elimination.
    #[allow(clippy::too_many_arguments)]
    fn dense_joint_matrix(
        layout: &Layout,
        problem: &Problem,
        params: &Params,
        options: &FitOptions,
        blocks: &[BlockAcc],
        penalties: &[Vec<f64>],
        lambdas: &StageLambdas,
        roles: &[Role],
        pass: &PassOutput,
        damping: f64,
        robust: bool,
    ) -> (Vec<f64>, Vec<f64>, Vec<usize>, Vec<usize>) {
        let mut globals: Vec<usize> = Vec::new();
        let mut local = vec![usize::MAX; layout.total];
        for block in blocks {
            for &global in &block.params {
                if local[global] == usize::MAX {
                    local[global] = globals.len();
                    globals.push(global);
                }
            }
        }
        let n = globals.len();
        let mut a = vec![0.0_f64; n * n];
        let mut rhs = vec![0.0_f64; n];
        for row in &pass.coupled_rows {
            for (position, (left, left_derivative)) in row.columns.iter().enumerate() {
                let i = local[*left as usize];
                if i == usize::MAX {
                    continue;
                }
                for (other, (right, right_derivative)) in row.columns.iter().enumerate() {
                    let j = local[*right as usize];
                    if j == usize::MAX {
                        continue;
                    }
                    a[i * n + j] += row.weight * dot3(*left_derivative, *right_derivative)
                        + row.barrier * row.margin_terms[position] * row.margin_terms[other]
                        + if robust {
                            row.robust * row.robust_terms[position] * row.robust_terms[other]
                        } else {
                            0.0
                        };
                }
            }
        }
        for (index, block) in blocks.iter().enumerate() {
            let m = block.len();
            let lambda = block_lambda(block, lambdas);
            let (prior_gradient, prior_curvature) =
                prior_terms(problem, roles, params, options, block);
            let identity = identity_curvature(layout, block);
            for (entry, &global) in block.params.iter().enumerate() {
                let i = local[global];
                let mut gradient = block.gradient[entry]
                    + prior_gradient[entry]
                    + identity[entry] * params.theta[global];
                for (other, &target) in block.params.iter().enumerate() {
                    let j = local[target];
                    let value = lambda * penalties[index][entry * m + other];
                    a[i * n + j] += value;
                    gradient += value * params.theta[target];
                }
                a[i * n + i] += identity[entry]
                    + prior_curvature[entry]
                    + damping * block.full_diagonal[entry].max(1e-12);
                rhs[i] = -gradient;
            }
        }
        (a, rhs, globals, local)
    }

    #[allow(clippy::too_many_arguments)]
    fn dense_joint_solve(
        layout: &Layout,
        problem: &Problem,
        params: &Params,
        options: &FitOptions,
        blocks: &[BlockAcc],
        penalties: &[Vec<f64>],
        lambdas: &StageLambdas,
        roles: &[Role],
        pass: &PassOutput,
        damping: f64,
        robust: bool,
    ) -> Vec<f64> {
        let (mut factor, rhs, globals, _) = dense_joint_matrix(
            layout, problem, params, options, blocks, penalties, lambdas, roles, pass, damping,
            robust,
        );
        let n = globals.len();
        assert!(
            factor_in_place(&mut factor, n),
            "dense joint curvature is not positive definite"
        );
        let mut solution = rhs;
        solve_in_place(&factor, n, &mut solution);
        let mut out = vec![0.0_f64; layout.total];
        for (index, &global) in globals.iter().enumerate() {
            out[global] = solution[index];
        }
        out
    }

    fn schur_matches_direct_joint_solve(
        images: usize,
        samples: usize,
        modes: bool,
        zero_weight: bool,
        damping: f64,
        robust: bool,
    ) {
        let mut data = crate::synth::dataset(&crate::synth::SynthConfig {
            images,
            samples_per_image: samples,
            ..crate::synth::SynthConfig::default()
        });
        if modes {
            for image in data.images.iter_mut().skip(1).step_by(2) {
                image.exposure_mode = crate::types::ExposureModeId(1);
            }
        }
        if zero_weight {
            data.images[1].trust_weight = 0.0;
        }
        let options = FitOptions {
            hessian_samples: usize::MAX,
            exposure_prior_sd: 1.0,
            exposure_mode_prior_sd: 0.5,
            ..FitOptions::default()
        };
        let problem = Problem::new_with(&data.images, None, &options).expect("problem");
        let layout = &problem.layout;
        let params = initial_params(&problem, &options);
        let roles = prior_roles(&problem, layout);
        let lambdas = StageLambdas {
            tone: 1.0,
            tint: 1.0,
            grl: 1.0,
            ..StageLambdas::default()
        };
        let mut blocks = joint_blocks(&problem, false);
        let penalties: Vec<Vec<f64>> = blocks
            .iter()
            .map(|block| block_penalty(&problem, block))
            .collect();
        let pass = problem.pass_with_rows(
            &params,
            ActiveColumns::ALL,
            &mut blocks,
            false,
            false,
            false,
            IntentFilter::NativeOnly,
            true,
            true,
        );
        let model = JointModel::new(
            layout, &problem, &params, &options, &blocks, &penalties, &lambdas, &roles, &pass, 1.0,
            robust, false,
        )
        .expect("joint model");
        if robust {
            let (a, _, globals, local) = dense_joint_matrix(
                layout, &problem, &params, &options, &blocks, &penalties, &lambdas, &roles, &pass,
                0.0, true,
            );
            let n = globals.len();
            for seed in 0..4usize {
                let mut v = vec![0.0_f64; model.dim];
                let mut dense = vec![0.0_f64; n];
                for index in 0..model.dim {
                    let value = ((index * 37 + seed * 101) % 23) as f64 - 11.0;
                    v[index] = value;
                    dense[local[model.globals[index]]] = value;
                }
                let mut product = vec![0.0_f64; model.dim];
                model.apply_undamped(&v, &mut product);
                for index in 0..model.dim {
                    let row = local[model.globals[index]];
                    let mut sum = 0.0;
                    for column in 0..n {
                        sum += a[row * n + column] * dense[column];
                    }
                    assert!(
                        (product[index] - sum).abs() <= 1e-8 * sum.abs().max(1.0),
                        "robust operator entry {index}: model {} dense {}",
                        product[index],
                        sum
                    );
                }
            }
            return;
        }
        let solve = JointSolve::new(&model, damping).expect("factorization");
        let solution = solve
            .solve(&model, 1e-12, None, None)
            .expect("resolved joint solve");
        let dense = dense_joint_solve(
            layout, &problem, &params, &options, &blocks, &penalties, &lambdas, &roles, &pass,
            damping, robust,
        );
        let mut product = vec![0.0_f64; model.dim];
        model.apply_undamped(&solution.delta, &mut product);
        for (index, value) in product.iter_mut().enumerate() {
            *value += damping * model.data_diagonal[index].max(1e-12) * solution.delta[index];
        }
        let mut blocks = vec![0.0_f64; 3];
        for index in 0..model.dim {
            let residual = product[index] - model.rhs[index];
            let slot = if index < model.outer_count {
                0
            } else if index < model.outer_count + model.border_count {
                1
            } else {
                2
            };
            blocks[slot] = blocks[slot].max(residual.abs());
        }
        assert!(
            solution.relative_residual < 1e-9,
            "recovered joint residual {} iterations {} failure {:?} q {} dim {} block residuals {blocks:?}",
            solution.relative_residual,
            solution.iterations,
            solution.failure,
            model.outer_count,
            model.dim
        );
        let mut largest = 0.0_f64;
        for (index, &global) in model.globals.iter().enumerate() {
            largest = largest.max((solution.delta[index] - dense[global]).abs());
        }
        assert!(
            largest < 1e-8,
            "Schur and direct joint steps differ by {largest}"
        );
    }

    #[cfg(feature = "synth")]
    #[test]
    fn schur_matches_direct_joint_solve_with_modes_and_weak_exposure_priors() {
        schur_matches_direct_joint_solve(4, 32, true, false, 1e-2, false);
    }

    #[cfg(feature = "synth")]
    #[test]
    fn schur_matches_direct_joint_solve_with_a_zero_weight_image() {
        schur_matches_direct_joint_solve(3, 32, true, true, 1e-3, false);
    }

    #[cfg(feature = "synth")]
    #[test]
    fn schur_matches_direct_joint_solve_below_the_nuisance_split() {
        schur_matches_direct_joint_solve(64, 4, true, false, 1e-2, false);
    }

    #[cfg(feature = "synth")]
    #[test]
    fn schur_matches_direct_joint_solve_above_the_nuisance_split() {
        schur_matches_direct_joint_solve(65, 4, true, false, 1e-2, false);
    }

    #[cfg(feature = "synth")]
    #[test]
    fn schur_matches_direct_joint_solve_with_the_robust_correction() {
        schur_matches_direct_joint_solve(4, 32, true, false, 1e-2, true);
    }

    fn one_coordinate_certificate(identity_scale: f64) -> f64 {
        let data = crate::synth::dataset(&crate::synth::SynthConfig {
            images: 1,
            samples_per_image: 8,
            ..crate::synth::SynthConfig::default()
        });
        let options = FitOptions {
            identity_scale,
            ..FitOptions::default()
        };
        let problem = Problem::new_with(&data.images, None, &options).expect("problem");
        let layout = &problem.layout;
        let mut params = initial_params(&problem, &options);
        params.theta[layout.tone_c0] = 0.0;
        let roles = prior_roles(&problem, layout);
        let mut block = BlockAcc::new(BlockKind::Tone, vec![layout.tone_c0], layout.total);
        block.hessian[0] = 4.0;
        block.full_diagonal[0] = 4.0;
        block.gradient[0] = -2.0;
        let mut pass = problem.pass(
            &params,
            ActiveColumns::ALL,
            &mut [],
            false,
            false,
            false,
            IntentFilter::NativeOnly,
        );
        pass.coupled_rows = vec![crate::problem::HessianRow {
            columns: vec![(layout.tone_c0 as u32, [2.0, 0.0, 0.0])],
            weight: 1.0,
            robust_terms: vec![0.0],
            robust: 0.0,
            margin_terms: vec![0.0],
            barrier: 0.0,
            residual: [0.0; 3],
            residual_columns: Vec::new(),
            layers: vec![None; 1],
            layer_weights: vec![[0.0; 4]; 1],
            layer_slopes: vec![0.0; 1],
            motion: [0.0; 3],
            tint: [0.0; 4],
            second: Box::new([0.0; 81]),
        }];
        stationarity_decrement(
            layout,
            &problem,
            &params,
            &options,
            &[block],
            &[vec![0.0]],
            &options.initial_lambdas,
            &roles,
            &pass,
            1.0,
            &mut None,
        )
    }

    #[cfg(feature = "synth")]
    #[test]
    fn certificate_reports_the_exact_decrement_of_a_resolved_system() {
        let decrement = one_coordinate_certificate(1.0);
        assert!((decrement - 0.4).abs() < 1e-9, "decrement {decrement}");
    }

    #[cfg(feature = "synth")]
    #[test]
    fn certificate_rejects_a_missing_prior_floor() {
        let decrement = one_coordinate_certificate(0.0);
        assert!(
            decrement > STATIONARY_DECREMENT_NATS,
            "zero prior floor certified stationarity at {decrement}"
        );
    }
}

/// Exact-Hessian harness for the joint step model (test only).
#[cfg(all(test, feature = "synth"))]
mod curvature_tests {
    use super::*;

    struct Fixture {
        problem: Problem,
        options: FitOptions,
        lambdas: StageLambdas,
        kinds: Vec<BlockKind>,
        roles: Vec<Role>,
        truth: looklab_model::Look,
    }

    impl Fixture {
        fn new(images: usize, samples: usize, seed: u64) -> Self {
            let data = crate::synth::dataset(&crate::synth::SynthConfig {
                seed,
                images,
                samples_per_image: samples,
                ..crate::synth::SynthConfig::default()
            });
            let options = FitOptions::default();
            let problem = Problem::new_with(&data.images, None, &options).expect("problem");
            let roles = prior_roles(&problem, &problem.layout);
            Self {
                kinds: vec![
                    BlockKind::Tone,
                    BlockKind::Tint,
                    BlockKind::G,
                    BlockKind::R,
                    BlockKind::Lambda,
                ],
                lambdas: StageLambdas {
                    tone: 0.7,
                    tint: 0.4,
                    grl: 0.15,
                    residual: 0.1,
                    ..StageLambdas::default()
                },
                problem,
                options,
                roles,
                truth: data.truth,
            }
        }

        fn params(&self) -> Params {
            let mut params = params_from_look(&self.problem, &self.truth);
            let layout = &self.problem.layout;
            for (offset, index) in layout.g.clone().enumerate() {
                params.theta[index] += 0.02 * (1 + offset % 3) as f64;
            }
            for (offset, index) in layout.r.clone().enumerate() {
                params.theta[index] -= 0.01 * (1 + offset % 2) as f64;
            }
            for (offset, index) in layout.lambda.clone().enumerate() {
                params.theta[index] += 0.005 * (1 + offset % 4) as f64;
            }
            for (offset, index) in layout.tint_a.clone().enumerate() {
                params.theta[index] += 0.01 * (1 + offset % 2) as f64;
            }
            for (offset, index) in layout.tone_beta.clone().enumerate() {
                params.theta[index] += 0.02 * (1 + offset % 3) as f64;
            }
            params
        }

        fn blocks(&self) -> Vec<BlockAcc> {
            self.kinds
                .iter()
                .map(|kind| make_block(&self.problem, *kind, ActiveColumns::ALL))
                .collect()
        }

        fn joint(&self, params: &Params) -> (JointModel, PassOutput) {
            let mut blocks = self.blocks();
            let penalties: Vec<Vec<f64>> = blocks
                .iter()
                .map(|block| block_penalty(&self.problem, block))
                .collect();
            let pass = self.problem.pass_with_rows(
                params,
                ActiveColumns::ALL,
                &mut blocks,
                true,
                false,
                false,
                IntentFilter::NativeOnly,
                true,
                true,
            );
            assert!(!pass.infeasible, "harness point is infeasible");
            let model = JointModel::new(
                &self.problem.layout,
                &self.problem,
                params,
                &self.options,
                &blocks,
                &penalties,
                &self.lambdas,
                &self.roles,
                &pass,
                1.0,
                true,
                true,
            )
            .expect("joint model");
            (model, pass)
        }
    }

    struct Noise {
        state: u64,
    }

    impl Noise {
        fn signed(&mut self) -> f64 {
            self.state ^= self.state << 13;
            self.state ^= self.state >> 7;
            self.state ^= self.state << 17;
            2.0 * ((self.state >> 11) as f64 / (1u64 << 53) as f64) - 1.0
        }
    }

    fn dot(a: &[f64], b: &[f64]) -> f64 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    /// Data Hessian-vector product from independent per-sample evaluations: the exact
    /// central difference and the GN + Student-t + acceleration model.
    fn sample_hessian_vectors(
        problem: &Problem,
        params: &Params,
        v: &[f64],
        h: f64,
    ) -> (Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>) {
        let layout = &problem.layout;
        let nu = problem.nu;
        let mut plus = params.clone();
        let mut minus = params.clone();
        for (slot, value) in v.iter().enumerate() {
            plus.theta[slot] += h * value;
            minus.theta[slot] -= h * value;
        }
        let terms_at = |params: &Params, index: usize| {
            let (look, slope) = problem.look_and_slope(params);
            let sample = &problem.samples[index];
            let image = sample.image as usize;
            let context = problem.image_ctx(params, image);
            let columns = problem.column_map(params, image, ActiveColumns::ALL);
            let mut jac = crate::predict::Jac::default();
            let out = crate::predict::evaluate_sample(
                &look,
                layout,
                &context,
                &columns,
                sample.rgb,
                params.clip_k(),
                params.clip_slope(),
                &slope,
                &[],
                &sample.target,
                &mut jac,
            );
            let sigma = params.sigma(problem.images[image].contributor);
            let squared = dot(&out.residual, &out.residual);
            let alpha = sample.weight * (nu + 3.0) / (nu * sigma * sigma + squared);
            let barrier_slope =
                looklab_model::look::barrier_derivatives(out.margin).0 * sample.weight;
            let gradient: Vec<f64> = jac
                .derivs
                .iter()
                .enumerate()
                .map(|(position, derivative)| {
                    alpha * dot(&derivative[..], &out.residual)
                        + barrier_slope * out.margin_terms[position]
                })
                .collect();
            let barrier_gradient: Vec<f64> = out
                .margin_terms
                .iter()
                .map(|term| barrier_slope * term)
                .collect();
            (
                out.residual,
                jac.cols,
                jac.derivs,
                alpha,
                squared,
                gradient,
                barrier_gradient,
            )
        };
        let mut model = vec![0.0_f64; layout.total];
        let mut exact = vec![0.0_f64; layout.total];
        let mut barrier_second = vec![0.0_f64; layout.total];
        let mut gradient_sum = vec![0.0_f64; layout.total];
        for index in 0..problem.samples.len() {
            let (residual, cols, derivs, alpha, squared, gradient, _) = terms_at(params, index);
            let (_, cols_plus, derivs_plus, _, _, gradient_plus, barrier_plus) =
                terms_at(&plus, index);
            let (_, _, derivs_minus, _, _, gradient_minus, barrier_minus) = terms_at(&minus, index);
            assert_eq!(cols, cols_plus);
            let mut projected = [0.0_f64; 3];
            for (column, derivative) in cols.iter().zip(&derivs) {
                let value = v[*column as usize];
                for channel in 0..3 {
                    projected[channel] += derivative[channel] * value;
                }
            }
            let mut robust_projection = 0.0;
            for (column, derivative) in cols.iter().zip(&derivs) {
                robust_projection += dot(&derivative[..], &residual) * v[*column as usize];
            }
            let image = problem.samples[index].image as usize;
            let sigma = params.sigma(problem.images[image].contributor);
            let q = nu * sigma * sigma + squared;
            let robust = -2.0 * (nu + 3.0) / (q * q) * problem.samples[index].weight;
            for position in 0..cols.len() {
                let column = cols[position] as usize;
                let mut accel = 0.0;
                for channel in 0..3 {
                    accel += alpha
                        * residual[channel]
                        * (derivs_plus[position][channel] - derivs_minus[position][channel])
                        / (2.0 * h);
                }
                let gn = alpha
                    * (derivs[position][0] * projected[0]
                        + derivs[position][1] * projected[1]
                        + derivs[position][2] * projected[2]);
                let term = dot(&derivs[position][..], &residual);
                model[column] += gn + robust * term * robust_projection + accel;
                exact[column] += (gradient_plus[position] - gradient_minus[position]) / (2.0 * h);
                barrier_second[column] +=
                    (barrier_plus[position] - barrier_minus[position]) / (2.0 * h);
                gradient_sum[column] += gradient[position];
            }
        }
        (exact, model, gradient_sum, barrier_second)
    }

    fn norm(a: &[f64]) -> f64 {
        dot(a, a).sqrt()
    }

    /// GN, robust and barrier curvature of the retained rows along `v`, joint coordinates.
    fn row_curvature(
        rows: &[crate::problem::HessianRow],
        of: &[usize],
        v: &[f64],
    ) -> (Vec<f64>, Vec<f64>, Vec<f64>) {
        let dim = v.len();
        let mut gn = vec![0.0_f64; dim];
        let mut robust = vec![0.0_f64; dim];
        let mut barrier = vec![0.0_f64; dim];
        for row in rows {
            let mut projected = [0.0_f64; 3];
            let mut robust_projection = 0.0_f64;
            let mut margin_projection = 0.0_f64;
            for (position, (global, derivative)) in row.columns.iter().enumerate() {
                let value = v[of[*global as usize]];
                projected[0] += derivative[0] * value;
                projected[1] += derivative[1] * value;
                projected[2] += derivative[2] * value;
                robust_projection += row.robust_terms[position] * value;
                margin_projection += row.margin_terms[position] * value;
            }
            for (position, (global, derivative)) in row.columns.iter().enumerate() {
                let index = of[*global as usize];
                gn[index] += row.weight * dot3(*derivative, projected);
                robust[index] += row.robust * row.robust_terms[position] * robust_projection;
                barrier[index] += row.barrier * row.margin_terms[position] * margin_projection;
            }
        }
        (gn, robust, barrier)
    }

    /// `α Σ_c r_c ∂²p_c/∂θ² v` and the margin's second-derivative term, from central
    /// differences of the retained Jacobian rows.
    fn second_order_reference(
        base: &PassOutput,
        plus: &PassOutput,
        minus: &PassOutput,
        of: &[usize],
        h: f64,
    ) -> Vec<f64> {
        assert_eq!(base.coupled_rows.len(), plus.coupled_rows.len());
        assert_eq!(base.coupled_rows.len(), minus.coupled_rows.len());
        let dim = of.len();
        let mut accel = vec![0.0_f64; dim];
        for ((row, row_plus), row_minus) in base
            .coupled_rows
            .iter()
            .zip(&plus.coupled_rows)
            .zip(&minus.coupled_rows)
        {
            assert_eq!(row.columns.len(), row_plus.columns.len());
            assert_eq!(row.columns.len(), row_minus.columns.len());
            for position in 0..row.columns.len() {
                assert_eq!(row.columns[position].0, row_plus.columns[position].0);
                assert_eq!(row.columns[position].0, row_minus.columns[position].0);
                let index = of[row.columns[position].0 as usize];
                let mut derivative = [0.0_f64; 3];
                for channel in 0..3 {
                    derivative[channel] = (row_plus.columns[position].1[channel]
                        - row_minus.columns[position].1[channel])
                        / (2.0 * h);
                }
                accel[index] += row.weight * dot3(row.residual, derivative);
            }
        }
        accel
    }

    #[test]
    fn single_sample_data_terms_match_differences() {
        let fixture = Fixture::new(1, 8, 0x5eed_1234);
        let problem = &fixture.problem;
        let params = fixture.params();
        let nu = problem.nu;
        let layout = &problem.layout;

        // The likelihood's data part for one sample, its Jacobian and residual.
        let sample_terms = |params: &Params, index: usize| {
            let (look, slope) = problem.look_and_slope(params);
            let sample = &problem.samples[index];
            let image = sample.image as usize;
            let context = problem.image_ctx(params, image);
            let columns = problem.column_map(params, image, ActiveColumns::ALL);
            let mut jac = crate::predict::Jac::default();
            let out = crate::predict::evaluate_sample(
                &look,
                layout,
                &context,
                &columns,
                sample.rgb,
                params.clip_k(),
                params.clip_slope(),
                &slope,
                &[],
                &sample.target,
                &mut jac,
            );
            let sigma = params.sigma(problem.images[image].contributor);
            let squared = dot(&out.residual, &out.residual);
            let loss = sample.weight
                * (0.5 * (nu + 3.0) * libm::log1p(squared / (nu * sigma * sigma))
                    + 1.5 * libm::log(sigma * sigma));
            let alpha = sample.weight * (nu + 3.0) / (nu * sigma * sigma + squared);
            (loss, out.residual, jac.cols, jac.derivs, alpha)
        };

        let mut noise = Noise {
            state: 0x0bad_c0de_1234_5678,
        };
        for index in [0usize, 3] {
            let (loss, residual, cols, derivs, alpha) = sample_terms(&params, index);
            assert!(loss.is_finite());
            let mut v = vec![0.0_f64; layout.total];
            for (column, _) in cols.iter().zip(&derivs) {
                let column = *column as usize;
                let scale = match kind_of(layout, column) {
                    Some(BlockKind::Tone) => 0.05,
                    Some(BlockKind::Tint) => 0.02,
                    Some(BlockKind::G | BlockKind::R | BlockKind::Lambda) => 0.01,
                    _ => 0.0,
                };
                v[column] = scale * noise.signed();
            }
            let projected = |derivs: &[[f64; 3]], cols: &[u32]| {
                let mut out = [0.0_f64; 3];
                for (column, derivative) in cols.iter().zip(derivs) {
                    let value = v[*column as usize];
                    out[0] += derivative[0] * value;
                    out[1] += derivative[1] * value;
                    out[2] += derivative[2] * value;
                }
                out
            };
            let gradient_along = |params: &Params| -> (f64, f64) {
                let (_, residual, cols, derivs, alpha) = sample_terms(params, index);
                let mut projected = [0.0_f64; 3];
                for (column, derivative) in cols.iter().zip(&derivs) {
                    let value = v[*column as usize];
                    projected[0] += derivative[0] * value;
                    projected[1] += derivative[1] * value;
                    projected[2] += derivative[2] * value;
                }
                (alpha * dot(&residual, &projected), alpha)
            };
            let h = 1e-6;
            let mut plus = params.clone();
            let mut minus = params.clone();
            for (slot, value) in v.iter().enumerate() {
                plus.theta[slot] += h * value;
                minus.theta[slot] -= h * value;
            }
            let (gradient, _) = gradient_along(&params);
            let (gradient_plus, _) = gradient_along(&plus);
            let (gradient_minus, _) = gradient_along(&minus);
            let finite = (gradient_plus - gradient_minus) / (2.0 * h);
            let (loss_plus, _, _, _, _) = sample_terms(&plus, index);
            let (loss_minus, _, _, _, _) = sample_terms(&minus, index);
            let first = (loss_plus - loss_minus) / (2.0 * h);
            // Model pieces at the base point.
            let base_projection = projected(&derivs, &cols);
            let gn = alpha * dot(&base_projection, &base_projection);
            let mut robust_terms = Vec::with_capacity(cols.len());
            for derivative in &derivs {
                robust_terms.push(dot(&derivative[..], &residual));
            }
            let mut robust_projection = 0.0;
            for (column, term) in cols.iter().zip(&robust_terms) {
                robust_projection += term * v[*column as usize];
            }
            let squared = dot(&residual, &residual);
            let sigma =
                params.sigma(problem.images[problem.samples[index].image as usize].contributor);
            let q = nu * sigma * sigma + squared;
            let robust = -2.0 * (nu + 3.0) / (q * q)
                * sample_weight(problem, index)
                * robust_projection
                * robust_projection;
            let (_, _, cols_plus, derivs_plus, _) = sample_terms(&plus, index);
            let (_, _, cols_minus, derivs_minus, _) = sample_terms(&minus, index);
            assert_eq!(cols, cols_plus);
            assert_eq!(cols, cols_minus);
            let mut accel = 0.0;
            for channel in 0..3 {
                let plus_projection = projected(&derivs_plus, &cols_plus)[channel];
                let minus_projection = projected(&derivs_minus, &cols_minus)[channel];
                accel += residual[channel] * (plus_projection - minus_projection) / (2.0 * h);
            }
            let accel = alpha * accel;
            println!(
                "sample {index}: gradient {gradient:.9e} difference {first:.9e} | loss {loss:.9e}"
            );
            println!(
                "  curvature: finite {finite:.9e} gn {gn:.9e} robust {robust:.9e} accel {accel:.9e} sum {:.9e}",
                gn + robust + accel
            );
            let scale = finite.abs().max(1.0);
            assert!(
                (gradient - first).abs() <= 1e-5 * scale,
                "gradient {gradient:.9e} vs difference {first:.9e}"
            );
            assert!(
                (gn + robust + accel - finite).abs() <= 1e-4 * scale,
                "model curvature {:.9e} vs finite {finite:.9e}",
                gn + robust + accel
            );
        }
    }

    fn sample_weight(problem: &Problem, index: usize) -> f64 {
        problem.samples[index].weight
    }

    #[test]
    fn single_sample_columns_match_differences() {
        let fixture = Fixture::new(2, 24, 0x5eed_1234);
        let problem = &fixture.problem;
        let params = fixture.params();
        let layout = &problem.layout;
        let nu = problem.nu;

        let sample_terms = |params: &Params, index: usize| {
            let (look, slope) = problem.look_and_slope(params);
            let sample = &problem.samples[index];
            let image = sample.image as usize;
            let context = problem.image_ctx(params, image);
            let columns = problem.column_map(params, image, ActiveColumns::ALL);
            let mut jac = crate::predict::Jac::default();
            let out = crate::predict::evaluate_sample(
                &look,
                layout,
                &context,
                &columns,
                sample.rgb,
                params.clip_k(),
                params.clip_slope(),
                &slope,
                &[],
                &sample.target,
                &mut jac,
            );
            let sigma = params.sigma(problem.images[image].contributor);
            let squared = dot(&out.residual, &out.residual);
            let alpha = sample.weight * (nu + 3.0) / (nu * sigma * sigma + squared);
            let terms: Vec<f64> = jac
                .derivs
                .iter()
                .map(|derivative| dot(&derivative[..], &out.residual))
                .collect();
            (out.residual, jac.cols, jac.derivs, alpha, squared, terms)
        };

        let mut noise = Noise {
            state: 0x0bad_c0de_1234_5678,
        };
        let mut v = vec![0.0_f64; layout.total];
        for global in layout.tone_c0..layout.lambda.end {
            let scale = match kind_of(layout, global) {
                Some(BlockKind::Tone) => 0.05,
                Some(BlockKind::Tint) => 0.02,
                Some(BlockKind::G | BlockKind::R | BlockKind::Lambda) => 0.01,
                _ => 0.0,
            };
            v[global] = scale * noise.signed();
        }
        let h = 1e-6;
        let mut plus = params.clone();
        let mut minus = params.clone();
        for (slot, value) in v.iter().enumerate() {
            plus.theta[slot] += h * value;
            minus.theta[slot] -= h * value;
        }
        let mut total = 0.0_f64;
        let mut modelled_total = 0.0_f64;
        let mut worst = (0.0_f64, 0usize);
        for index in 0..problem.samples.len() {
            let (residual, cols, derivs, alpha, squared, terms) = sample_terms(&params, index);
            let (_, cols_plus, derivs_plus, alpha_plus, _, terms_plus) = sample_terms(&plus, index);
            let (_, cols_minus, derivs_minus, alpha_minus, _, terms_minus) =
                sample_terms(&minus, index);
            assert_eq!(cols, cols_plus);
            assert_eq!(cols, cols_minus);
            let mut projected = [0.0_f64; 3];
            let mut robust_projection = 0.0;
            for (column, derivative) in cols.iter().zip(&derivs) {
                let value = v[*column as usize];
                for channel in 0..3 {
                    projected[channel] += derivative[channel] * value;
                }
            }
            for (column, term) in cols.iter().zip(&terms) {
                robust_projection += term * v[*column as usize];
            }
            let image = problem.samples[index].image as usize;
            let sigma = params.sigma(problem.images[image].contributor);
            let q = nu * sigma * sigma + squared;
            let robust = -2.0 * (nu + 3.0) / (q * q) * problem.samples[index].weight;
            let mut exact_norm = 0.0_f64;
            let mut model_norm = 0.0_f64;
            let mut gap_norm = 0.0_f64;
            for position in 0..cols.len() {
                let exact = (alpha_plus * terms_plus[position]
                    - alpha_minus * terms_minus[position])
                    / (2.0 * h);
                let mut accel = 0.0;
                for channel in 0..3 {
                    accel += alpha
                        * residual[channel]
                        * (derivs_plus[position][channel] - derivs_minus[position][channel])
                        / (2.0 * h);
                }
                let gn = alpha
                    * (derivs[position][0] * projected[0]
                        + derivs[position][1] * projected[1]
                        + derivs[position][2] * projected[2]);
                let modelled = gn + robust * terms[position] * robust_projection + accel;
                exact_norm += exact * exact;
                model_norm += modelled * modelled;
                gap_norm += (exact - modelled) * (exact - modelled);
            }
            total += exact_norm;
            modelled_total += model_norm;
            let relative = gap_norm.sqrt() / exact_norm.sqrt().max(1e-12);
            if relative > worst.0 {
                worst = (relative, index);
            }
        }
        println!(
            "columns: |exact| {:.6e} |model| {:.6e} worst relative gap {:.3e} at sample {}",
            total.sqrt(),
            modelled_total.sqrt(),
            worst.0,
            worst.1
        );
        assert!(worst.0 < 1e-5, "relative gap {:.3e}", worst.0);
    }

    #[test]
    fn joint_curvature_terms_match_the_objective_hessian() {
        let fixture = Fixture::new(2, 24, 0x5eed_1234);
        let params = fixture.params();
        let layout = &fixture.problem.layout;
        let (model, pass) = fixture.joint(&params);
        let dim = model.dim;
        let of = &model.globals;

        let mut noise = Noise {
            state: 0x1234_5678_9abc_def0,
        };
        for trial in 0..3 {
            let mut v_global = vec![0.0_f64; layout.total];
            let mut v = vec![0.0_f64; dim];
            for (slot, &global) in of.iter().enumerate() {
                let scale = match kind_of(layout, global) {
                    Some(BlockKind::Tone) => 0.05,
                    Some(BlockKind::Tint) => 0.02,
                    Some(BlockKind::G | BlockKind::R | BlockKind::Lambda) => 0.01,
                    _ => 0.0,
                };
                v[slot] = scale * noise.signed();
                v_global[global] = v[slot];
            }

            let mut model_v = vec![0.0_f64; dim];
            model.apply_undamped(&v, &mut model_v);
            let mut plain_v = vec![0.0_f64; dim];
            model.apply_undamped_with(&v, &mut plain_v, false);
            let analytic_second: Vec<f64> = (0..dim).map(|i| model_v[i] - plain_v[i]).collect();
            let blocks_of = |range: core::ops::Range<usize>| -> Vec<usize> {
                (0..dim).filter(|slot| range.contains(&of[*slot])).collect()
            };
            let tone_slots = blocks_of(layout.tone_c0..layout.tint_a.start);
            let mut best = f64::INFINITY;
            for h in [1e-4, 1e-5, 1e-6, 1e-7] {
                let mut plus = params.clone();
                let mut minus = params.clone();
                for (index, value) in v_global.iter().enumerate() {
                    plus.theta[index] += h * value;
                    minus.theta[index] -= h * value;
                }
                let (model_plus, pass_plus) = fixture.joint(&plus);
                let (model_minus, pass_minus) = fixture.joint(&minus);
                assert_eq!(model_plus.dim, dim);
                assert_eq!(model_minus.dim, dim);
                let exact: Vec<f64> = (0..dim)
                    .map(|index| -(model_plus.rhs[index] - model_minus.rhs[index]) / (2.0 * h))
                    .collect();
                let accel = second_order_reference(&pass, &pass_plus, &pass_minus, of, h);
                let gap: Vec<f64> = (0..dim)
                    .map(|index| exact[index] - model_v[index] - accel[index])
                    .collect();
                let residual: Vec<f64> = (0..dim)
                    .map(|index| exact[index] - model_v[index])
                    .collect();
                let relative = norm(&residual) / norm(&exact).max(1e-12);
                best = best.min(relative);
                println!(
                    "trial {trial} h {h:.0e}: |exact| {:.9e} |residual| {:.9e} |gap| {:.9e} tone gap {:.6e} relative {relative:.3e}",
                    norm(&exact),
                    norm(&residual),
                    norm(&gap),
                    norm(
                        &tone_slots
                            .iter()
                            .map(|slot| gap[*slot])
                            .collect::<Vec<f64>>()
                    )
                );
            }
            assert!(
                best <= 1e-6,
                "the analytic second order misses the objective Hessian by {best:.3e}"
            );
            let h = 1e-6;
            let mut plus = params.clone();
            let mut minus = params.clone();
            for (index, value) in v_global.iter().enumerate() {
                plus.theta[index] += h * value;
                minus.theta[index] -= h * value;
            }
            let (model_plus, pass_plus) = fixture.joint(&plus);
            let (model_minus, pass_minus) = fixture.joint(&minus);
            let exact: Vec<f64> = (0..dim)
                .map(|index| -(model_plus.rhs[index] - model_minus.rhs[index]) / (2.0 * h))
                .collect();
            let (gn, robust, barrier) = row_curvature(&pass.coupled_rows, of, &v);
            let accel = second_order_reference(&pass, &pass_plus, &pass_minus, of, h);
            println!(
                "  along v: exact {:.9e} model {:.9e} gn {:.9e} robust {:.9e} barrier {:.9e} acceleration {:.9e}",
                dot(&v, &exact),
                dot(&v, &model_v),
                dot(&v, &gn),
                dot(&v, &robust),
                dot(&v, &barrier),
                dot(&v, &accel)
            );
            let cost_at = |params: &Params| -> f64 {
                let mut blocks = fixture.blocks();
                let penalties: Vec<Vec<f64>> = blocks
                    .iter()
                    .map(|block| block_penalty(&fixture.problem, block))
                    .collect();
                let pass = fixture.problem.pass(
                    params,
                    ActiveColumns::ALL,
                    &mut blocks,
                    false,
                    false,
                    false,
                    IntentFilter::NativeOnly,
                );
                pass.cost
                    + total_cost(
                        &fixture.problem,
                        params,
                        &blocks,
                        &penalties,
                        &fixture.lambdas,
                        &fixture.options,
                    )
            };
            let step = 1e-4;
            let mut cost_plus = params.clone();
            let mut cost_minus = params.clone();
            for (index, value) in v_global.iter().enumerate() {
                cost_plus.theta[index] += step * value;
                cost_minus.theta[index] -= step * value;
            }
            let curvature = (cost_at(&cost_plus) - 2.0 * cost_at(&params) + cost_at(&cost_minus))
                / (step * step);
            println!(
                "  cost second difference {curvature:.9e} vs exact {:.9e} vs model {:.9e}",
                dot(&v, &exact),
                dot(&v, &model_v)
            );
            let (sample_exact, sample_model, sample_gradient, sample_barrier) =
                sample_hessian_vectors(&fixture.problem, &params, &v_global, h);
            let mut fixed = vec![0.0_f64; dim];
            let mut penalty = vec![0.0_f64; dim];
            for index in 0..dim {
                fixed[index] = model.fixed[index] * v[index];
            }
            for &(row, column, value) in &model.penalty {
                penalty[row as usize] += value * v[column as usize];
            }
            let data_exact: Vec<f64> = (0..dim)
                .map(|index| exact[index] - fixed[index] - penalty[index])
                .collect();
            let data_model: Vec<f64> = (0..dim)
                .map(|index| model_v[index] - fixed[index] - penalty[index])
                .collect();
            println!(
                "  per-sample exact vs joint exact {:.6e}; per-sample model vs joint model {:.6e}",
                norm(
                    &(0..dim)
                        .map(|index| sample_exact[of[index]] - data_exact[index])
                        .collect::<Vec<f64>>()
                ),
                norm(
                    &(0..dim)
                        .map(|index| sample_model[of[index]] - data_model[index])
                        .collect::<Vec<f64>>()
                )
            );
            println!(
                "  gradient: per-sample {:.6e} vs rhs {:.6e}",
                norm(
                    &(0..dim)
                        .map(|index| sample_gradient[of[index]] + model.rhs[index])
                        .collect::<Vec<f64>>()
                ),
                norm(&model.rhs)
            );
            println!(
                "  norms: |accel| {:.4e} |barrier| {:.4e} |exact-model| {:.4e}",
                norm(&accel),
                norm(&barrier),
                norm(
                    &(0..dim)
                        .map(|index| exact[index] - model_v[index])
                        .collect::<Vec<f64>>()
                )
            );
            println!(
                "  barrier second (not in the step model): {:.4e}",
                norm(&sample_barrier)
            );
            let mut worst: Vec<(f64, usize)> = tone_slots
                .iter()
                .map(|slot| ((exact[*slot] - model_v[*slot] - accel[*slot]).abs(), *slot))
                .collect();
            worst.sort_by(|a, b| b.0.total_cmp(&a.0));
            for (_, slot) in worst.iter().take(6) {
                println!(
                    "    slot {slot} global {} v {:.6e}: exact {:.6e} model {:.6e} gn {:.6e} robust {:.6e} barrier {:.6e} accel {:.6e} analytic {:.6e} diff {:.6e}",
                    of[*slot],
                    v[*slot],
                    exact[*slot],
                    model_v[*slot],
                    gn[*slot],
                    robust[*slot],
                    barrier[*slot],
                    accel[*slot],
                    analytic_second[*slot],
                    analytic_second[*slot] - accel[*slot]
                );
            }
            let relative = norm(
                &(0..dim)
                    .map(|i| exact[i] - model_v[i])
                    .collect::<Vec<f64>>(),
            ) / norm(&exact).max(1e-12);
            let gap = norm(
                &(0..dim)
                    .map(|i| exact[i] - plain_v[i] - accel[i])
                    .collect::<Vec<f64>>(),
            );
            println!(
                "  second-order term: analytic vs data FD {:.3e} relative to |analytic| {:.3e}",
                norm(
                    &(0..dim)
                        .map(|i| analytic_second[i] - accel[i])
                        .collect::<Vec<f64>>()
                ),
                norm(&analytic_second)
            );
            println!(
                "  exact vs model {relative:.3e} relative, exact vs model+FD acceleration {gap:.3e}"
            );
            assert!(
                gap <= 5e-2
                    * norm(
                        &(0..dim)
                            .map(|i| exact[i] - plain_v[i])
                            .collect::<Vec<f64>>()
                    ),
                "the reference acceleration leaves a gap {gap:.3e}"
            );
        }
    }
}
