// SPDX-License-Identifier: AGPL-3.0-or-later
//! Staged Gauss-Newton/Levenberg-Marquardt with block solves and per-stage GCV.

use looklab_model::basis::{CORE_TERMS, RESIDUAL_COEFS, S_COEFS, TONE_COEFS, VOLUME_COEFS};
use looklab_model::realization::{
    ControlResponseModel, ControlValue, LayerKind, ResponseComponent, ResponseParamId,
};
use looklab_schema::{Control, ControlDefault, ControlSchema, ScalarValue, Tolerated};

use crate::basis;
use crate::density;
use crate::layout::{BlockKind, Layout, NUISANCE_PER_IMAGE, Params};
use crate::model_error::{apply_model_error, estimate_model_error};
use crate::outlier;
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

fn fit_diag() -> bool {
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

pub(crate) fn run_with_initial(
    images: &[FitImage],
    options: &FitOptions,
    initial: &looklab_model::Look,
) -> Result<FitResult> {
    run_inner(
        images,
        options,
        None,
        Vec::new(),
        Some(initial),
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
            let gradient = gradient_max(
                &layout,
                &problem,
                &params,
                options,
                &blocks,
                &penalties,
                &lambdas,
                &roles,
            );
            let accepted_damping = damping;
            damping = (damping * 0.3).max(1e-6);
            recent.push(cost);
            if recent.len() > 5 {
                recent.remove(0);
            }
            let stationary = gradient <= STATIONARY_GRADIENT;
            last_gradient = gradient;
            let stable_step = accepted_damping <= 1e-2 && last_step <= 1e-6 && stationary;
            let stable_cost = accepted_damping <= 1e-2
                && stationary
                && recent.len() >= 3
                && {
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
                    "nuisance",
                    &layout,
                    &problem,
                    &params,
                    options,
                    &blocks,
                    &penalties,
                    &lambdas,
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
            let gradient = gradient_max(
                &layout,
                &problem,
                &params,
                options,
                &blocks,
                &penalties,
                &lambdas,
                &roles,
            );
            last_gradient = gradient;
            if fit_diag() {
                eprintln!(
                    "nuisance attempt {attempts} rejected cost {cost:.12e} gradient {gradient:.9e} damping {damping:.3e} step {last_step:.9e} history {}",
                    recent.len()
                );
                print_block_gradients(
                    "nuisance",
                    &layout,
                    &problem,
                    &params,
                    options,
                    &blocks,
                    &penalties,
                    &lambdas,
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
    let mut resume_state: Option<FitState> = None;
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
                false,
            );
            evaluations += 1;
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
                print_block_gradients(
                    "current",
                    &layout,
                    &problem,
                    &params,
                    options,
                    &blocks,
                    &penalties,
                    &lambdas,
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
                    &layout, &problem, &params, options, &blocks, &penalties, &lambdas,
                    &roles, &pass, hessian_scale,
                );
                if fit_diag() {
                    eprintln!(
                        "fit stage {stage_index} sweep {sweep_index} plateau decrement {decrement:.9e} nats threshold {STATIONARY_DECREMENT_NATS:.3e}"
                    );
                    print_block_gradients(
                        "plateau",
                        &layout,
                        &problem,
                        &params,
                        options,
                        &blocks,
                        &penalties,
                        &lambdas,
                        &roles,
                    );
                }
                if decrement <= STATIONARY_DECREMENT_NATS {
                    final_converged = true;
                    break;
                }
            }

            let mut step = vec![0.0_f64; layout.total];
            let mut prediction = 0.0;
            let coupled_prediction = stage.converge.then(|| coupled_step(
                &layout, &problem, &params, options, &blocks, &penalties, &lambdas,
                &roles, &pass, hessian_scale, damping, &mut step,
            )).flatten();
            if let Some(value) = coupled_prediction {
                prediction = value;
            } else {
            for (index, block) in blocks.iter().enumerate() {
                if block.kind == BlockKind::Residual {
                    continue;
                }
                let (prior_gradient, prior_curvature) =
                    prior_terms(&problem, &roles, &params, options, block);
                prediction += solve_block(
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
                    StepLimit::PerBlock,
                );
                for (index, value) in delta.iter().enumerate() {
                    step[layout.residual.start + index] = *value;
                }
            }
            let step_max = step
                .iter()
                .filter(|value| value.is_finite())
                .fold(0.0_f64, |largest, value| largest.max(value.abs()));
            let step_params = params.clone();
            let mut directional_reduction = 0.0;
            if stage.converge {
                for (index, block) in blocks.iter().enumerate() {
                    if block.kind == BlockKind::Residual {
                        continue;
                    }
                    let gradients = block_gradients(
                        &layout, &problem, &params, options, block, &penalties[index],
                        block_lambda(block, &lambdas), &roles,
                    );
                    directional_reduction -= gradients.iter().zip(&block.params)
                        .map(|(gradient, global)| gradient * step[*global]).sum::<f64>();
                }
            }
            if stage.converge {
                apply_step_unclamped(&mut params, &step);
            } else {
                apply_step(&mut params, &step);
            }
            if kinds.contains(&BlockKind::Residual) {
                pin_residual(&mut params, &layout, &pass.residual_samples);
                let lambda = lam(&lambdas, BlockKind::Residual);
                let ridge = 2.0 * lambda;
                let identity = identity_weight(BlockKind::Residual);
                let penalty = residual_penalty_diagonal();
                let before = &step_params.theta[layout.residual.clone()];
                let after = &params.theta[layout.residual.clone()];
                let mut rhs = vec![0.0_f64; RESIDUAL_COEFS];
                let mut effective = vec![0.0_f64; RESIDUAL_COEFS];
                for index in 0..RESIDUAL_COEFS {
                    effective[index] = after[index] - before[index];
                    rhs[index] = pass.residual_gradient[index]
                        - ridge * laplacian_at(before, index, &penalty)
                        - identity * before[index];
                }
                let mut curvature = vec![0.0_f64; RESIDUAL_COEFS];
                residual_undamped_apply(
                    &pass.residual_samples,
                    &effective,
                    &mut curvature,
                    &penalty,
                    ridge,
                    identity,
                    hessian_scale,
                );
                let first: f64 = rhs.iter().zip(effective.iter()).map(|(r, d)| r * d).sum();
                let quadratic: f64 = effective
                    .iter()
                    .zip(curvature.iter())
                    .map(|(d, b)| d * b)
                    .sum();
                directional_reduction += first;
                prediction += first - 0.5 * quadratic;
            }
            let full_prediction = prediction;
            let mut fraction = 1.0;
            let (trial, trial_cost) = loop {
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
                let trial_cost = trial.cost
                    + total_cost(&problem, &params, &blocks, &penalties, &lambdas, options);
                if !stage.converge || accepted(cost, trial_cost)
                    || directional_reduction <= 0.0
                    || sample_evaluations.get() as f64 + samples_per_pass as f64 > options.sweep_budget
                {
                    break (trial, trial_cost);
                }
                fraction *= 0.5;
                let mut changed = false;
                for (global, value) in params.theta.iter_mut().enumerate() {
                    let next = if step[global].is_finite() {
                        step_params.theta[global] + fraction * step[global]
                    } else {
                        step_params.theta[global]
                    };
                    changed |= next != *value;
                    *value = next;
                }
                if kinds.contains(&BlockKind::Residual) {
                    pin_residual(&mut params, &layout, &pass.residual_samples);
                }
                prediction = fraction * directional_reduction
                    - fraction * fraction * (directional_reduction - full_prediction);
                if !changed {
                    break (trial, trial_cost);
                }
            };
            let step_max = fraction * step_max;
            if !accepted(cost, trial_cost) {
                params = step_params;
                damping = (damping * 10.0).min(1e6_f64);
                if fit_diag() && stage.converge {
                    eprintln!(
                        "fit stage {stage_index} sweep {sweep_index} rejected trial {trial_cost:.12e} current {cost:.12e} step {step_max:.9e} prediction {prediction:.9e} damping {damping:.3e} history {}",
                        recent.len()
                    );
                }
                if damping >= 1e6 {
                    if fit_diag() {
                        eprintln!(
                            "fit checkpoint stage {stage_index} sweep {sweep_index} reason damping damping {damping:.3e} history {}",
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
                continue;
            }
            let ratio = if prediction.is_finite() && prediction > 0.0 {
                (cost - trial_cost) / prediction
            } else {
                f64::NAN
            };
            if fraction < 1.0 {
                damping = (damping * 2.0).min(1e6);
            } else if ratio.is_finite() {
                if ratio > 0.75 {
                    damping = (damping * 0.3).max(1e-6);
                } else if ratio < 0.25 {
                    damping = (damping * 2.0).min(1e6);
                }
            }
            if fit_diag() && stage.converge {
                eprintln!(
                    "fit stage {stage_index} sweep {sweep_index} accepted trial {trial_cost:.12e} ratio {ratio:.6} prediction {prediction:.9e} step {step_max:.9e} damping {damping:.3e} history {}",
                    recent.len()
                );
            }
            if fit_diag() && stage.converge && matches!(sweep_index, 96 | 112 | 127) {
                diagnose_directional_curvature(
                    stage_problem, &step_params, &params, options, &blocks, &penalties,
                    &lambdas, &roles, &pass, hessian_scale, stage.active, stage.neutral,
                );
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
                    "final",
                    &layout,
                    &problem,
                    &params,
                    options,
                    &blocks,
                    &penalties,
                    &lambdas,
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
                            (
                                S_COEFS + VOLUME_COEFS + s_index * CORE_TERMS + term,
                                *value,
                            )
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
            cost += residual_cost(&problem.layout, &params.theta, lam(lambdas, BlockKind::Residual));
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
                .hessian
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
    let gradients = block_gradients(layout, problem, params, options, block, penalty, lambda, roles);
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

/// Largest absolute derivative of the penalized objective over the active blocks.
fn gradient_max(
    layout: &Layout,
    problem: &Problem,
    params: &Params,
    options: &FitOptions,
    blocks: &[BlockAcc],
    penalties: &[Vec<f64>],
    lambdas: &StageLambdas,
    roles: &[Role],
) -> f64 {
    blocks
        .iter()
        .enumerate()
        .filter(|(_, block)| block.kind != BlockKind::Residual && !block.params.is_empty())
        .map(|(index, block)| {
            block_gradient_peak(
                layout,
                problem,
                params,
                options,
                block,
                &penalties[index],
                block_lambda(block, lambdas),
                roles,
            )
            .0
        })
        .fold(0.0_f64, f64::max)
}

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
) -> f64 {
    let Some(system) = CoupledSystem::new(
        layout, problem, params, options, blocks, penalties, lambdas, roles, pass, data_scale, 0.0,
    ) else {
        return f64::INFINITY;
    };
    let lower_bound = if system.exact_data.is_some() { 0.0 } else { system.energy_lower_bound() };
    if lower_bound > STATIONARY_DECREMENT_NATS {
        if fit_diag() {
            eprintln!("fit decrement rejected by lower bound {lower_bound:.9e} nats");
        }
        return lower_bound;
    }
    let Some(solution) = system.solve() else {
        return f64::INFINITY;
    };
    let mut energy = 0.5 * crate::solve::dot(&system.rhs, &solution);
    if !energy.is_finite() || energy < 0.0 {
        return f64::INFINITY;
    }
    if blocks.iter().any(|block| block.kind == BlockKind::Residual) {
        energy += residual_decrement(pass, layout, params, lambdas, data_scale);
    }
    energy
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

#[allow(clippy::too_many_arguments)]
fn diagnose_directional_curvature(
    problem: &Problem, before: &Params, after: &Params, options: &FitOptions,
    blocks: &[BlockAcc], penalties: &[Vec<f64>], lambdas: &StageLambdas,
    roles: &[Role], pass: &PassOutput, data_scale: f64, active: ActiveColumns, neutral: bool,
) {
    let layout = &problem.layout;
    let gradient = diagnostic_gradient_parts(problem, before, options, blocks, penalties, lambdas, roles, pass);
    let accepted: Vec<f64> = after.theta.iter().zip(&before.theta).map(|(a, b)| a - b).collect();
    let mut steepest: Vec<f64> = gradient.iter().map(|parts| -parts.iter().sum::<f64>()).collect();
    let norm = steepest.iter().fold(0.0_f64, |max, value| max.max(value.abs()));
    if norm > 0.0 {
        for value in &mut steepest {
            *value /= norm;
        }
    }
    let mut diagnostic_blocks: Vec<BlockAcc> = blocks.iter().map(|block| {
        let mut copy = BlockAcc::new(block.kind, block.params.clone(), layout.total);
        copy.image = block.image;
        copy
    }).collect();
    let full_pass = problem.pass_with_rows(
        before, active, &mut diagnostic_blocks, false, false, neutral, IntentFilter::NativeOnly, true, true,
    );
    let full_models = [&accepted, &steepest].map(|direction| {
        diagnostic_model_curvature(
            problem, before, options, &diagnostic_blocks, penalties, lambdas, roles,
            &full_pass, 1.0, false, direction,
        )
    });
    let sigma: Vec<f64> = (0..problem.contributors.len()).map(|i| before.sigma(i)).collect();
    eprintln!("fit curvature start images {} sigma_v {sigma:?} sampled_rows {} all_rows {}",
        problem.images.len(), pass.hessian_rows, full_pass.hessian_rows);
    for (which, direction) in [accepted, steepest].iter().enumerate() {
        let label = if which == 0 { "accepted" } else { "steepest" };
        let model = diagnostic_model_curvature(
            problem, before, options, blocks, penalties, lambdas, roles,
            pass, data_scale, true, direction,
        );
        let mut groups: Vec<(Option<BlockKind>, Role, f64, f64)> = Vec::new();
        for (global, value) in direction.iter().enumerate().filter(|(_, value)| **value != 0.0) {
            let kind = kind_of(layout, global);
            let role = match roles[global] {
                Role::Exposure(_) => Role::Exposure(0),
                Role::Wb(_) => Role::Wb(0),
                Role::Mode(_) => Role::Mode(0),
                role => role,
            };
            if let Some(group) = groups.iter_mut().find(|group| group.0 == kind && group.1 == role) {
                group.2 += value * value;
                group.3 = group.3.max(value.abs());
            } else {
                groups.push((kind, role, value * value, value.abs()));
            }
        }
        for (kind, role, squared_norm, peak) in groups {
            eprintln!("fit curvature direction {label} role {kind:?}/{role:?} norm {:.9e} max {peak:.9e}", squared_norm.sqrt());
        }
        for h in [1e-3, 1e-4] {
            let gradients = [-h, h].map(|offset| {
                let mut perturbed = before.clone();
                for (theta, value) in perturbed.theta.iter_mut().zip(direction) {
                    *theta += offset * value;
                }
                for block in &mut diagnostic_blocks {
                    block.reset();
                }
                let perturbed_pass = problem.pass(
                    &perturbed, active, &mut diagnostic_blocks, false, false, neutral, IntentFilter::NativeOnly,
                );
                let parts = diagnostic_gradient_parts(
                    problem, &perturbed, options, &diagnostic_blocks, penalties, lambdas, roles, &perturbed_pass,
                );
                let mut projected = [0.0; 4];
                for (values, value) in parts.iter().zip(direction) {
                    for component in 0..4 {
                        projected[component] += values[component] * value;
                    }
                }
                projected
            });
            let actual = std::array::from_fn::<_, 4, _>(|i| (gradients[1][i] - gradients[0][i]) / (2.0 * h));
            let full = full_models[which];
            eprintln!("fit curvature {label} h {h:.1e} sampled {:.12e} true {:.12e} all {:.12e} data {:.12e}/{:.12e}/{:.12e} prior {:.12e}/{:.12e}/{:.12e} identity {:.12e}/{:.12e}/{:.12e} penalty {:.12e}/{:.12e}/{:.12e}",
                model.iter().sum::<f64>(), actual.iter().sum::<f64>(), full.iter().sum::<f64>(),
                model[0], actual[0], full[0], model[1], actual[1], full[1],
                model[2], actual[2], full[2], model[3], actual[3], full[3]);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn diagnostic_gradient_parts(
    problem: &Problem, params: &Params, options: &FitOptions, blocks: &[BlockAcc],
    penalties: &[Vec<f64>], lambdas: &StageLambdas, roles: &[Role], pass: &PassOutput,
) -> Vec<[f64; 4]> {
    let layout = &problem.layout;
    let mut parts = vec![[0.0; 4]; layout.total];
    for (index, block) in blocks.iter().enumerate() {
        if block.kind == BlockKind::Residual {
            let diagonal = residual_penalty_diagonal();
            let theta = &params.theta[layout.residual.clone()];
            for i in 0..RESIDUAL_COEFS {
                parts[layout.residual.start + i] = [
                    -pass.residual_gradient[i], 0.0, identity_weight(BlockKind::Residual) * theta[i],
                    2.0 * lambdas.residual * laplacian_at(theta, i, &diagonal),
                ];
            }
            continue;
        }
        let (prior, _) = prior_terms(problem, roles, params, options, block);
        let identity = identity_apply(layout, block, &params.theta);
        for (i, &global) in block.params.iter().enumerate() {
            let regularizer: f64 = (0..block.len())
                .map(|j| penalties[index][i * block.len() + j] * params.theta[block.params[j]]).sum();
            parts[global] = [block.gradient[i], prior[i], identity[i], block_lambda(block, lambdas) * regularizer];
        }
    }
    parts
}

#[allow(clippy::too_many_arguments)]
fn diagnostic_model_curvature(
    problem: &Problem, params: &Params, options: &FitOptions, blocks: &[BlockAcc],
    penalties: &[Vec<f64>], lambdas: &StageLambdas, roles: &[Role], pass: &PassOutput,
    data_scale: f64, reconcile: bool, direction: &[f64],
) -> [f64; 4] {
    let layout = &problem.layout;
    let mut result = [0.0; 4];
    let mut scales = vec![1.0; layout.total];
    for (index, block) in blocks.iter().enumerate() {
        if block.kind == BlockKind::Residual {
            let residual = &direction[layout.residual.clone()];
            let diagonal = residual_penalty_diagonal();
            for sample in &pass.residual_samples {
                let mut projected = [0.0; 3];
                for (m, node) in sample.nodes.iter().enumerate() {
                    for channel in 0..3 {
                        for row in 0..3 {
                            projected[row] += sample.clip.0[row][channel] * sample.weights[m]
                                * residual[*node as usize * 3 + channel];
                        }
                    }
                }
                result[0] += data_scale * sample.alpha * projected.iter().map(|v| v * v).sum::<f64>();
            }
            for i in 0..RESIDUAL_COEFS {
                result[2] += (identity_weight(BlockKind::Residual) + 1e-3) * residual[i] * residual[i];
                result[3] += 2.0 * lambdas.residual * residual[i] * laplacian_at(residual, i, &diagonal);
            }
            continue;
        }
        let (_, prior) = prior_terms(problem, roles, params, options, block);
        let identity = identity_curvature(layout, block);
        for (i, &global) in block.params.iter().enumerate() {
            if reconcile {
                let sampled = data_scale * block.hessian[i * block.len() + i];
                if sampled > 0.0 {
                    scales[global] = (block.full_diagonal[i] / sampled).max(0.0).sqrt();
                } else {
                    result[0] += block.full_diagonal[i] * direction[global] * direction[global];
                }
            }
            result[1] += prior[i] * direction[global] * direction[global];
            result[2] += identity[i] * direction[global] * direction[global];
            for (j, &other) in block.params.iter().enumerate() {
                result[3] += block_lambda(block, lambdas) * penalties[index][i * block.len() + j]
                    * direction[global] * direction[other];
            }
        }
    }
    for row in &pass.coupled_rows {
        let mut projected = [0.0; 3];
        for (global, derivative) in &row.columns {
            let value = direction[*global as usize] * scales[*global as usize];
            for channel in 0..3 {
                projected[channel] += derivative[channel] * value;
            }
        }
        result[0] += data_scale * row.weight * projected.iter().map(|v| v * v).sum::<f64>();
    }
    result
}

struct CoupledFactor {
    range: core::ops::Range<usize>,
    factor: Vec<f64>,
}

struct CoarseSpace {
    basis: Vec<Vec<f64>>,
    images: Vec<Vec<f64>>,
    factor: Vec<f64>,
}

struct ExactDataCurvature<'a> {
    problem: &'a Problem,
    params: &'a Params,
}

impl ExactDataCurvature<'_> {
    fn add_product(&self, globals: &[usize], v: &[f64], product: &mut [f64]) {
        let relative = globals.iter().zip(v).fold(0.0_f64, |max, (global, value)| {
            max.max(value.abs() / (1.0 + self.params.theta[*global].abs()))
        });
        if relative == 0.0 {
            return;
        }
        let epsilon = 1e-5 / relative;
        let mut negative = vec![0.0; globals.len()];
        let mut gradient = vec![0.0; globals.len()];
        for sign in [-1.0, 1.0] {
            let mut perturbed = self.params.clone();
            for (&global, value) in globals.iter().zip(v) {
                perturbed.theta[global] += sign * epsilon * value;
            }
            let pass = self.problem.pass(
                &perturbed, ActiveColumns::ALL, &mut [], false, true, false, IntentFilter::NativeOnly,
            );
            gradient.fill(0.0);
            for image in &pass.per_image_gradient {
                for (i, &global) in globals.iter().enumerate() {
                    gradient[i] += image[global];
                }
            }
            if sign < 0.0 {
                negative.copy_from_slice(&gradient);
            } else {
                for i in 0..globals.len() {
                    product[i] += (gradient[i] - negative[i]) / (2.0 * epsilon);
                }
            }
        }
    }
}

struct CoupledSystem<'a> {
    pass: &'a PassOutput,
    globals: Vec<usize>,
    local: Vec<usize>,
    scales: Vec<f64>,
    diagonal: Vec<f64>,
    damping_diagonal: Vec<f64>,
    penalty: Vec<(usize, usize, f64)>,
    factors: Vec<CoupledFactor>,
    rhs: Vec<f64>,
    data_scale: f64,
    labels: Vec<(Option<BlockKind>, Role)>,
    coarse_groups: Vec<Vec<usize>>,
    exact_data: Option<ExactDataCurvature<'a>>,
}

impl<'a> CoupledSystem<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        layout: &Layout, problem: &'a Problem, params: &'a Params, options: &FitOptions,
        blocks: &[BlockAcc], penalties: &[Vec<f64>], lambdas: &StageLambdas,
        roles: &[Role], pass: &'a PassOutput, data_scale: f64, damping: f64,
    ) -> Option<Self> {
        let mut out = Self {
            pass, globals: Vec::new(), local: vec![usize::MAX; layout.total],
            scales: Vec::new(), diagonal: Vec::new(), damping_diagonal: Vec::new(),
            penalty: Vec::new(), factors: Vec::new(), rhs: Vec::new(), data_scale,
            labels: Vec::new(),
            coarse_groups: vec![Vec::new(); problem.modes.len() + 4],
            exact_data: std::env::var_os("LOOKLAB_FIT_EXACT_HV").is_some()
                .then_some(ExactDataCurvature { problem, params }),
        };
        for (index, block) in blocks.iter().enumerate() {
            if block.kind == BlockKind::Residual || block.params.is_empty() {
                continue;
            }
            let n = block.len();
            let start = out.globals.len();
            let lambda = block_lambda(block, lambdas);
            let (prior_gradient, prior_curvature) = prior_terms(problem, roles, params, options, block);
            let (mut factor, rhs) = block_system(
                layout, block, &penalties[index], &prior_gradient, &prior_curvature,
                params, lambda, damping, data_scale, StepLimit::PerBlock,
            );
            if !factor_in_place(&mut factor, n) {
                if fit_diag() {
                    eprintln!("fit cg factor failure block {:?} damping {damping:.9e}", block.kind);
                }
                return None;
            }
            out.factors.push(CoupledFactor { range: start..start + n, factor });
            out.rhs.extend(rhs);
            let identity = identity_curvature(layout, block);
            for (i, &global) in block.params.iter().enumerate() {
                out.local[global] = start + i;
                out.globals.push(global);
                out.labels.push((kind_of(layout, global), roles[global]));
                let group = match roles[global] {
                    Role::Exposure(_) => Some(0),
                    Role::Wb(_) => Some(if (global - layout.images.start) % NUISANCE_PER_IMAGE == 1 { 1 } else { 2 }),
                    Role::Mode(mode) => Some(3 + mode),
                    Role::Clip => Some(3 + problem.modes.len()),
                    Role::None => None,
                };
                if let Some(group) = group {
                    out.coarse_groups[group].push(start + i);
                }
                let sampled = data_scale * block.hessian[i * n + i];
                let full = block.full_diagonal[i];
                out.scales.push(if sampled > 0.0 { (full / sampled).max(0.0).sqrt() } else { 1.0 });
                let damp = damping * full.max(1e-12);
                out.damping_diagonal.push(damp);
                let mut prior = prior_curvature[i];
                if out.exact_data.is_some() && matches!(roles[global], Role::Clip) {
                    let (mean, sd) = options.clip_prior;
                    let p = (params.clip_k() - 0.005) / 0.095;
                    prior += (params.clip_k() - mean) * params.clip_slope() * (1.0 - 2.0 * p) / (sd * sd);
                }
                out.diagonal.push(prior + identity[i] + damp
                    + if sampled > 0.0 || out.exact_data.is_some() { 0.0 } else { full });
                for j in 0..n {
                    let value = lambda * penalties[index][i * n + j];
                    if value != 0.0 {
                        out.penalty.push((start + i, start + j, value));
                    }
                }
            }
        }
        Some(out)
    }

    fn apply(&self, v: &[f64], product: &mut [f64]) {
        for (i, value) in product.iter_mut().enumerate() {
            *value = self.diagonal[i] * v[i];
        }
        if let Some(exact) = &self.exact_data {
            exact.add_product(&self.globals, v, product);
        } else {
        for row in &self.pass.coupled_rows {
            let mut projected = [0.0; 3];
            for (global, derivative) in &row.columns {
                let i = self.local[*global as usize];
                let value = self.scales[i] * v[i];
                for channel in 0..3 {
                    projected[channel] += derivative[channel] * value;
                }
            }
            let weight = self.data_scale * row.weight;
            for (global, derivative) in &row.columns {
                let i = self.local[*global as usize];
                product[i] += weight * self.scales[i]
                    * (derivative[0] * projected[0] + derivative[1] * projected[1]
                        + derivative[2] * projected[2]);
            }
        }
        }
        for &(i, j, value) in &self.penalty {
            product[i] += value * v[j];
        }
    }

    fn block_precondition(&self, residual: &[f64], z: &mut [f64]) {
        for block in &self.factors {
            z[block.range.clone()].copy_from_slice(&residual[block.range.clone()]);
            solve_in_place(&block.factor, block.range.len(), &mut z[block.range.clone()]);
        }
    }

    // (gᵀd)² / (2 dᵀBd) ≤ ½ gᵀB⁻¹g.
    fn energy_lower_bound(&self) -> f64 {
        let mut direction = vec![0.0; self.rhs.len()];
        self.block_precondition(&self.rhs, &mut direction);
        let first = crate::solve::dot(&self.rhs, &direction);
        if first == 0.0 {
            return 0.0;
        }
        let mut product = vec![0.0; direction.len()];
        self.apply(&direction, &mut product);
        let curvature = crate::solve::dot(&direction, &product);
        if first.is_finite() && curvature.is_finite() && curvature > 0.0 {
            first * first / (2.0 * curvature)
        } else {
            f64::INFINITY
        }
    }

    fn coarse_space(&self) -> Option<CoarseSpace> {
        let n = self.rhs.len();
        let mut look_factors = Vec::with_capacity(self.factors.len());
        for block in &self.factors {
            let width = block.range.len();
            let look = block.range.clone().take_while(|i| self.labels[*i].0.is_some()).count();
            let principal = if look > 0 && look < width {
                let mut principal = vec![0.0; look * look];
                for i in 0..look {
                    principal[i * look..(i + 1) * look]
                        .copy_from_slice(&block.factor[i * width..i * width + look]);
                }
                Some(principal)
            } else {
                None
            };
            look_factors.push((look, principal));
        }
        let mut basis = Vec::new();
        let mut images = Vec::new();
        for group in self.coarse_groups.iter().filter(|group| !group.is_empty()) {
            let mut vector = vec![0.0; n];
            for &i in group {
                vector[i] = 1.0;
            }
            let mut image = vec![0.0; n];
            self.apply(&vector, &mut image);
            for (block, (look, principal)) in self.factors.iter().zip(&look_factors) {
                if *look == 0 {
                    continue;
                }
                let range = block.range.start..block.range.start + *look;
                vector[range.clone()].copy_from_slice(&image[range.clone()]);
                solve_in_place(principal.as_ref().unwrap_or(&block.factor), *look, &mut vector[range.clone()]);
                for value in &mut vector[range] {
                    *value = -*value;
                }
            }
            self.apply(&vector, &mut image);
            basis.push(vector);
            images.push(image);
        }
        let m = basis.len();
        if m == 0 {
            return None;
        }
        let mut factor = vec![0.0; m * m];
        for i in 0..m {
            for j in 0..=i {
                let value = crate::solve::dot(&basis[i], &images[j]);
                factor[i * m + j] = value;
                factor[j * m + i] = value;
            }
        }
        factor_in_place(&mut factor, m).then_some(CoarseSpace { basis, images, factor })
    }

    fn solve(&self) -> Option<Vec<f64>> {
        let n = self.rhs.len();
        let mut solution = vec![0.0; n];
        let coarse = self.coarse_space();
        let m = coarse.as_ref().map_or(0, |space| space.basis.len());
        let mut coefficients = vec![0.0; m];
        let mut correction = vec![0.0; m];
        let mut projected_residual = vec![0.0; n];
        let report = crate::solve::conjugate_gradient_preconditioned(
            n, |v, product| self.apply(v, product),
            |residual, z| {
                if let Some(space) = &coarse {
                    for (i, basis) in space.basis.iter().enumerate() {
                        coefficients[i] = crate::solve::dot(basis, residual);
                    }
                    solve_in_place(&space.factor, m, &mut coefficients);
                    projected_residual.copy_from_slice(residual);
                    for (image, coefficient) in space.images.iter().zip(&coefficients) {
                        crate::solve::scaled_add(&mut projected_residual, image, -*coefficient);
                    }
                    self.block_precondition(&projected_residual, z);
                    for (i, image) in space.images.iter().enumerate() {
                        correction[i] = crate::solve::dot(image, z);
                    }
                    solve_in_place(&space.factor, m, &mut correction);
                    for (i, basis) in space.basis.iter().enumerate() {
                        crate::solve::scaled_add(z, basis, coefficients[i] - correction[i]);
                    }
                } else {
                    self.block_precondition(residual, z);
                }
            },
            &self.rhs, &mut solution, 256, 1e-8,
        );
        let mut product = vec![0.0; n];
        self.apply(&solution, &mut product);
        let error: f64 = product.iter().zip(&self.rhs).map(|(a, b)| (a - b) * (a - b)).sum();
        let norm = crate::solve::dot(&self.rhs, &self.rhs);
        if fit_diag() {
            let damped = self.damping_diagonal.iter().any(|v| *v > 0.0);
            eprintln!("fit cg exact {} damped {damped} dim {n} coarse {m} iterations {} cap 256 relative_residual {:.9e} positive_curvature {} resolved {}",
                self.exact_data.is_some(), report.iterations, (error / norm.max(1e-300)).sqrt(), report.positive_curvature,
                report.positive_curvature && error.is_finite() && error <= 1e-12 * norm.max(1.0));
            let mut worst: Vec<usize> = (0..n).collect();
            worst.sort_by(|a, b| (product[*b] - self.rhs[*b]).abs().total_cmp(&(product[*a] - self.rhs[*a]).abs()));
            for &i in worst.iter().take(3) {
                eprintln!("fit cg residual param {} label {:?} residual {:.9e} rhs {:.9e} solution {:.9e}",
                    self.globals[i], self.labels[i], product[i] - self.rhs[i], self.rhs[i], solution[i]);
            }
        }
        (report.positive_curvature && error.is_finite() && error <= 1e-12 * norm.max(1.0)).then_some(solution)
    }
}

#[allow(clippy::too_many_arguments)]
fn coupled_step(
    layout: &Layout, problem: &Problem, params: &Params, options: &FitOptions,
    blocks: &[BlockAcc], penalties: &[Vec<f64>], lambdas: &StageLambdas, roles: &[Role],
    pass: &PassOutput, data_scale: f64, damping: f64, step: &mut [f64],
) -> Option<f64> {
    let system = CoupledSystem::new(
        layout, problem, params, options, blocks, penalties, lambdas, roles, pass, data_scale, damping,
    )?;
    let mut delta = system.solve()?;
    clamp_values(&mut delta, STEP_RADIUS);
    let mut product = vec![0.0; delta.len()];
    system.apply(&delta, &mut product);
    let mut prediction = crate::solve::dot(&system.rhs, &delta);
    for (i, &global) in system.globals.iter().enumerate() {
        step[global] = delta[i];
        prediction -= 0.5 * delta[i] * (product[i] - system.damping_diagonal[i] * delta[i]);
    }
    Some(prediction)
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
) -> f64 {
    let n = block.len();
    if n == 0 {
        return 0.0;
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
    if block.kind == BlockKind::Nuisance
        && solve_nuisance_schur(layout, block, &a, &rhs, step)
    {
        let mut delta: Vec<f64> = block.params.iter().map(|global| step[*global]).collect();
        if step_limit == StepLimit::PerBlock {
            clamp_block_step(step, &block.params, STEP_RADIUS);
            delta = block.params.iter().map(|global| step[*global]).collect();
        }
        return predicted_reduction(&a, &rhs, &delta, &damping_diag);
    }
    let mut delta = rhs.clone();
    let mut factor = a.clone();
    if !factor_in_place(&mut factor, n) {
        return 0.0;
    }
    solve_in_place(&factor, n, &mut delta);
    if step_limit == StepLimit::PerBlock {
        clamp_values(&mut delta, STEP_RADIUS);
    }
    for i in 0..n {
        step[block.params[i]] = delta[i];
    }
    predicted_reduction(&a, &rhs, &delta, &damping_diag)
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
    if step_limit == StepLimit::PerBlock && block.full_diagonal.len() == n {
        let mut scales = vec![1.0_f64; n];
        for (index, scale) in scales.iter_mut().enumerate() {
            let sampled = block.hessian[index * n + index] * data_scale;
            if sampled > 0.0 {
                *scale = (block.full_diagonal[index] / sampled).max(0.0).sqrt();
            }
        }
        for i in 0..n {
            for j in 0..n {
                a[i * n + j] *= scales[i] * scales[j];
            }
        }
    }
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
    let free: Vec<bool> = support.iter().map(|s| peak > 0.0 && *s >= 1e-4 * peak).collect();
    let mut rhs = vec![0.0; RESIDUAL_COEFS];
    let mut diagonal = residual_diagonal(&pass.residual_samples);
    for i in 0..RESIDUAL_COEFS {
        if free[i] {
            rhs[i] = pass.residual_gradient[i]
                - ridge * laplacian_at(theta, i, &penalty) - identity * theta[i];
        }
        diagonal[i] = if free[i] {
            data_scale * diagonal[i] + ridge * penalty[i] + identity
        } else {
            1.0
        };
    }
    let apply = |v: &[f64], product: &mut [f64]| {
        residual_undamped_apply(
            &pass.residual_samples, v, product, &penalty, ridge, identity, data_scale,
        );
        for i in 0..RESIDUAL_COEFS {
            product[i] = if free[i] { product[i] - 1e-3 * v[i] } else { v[i] };
        }
    };
    let mut solution = vec![0.0; RESIDUAL_COEFS];
    crate::solve::conjugate_gradient(
        RESIDUAL_COEFS, apply, &diagonal, &rhs, &mut solution, 40, 1e-6,
    );
    let mut product = vec![0.0; RESIDUAL_COEFS];
    apply(&solution, &mut product);
    let error: f64 = product.iter().zip(&rhs).map(|(a, b)| (a - b) * (a - b)).sum();
    let norm: f64 = rhs.iter().map(|v| v * v).sum();
    let energy = 0.5 * rhs.iter().zip(solution).map(|(g, z)| g * z).sum::<f64>();
    if error.is_finite() && error <= 1e-12 * norm.max(1.0)
        && energy.is_finite() && energy >= 0.0
    {
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
        let mut params = initial_params(&problem);
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
            layout, &block, &mut step, &penalty, &prior_gradient, &prior_curvature,
            &params, &options.initial_lambdas, 1e12, 1.0, StepLimit::PerBlock,
        );
        assert!(step[layout.tone_c0].abs() < 1e-9);
        let pass = problem.pass(
            &params, ActiveColumns::ALL, &mut [], false, false, false, IntentFilter::NativeOnly,
        );
        let decrement = stationarity_decrement(
            layout, &problem, &params, &options, &[block], &[penalty],
            &options.initial_lambdas, &roles, &pass, 1.0,
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
        let mut params = initial_params(&problem);
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
            &params, ActiveColumns::ALL, &mut [], false, false, false, IntentFilter::NativeOnly,
        );
        pass.coupled_rows = vec![crate::problem::HessianRow {
            columns: vec![(globals[0] as u32, [2.0, 0.0, 0.0]),
                (globals[1] as u32, [3.0, 0.0, 0.0])],
            weight: 1.0,
        }];
        let system = CoupledSystem::new(
            layout, &problem, &params, &options, &blocks, &[vec![0.0], vec![0.0]],
            &options.initial_lambdas, &prior_roles(&problem, layout), &pass, 1.0, 0.0,
        ).expect("positive definite curvature");
        let solution = system.solve().expect("resolved coupled solve");
        assert!((5.0 * solution[0] + 6.0 * solution[1] - 2.0).abs() < 1e-10);
        assert!((6.0 * solution[0] + 9.25 * solution[1] - 3.0).abs() < 1e-10);
        let lower_bound = system.energy_lower_bound();
        let energy = 0.5 * crate::solve::dot(&system.rhs, &solution);
        assert!(lower_bound > STATIONARY_DECREMENT_NATS);
        assert!(lower_bound <= energy + 1e-12);
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
}
