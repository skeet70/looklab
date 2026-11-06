// SPDX-License-Identifier: AGPL-3.0-or-later
//! Staged Gauss-Newton/Levenberg-Marquardt with block solves and per-stage GCV.

use looklab_model::LookInput;
use looklab_model::basis::{RESIDUAL_COEFS, S_COEFS, TONE_COEFS};
use looklab_model::realization::{
    ControlResponseModel, ControlValue, LayerKind, ResponseComponent, ResponseParamId,
};
use looklab_schema::{Control, ControlDefault, ControlSchema, ScalarValue, Tolerated};

use crate::basis;
use crate::density;
use crate::layout::{BlockKind, Layout, Params};
use crate::model_error::{MODEL_HUE_BINS, MODEL_S_BINS, estimate_model_error, region};
use crate::outlier;
use crate::problem::{ActiveColumns, BlockAcc, IntentFilter, PassOutput, Problem, ResidualSample};
use crate::solve::{factor_in_place, select_lambda, select_lambda_diagonal, solve_in_place};
use crate::types::{
    ControlResponseFit, Diagnostics, ExcludeReason, ExcludedImage, ExposureOffset, FitImage,
    FitOptions, FitResult, FitState, ImageIntent, ImageNuisances, PreviousSupport,
    ProbeUncertainty, Realization, Result, StageLambdas, Verdict,
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
    run_inner(images, options, model, excluded, None, true, None, None).map(|(result, _)| result)
}

pub(crate) fn run_with_history(
    images: &[FitImage],
    options: &FitOptions,
    model: Option<ControlResponseModel>,
    excluded: Vec<ExcludedImage>,
    previous: Option<&PreviousSupport>,
) -> Result<FitResult> {
    run_inner(images, options, model, excluded, None, true, previous, None)
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
        images, options, model, excluded, None, true, previous, resume,
    )
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
        None => initial_params(&problem),
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
        let mut sample_evaluations = 0.0_f64;
        for _ in 0..stage_sweeps {
            if sample_evaluations + problem.samples.len() as f64 > options.sweep_budget {
                break;
            }
            sample_evaluations += problem.samples.len() as f64;
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
                );
            }
            if kinds.contains(&BlockKind::Residual) {
                let lambda = lam(&lambdas, BlockKind::Residual);
                let delta = residual_step(
                    &pass.residual_samples,
                    &params.theta[layout.residual.clone()],
                    lambda,
                    damping,
                    1.0,
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

/// Fit without the probe/response uncertainty.
pub(crate) fn run_quick(
    images: &[FitImage],
    options: &FitOptions,
    initial: Option<&looklab_model::Look>,
) -> Result<FitResult> {
    run_inner(
        images,
        options,
        None,
        Vec::new(),
        initial,
        false,
        None,
        None,
    )
    .map(|(result, _)| result)
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
) -> Result<(FitResult, Option<FitState>)> {
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
    let mut params = initial_params(&problem);
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

    if model.is_some() {
        let reference: Vec<FitImage> = native
            .iter()
            .enumerate()
            .filter(|(index, _)| problem.terms[*index].is_empty())
            .map(|(_, image)| image.clone())
            .collect();
        if reference.len() >= 4 && reference.len() < native.len() {
            let reference_fit = run_with(&reference, options, None, Vec::new())?;
            evaluations += reference_fit.diagnostics.evaluations;
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

    // The warm-up stages keep their own budget; the convergent stage spends up to
    // `FitOptions::max_sweeps` and stops on the objective and step tolerances.
    let warmup_allowed =
        (options.sweep_budget / problem.samples.len().max(1) as f64).clamp(8.0, 64.0) as usize;
    let mut final_converged = false;
    let mut resume_state: Option<FitState> = None;
    let mut sample_evaluations = 0.0_f64;
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
        let mut previous: Option<(f64, Params)> = None;
        let mut gcv_done = false;
        let mut recent: Vec<f64> = Vec::new();
        for sweep_index in start_sweep..stage_sweeps {
            if sample_evaluations + stage_problem.samples.len() as f64 > options.sweep_budget {
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
            sample_evaluations += stage_problem.samples.len() as f64;
            for block in &mut blocks {
                block.reset();
            }
            let pass = stage_problem.pass(
                &params,
                stage.active,
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
                damping = (damping * 10.0).min(1e6_f64);
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
            if previous
                .as_ref()
                .is_none_or(|(previous_cost, _)| cost < *previous_cost)
            {
                recent.push(cost);
                if recent.len() > 5 {
                    recent.remove(0);
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
                );
            }
            if kinds.contains(&BlockKind::Residual) {
                let lambda = lam(&lambdas, BlockKind::Residual);
                let delta = residual_step(
                    &pass.residual_samples,
                    &params.theta[layout.residual.clone()],
                    lambda,
                    damping,
                    hessian_scale,
                );
                for (index, value) in delta.iter().enumerate() {
                    step[layout.residual.start + index] = *value;
                }
            }
            apply_step(&mut params, &step);
            if kinds.contains(&BlockKind::Residual) {
                pin_residual(&mut params, &layout, &pass.residual_samples);
            }
            update_sigma(stage_problem, &mut params, &pass, options);
            damping = (damping * 0.3).max(1e-6);
            if converged(&recent) {
                final_converged = stage.converge;
                break;
            }
        }
        if resume_state.is_some() {
            break;
        }
        if stage.converge && !final_converged {
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

    if model.is_some() && !problem.layout.response.is_empty() {
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
        true,
        true,
        false,
        IntentFilter::All,
    );
    evaluations += 1;
    // The exposure/body gauge is a definition (docs/02 §2.1), not an estimate, so
    // the Look's covariance is conditional on the exposure nuisances.
    let uncertainty_columns = ActiveColumns::ALL;
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
    let image_stats = outlier::score(&problem, &final_pass, options);
    let mut grl_precision_ratio = Vec::new();
    let (mut probe_result, parameter_ids, covariance) =
        match (uncertainty_blocks.as_ref(), uncertainty_pass.as_ref()) {
            (Some(uncertainty_blocks), Some(uncertainty_pass)) => {
                let uncertainty_penalty = double(combined_look_penalty(
                    &problem,
                    &layout,
                    &lambdas,
                    uncertainty_blocks[0].len(),
                    true,
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
        let probes_v1 = looklab_model::probes::v1();
        let mut sums = vec![0.0_f64; MODEL_HUE_BINS * MODEL_S_BINS];
        let mut counts = vec![0.0_f64; MODEL_HUE_BINS * MODEL_S_BINS];
        for (index, probe) in probe_result.probes.iter().enumerate() {
            let input = LookInput::new(probes_v1.probes[index].xyz);
            let bin = region(input.s, input.h_in);
            sums[bin] += probe.post_sigma_de * probe.post_sigma_de;
            counts[bin] += 1.0;
        }
        let reference_variance: Vec<f64> = sums
            .iter()
            .zip(counts.iter())
            .map(|(sum, count)| if *count > 0.0 { sum / count } else { 0.0 })
            .collect();
        let clip_k = params.clip_k();
        let field = estimate_model_error(&native, &[], &reference_variance, options, clip_k)?;
        for (index, probe) in probe_result.probes.iter_mut().enumerate() {
            let input = LookInput::new(probes_v1.probes[index].xyz);
            let variance = field.at(input.s, input.h_in);
            probe.post_sigma_de = (probe.post_sigma_de * probe.post_sigma_de + variance).sqrt();
        }
    }
    let control_response = model.map(|template| {
        build_control_response(template, &problem, &params, parameter_ids, covariance)
    });

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

fn params_from_look(problem: &Problem, look: &looklab_model::Look) -> Params {
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

fn initial_params(problem: &Problem) -> Params {
    let mut params = Params::initial(problem.layout.clone(), problem.contributors.len());
    for (index, image) in problem.images.iter().enumerate() {
        let range = params.layout.image(index);
        params.theta[range.start] = image.exposure_hint.clamp(-8.0, 8.0);
    }
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
            cost += lam(lambdas, BlockKind::Residual)
                * residual_penalty_value(&params.theta[problem.layout.residual.clone()]);
            continue;
        }
        cost += lam(lambdas, block.kind)
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
            continue;
        }
        reg += lam(lambdas, block.kind)
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
    for (index, block) in blocks.iter().enumerate() {
        if block.kind == BlockKind::Nuisance || lam(lambdas, block.kind) != 0.0 {
            continue;
        }
        let choice = if block.kind == BlockKind::Residual {
            let hessian_scale = pass.active_samples.max(1) as f64 / pass.hessian_rows.max(1) as f64;
            let identity = identity_weight(BlockKind::Residual);
            let diagonal: Vec<f64> = residual_diagonal(&pass.residual_samples)
                .into_iter()
                .map(|value| value * hessian_scale + identity)
                .collect();
            let penalty_diagonal: Vec<f64> = residual_penalty_diagonal()
                .into_iter()
                .map(|v| 2.0 * v)
                .collect();
            let (residual_rhs, rss) = residual_gradient(&pass.residual_samples);
            let theta = &params.theta[problem.layout.residual.clone()];
            select_lambda_diagonal(
                &diagonal,
                &penalty_diagonal,
                &residual_rhs,
                theta,
                rss.max(1e-12),
                observations,
            )
        } else {
            let theta0: Vec<f64> = block
                .params
                .iter()
                .map(|index| params.theta[*index])
                .collect();
            let identity = identity_curvature(&problem.layout, block);
            let mut hessian = block.hessian.clone();
            for i in 0..block.len() {
                hessian[i * block.len() + i] += identity[i];
            }
            select_lambda(
                &hessian,
                &penalties[index],
                &block.gradient,
                &theta0,
                pass.rss.max(1e-12),
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
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
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
        a[i * n + i] += damping * diagonal + prior_curvature[i];
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

fn residual_step(
    samples: &[ResidualSample],
    theta: &[f64],
    lambda: f64,
    damping: f64,
    hessian_scale: f64,
) -> Vec<f64> {
    let dim = RESIDUAL_COEFS;
    let penalty_diagonal = residual_penalty_diagonal();
    let data_diagonal: Vec<f64> = residual_diagonal(samples)
        .into_iter()
        .map(|value| value * hessian_scale)
        .collect();
    let (mut rhs, _) = residual_gradient(samples);
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
            for index in 0..dim {
                product[index] += ridge * laplacian_at(v, index, &penalty_diagonal);
                product[index] +=
                    (damping * data_diagonal[index].max(1e-9) + 1e-3 + identity) * v[index];
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
}
