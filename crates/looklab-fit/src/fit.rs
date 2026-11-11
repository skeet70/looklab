// SPDX-License-Identifier: AGPL-3.0-or-later
//! Staged Gauss-Newton/Levenberg-Marquardt with block solves and per-stage GCV.

use looklab_model::basis::{CORE_TERMS, RESIDUAL_COEFS, S_COEFS, TONE_COEFS, VOLUME_COEFS};
use looklab_model::domain_penalty::DOMAIN_AXES;
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
use crate::problem::{
    ActiveColumns, BlockAcc, IntentFilter, PassOutput, Problem, ResidualSample, RowModel,
};
use crate::solve::{factor_in_place, select_lambda, select_lambda_diagonal, solve_in_place};
use crate::types::{
    CertificateOrigin, CertificateTermination, CertificateWork, ContributorId, ControlResponseFit,
    Diagnostics, ExcludeReason, ExcludedImage, ExposureOffset, FitError, FitImage, FitOptions,
    FitResult, FitState, FitStateGeneration, ImageIntent, ImageNuisances, Nats, PLATEAU_SAMPLES,
    PlateauHistory, PlateauObjective, PreviousSupport, ProbeUncertainty, Realization, Result,
    SampleEvaluations, StageLambdas, StationarityBounds, StationarityCertificate,
    StationarityReport, Verdict,
};
use crate::uncertainty;

const RESIDUAL_NODES: usize = RESIDUAL_COEFS / 3;
const RESIDUAL_GRID: usize = 17;
const STEP_RADIUS: f64 = 2.0;
const STATIONARY_GRADIENT: f64 = 1e-3;
pub(crate) const STATIONARY_DECREMENT_NATS: f64 = 1e-3;
/// Log-sigma search interval used by [`update_sigma`].
const SIGMA_RANGE: f64 = 6.0;
/// Bisection steps in the log-sigma root search.
const SIGMA_BRACKET_STEPS: usize = 48;

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

pub(crate) fn converged(recent: &[f64]) -> bool {
    if recent.len() < PLATEAU_SAMPLES {
        return false;
    }
    let oldest = recent[0];
    let newest = recent[recent.len() - 1];
    (oldest - newest).abs() <= 1e-3 * oldest.abs().max(1.0)
}

/// Objective identity a checkpointed plateau history must match to resume.
fn plateau_objective(
    problem: &Problem,
    params: &Params,
    lambdas: &StageLambdas,
    stage: usize,
    options: &FitOptions,
) -> PlateauObjective {
    PlateauObjective {
        stage,
        residual: options.fit_residual,
        lambdas: *lambdas,
        sigma: problem
            .contributors
            .iter()
            .enumerate()
            .map(|(index, _)| params.theta[params.layout.sigma_of(index)])
            .collect(),
        samples: problem.samples.len(),
        excluded: problem.excluded_samples,
    }
}

/// A saved state from an older solver generation is not reinterpreted under the current objective.
fn current_resume(resume: Option<&FitState>) -> Option<&FitState> {
    resume.filter(|state| state.generation == FitStateGeneration::CURRENT)
}

/// Accepted-state samples, or empty when the checkpoint objective differs.
fn resume_history(state: &FitState, objective: &PlateauObjective) -> Vec<f64> {
    if state.plateau.compatible(objective) {
        state
            .plateau
            .samples()
            .iter()
            .map(|sample| sample.get())
            .collect()
    } else {
        Vec::new()
    }
}

/// Checkpoint carrying the live plateau samples under the objective they belong to.
#[allow(clippy::too_many_arguments)]
fn checkpoint_state(
    problem: &Problem,
    params: &Params,
    lambdas: StageLambdas,
    stage: usize,
    sweep: usize,
    damping: f64,
    options: &FitOptions,
    recent: &[f64],
    pending: bool,
) -> FitState {
    FitState {
        generation: FitStateGeneration::CURRENT,
        theta: params.theta.clone(),
        lambdas,
        stage,
        sweep,
        damping,
        trust_radius: None,
        converged: false,
        plateau: PlateauHistory::snapshot(
            recent,
            pending,
            plateau_objective(problem, params, &lambdas, stage, options),
        ),
    }
}

fn sync_checkpoint_params(state: &mut Option<FitState>, params: &Params) {
    if let Some(state) = state.as_mut().filter(|state| state.theta != params.theta) {
        state.theta.clone_from(&params.theta);
        state.plateau = PlateauHistory::default();
    }
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
    /// Full solver state of the conditional block, for subject-bound resume.
    pub theta: Vec<f64>,
    pub lambdas: StageLambdas,
}

/// Seed the conditional Look solve from a warm Look, the live row context and the frozen scales.
#[allow(clippy::too_many_arguments)]
pub(crate) fn seed_look_only_params(
    problem: &Problem,
    options: &FitOptions,
    initial: Option<&looklab_model::Look>,
    clip_k: f64,
    rows: &[RowModel<'_>],
    initial_theta: Option<&[f64]>,
    sigma: &[(ContributorId, f64)],
) -> Result<Params> {
    let layout = problem.layout.clone();
    let mut params = match initial {
        Some(look) => params_from_look(problem, look),
        None => initial_params(problem, options),
    };
    if let Some(theta) = initial_theta {
        if theta.len() != layout.total {
            return Err(FitError::RowContext(format!(
                "resume theta has {} entries, layout has {}",
                theta.len(),
                layout.total
            )));
        }
        params.theta[layout.tone_c0] = theta[layout.tone_c0];
        for range in [
            layout.tone_beta.clone(),
            layout.tint_a.clone(),
            layout.tint_b.clone(),
            layout.g.clone(),
            layout.r.clone(),
            layout.lambda.clone(),
            layout.residual.clone(),
        ] {
            params.theta[range.clone()].copy_from_slice(&theta[range]);
        }
    }
    for index in 0..problem.images.len() {
        let range = layout.image(index);
        params.theta[range.start] = problem.images[index].exposure_hint.clamp(-8.0, 8.0);
        params.theta[range.start + 1] = 0.0;
        params.theta[range.start + 2] = 0.0;
    }
    params.theta[layout.clip] = crate::layout::zeta_from_clip(clip_k);
    seed_row_responses(problem, &mut params, rows);
    for (index, id) in problem.contributors.iter().enumerate() {
        if let Some((_, log_sigma)) = sigma.iter().find(|(candidate, _)| candidate == id) {
            params.theta[layout.sigma_of(index)] = *log_sigma;
        }
    }
    Ok(params)
}

/// Recertify a frozen conditional Look block after its row context is final.
#[allow(clippy::too_many_arguments)]
pub(crate) fn certify_look_rows(
    images: &[FitImage],
    options: &FitOptions,
    initial: Option<&looklab_model::Look>,
    clip_k: f64,
    rows: &[RowModel<'_>],
    initial_theta: Option<&[f64]>,
    lambdas: StageLambdas,
    sigma: &[(ContributorId, f64)],
    evaluations: &mut usize,
    remaining: &mut f64,
) -> Result<StationarityReport> {
    let problem = Problem::new_with_rows(images, rows, options)?;
    let mut params = seed_look_only_params(
        &problem,
        options,
        initial,
        clip_k,
        rows,
        initial_theta,
        sigma,
    )?;
    Ok(certify_look(
        &problem,
        &mut params,
        options,
        &lambdas,
        evaluations,
        remaining,
        true,
    ))
}

/// Optimize only θ_L over rows that carry their own realization clip and control response.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_look_only_rows(
    images: &[FitImage],
    options: &FitOptions,
    initial: Option<&looklab_model::Look>,
    clip_k: f64,
    rows: &[RowModel<'_>],
    previous: Option<&PreviousSupport>,
    initial_theta: Option<&[f64]>,
    initial_lambdas: Option<StageLambdas>,
    sigma: &[(ContributorId, f64)],
    remaining: &mut f64,
) -> Result<LookOnlyResult> {
    let problem = Problem::new_with_rows(images, rows, options)?;
    let layout = problem.layout.clone();
    let mut params = seed_look_only_params(
        &problem,
        options,
        initial,
        clip_k,
        rows,
        initial_theta,
        sigma,
    )?;
    let active = ActiveColumns {
        image: false,
        mode: false,
        wb: false,
        clip: false,
        response: false,
    };
    let mut lambdas = initial_lambdas.unwrap_or(options.initial_lambdas);
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
        let mut stepped = false;
        let stage_sweeps = if stage.converge {
            options.max_sweeps.max(1)
        } else {
            stage.sweeps.min(options.max_sweeps.max(1))
        };
        for _ in 0..stage_sweeps {
            stepped = false;
            let samples = problem.samples.len() as f64;
            if *remaining < samples {
                break;
            }
            *remaining -= samples;
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
            if pass.infeasible {
                if let Some((_, previous_params)) = &previous {
                    params = previous_params.clone();
                    damping = (damping * 10.0).min(1e6);
                    if damping >= 1e6 {
                        break;
                    }
                    continue;
                }
                return Err(FitError::RowContext(
                    "retained rows left the evaluator domain".to_string(),
                ));
            }
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
            let residual_mask = kinds
                .contains(&BlockKind::Residual)
                .then(|| ResidualMask::from_samples(&pass.residual_samples));
            if let Some(mask) = &residual_mask {
                let lambda = lam(&lambdas, BlockKind::Residual);
                let delta = residual_step(
                    &pass,
                    &params.theta[layout.residual.clone()],
                    lambda,
                    damping,
                    hessian_scale,
                    StepLimit::WholeStep,
                    mask,
                );
                for (index, value) in delta.iter().enumerate() {
                    step[layout.residual.start + index] = *value;
                }
            }
            apply_step(&mut params, &step);
            stepped = true;
            if let Some(mask) = &residual_mask {
                mask.pin(&mut params.theta[layout.residual.clone()]);
            }
            damping = (damping * 0.3).max(1e-6);
        }
        // A stage ends on a step that no pass has evaluated, and the next stage starts
        // without an accepted state to revert to. Validate the state the stage leaves
        // behind on every retained row, and drop the step when it left the domain.
        if stepped {
            let samples = problem.samples.len() as f64;
            if *remaining >= samples {
                *remaining -= samples;
                let pass = problem.pass(
                    &params,
                    active,
                    &mut Vec::new(),
                    false,
                    false,
                    false,
                    IntentFilter::NativeOnly,
                );
                evaluations += 1;
                if pass.infeasible
                    && let Some((_, before)) = &previous
                {
                    params = before.clone();
                }
            }
        }
    }
    certify_look(
        &problem,
        &mut params,
        options,
        &lambdas,
        &mut evaluations,
        remaining,
        false,
    );
    let (probes, density, output_space) =
        look_only_probes(&problem, &params, &lambdas, previous, options)?;
    evaluations += 1;
    Ok(LookOnlyResult {
        look: params.look(),
        probes,
        density,
        clip_k,
        output_space,
        evaluations,
        theta: params.theta,
        lambdas,
    })
}

/// Published probes and density at the supplied conditional state.
pub(crate) fn look_only_probes(
    problem: &Problem,
    params: &Params,
    lambdas: &StageLambdas,
    previous: Option<&PreviousSupport>,
    options: &FitOptions,
) -> Result<(
    ProbeUncertainty,
    crate::types::Density,
    looklab_color::RgbSpace,
)> {
    let layout = problem.layout.clone();
    let look = params.look();
    let density = density::build(problem, params, options.density_floor);
    let frozen = ActiveColumns {
        image: false,
        mode: false,
        wb: false,
        clip: false,
        response: false,
    };
    let mut uncertainty_blocks = vec![
        make_look_block(&layout, frozen),
        make_block(problem, BlockKind::Residual, frozen),
    ];
    let uncertainty_pass = problem.pass(
        params,
        frozen,
        &mut uncertainty_blocks,
        false,
        true,
        false,
        IntentFilter::All,
    );
    let penalty = double(combined_look_penalty(
        problem,
        &layout,
        lambdas,
        uncertainty_blocks[0].len(),
        false,
    ));
    let probes = uncertainty::probe_uncertainty(
        problem,
        params,
        &look,
        &uncertainty_blocks[0],
        &penalty,
        uncertainty_pass.active_samples.max(1) as f64 / uncertainty_pass.hessian_rows.max(1) as f64,
        lambdas,
        &uncertainty_pass.per_image_gradient,
        &density,
        &uncertainty_pass.residual_samples,
        options,
        previous,
    )?;
    let output_space = problem
        .images
        .first()
        .map_or(looklab_color::RgbSpace::Srgb, |image| image.space);
    Ok((probes, density, output_space))
}

fn pinned_residual_blocks(mask: &ResidualMask, theta: &[f64], fail_closed: bool) -> bool {
    fail_closed && mask.has_pinned_value(theta)
}

fn certify_look(
    problem: &Problem,
    params: &mut Params,
    options: &FitOptions,
    lambdas: &StageLambdas,
    evaluations: &mut usize,
    remaining: &mut f64,
    fail_closed_on_pin: bool,
) -> StationarityReport {
    let report =
        |certificate: StationarityCertificate, work: Option<CertificateWork>| StationarityReport {
            origin: CertificateOrigin::PostFitRecertification,
            certificate,
            work,
            bounds: None,
        };
    let layout = problem.layout.clone();
    let roles = prior_roles(problem, &layout);
    let final_stage = STAGES.last().expect("stages");
    let kinds: Vec<BlockKind> = final_stage
        .blocks
        .iter()
        .copied()
        .filter(|kind| !is_response_kind(*kind))
        .filter(|kind| options.fit_residual || *kind != BlockKind::Residual)
        .collect();
    let active = ActiveColumns {
        image: false,
        mode: false,
        wb: false,
        clip: false,
        response: false,
    };
    let mut blocks: Vec<BlockAcc> = kinds
        .iter()
        .map(|kind| make_block(problem, *kind, active))
        .collect();
    let penalties: Vec<Vec<f64>> = blocks
        .iter()
        .map(|block| block_penalty(problem, block))
        .collect();
    let samples = problem.samples.len();
    let mut warm = None;
    for _ in 0..3 {
        if *remaining < samples as f64 {
            return report(
                StationarityCertificate::Unresolved {
                    termination: CertificateTermination::WorkLimit,
                },
                None,
            );
        }
        *remaining -= samples as f64;
        for block in &mut blocks {
            block.reset();
        }
        let pass = problem.pass_with_rows(
            params,
            active,
            &mut blocks,
            false,
            false,
            false,
            IntentFilter::NativeOnly,
            true,
            samples <= FULL_ROW_LIMIT,
        );
        *evaluations += 1;
        if pass.infeasible || !pass.cost.is_finite() {
            return report(
                StationarityCertificate::Unresolved {
                    termination: CertificateTermination::InvalidObjective,
                },
                None,
            );
        }
        let data_scale = pass.active_samples.max(1) as f64 / pass.hessian_rows.max(1) as f64;
        let mask = ResidualMask::from_samples(&pass.residual_samples);
        let residual_active = blocks.iter().any(|block| block.kind == BlockKind::Residual);
        if residual_active && mask.has_pinned_value(&params.theta[layout.residual.clone()]) {
            if pinned_residual_blocks(
                &mask,
                &params.theta[layout.residual.clone()],
                fail_closed_on_pin,
            ) {
                return report(
                    StationarityCertificate::Unresolved {
                        termination: CertificateTermination::PinViolation,
                    },
                    None,
                );
            }
            mask.pin(&mut params.theta[layout.residual.clone()]);
            continue;
        }
        return stationarity_bound(
            &layout,
            problem,
            params,
            options,
            &blocks,
            &penalties,
            lambdas,
            &roles,
            &pass,
            data_scale,
            &mut warm,
            &mask,
            CertificateOrigin::PostFitRecertification,
        );
    }
    report(
        StationarityCertificate::Unresolved {
            termination: CertificateTermination::WorkLimit,
        },
        None,
    )
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
    let mut accepted_count = 0usize;
    let mut attempts = 0usize;
    let mut last_step = 0.0_f64;
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
    let mut last_gradient = gradient_max(GradientArgs {
        layout: &layout,
        problem: &problem,
        params: &params,
        options,
        blocks: &blocks,
        penalties: &penalties,
        lambdas: &lambdas,
        roles: &roles,
    });
    let mut converged_fit =
        !pass.infeasible && cost.is_finite() && last_gradient <= STATIONARY_GRADIENT;
    for _ in 0..max_attempts {
        if converged_fit {
            break;
        }
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
                let block = &blocks[index];
                let n = block.len();
                eprintln!(
                    "nuisance attempt {attempts} block {index} factorization failed damping {damping:.3e} n {n} diagonal {:?} gradient {:?}",
                    (0..n)
                        .map(|i| block.hessian[i * n + i])
                        .collect::<Vec<f64>>(),
                    block.gradient
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
    if let Some(state) = resume.filter(|state| state.generation != FitStateGeneration::CURRENT)
        && fit_diag()
    {
        eprintln!(
            "fit resume rejected generation {} current {}",
            state.generation.0,
            FitStateGeneration::CURRENT.0
        );
    }
    let resume = current_resume(resume);
    let resume = match resume {
        Some(state) if state.theta.len() != params.theta.len() => {
            if fit_diag() {
                eprintln!(
                    "fit resume rejected layout {} current {}",
                    state.theta.len(),
                    params.theta.len()
                );
            }
            None
        }
        other => other,
    };
    if let Some(state) = resume {
        params.theta.copy_from_slice(&state.theta);
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
    let mut certificate: Option<StationarityReport> = None;
    let mut operator_rows = 0usize;
    let mut resume_state: Option<FitState> = None;
    let mut cert_warm: Option<Vec<f64>> = None;
    let mut recent: Vec<f64> = Vec::new();
    let mut trust_radius = resume.and_then(|state| state.trust_radius);
    let mut final_scratch = FinalScratch::default();
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
        let (kinds, mut blocks, penalties) = stage_blocks(stage, stage_problem, options);
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
        recent.clear();
        if resuming && let Some(state) = resume {
            let objective = plateau_objective(&problem, &params, &lambdas, stage_index, options);
            accepted_state = state.plateau.compatible(&objective) && state.plateau.pending;
            recent = resume_history(state, &objective);
        }
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
                resume_state = Some(checkpoint_state(
                    &problem,
                    &params,
                    lambdas,
                    stage_index,
                    sweep_index,
                    damping,
                    options,
                    &recent,
                    accepted_state,
                ));
                break;
            }
            sample_evaluations += samples_per_pass;
            for block in &mut blocks {
                block.reset();
            }
            let pass = if stage.converge {
                let rows = if stage_problem.samples.len() <= FULL_ROW_LIMIT {
                    crate::problem::HessianRows::All
                } else {
                    crate::problem::HessianRows::Retained
                };
                stage_problem.pass_with_analytic_rows(
                    &params,
                    stage.active,
                    &mut blocks,
                    false,
                    false,
                    stage.neutral,
                    IntentFilter::NativeOnly,
                    rows,
                )
            } else {
                stage_problem.pass(
                    &params,
                    stage.active,
                    &mut blocks,
                    false,
                    false,
                    stage.neutral,
                    IntentFilter::NativeOnly,
                )
            };
            evaluations += 1;
            if stage.converge {
                operator_rows = pass.hessian_rows;
            }
            let hessian_scale = pass.active_samples.max(1) as f64 / pass.hessian_rows.max(1) as f64;
            let residual_active = kinds.contains(&BlockKind::Residual);
            let mask = if stage.converge && residual_active {
                ResidualMask::from_samples(&pass.residual_samples)
            } else {
                ResidualMask::empty()
            };
            if stage.converge
                && residual_active
                && mask.has_pinned_value(&params.theta[layout.residual.clone()])
            {
                mask.pin(&mut params.theta[layout.residual.clone()]);
                certificate = None;
                accepted_state = true;
                continue;
            }
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
                certificate = None;
            }

            if converged(&recent) {
                if !stage.converge {
                    break;
                }
                match conditional_sigma_state(&problem, &mut params, &pass, options) {
                    SigmaState::Settled => {}
                    SigmaState::Refresh => {
                        certificate = None;
                        accepted_state = true;
                        continue;
                    }
                    SigmaState::Unresolved(termination) => {
                        certificate = Some(StationarityReport {
                            origin: CertificateOrigin::FinalStage,
                            certificate: StationarityCertificate::Unresolved { termination },
                            work: None,
                            bounds: None,
                        });
                        break;
                    }
                }
                let report = match certificate {
                    Some(report) => report,
                    None => stationarity_bound(
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
                        &mask,
                        CertificateOrigin::FinalStage,
                    ),
                };
                if fit_diag() {
                    eprintln!(
                        "fit stage {stage_index} sweep {sweep_index} plateau threshold {STATIONARY_DECREMENT_NATS:.3e}"
                    );
                    print_block_gradients(
                        "plateau", &layout, &problem, &params, options, &blocks, &penalties,
                        &lambdas, &roles,
                    );
                }
                let stationary = report.certificate.is_certified_stationary();
                certificate = Some(report);
                if stationary {
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
                    &mut trust_radius,
                    &mut gradient_scale,
                    &mut evaluations,
                    &mut sample_evaluations,
                    samples_per_pass,
                    stage_index,
                    sweep_index,
                    recent.len(),
                    &mut final_scratch,
                    &mut cert_warm,
                    &mask,
                    residual_active,
                ) {
                    FinalOutcome::Accepted(trial) => {
                        update_sigma(stage_problem, &mut params, &trial, options);
                        certificate = None;
                        accepted_state = true;
                        continue;
                    }
                    FinalOutcome::Stationary(report) => {
                        certificate = Some(report);
                        accepted_state = true;
                        continue;
                    }
                    FinalOutcome::SigmaRefresh => {
                        certificate = None;
                        accepted_state = true;
                        continue;
                    }
                    FinalOutcome::Unresolved(report) => {
                        certificate = Some(report);
                        if fit_diag() {
                            eprintln!(
                                "fit checkpoint stage {stage_index} sweep {sweep_index} reason certificate damping {damping:.3e} history {}",
                                recent.len()
                            );
                        }
                        resume_state = Some(checkpoint_state(
                            &problem,
                            &params,
                            lambdas,
                            stage_index,
                            sweep_index,
                            damping,
                            options,
                            &recent,
                            accepted_state,
                        ));
                        break;
                    }
                    FinalOutcome::Budget | FinalOutcome::Stalled => {
                        if fit_diag() {
                            eprintln!(
                                "fit checkpoint stage {stage_index} sweep {sweep_index} reason joint damping {damping:.3e} history {}",
                                recent.len()
                            );
                        }
                        resume_state = Some(checkpoint_state(
                            &problem,
                            &params,
                            lambdas,
                            stage_index,
                            sweep_index,
                            damping,
                            options,
                            &recent,
                            accepted_state,
                        ));
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
            let residual_mask = kinds
                .contains(&BlockKind::Residual)
                .then(|| ResidualMask::from_samples(&pass.residual_samples));
            if let Some(mask) = &residual_mask {
                let delta = residual_step(
                    &pass,
                    &params.theta[layout.residual.clone()],
                    lam(&lambdas, BlockKind::Residual),
                    damping,
                    hessian_scale,
                    StepLimit::PerBlock,
                    mask,
                );
                for (index, value) in delta.iter().enumerate() {
                    step[layout.residual.start + index] = *value;
                }
            }
            apply_step(&mut params, &step);
            if let Some(mask) = &residual_mask {
                mask.pin(&mut params.theta[layout.residual.clone()]);
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
                    resume_state = Some(checkpoint_state(
                        &problem,
                        &params,
                        lambdas,
                        stage_index,
                        sweep_index,
                        damping,
                        options,
                        &recent,
                        accepted_state,
                    ));
                    break;
                }
                continue;
            }
            update_sigma(stage_problem, &mut params, &trial, options);
            certificate = None;
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
            resume_state = Some(checkpoint_state(
                &problem,
                &params,
                lambdas,
                stage_index,
                0,
                damping,
                options,
                &recent,
                accepted_state,
            ));
            break;
        }
    }
    let has_recipe = images
        .iter()
        .any(|image| image.intent == ImageIntent::Recipe);

    if !has_recipe {
        let report = recertify_native(
            &problem,
            &mut params,
            options,
            &lambdas,
            &mut evaluations,
            &mut sample_evaluations,
            &mut cert_warm,
            &mut operator_rows,
        );
        if !report.certificate.is_certified_stationary() {
            final_converged = false;
        }
        sync_checkpoint_params(&mut resume_state, &params);
        certificate = Some(report);
    }

    if has_recipe {
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
        let mut native_params =
            native_params_from_all(&problem, &all_problem, &all_params, &native_positions);
        let report = recertify_native(
            &problem,
            &mut native_params,
            options,
            &lambdas,
            &mut evaluations,
            &mut sample_evaluations,
            &mut cert_warm,
            &mut operator_rows,
        );
        if !report.certificate.is_certified_stationary() {
            final_converged = false;
        }
        if !final_converged && resume_state.is_none() {
            resume_state = Some(checkpoint_state(
                &problem,
                &native_params,
                lambdas,
                STAGES.len() - 1,
                0,
                1e-3,
                options,
                &[],
                false,
            ));
        }
        sync_checkpoint_params(&mut resume_state, &native_params);
        certificate = Some(report);
        propagate_shared(&problem, &native_params, &all_problem, &mut all_params);
        problem = all_problem;
        params = all_params;
    }
    if !final_converged && certificate.is_some() && resume_state.is_none() {
        resume_state = Some(checkpoint_state(
            &problem,
            &params,
            lambdas,
            STAGES.len() - 1,
            0,
            1e-3,
            options,
            &[],
            false,
        ));
    }
    let want_uncertainty = want_uncertainty && final_converged;

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
        let space = problem
            .images
            .first()
            .map_or(looklab_color::RgbSpace::Srgb, |image| image.space);
        match estimate_model_error(&native, &[], model.as_ref(), options, clip_k) {
            Ok(field) => {
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
                crate::uncertainty::unsupport_unreliable(&mut probe_result);
            }
            Err(FitError::NoModelErrorEvidence) => {
                for probe in &mut probe_result.probes {
                    probe.supported_evidence = false;
                    probe.supported = false;
                }
                probe_result.supported_fraction = 0.0;
            }
            Err(error) => return Err(error),
        }
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
        stationarity: certificate,
    };
    diagnostics.sweeps = evaluations;
    if fit_diag() {
        let report = diagnostics.stationarity;
        let (kind, termination) = match report {
            None => ("none", "none"),
            Some(report) => match report.certificate {
                StationarityCertificate::CertifiedStationary { .. } => ("stationary", "resolved"),
                StationarityCertificate::CertifiedNonstationary { .. } => {
                    ("nonstationary", "resolved")
                }
                StationarityCertificate::Unresolved { termination } => {
                    ("unresolved", termination.label())
                }
            },
        };
        let bounds = report.and_then(|report| report.bounds);
        let lower = match report.map(|report| report.certificate) {
            Some(StationarityCertificate::CertifiedNonstationary { lower }) => {
                format!("{:.9e}", lower.get())
            }
            _ => bounds.map_or_else(
                || "none".to_string(),
                |bounds| format!("{:.9e}", bounds.lower.get()),
            ),
        };
        let upper = match report.map(|report| report.certificate) {
            Some(StationarityCertificate::CertifiedStationary { upper }) => {
                format!("{:.9e}", upper.get())
            }
            _ => bounds.map_or_else(
                || "none".to_string(),
                |bounds| format!("{:.9e}", bounds.upper.get()),
            ),
        };
        let work = report.and_then(|report| report.work);
        let complete = u8::from(work.is_some());
        let dim = work.map_or_else(|| "none".to_string(), |work| work.dim.to_string());
        let residual = work.map_or_else(
            || "none".to_string(),
            |work| work.residual_coordinates.to_string(),
        );
        let iterations =
            work.map_or_else(|| "none".to_string(), |work| work.iterations.to_string());
        let relative = work
            .and_then(|work| work.relative_residual)
            .map_or_else(|| "none".to_string(), |value| format!("{value:.3e}"));
        eprintln!(
            "fit returned certificate kind {kind} scope fullnative lower {lower} upper {upper} complete {complete} dim {dim} residual_coords {residual} iterations {iterations} relative {relative} termination {termination}"
        );
    }

    if let Some(state) = &mut resume_state {
        state.trust_radius = trust_radius;
    }

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
    /// The complete upper-bound certificate is at or below the stationarity threshold and
    /// the candidate does not increase the objective.
    Stationary(StationarityReport),
    /// The conditional `log sigma` moved, so the linearization is stale.
    SigmaRefresh,
    /// The stationary certificate could not resolve.
    Unresolved(StationarityReport),
    Budget,
    Stalled,
}

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
    radius: &mut Option<crate::TrustRadius>,
    gradient_scale: &mut Option<f64>,
    evaluations: &mut usize,
    sample_evaluations: &mut SampleEvaluations,
    samples_per_pass: usize,
    stage_index: usize,
    sweep_index: usize,
    history: usize,
    scratch: &mut FinalScratch,
    cert_warm: &mut Option<Vec<f64>>,
    mask: &ResidualMask,
    residual_active: bool,
) -> FinalOutcome {
    let model = match JointModel::new(
        layout, problem, params, options, blocks, penalties, lambdas, roles, pass, data_scale,
        false, false, mask,
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
    analytic_attempts(
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
        cost,
        radius,
        gradient_scale,
        evaluations,
        sample_evaluations,
        samples_per_pass,
        stage_index,
        sweep_index,
        history,
        scratch,
        cert_warm,
        mask,
        residual_active,
    )
}

fn stationary_outcome(
    problem: &Problem,
    params: &mut Params,
    pass: &PassOutput,
    options: &FitOptions,
    model: &JointModel<'_>,
    cert_warm: &mut Option<Vec<f64>>,
) -> Option<FinalOutcome> {
    let report = certified_stationary(model, cert_warm)?;
    Some(
        match conditional_sigma_state(problem, params, pass, options) {
            SigmaState::Settled => FinalOutcome::Stationary(report),
            SigmaState::Refresh => FinalOutcome::SigmaRefresh,
            SigmaState::Unresolved(termination) => FinalOutcome::Unresolved(StationarityReport {
                origin: CertificateOrigin::FinalStage,
                certificate: StationarityCertificate::Unresolved { termination },
                work: None,
                bounds: None,
            }),
        },
    )
}

fn shrink_trust_radius(radius: &mut Option<crate::TrustRadius>) -> bool {
    let Some(next) = radius.and_then(|current| current.shrink(0.25)) else {
        return false;
    };
    *radius = Some(next);
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppliedStep {
    TrustRegion,
    Cauchy,
    Box,
}

impl AppliedStep {
    fn label(self) -> &'static str {
        match self {
            Self::TrustRegion => "tr",
            Self::Cauchy => "cauchy",
            Self::Box => "box",
        }
    }
}

/// The trust-region or Cauchy candidate that wins the ordinary predicted-decrease ranking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OrdinaryStep {
    TrustRegion,
    Cauchy,
}

impl From<OrdinaryStep> for AppliedStep {
    fn from(step: OrdinaryStep) -> Self {
        match step {
            OrdinaryStep::TrustRegion => Self::TrustRegion,
            OrdinaryStep::Cauchy => Self::Cauchy,
        }
    }
}

/// The box alternative replaces the ordinary winner only when it ranks first on predicted
/// decrease and its realized decrease is strictly larger.
fn guarded_selection(
    ordinary: OrdinaryStep,
    box_ranked_first: bool,
    ordinary_decrease: f64,
    box_decrease: f64,
) -> AppliedStep {
    if box_ranked_first && box_decrease > ordinary_decrease {
        AppliedStep::Box
    } else {
        ordinary.into()
    }
}

#[allow(clippy::too_many_arguments)]
fn analytic_attempts(
    model: &JointModel<'_>,
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
    cost: f64,
    radius: &mut Option<crate::TrustRadius>,
    gradient_scale: &mut Option<f64>,
    evaluations: &mut usize,
    sample_evaluations: &mut SampleEvaluations,
    samples_per_pass: usize,
    stage_index: usize,
    sweep_index: usize,
    history: usize,
    scratch: &mut FinalScratch,
    cert_warm: &mut Option<Vec<f64>>,
    mask: &ResidualMask,
    residual_active: bool,
) -> FinalOutcome {
    use crate::trust_region::{TrustRegionError, TrustRegionTermination};

    let full = match AnalyticModel::new(model, pass, params, options, model.residual_data_scale) {
        Ok(full) => full,
        Err(failure) => {
            if fit_diag() {
                eprintln!(
                    "fit analytic model failed ({}) {failure:?}",
                    failure.label()
                );
            }
            return FinalOutcome::Stalled;
        }
    };
    if let Err(failure) = full.prepare(scratch) {
        if fit_diag() {
            eprintln!(
                "fit analytic workspace failed ({}) {failure:?}",
                failure.label()
            );
        }
        return FinalOutcome::Stalled;
    }
    let norm = model
        .rhs
        .iter()
        .fold(0.0, |norm, value| libm::hypot(norm, *value));
    if norm == 0.0 {
        return stationary_outcome(problem, params, pass, options, model, cert_warm)
            .unwrap_or(FinalOutcome::Stalled);
    }
    let reference = *gradient_scale.get_or_insert(norm.max(1e-300));
    let tolerance = (0.02 * norm / reference).clamp(1e-3, 0.02);
    if radius.is_none() {
        let initial = scratch
            .gradient
            .iter()
            .zip(&scratch.diagonal)
            .fold(0.0, |norm, (gradient, diagonal)| {
                libm::hypot(norm, gradient / diagonal.sqrt())
            });
        *radius = crate::TrustRadius::checked(initial.max(1.0));
    }
    let preconditioner = match JointSolve::new(model, 0.0) {
        Ok(preconditioner) => preconditioner,
        Err(failure) => {
            if fit_diag() {
                eprintln!("fit analytic preconditioner failed ({})", failure.label());
            }
            return FinalOutcome::Stalled;
        }
    };
    let FinalScratch {
        hessian,
        solver,
        gradient,
        diagonal,
        effective,
        product,
        cauchy_effective,
        cauchy_product,
        box_effective,
        box_product,
        reduced,
        reduced_out,
        trial,
        cauchy_trial,
        box_trial,
    } = scratch;
    if let Some(trial) = trial.as_mut() {
        trial.theta.copy_from_slice(&params.theta);
    }
    if let Some(trial) = cauchy_trial.as_mut() {
        trial.theta.copy_from_slice(&params.theta);
    }
    if let Some(trial) = box_trial.as_mut() {
        trial.theta.copy_from_slice(&params.theta);
    }
    let trial_params = trial.get_or_insert_with(|| params.clone());
    let cauchy_params = cauchy_trial.get_or_insert_with(|| params.clone());
    let box_params = box_trial.get_or_insert_with(|| params.clone());
    let cauchy_ray = std::cell::OnceCell::new();
    for attempt in 1..=12 {
        let Some(current_radius) = *radius else {
            return FinalOutcome::Stalled;
        };
        if sample_evaluations.get() as f64 + samples_per_pass as f64 > options.sweep_budget {
            return FinalOutcome::Budget;
        }
        // Reserve the Cauchy gradient product, both realized applied-step products and the box product.
        let outcome = crate::trust_region::solve(
            gradient,
            diagonal,
            current_radius,
            JOINT_CG_LIMIT.saturating_sub(4),
            tolerance,
            |direction, out| full.apply(direction, out, hessian),
            |residual, out| {
                preconditioner.precondition_full(model, residual, out, reduced, reduced_out)
            },
            solver,
        );
        let solution = match outcome {
            Ok(solution) => solution,
            Err(failure) => {
                if fit_diag() {
                    eprintln!(
                        "fit stage {stage_index} sweep {sweep_index} analytic solve failed {failure:?} radius {:.3e} attempt {attempt}",
                        current_radius.get()
                    );
                }
                if !matches!(
                    failure,
                    TrustRegionError::WorkLimit { .. } | TrustRegionError::Breakdown
                ) || !shrink_trust_radius(radius)
                {
                    return FinalOutcome::Stalled;
                }
                continue;
            }
        };
        if solution.termination == TrustRegionTermination::ZeroGradient {
            return stationary_outcome(problem, params, pass, options, model, cert_warm)
                .unwrap_or(FinalOutcome::Stalled);
        }
        let largest = solution
            .step
            .iter()
            .fold(0.0_f64, |largest, value| largest.max(value.abs()));
        let scale = if largest > STEP_RADIUS {
            STEP_RADIUS / largest
        } else {
            1.0
        };
        for (index, value) in solution.step.iter().enumerate() {
            trial_params.theta[model.globals[index]] =
                params.theta[model.globals[index]] + scale * value;
        }
        if residual_active {
            mask.pin(&mut trial_params.theta[layout.residual.clone()]);
        }
        for (index, global) in model.globals.iter().enumerate() {
            effective[index] = trial_params.theta[*global] - params.theta[*global];
        }
        let tr_zero = effective.iter().all(|value| *value == 0.0);
        let tr_eligible = solution.termination != TrustRegionTermination::Stationary
            || solution.relative_residual <= tolerance;
        let mut products = 0usize;
        let tr_prediction = if tr_zero || !tr_eligible {
            None
        } else {
            if let Err(failure) = full.apply(effective, product, hessian) {
                if fit_diag() {
                    eprintln!(
                        "fit analytic applied-step curvature failed ({}) {failure:?}",
                        failure.label()
                    );
                }
                return FinalOutcome::Stalled;
            }
            products += 1;
            match crate::trust_region::predicted_decrease(gradient, effective, product) {
                Some(value) if value > 0.0 => Some(value),
                Some(_) => None,
                None => return FinalOutcome::Stalled,
            }
        };
        let ray = cauchy_ray.get_or_init(|| {
            products += 1;
            full.apply(gradient, cauchy_product, hessian).map(|()| {
                crate::trust_region::CauchyRay::new(gradient, diagonal, STEP_RADIUS, cauchy_product)
            })
        });
        let ray = match ray {
            Ok(ray) => *ray,
            Err(failure) => {
                if fit_diag() {
                    eprintln!(
                        "fit analytic Cauchy gradient curvature failed ({}) {failure:?}",
                        failure.label()
                    );
                }
                return FinalOutcome::Stalled;
            }
        };
        let mut cauchy_metric_norm = None;
        let cauchy_prediction = if let Some(alpha) =
            ray.and_then(|ray| ray.step_length(current_radius))
        {
            for (index, value) in gradient.iter().enumerate() {
                cauchy_params.theta[model.globals[index]] =
                    params.theta[model.globals[index]] - alpha * value;
            }
            if residual_active {
                mask.pin(&mut cauchy_params.theta[layout.residual.clone()]);
            }
            for (index, global) in model.globals.iter().enumerate() {
                cauchy_effective[index] = cauchy_params.theta[*global] - params.theta[*global];
            }
            if cauchy_effective.iter().all(|value| *value == 0.0) {
                None
            } else {
                let metric_norm = cauchy_effective
                    .iter()
                    .zip(diagonal.iter())
                    .fold(0.0, |norm, (step, diagonal)| {
                        libm::hypot(norm, step * diagonal.sqrt())
                    });
                let largest = cauchy_effective
                    .iter()
                    .fold(0.0_f64, |largest, value| largest.max(value.abs()));
                let slack = 1.0 + 8.0 * f64::EPSILON;
                if metric_norm.is_finite()
                    && metric_norm <= current_radius.get() * slack
                    && largest.is_finite()
                    && largest <= STEP_RADIUS * slack
                {
                    cauchy_metric_norm = Some(metric_norm);
                    if let Err(failure) = full.apply(cauchy_effective, cauchy_product, hessian) {
                        if fit_diag() {
                            eprintln!(
                                "fit analytic Cauchy applied-step curvature failed ({}) {failure:?}",
                                failure.label()
                            );
                        }
                        return FinalOutcome::Stalled;
                    }
                    products += 1;
                    match crate::trust_region::predicted_decrease(
                        gradient,
                        cauchy_effective,
                        cauchy_product,
                    ) {
                        Some(value) if value > 0.0 => Some(value),
                        _ => None,
                    }
                } else {
                    None
                }
            }
        } else {
            None
        };
        let cauchy_wins = match (tr_prediction, cauchy_prediction) {
            (Some(tr), Some(cauchy)) => cauchy > tr,
            (None, Some(_)) => true,
            _ => false,
        };
        let ordinary = if cauchy_wins {
            Some(OrdinaryStep::Cauchy)
        } else if tr_prediction.is_some() {
            Some(OrdinaryStep::TrustRegion)
        } else {
            None
        };
        let Some(ordinary) = ordinary else {
            if tr_zero
                && solution.predicted_decrease.is_finite()
                && solution.predicted_decrease > 0.0
                && tr_eligible
            {
                return stationary_outcome(problem, params, pass, options, model, cert_warm)
                    .unwrap_or(FinalOutcome::Stalled);
            }
            if !shrink_trust_radius(radius) {
                return FinalOutcome::Stalled;
            }
            continue;
        };
        let mut box_prediction = None;
        let mut box_metric_norm = None;
        for (index, value) in solution.step.iter().enumerate() {
            box_params.theta[model.globals[index]] =
                params.theta[model.globals[index]] + (*value).clamp(-STEP_RADIUS, STEP_RADIUS);
        }
        if residual_active {
            mask.pin(&mut box_params.theta[layout.residual.clone()]);
        }
        for (index, global) in model.globals.iter().enumerate() {
            box_effective[index] = box_params.theta[*global] - params.theta[*global];
        }
        if box_effective[..] != effective[..] {
            let metric_norm = box_effective
                .iter()
                .zip(diagonal.iter())
                .fold(0.0, |norm, (step, diagonal)| {
                    libm::hypot(norm, step * diagonal.sqrt())
                });
            let largest = box_effective
                .iter()
                .fold(0.0_f64, |largest, value| largest.max(value.abs()));
            let slack = 1.0 + 8.0 * f64::EPSILON;
            if metric_norm.is_finite()
                && metric_norm <= current_radius.get() * slack
                && largest.is_finite()
                && largest <= STEP_RADIUS * slack
            {
                products += 1;
                match full.apply(box_effective, box_product, hessian) {
                    Ok(()) => {
                        if let Some(value) = crate::trust_region::predicted_decrease(
                            gradient,
                            box_effective,
                            box_product,
                        ) && value > 0.0
                        {
                            box_prediction = Some(value);
                            box_metric_norm = Some(metric_norm);
                        }
                    }
                    Err(failure) => {
                        if fit_diag() {
                            eprintln!(
                                "fit analytic box applied-step curvature failed ({}) {failure:?}",
                                failure.label()
                            );
                        }
                    }
                }
            }
        }
        let ordinary_prediction = match ordinary {
            OrdinaryStep::TrustRegion => tr_prediction,
            OrdinaryStep::Cauchy => cauchy_prediction,
        };
        let box_ranked_first = box_prediction
            .zip(ordinary_prediction)
            .is_some_and(|(boxed, ordinary)| boxed > ordinary);
        let mut evaluate = |trial_params: &Params| {
            let candidate = stage_problem.pass(
                trial_params,
                stage.active,
                &mut [],
                false,
                false,
                stage.neutral,
                IntentFilter::NativeOnly,
            );
            *evaluations += 1;
            *sample_evaluations += samples_per_pass;
            let trial_cost = candidate.cost
                + total_cost(problem, trial_params, blocks, penalties, lambdas, options);
            (candidate, trial_cost)
        };
        let (mut candidate, mut trial_cost) = evaluate(match ordinary {
            OrdinaryStep::TrustRegion => &*trial_params,
            OrdinaryStep::Cauchy => &*cauchy_params,
        });
        let selected = if box_ranked_first {
            let (box_candidate, box_cost) = evaluate(box_params);
            let applied = guarded_selection(
                ordinary,
                box_ranked_first,
                cost - trial_cost,
                cost - box_cost,
            );
            if applied == AppliedStep::Box {
                candidate = box_candidate;
                trial_cost = box_cost;
            }
            applied
        } else {
            ordinary.into()
        };
        let (selected_params, selected_step, selected_product, prediction) = match selected {
            AppliedStep::TrustRegion => (
                &mut *trial_params,
                &effective[..],
                &product[..],
                tr_prediction.unwrap_or(f64::NAN),
            ),
            AppliedStep::Cauchy => (
                &mut *cauchy_params,
                &cauchy_effective[..],
                &cauchy_product[..],
                cauchy_prediction.unwrap_or(f64::NAN),
            ),
            AppliedStep::Box => (
                &mut *box_params,
                &box_effective[..],
                &box_product[..],
                box_prediction.unwrap_or(f64::NAN),
            ),
        };
        let decrease = cost - trial_cost;
        let ratio = decrease / prediction;
        if !candidate.infeasible
            && trial_cost.is_finite()
            && decrease > 1e-12 * cost.abs().max(1.0)
            && ratio >= 1e-4
        {
            let metric_norm = match selected {
                AppliedStep::TrustRegion => effective
                    .iter()
                    .zip(diagonal.iter())
                    .fold(0.0, |norm, (step, diagonal)| {
                        libm::hypot(norm, step * diagonal.sqrt())
                    }),
                AppliedStep::Cauchy => cauchy_metric_norm.unwrap_or(f64::NAN),
                AppliedStep::Box => box_metric_norm.unwrap_or(f64::NAN),
            };
            let next = if ratio < 0.25 {
                current_radius.shrink(0.25).unwrap_or(current_radius)
            } else if ratio > 0.75 && metric_norm >= 0.8 * current_radius.get() {
                current_radius.expand(2.0).unwrap_or(current_radius)
            } else {
                current_radius
            };
            *radius = Some(next);
            if fit_diag() {
                let residual_norm = gradient
                    .iter()
                    .zip(selected_product.iter())
                    .fold(0.0, |norm, (gradient, product)| {
                        libm::hypot(norm, gradient + product)
                    });
                let relative = residual_norm / norm;
                let (worst, step_max) = selected_step.iter().enumerate().fold(
                    (0, 0.0_f64),
                    |(worst, largest), (index, value)| {
                        if value.abs() > largest {
                            (index, value.abs())
                        } else {
                            (worst, largest)
                        }
                    },
                );
                let termination = match solution.termination {
                    TrustRegionTermination::Stationary => "interior",
                    TrustRegionTermination::Boundary => "boundary",
                    TrustRegionTermination::NegativeCurvature => "negative",
                    TrustRegionTermination::ZeroGradient => "zero",
                };
                eprintln!(
                    "fit stage {stage_index} sweep {sweep_index} accepted trial {trial_cost:.17e} ratio {ratio:.17e} prediction {prediction:.17e} step {step_max:.17e} at {worst} radius {:.17e} next_radius {:.17e} iterations {} residual {relative:.17e} tolerance {tolerance:.17e} stalled 0 failure none dim {} residual_coords {} history {history} solver analytic-tr termination {termination} proposal_residual {:.17e} scale {scale:.17e} selected {} gradient_reference {reference:.17e}",
                    current_radius.get(),
                    next.get(),
                    solution.iterations + products,
                    model.dim,
                    model.residual_count,
                    solution.relative_residual,
                    selected.label()
                );
            }
            core::mem::swap(params, selected_params);
            return FinalOutcome::Accepted(Box::new(candidate));
        }
        if !candidate.infeasible
            && trial_cost.is_finite()
            && accepted(cost, trial_cost)
            && let Some(outcome) =
                stationary_outcome(problem, params, pass, options, model, cert_warm)
        {
            return outcome;
        }
        if fit_diag() {
            eprintln!(
                "fit stage {stage_index} sweep {sweep_index} rejected trial {trial_cost:.17e} current {cost:.17e} ratio {ratio:.17e} prediction {prediction:.17e} radius {:.17e} attempt {attempt} selected {} gradient_reference {reference:.17e}",
                current_radius.get(),
                selected.label()
            );
        }
        if !shrink_trust_radius(radius) {
            return FinalOutcome::Stalled;
        }
    }
    FinalOutcome::Stalled
}

/// Absolute coordinates and weights copied from a captured state, for diagnostic use only.
/// The recorded generation is never resumed.
#[cfg(feature = "synth")]
#[derive(Clone, Debug)]
pub struct FixedLinearizationInput {
    pub theta: Vec<f64>,
    pub lambdas: StageLambdas,
    /// Conditional `log sigma` per contributor; `None` uses the coordinates in `theta`.
    pub sigma_log: Option<Vec<f64>>,
    pub trust_radius: Option<crate::TrustRadius>,
    /// Per-stage gradient reference used by the production tolerance; `None` resets it to the
    /// fresh norm and labels the report a first-resume attempt.
    pub gradient_reference: Option<f64>,
    pub checkpoint_generation: u32,
    pub checkpoint_stage: usize,
    pub checkpoint_sweep: usize,
}

#[cfg(feature = "synth")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixedLinearizationStatus {
    Compared,
    /// The trust-region solve hit its work limit. The finite CG iterate is reported for
    /// diagnosis only; production still returns `WorkLimit` and discards it.
    WorkLimitPartial,
    InvalidObjective,
    MissingPrediction,
    PredictionCurvature,
    InvalidMetric,
    Nonfinite,
    JointModelFailed,
    PreconditionerFailed,
    TrustSolveFailed,
    ZeroGradient,
}

#[cfg(feature = "synth")]
impl FixedLinearizationStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Compared => "compared",
            Self::WorkLimitPartial => "work-limit-partial",
            Self::InvalidObjective => "invalid-objective",
            Self::MissingPrediction => "missing-prediction",
            Self::PredictionCurvature => "prediction-curvature",
            Self::InvalidMetric => "invalid-metric",
            Self::Nonfinite => "nonfinite",
            Self::JointModelFailed => "joint-model-failed",
            Self::PreconditionerFailed => "preconditioner-failed",
            Self::TrustSolveFailed => "trust-solve-failed",
            Self::ZeroGradient => "zero-gradient",
        }
    }

    /// Whether the report completed the candidate comparison.
    pub fn completed(self) -> bool {
        matches!(self, Self::Compared | Self::WorkLimitPartial)
    }
}

/// The production TR/Cauchy predicted-decrease winner, before the box candidate.
#[cfg(feature = "synth")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixedLinearizationOrdinary {
    TrustRegion,
    Cauchy,
    None,
}

#[cfg(feature = "synth")]
impl FixedLinearizationOrdinary {
    pub fn label(self) -> &'static str {
        match self {
            Self::TrustRegion => "tr",
            Self::Cauchy => "cauchy",
            Self::None => "none",
        }
    }

    fn winner(self) -> FixedLinearizationWinner {
        match self {
            Self::TrustRegion => FixedLinearizationWinner::TrustRegion,
            Self::Cauchy => FixedLinearizationWinner::Cauchy,
            Self::None => FixedLinearizationWinner::None,
        }
    }
}

/// The candidate production applies after the box guard.
#[cfg(feature = "synth")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixedLinearizationWinner {
    TrustRegion,
    Cauchy,
    Box,
    None,
}

#[cfg(feature = "synth")]
impl FixedLinearizationWinner {
    pub fn label(self) -> &'static str {
        match self {
            Self::TrustRegion => "tr",
            Self::Cauchy => "cauchy",
            Self::Box => "box",
            Self::None => "none",
        }
    }
}

/// Whether the tolerance reference came from the checkpoint or was reset for a first resume.
#[cfg(feature = "synth")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixedLinearizationReference {
    Checkpoint,
    FirstResume,
}

#[cfg(feature = "synth")]
impl FixedLinearizationReference {
    pub fn label(self) -> &'static str {
        match self {
            Self::Checkpoint => "checkpoint",
            Self::FirstResume => "first-resume",
        }
    }
}

#[cfg(feature = "synth")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixedLinearizationBoxSource {
    TrustRegionProposal,
    WorkLimitPartial,
}

#[cfg(feature = "synth")]
impl FixedLinearizationBoxSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::TrustRegionProposal => "tr-proposal",
            Self::WorkLimitPartial => "work-limit-partial",
        }
    }
}

/// What production would do with the applied candidate.
#[cfg(feature = "synth")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixedLinearizationDisposition {
    Accepted,
    NotAccepted,
    /// Production returned `WorkLimit`; ranking and acceptance are diagnostic only.
    WorkLimitErr,
}

#[cfg(feature = "synth")]
impl FixedLinearizationDisposition {
    pub fn label(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::NotAccepted => "not-accepted",
            Self::WorkLimitErr => "work-limit-err",
        }
    }
}

#[cfg(feature = "synth")]
#[derive(Clone, Copy, Debug, Default)]
pub struct FixedLinearizationCandidate {
    /// Whether the production path considered this candidate at all.
    pub available: bool,
    /// Raw `q(0) - q(d)` from a fresh full-operator product; may be nonpositive.
    pub prediction: Option<f64>,
    pub metric_norm: Option<f64>,
    pub largest: Option<f64>,
    pub coordinate_feasible: bool,
    pub radius_feasible: bool,
}

#[cfg(feature = "synth")]
impl FixedLinearizationCandidate {
    pub fn descent(&self) -> bool {
        self.prediction.is_some_and(|value| value > 0.0)
    }

    pub fn feasible(&self) -> bool {
        self.available && self.coordinate_feasible && self.radius_feasible
    }
}

/// The finite CG iterate left in scratch when the trust-region solve hits `WorkLimit`.
#[cfg(feature = "synth")]
#[derive(Clone, Copy, Debug, Default)]
pub struct FixedLinearizationPartial {
    /// `q(0) - q(d)` accumulated by the CG recurrences; no post-solve product.
    pub recursive_prediction: Option<f64>,
    pub metric_norm: f64,
    pub largest: f64,
    pub coordinate_feasible: bool,
    pub radius_feasible: bool,
}

/// One objective trial at fixed coordinates, with the production acceptance decision.
#[cfg(feature = "synth")]
#[derive(Clone, Copy, Debug, Default)]
pub struct FixedLinearizationTrial {
    pub cost: f64,
    pub decrease: f64,
    pub prediction: f64,
    pub ratio: f64,
    pub trial_infeasible: bool,
    pub accepted: bool,
}

#[cfg(feature = "synth")]
#[derive(Clone, Copy, Debug, Default)]
pub struct FixedLinearizationOperations {
    pub linearization_passes: usize,
    pub objective_passes: usize,
    pub sample_evaluations: u64,
    pub inner_products: usize,
    pub post_products: usize,
    pub total_products: usize,
}

#[cfg(feature = "synth")]
#[derive(Clone, Debug)]
pub struct FixedLinearizationReport {
    pub status: FixedLinearizationStatus,
    pub disposition: FixedLinearizationDisposition,
    pub note: String,
    pub checkpoint_generation: u32,
    pub checkpoint_stage: usize,
    pub checkpoint_sweep: usize,
    pub images: usize,
    pub samples: usize,
    pub excluded: usize,
    pub excluded_samples: usize,
    pub coordinates: usize,
    pub joint_dim: usize,
    pub residual_coordinates: usize,
    pub sigma_log: Vec<f64>,
    pub lambdas: StageLambdas,
    pub cost: f64,
    pub infeasible: bool,
    pub hessian_rows: usize,
    pub active_samples: usize,
    pub radius: f64,
    pub gradient_norm: f64,
    pub gradient_reference: f64,
    pub reference_source: FixedLinearizationReference,
    pub tolerance: f64,
    pub tr_termination: Option<&'static str>,
    pub tr_iterations: usize,
    pub tr_relative_residual: f64,
    pub tr_scale: f64,
    pub tr_limiter: Option<&'static str>,
    pub tr: FixedLinearizationCandidate,
    /// Present only when the solve hit `WorkLimit`.
    pub partial: Option<FixedLinearizationPartial>,
    pub cauchy_alpha: f64,
    pub cauchy_limiter: Option<&'static str>,
    pub cauchy: FixedLinearizationCandidate,
    pub ordinary: FixedLinearizationOrdinary,
    pub ordinary_candidate: FixedLinearizationCandidate,
    pub ordinary_trial: Option<FixedLinearizationTrial>,
    pub box_source: FixedLinearizationBoxSource,
    pub box_clipped: usize,
    pub box_candidate: FixedLinearizationCandidate,
    pub box_trial: Option<FixedLinearizationTrial>,
    pub winner: FixedLinearizationWinner,
    pub winner_candidate: FixedLinearizationCandidate,
    pub box_actual_improvement: f64,
    pub operations: FixedLinearizationOperations,
    pub trace: crate::TrustRegionTrace,
}

#[cfg(feature = "synth")]
#[derive(Clone, Debug, Default)]
struct FixedLinearizationBaseFields {
    images: usize,
    samples: usize,
    excluded: usize,
    excluded_samples: usize,
    coordinates: usize,
    sigma_log: Vec<f64>,
    cost: f64,
    infeasible: bool,
    hessian_rows: usize,
    active_samples: usize,
}

#[cfg(feature = "synth")]
struct FixedLinearizationBase {
    problem: Problem,
    layout: Layout,
    params: Params,
    pass: PassOutput,
    blocks: Vec<BlockAcc>,
    penalties: Vec<Vec<f64>>,
    mask: ResidualMask,
    roles: Vec<Role>,
    residual_active: bool,
    data_scale: f64,
    fields: FixedLinearizationBaseFields,
}

#[cfg(feature = "synth")]
enum FixedLinearizationSetupError {
    Fit(FitError),
    Reported(
        FixedLinearizationStatus,
        String,
        FixedLinearizationBaseFields,
    ),
}

/// One linearization shared by the fixed-step diagnostics.
#[cfg(feature = "synth")]
fn fixed_linearization_base(
    images: &[FitImage],
    schema: &ControlSchema,
    epoch: u32,
    base_sim: &looklab_schema::SimKey,
    options: &FitOptions,
    input: &FixedLinearizationInput,
) -> core::result::Result<FixedLinearizationBase, FixedLinearizationSetupError> {
    use crate::problem::HessianRows;

    let model = ControlResponseModel::zeroed(schema, epoch, base_sim, 1)
        .map_err(|error| FixedLinearizationSetupError::Fit(error.into()))?;
    let (kept, excluded) =
        filter_spatial(images, schema, epoch).map_err(FixedLinearizationSetupError::Fit)?;
    let native: Vec<FitImage> = kept
        .iter()
        .filter(|image| image.intent == ImageIntent::Native)
        .cloned()
        .collect();
    if native.is_empty() {
        return Err(FixedLinearizationSetupError::Fit(FitError::NoNativeImages));
    }
    let problem = Problem::new_with(&native, Some(&model), options)
        .map_err(FixedLinearizationSetupError::Fit)?;
    let layout = problem.layout.clone();
    if input.theta.len() != layout.total {
        return Err(FixedLinearizationSetupError::Fit(FitError::RowContext(
            format!(
                "fixed-linearization theta has {} entries, layout has {}",
                input.theta.len(),
                layout.total
            ),
        )));
    }
    let mut params = initial_params(&problem, options);
    params.theta.copy_from_slice(&input.theta);
    let mut fields = FixedLinearizationBaseFields {
        images: problem.images.len(),
        samples: problem.samples.len(),
        excluded: excluded.len(),
        excluded_samples: problem.excluded_samples,
        coordinates: layout.total,
        sigma_log: (0..problem.contributors.len())
            .map(|index| params.theta[layout.sigma_of(index)])
            .collect(),
        cost: f64::NAN,
        infeasible: false,
        hessian_rows: 0,
        active_samples: 0,
    };
    if let Some(sigma) = &input.sigma_log {
        if sigma.len() != fields.sigma_log.len() {
            return Err(FixedLinearizationSetupError::Fit(FitError::RowContext(
                format!(
                    "fixed-linearization sigma has {} entries, model has {}",
                    sigma.len(),
                    fields.sigma_log.len()
                ),
            )));
        }
        if let Some((index, (given, actual))) = sigma
            .iter()
            .zip(&fields.sigma_log)
            .enumerate()
            .find(|(_, (given, actual))| given.to_bits() != actual.to_bits())
        {
            return Err(FixedLinearizationSetupError::Fit(FitError::RowContext(
                format!(
                    "fixed-linearization sigma {index} is {given:.17e}, theta has {actual:.17e}"
                ),
            )));
        }
    }

    let stage = &STAGES[4];
    let (kinds, mut blocks, penalties) = stage_blocks(stage, &problem, options);
    let pass = problem.pass_with_analytic_rows(
        &params,
        stage.active,
        &mut blocks,
        false,
        false,
        stage.neutral,
        IntentFilter::NativeOnly,
        HessianRows::All,
    );
    fields.infeasible = pass.infeasible;
    fields.hessian_rows = pass.hessian_rows;
    fields.active_samples = pass.active_samples;
    fields.cost = pass.cost;
    if pass.infeasible || !pass.cost.is_finite() {
        return Err(FixedLinearizationSetupError::Reported(
            FixedLinearizationStatus::InvalidObjective,
            format!(
                "linearization pass infeasible={} cost={:.17e}",
                pass.infeasible, pass.cost
            ),
            fields,
        ));
    }
    let cost = pass.cost
        + total_cost(
            &problem,
            &params,
            &blocks,
            &penalties,
            &input.lambdas,
            options,
        );
    fields.cost = cost;
    if !cost.is_finite() {
        return Err(FixedLinearizationSetupError::Reported(
            FixedLinearizationStatus::Nonfinite,
            format!("linearization total cost is {cost:.17e}"),
            fields,
        ));
    }
    let data_scale = pass.active_samples.max(1) as f64 / pass.hessian_rows.max(1) as f64;
    let residual_active = kinds.contains(&BlockKind::Residual);
    let mask = if residual_active {
        ResidualMask::from_samples(&pass.residual_samples)
    } else {
        ResidualMask::empty()
    };
    let roles = prior_roles(&problem, &layout);
    Ok(FixedLinearizationBase {
        problem,
        layout,
        params,
        pass,
        blocks,
        penalties,
        mask,
        roles,
        residual_active,
        data_scale,
        fields,
    })
}

#[cfg(feature = "synth")]
fn fixed_linearization_report_fields(
    report: &mut FixedLinearizationReport,
    fields: FixedLinearizationBaseFields,
) {
    report.images = fields.images;
    report.samples = fields.samples;
    report.excluded = fields.excluded;
    report.excluded_samples = fields.excluded_samples;
    report.coordinates = fields.coordinates;
    report.sigma_log = fields.sigma_log;
    report.cost = fields.cost;
    report.infeasible = fields.infeasible;
    report.hessian_rows = fields.hessian_rows;
    report.active_samples = fields.active_samples;
    report.operations.linearization_passes = 1;
    report.operations.sample_evaluations = fields.samples as u64;
}

#[cfg(feature = "synth")]
fn fixed_linearization_failure(failure: AnalyticFailure) -> FixedLinearizationStatus {
    match failure {
        AnalyticFailure::InvalidObjective => FixedLinearizationStatus::InvalidObjective,
        AnalyticFailure::MissingPrediction => FixedLinearizationStatus::MissingPrediction,
        AnalyticFailure::Prediction(_) => FixedLinearizationStatus::PredictionCurvature,
        AnalyticFailure::InvalidMetric => FixedLinearizationStatus::InvalidMetric,
        AnalyticFailure::Nonfinite => FixedLinearizationStatus::Nonfinite,
    }
}

#[cfg(feature = "synth")]
fn fixed_linearization_metric_norm(diagonal: &[f64], step: &[f64]) -> f64 {
    step.iter()
        .zip(diagonal)
        .fold(0.0, |norm, (step, diagonal)| {
            libm::hypot(norm, *step * diagonal.sqrt())
        })
}

#[cfg(feature = "synth")]
fn fixed_linearization_largest(step: &[f64]) -> f64 {
    step.iter()
        .fold(0.0_f64, |largest, value| largest.max((*value).abs()))
}

/// `q(0) - q(scale * step)` from the recursively maintained `H step`.
#[cfg(feature = "synth")]
fn fixed_linearization_scaled_prediction(
    gradient: &[f64],
    step: &[f64],
    hessian_step: &[f64],
    scale: f64,
) -> Option<f64> {
    if gradient.len() != step.len() || hessian_step.len() != step.len() {
        return None;
    }
    let linear: f64 = gradient
        .iter()
        .zip(step)
        .map(|(gradient, step)| gradient * step)
        .sum();
    let quadratic: f64 = step
        .iter()
        .zip(hessian_step)
        .map(|(step, hessian_step)| step * hessian_step)
        .sum();
    let value = -(scale * linear + 0.5 * scale * scale * quadratic);
    value.is_finite().then_some(value)
}

#[cfg(feature = "synth")]
fn fixed_linearization_termination(
    termination: crate::trust_region::TrustRegionTermination,
) -> &'static str {
    use crate::trust_region::TrustRegionTermination;
    match termination {
        TrustRegionTermination::Stationary => "stationary",
        TrustRegionTermination::Boundary => "boundary",
        TrustRegionTermination::NegativeCurvature => "negative-curvature",
        TrustRegionTermination::ZeroGradient => "zero-gradient",
    }
}

/// Compares the production TR/Cauchy applied step with one componentwise-box projection at
/// fixed captured coordinates. Does not advance the fit or re-optimize sigma or lambdas.
#[cfg(feature = "synth")]
#[allow(clippy::too_many_arguments)]
pub fn diagnose_fixed_linearization(
    images: &[FitImage],
    schema: &looklab_schema::ControlSchema,
    epoch: u32,
    base_sim: &looklab_schema::SimKey,
    options: &FitOptions,
    input: &FixedLinearizationInput,
) -> Result<FixedLinearizationReport> {
    use crate::trust_region::TrustRegionTermination;

    let mut report = FixedLinearizationReport {
        status: FixedLinearizationStatus::Compared,
        disposition: FixedLinearizationDisposition::NotAccepted,
        note: String::new(),
        checkpoint_generation: input.checkpoint_generation,
        checkpoint_stage: input.checkpoint_stage,
        checkpoint_sweep: input.checkpoint_sweep,
        images: 0,
        samples: 0,
        excluded: 0,
        excluded_samples: 0,
        coordinates: 0,
        joint_dim: 0,
        residual_coordinates: 0,
        sigma_log: Vec::new(),
        lambdas: input.lambdas,
        cost: f64::NAN,
        infeasible: false,
        hessian_rows: 0,
        active_samples: 0,
        radius: f64::NAN,
        gradient_norm: f64::NAN,
        gradient_reference: f64::NAN,
        reference_source: if input.gradient_reference.is_some() {
            FixedLinearizationReference::Checkpoint
        } else {
            FixedLinearizationReference::FirstResume
        },
        tolerance: f64::NAN,
        tr_termination: None,
        tr_iterations: 0,
        tr_relative_residual: f64::NAN,
        tr_scale: f64::NAN,
        tr_limiter: None,
        tr: FixedLinearizationCandidate::default(),
        partial: None,
        cauchy_alpha: f64::NAN,
        cauchy_limiter: None,
        cauchy: FixedLinearizationCandidate::default(),
        ordinary: FixedLinearizationOrdinary::None,
        ordinary_candidate: FixedLinearizationCandidate::default(),
        ordinary_trial: None,
        box_source: FixedLinearizationBoxSource::TrustRegionProposal,
        box_clipped: 0,
        box_candidate: FixedLinearizationCandidate::default(),
        box_trial: None,
        winner: FixedLinearizationWinner::None,
        winner_candidate: FixedLinearizationCandidate::default(),
        box_actual_improvement: f64::NAN,
        operations: FixedLinearizationOperations::default(),
        trace: crate::trust_region::TrustRegionTrace::default(),
    };
    let base = match fixed_linearization_base(images, schema, epoch, base_sim, options, input) {
        Ok(base) => base,
        Err(FixedLinearizationSetupError::Fit(error)) => return Err(error),
        Err(FixedLinearizationSetupError::Reported(status, note, fields)) => {
            report.status = status;
            report.note = note;
            fixed_linearization_report_fields(&mut report, fields);
            return Ok(report);
        }
    };
    let FixedLinearizationBase {
        problem,
        layout,
        params,
        pass,
        blocks,
        penalties,
        mask,
        roles,
        residual_active,
        data_scale,
        fields,
    } = base;
    fixed_linearization_report_fields(&mut report, fields);
    let cost = report.cost;
    let stage = &STAGES[4];
    let model = match JointModel::new(
        &layout,
        &problem,
        &params,
        options,
        &blocks,
        &penalties,
        &input.lambdas,
        &roles,
        &pass,
        data_scale,
        false,
        false,
        &mask,
    ) {
        Ok(model) => model,
        Err(failure) => {
            report.status = FixedLinearizationStatus::JointModelFailed;
            report.note = failure.label().to_string();
            return Ok(report);
        }
    };
    report.joint_dim = model.dim;
    report.residual_coordinates = model.residual_count;
    let full = match AnalyticModel::new(&model, &pass, &params, options, model.residual_data_scale)
    {
        Ok(full) => full,
        Err(failure) => {
            report.status = fixed_linearization_failure(failure);
            report.note = failure.label().to_string();
            return Ok(report);
        }
    };
    let mut scratch = FinalScratch::default();
    if let Err(failure) = full.prepare(&mut scratch) {
        report.status = fixed_linearization_failure(failure);
        report.note = failure.label().to_string();
        return Ok(report);
    }
    let norm = model
        .rhs
        .iter()
        .fold(0.0, |norm, value| libm::hypot(norm, *value));
    report.gradient_norm = norm;
    if norm == 0.0 {
        report.status = FixedLinearizationStatus::ZeroGradient;
        report.note = "model right-hand side is zero".to_string();
        return Ok(report);
    }
    let reference = input.gradient_reference.unwrap_or(norm.max(1e-300));
    if !(reference.is_finite() && reference > 0.0) {
        report.status = FixedLinearizationStatus::Nonfinite;
        report.note = format!("gradient reference {reference:.17e} is not positive finite");
        return Ok(report);
    }
    report.gradient_reference = reference;
    let tolerance = (0.02 * norm / reference).clamp(1e-3, 0.02);
    report.tolerance = tolerance;
    let radius = match input.trust_radius {
        Some(radius) => radius,
        None => {
            let initial = scratch
                .gradient
                .iter()
                .zip(&scratch.diagonal)
                .fold(0.0, |norm, (gradient, diagonal)| {
                    libm::hypot(norm, *gradient / diagonal.sqrt())
                });
            match crate::TrustRadius::checked(initial.max(1.0)) {
                Some(radius) => radius,
                None => {
                    report.status = FixedLinearizationStatus::Nonfinite;
                    report.note = format!("initial trust radius is {initial:.17e}");
                    return Ok(report);
                }
            }
        }
    };
    report.radius = radius.get();
    let preconditioner = match JointSolve::new(&model, 0.0) {
        Ok(preconditioner) => preconditioner,
        Err(failure) => {
            report.status = FixedLinearizationStatus::PreconditionerFailed;
            report.note = failure.label().to_string();
            return Ok(report);
        }
    };
    let FinalScratch {
        hessian,
        solver,
        gradient,
        diagonal,
        product,
        cauchy_product,
        reduced,
        reduced_out,
        ..
    } = &mut scratch;
    let mut post_products = 0usize;
    let mut trace = crate::trust_region::TrustRegionTrace::default();
    let outcome = crate::trust_region::solve_diagnostic(
        crate::trust_region::DiagnosticTrustRegionInput {
            gradient: &gradient[..],
            metric_diagonal: &diagonal[..],
            radius,
            work_limit: JOINT_CG_LIMIT.saturating_sub(4),
            relative_tolerance: tolerance,
        },
        |direction, out| full.apply(direction, out, hessian),
        |residual, out| {
            preconditioner.precondition_full(&model, residual, out, reduced, reduced_out)
        },
        solver,
        &mut trace,
    );
    report.operations.inner_products = trace.hessian_products;
    report.tr_iterations = trace.hessian_products;
    let (raw_proposal, partial_hessian, termination, tr_relative_residual) = match outcome {
        Ok(solution) => (
            solution.step.to_vec(),
            None,
            Some(solution.termination),
            solution.relative_residual,
        ),
        Err(crate::trust_region::TrustRegionError::WorkLimit { .. }) => {
            report.disposition = FixedLinearizationDisposition::WorkLimitErr;
            let step = solver.diagnostic_step().to_vec();
            let hessian_step = solver.diagnostic_hessian_step().to_vec();
            if step
                .iter()
                .chain(hessian_step.iter())
                .any(|value| !value.is_finite())
            {
                report.status = FixedLinearizationStatus::Nonfinite;
                report.note = "work-limit scratch iterate is not finite".to_string();
                report.operations.total_products = trace.hessian_products;
                report.trace = trace;
                return Ok(report);
            }
            (step, Some(hessian_step), None, f64::NAN)
        }
        Err(failure) => {
            report.status = FixedLinearizationStatus::TrustSolveFailed;
            report.note = format!("{failure:?}");
            report.operations.total_products = trace.hessian_products;
            report.trace = trace;
            return Ok(report);
        }
    };
    report.tr_relative_residual = tr_relative_residual;
    if let Some(termination) = termination {
        report.tr_termination = Some(fixed_linearization_termination(termination));
        if termination == TrustRegionTermination::ZeroGradient {
            report.status = FixedLinearizationStatus::ZeroGradient;
            report.note = "trust-region solve returned a zero gradient".to_string();
            report.operations.total_products = trace.hessian_products;
            report.trace = trace;
            return Ok(report);
        }
    }

    let realize = |delta: &[f64], trial: &mut Params| -> Vec<f64> {
        for (index, global) in model.globals.iter().enumerate() {
            trial.theta[*global] = params.theta[*global] + delta[index];
        }
        if residual_active {
            mask.pin(&mut trial.theta[layout.residual.clone()]);
        }
        model
            .globals
            .iter()
            .map(|global| trial.theta[*global] - params.theta[*global])
            .collect()
    };

    let slack = 1.0 + 8.0 * f64::EPSILON;
    let proposal_largest = fixed_linearization_largest(&raw_proposal);
    let tr_scale = if proposal_largest > STEP_RADIUS {
        STEP_RADIUS / proposal_largest
    } else {
        1.0
    };
    report.tr_scale = tr_scale;
    let tr_delta: Vec<f64> = raw_proposal.iter().map(|value| tr_scale * *value).collect();
    let mut tr_params = params.clone();
    let tr_effective = realize(&tr_delta, &mut tr_params);
    let tr_metric_norm = fixed_linearization_metric_norm(diagonal, &tr_effective);
    let tr_largest = fixed_linearization_largest(&tr_effective);
    let tr_zero = tr_effective.iter().all(|value| *value == 0.0);
    let tr_eligible = termination != Some(TrustRegionTermination::Stationary)
        || tr_relative_residual <= tolerance;
    let tr_coordinate_feasible = tr_largest.is_finite() && tr_largest <= STEP_RADIUS * slack;
    let tr_radius_feasible = tr_metric_norm.is_finite() && tr_metric_norm <= radius.get() * slack;
    let tr_prediction = if termination.is_none() || tr_zero || !tr_eligible {
        None
    } else {
        post_products += 1;
        match full.apply(&tr_effective, product, hessian) {
            Ok(()) => crate::trust_region::predicted_decrease(gradient, &tr_effective, product),
            Err(failure) => {
                report.status = fixed_linearization_failure(failure);
                report.note = failure.label().to_string();
                report.operations.post_products = post_products;
                report.operations.total_products = trace.hessian_products + post_products;
                report.trace = trace;
                return Ok(report);
            }
        }
    };
    match termination {
        Some(termination) => {
            report.tr = FixedLinearizationCandidate {
                available: !tr_zero && tr_eligible,
                prediction: tr_prediction,
                metric_norm: Some(tr_metric_norm),
                largest: Some(tr_largest),
                coordinate_feasible: tr_coordinate_feasible,
                radius_feasible: tr_radius_feasible,
            };
            report.tr_limiter = Some(if tr_zero {
                "zero-step"
            } else if !tr_eligible {
                "ineligible"
            } else if tr_scale < 1.0 {
                "coordinate-cap"
            } else {
                match termination {
                    TrustRegionTermination::Boundary => "metric-radius",
                    TrustRegionTermination::NegativeCurvature => "negative-curvature",
                    TrustRegionTermination::Stationary => "interior",
                    TrustRegionTermination::ZeroGradient => "zero-gradient",
                }
            });
        }
        None => {
            let recursive_prediction = partial_hessian.as_ref().and_then(|hessian_step| {
                if tr_effective == tr_delta {
                    fixed_linearization_scaled_prediction(
                        gradient,
                        &raw_proposal,
                        hessian_step,
                        tr_scale,
                    )
                } else {
                    None
                }
            });
            report.partial = (!tr_zero).then_some(FixedLinearizationPartial {
                recursive_prediction,
                metric_norm: tr_metric_norm,
                largest: tr_largest,
                coordinate_feasible: tr_coordinate_feasible,
                radius_feasible: tr_radius_feasible,
            });
            report.tr_termination = Some("work-limit");
            report.tr_limiter = Some("work-limit");
            report.status = FixedLinearizationStatus::WorkLimitPartial;
        }
    }

    let gradient_largest = fixed_linearization_largest(gradient);
    let metric_gradient_norm = fixed_linearization_metric_norm(diagonal, gradient);
    let gradient_squared: f64 = gradient.iter().map(|value| *value * *value).sum();
    post_products += 1;
    let cauchy_ray = match full.apply(gradient, cauchy_product, hessian) {
        Ok(()) => {
            crate::trust_region::CauchyRay::new(gradient, diagonal, STEP_RADIUS, cauchy_product)
        }
        Err(failure) => {
            report.status = fixed_linearization_failure(failure);
            report.note = failure.label().to_string();
            report.operations.post_products = post_products;
            report.operations.total_products = trace.hessian_products + post_products;
            report.trace = trace;
            return Ok(report);
        }
    };
    let cauchy_curvature = gradient
        .iter()
        .zip(cauchy_product.iter())
        .map(|(gradient, product)| *gradient * *product)
        .sum::<f64>();
    let cauchy_cap = if gradient_largest > 0.0 {
        STEP_RADIUS / gradient_largest
    } else {
        f64::INFINITY
    };
    let cauchy_maximum = if cauchy_curvature > 0.0 {
        cauchy_cap.min(gradient_squared / cauchy_curvature)
    } else {
        cauchy_cap
    };
    let mut cauchy_prediction = None;
    let mut cauchy_metric_norm = f64::NAN;
    let mut cauchy_largest = f64::NAN;
    let mut cauchy_coordinate_feasible = false;
    let mut cauchy_radius_feasible = false;
    let mut cauchy_available = false;
    let mut cauchy_trial = None;
    if let Some(alpha) = cauchy_ray.and_then(|ray| ray.step_length(radius)) {
        report.cauchy_alpha = alpha;
        let cauchy_delta: Vec<f64> = gradient.iter().map(|value| -alpha * *value).collect();
        let mut cauchy_params = params.clone();
        let cauchy_effective = realize(&cauchy_delta, &mut cauchy_params);
        cauchy_metric_norm = fixed_linearization_metric_norm(diagonal, &cauchy_effective);
        cauchy_largest = fixed_linearization_largest(&cauchy_effective);
        cauchy_coordinate_feasible =
            cauchy_largest.is_finite() && cauchy_largest <= STEP_RADIUS * slack;
        cauchy_radius_feasible =
            cauchy_metric_norm.is_finite() && cauchy_metric_norm <= radius.get() * slack;
        let cauchy_zero = cauchy_effective.iter().all(|value| *value == 0.0);
        cauchy_available = !cauchy_zero && cauchy_coordinate_feasible && cauchy_radius_feasible;
        if cauchy_available {
            post_products += 1;
            match full.apply(&cauchy_effective, cauchy_product, hessian) {
                Ok(()) => {
                    cauchy_prediction = crate::trust_region::predicted_decrease(
                        gradient,
                        &cauchy_effective,
                        cauchy_product,
                    );
                }
                Err(failure) => {
                    report.status = fixed_linearization_failure(failure);
                    report.note = failure.label().to_string();
                    report.operations.post_products = post_products;
                    report.operations.total_products = trace.hessian_products + post_products;
                    report.trace = trace;
                    return Ok(report);
                }
            }
        }
        report.cauchy_limiter = Some(if cauchy_maximum < radius.get() / metric_gradient_norm {
            if cauchy_curvature > 0.0 && gradient_squared / cauchy_curvature < cauchy_cap {
                "ray-minimum"
            } else {
                "coordinate-cap"
            }
        } else {
            "metric-radius"
        });
        cauchy_trial = Some(cauchy_params);
    }
    report.cauchy = FixedLinearizationCandidate {
        available: cauchy_available,
        prediction: cauchy_prediction,
        metric_norm: cauchy_metric_norm.is_finite().then_some(cauchy_metric_norm),
        largest: cauchy_largest.is_finite().then_some(cauchy_largest),
        coordinate_feasible: cauchy_coordinate_feasible,
        radius_feasible: cauchy_radius_feasible,
    };

    let tr_positive = report.tr.prediction.filter(|value| *value > 0.0);
    let cauchy_positive = report.cauchy.prediction.filter(|value| *value > 0.0);
    let cauchy_wins = match (tr_positive, cauchy_positive) {
        (Some(tr), Some(cauchy)) => cauchy > tr,
        (None, Some(_)) => true,
        _ => false,
    };
    report.ordinary = if cauchy_wins {
        FixedLinearizationOrdinary::Cauchy
    } else if tr_positive.is_some() {
        FixedLinearizationOrdinary::TrustRegion
    } else {
        FixedLinearizationOrdinary::None
    };
    report.ordinary_candidate = match report.ordinary {
        FixedLinearizationOrdinary::TrustRegion => report.tr,
        FixedLinearizationOrdinary::Cauchy => report.cauchy,
        FixedLinearizationOrdinary::None => FixedLinearizationCandidate::default(),
    };
    let ordinary_prediction = match report.ordinary {
        FixedLinearizationOrdinary::TrustRegion => tr_positive,
        FixedLinearizationOrdinary::Cauchy => cauchy_positive,
        FixedLinearizationOrdinary::None => None,
    };

    let evaluate = |trial_params: &Params| -> (f64, bool) {
        let candidate = problem.pass(
            trial_params,
            stage.active,
            &mut [],
            false,
            false,
            stage.neutral,
            IntentFilter::NativeOnly,
        );
        let trial_cost = candidate.cost
            + total_cost(
                &problem,
                trial_params,
                &blocks,
                &penalties,
                &input.lambdas,
                options,
            );
        (trial_cost, candidate.infeasible)
    };
    let trial_result = |trial_cost: f64, infeasible: bool, prediction: f64| {
        let decrease = cost - trial_cost;
        let ratio = decrease / prediction;
        FixedLinearizationTrial {
            cost: trial_cost,
            decrease,
            prediction,
            ratio,
            trial_infeasible: infeasible,
            accepted: !infeasible
                && trial_cost.is_finite()
                && decrease > 1e-12 * cost.abs().max(1.0)
                && ratio >= 1e-4,
        }
    };

    if let Some(prediction) = ordinary_prediction {
        let trial_params = match report.ordinary {
            FixedLinearizationOrdinary::TrustRegion => Some(&tr_params),
            FixedLinearizationOrdinary::Cauchy => cauchy_trial.as_ref(),
            FixedLinearizationOrdinary::None => None,
        };
        if let Some(trial_params) = trial_params {
            let (trial_cost, infeasible) = evaluate(trial_params);
            report.operations.objective_passes += 1;
            report.operations.sample_evaluations += problem.samples.len() as u64;
            report.ordinary_trial = Some(trial_result(trial_cost, infeasible, prediction));
        }
    }

    report.box_clipped = raw_proposal
        .iter()
        .filter(|value| (*value).abs() > STEP_RADIUS)
        .count();
    let box_delta: Vec<f64> = raw_proposal
        .iter()
        .map(|value| (*value).clamp(-STEP_RADIUS, STEP_RADIUS))
        .collect();
    let mut box_params = params.clone();
    let box_effective = realize(&box_delta, &mut box_params);
    let box_metric_norm = fixed_linearization_metric_norm(diagonal, &box_effective);
    let box_largest = fixed_linearization_largest(&box_effective);
    let box_zero = box_effective.iter().all(|value| *value == 0.0);
    let box_coordinate_feasible = box_largest.is_finite() && box_largest <= STEP_RADIUS * slack;
    let box_radius_feasible =
        box_metric_norm.is_finite() && box_metric_norm <= radius.get() * slack;
    let box_considered = !box_zero
        && box_effective != tr_effective
        && (report.ordinary != FixedLinearizationOrdinary::None || termination.is_none());
    let box_prediction = if box_considered && box_coordinate_feasible && box_radius_feasible {
        post_products += 1;
        match full.apply(&box_effective, product, hessian) {
            Ok(()) => crate::trust_region::predicted_decrease(gradient, &box_effective, product),
            Err(failure) => {
                report.status = fixed_linearization_failure(failure);
                report.note = failure.label().to_string();
                report.operations.post_products = post_products;
                report.operations.total_products = trace.hessian_products + post_products;
                report.trace = trace;
                return Ok(report);
            }
        }
    } else {
        None
    };
    report.box_source = if termination.is_some() {
        FixedLinearizationBoxSource::TrustRegionProposal
    } else {
        FixedLinearizationBoxSource::WorkLimitPartial
    };
    report.box_candidate = FixedLinearizationCandidate {
        available: box_considered,
        prediction: box_prediction,
        metric_norm: Some(box_metric_norm),
        largest: Some(box_largest),
        coordinate_feasible: box_coordinate_feasible,
        radius_feasible: box_radius_feasible,
    };
    let box_positive = box_prediction.filter(|value| *value > 0.0);
    if let Some(prediction) = box_positive {
        let (trial_cost, infeasible) = evaluate(&box_params);
        report.operations.objective_passes += 1;
        report.operations.sample_evaluations += problem.samples.len() as u64;
        report.box_trial = Some(trial_result(trial_cost, infeasible, prediction));
    }

    let box_ranked_first = ordinary_prediction
        .zip(box_positive)
        .is_some_and(|(ordinary, boxed)| boxed > ordinary);
    let box_applied = box_ranked_first
        && report
            .ordinary_trial
            .zip(report.box_trial)
            .is_some_and(|(ordinary, boxed)| boxed.decrease > ordinary.decrease);
    report.winner = if box_applied {
        FixedLinearizationWinner::Box
    } else {
        report.ordinary.winner()
    };
    report.winner_candidate = match report.winner {
        FixedLinearizationWinner::TrustRegion => report.tr,
        FixedLinearizationWinner::Cauchy => report.cauchy,
        FixedLinearizationWinner::Box => report.box_candidate,
        FixedLinearizationWinner::None => FixedLinearizationCandidate::default(),
    };
    report.box_actual_improvement = match (&report.ordinary_trial, &report.box_trial) {
        (Some(ordinary), Some(boxed)) => boxed.decrease - ordinary.decrease,
        _ => f64::NAN,
    };
    report.disposition = if report.status == FixedLinearizationStatus::WorkLimitPartial {
        FixedLinearizationDisposition::WorkLimitErr
    } else {
        let winner_trial = match report.winner {
            FixedLinearizationWinner::TrustRegion | FixedLinearizationWinner::Cauchy => {
                report.ordinary_trial.as_ref()
            }
            FixedLinearizationWinner::Box => report.box_trial.as_ref(),
            FixedLinearizationWinner::None => None,
        };
        if winner_trial.is_some_and(|trial| trial.accepted) {
            FixedLinearizationDisposition::Accepted
        } else {
            FixedLinearizationDisposition::NotAccepted
        }
    };

    let mut notes = Vec::new();
    if report.reference_source == FixedLinearizationReference::FirstResume {
        notes.push(
            "first-resume: gradient reference reset to the fresh norm; not an uninterrupted continuation",
        );
    }
    if report.status == FixedLinearizationStatus::WorkLimitPartial {
        notes.push(
            "work-limit partial CG iterate is diagnostic and nonstationary; production returns Err and discards it",
        );
    }
    if report.ordinary == FixedLinearizationOrdinary::None {
        notes.push("no positive realized TR/Cauchy prediction");
    }
    if box_prediction.is_some_and(|value| value <= 0.0) {
        notes.push("box predicted decrease is nonpositive");
    }
    if report.box_candidate.available && !report.box_candidate.feasible() {
        notes.push("box candidate violates a trust-region constraint after projection");
    }
    if report.ordinary_trial.is_some_and(|trial| !trial.accepted) {
        notes.push("ordinary winner fails production acceptance");
    }
    if report.box_trial.is_some_and(|trial| !trial.accepted) {
        notes.push("box candidate fails production acceptance");
    }
    if report.box_actual_improvement.is_finite() && report.box_actual_improvement <= 0.0 {
        notes.push("box objective decrease does not improve the ordinary winner");
    }
    if !notes.is_empty() {
        report.note = notes.join("; ");
    }
    report.operations.post_products = post_products;
    report.operations.total_products = trace.hessian_products + post_products;
    report.trace = trace;
    Ok(report)
}

/// Residual-grid share of the model prediction for the applied residual delta.
#[cfg(all(test, feature = "synth"))]
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
#[cfg(all(test, feature = "synth"))]
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
        let mut axis_projection = [0.0_f64; DOMAIN_AXES];
        for (position, (column, derivative)) in row.columns.iter().enumerate() {
            let value = step[*column as usize];
            projected[0] += derivative[0] * value;
            projected[1] += derivative[1] * value;
            projected[2] += derivative[2] * value;
            robust_projection += row.robust_terms[position] * value;
            for (axis, total) in axis_projection.iter_mut().enumerate() {
                *total += row.axis_terms[position][axis] * value;
            }
        }
        let mut residual_projected = [0.0_f64; 3];
        let mut residual_robust = 0.0_f64;
        let mut residual_axis = [0.0_f64; DOMAIN_AXES];
        for (column, derivative, robust_term, axis) in &row.residual_columns {
            let value = residual_step[*column as usize - residual.start];
            residual_projected[0] += derivative[0] * value;
            residual_projected[1] += derivative[1] * value;
            residual_projected[2] += derivative[2] * value;
            residual_robust += robust_term * value;
            for (slot, total) in residual_axis.iter_mut().enumerate() {
                *total += axis[slot] * value;
            }
        }
        total += row.weight * dot3(projected, residual_projected)
            + if robust {
                row.robust * robust_projection * residual_robust
            } else {
                0.0
            }
            + penalty_pair(&row.penalty, &axis_projection, &residual_axis);
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
        params.theta[problem.layout.response.start + index] = model_value(model, id);
    }
}

/// Seeds each row group's response block from that group's model.
pub(crate) fn seed_row_responses(problem: &Problem, params: &mut Params, rows: &[RowModel<'_>]) {
    for (group, response) in problem.response_groups.iter().enumerate() {
        let Some(model) = rows
            .iter()
            .find(|row| row.group == group)
            .and_then(|row| row.model)
        else {
            continue;
        };
        for (index, id) in response.ids.iter().enumerate() {
            params.theta[problem.layout.response.start + response.start + index] =
                model_value(model, id);
        }
    }
}

fn model_value(model: &ControlResponseModel, id: &ResponseParamId) -> f64 {
    match id.component {
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

/// Blocks and penalties with the nuisance split applied.
fn stage_blocks(
    stage: &Stage,
    problem: &Problem,
    options: &FitOptions,
) -> (Vec<BlockKind>, Vec<BlockAcc>, Vec<Vec<f64>>) {
    let kinds: Vec<BlockKind> = stage
        .blocks
        .iter()
        .copied()
        .filter(|kind| options.fit_residual || *kind != BlockKind::Residual)
        .collect();
    let split_nuisances = problem.images.len() > 64;
    let layout = &problem.layout;
    let mut blocks: Vec<BlockAcc> = kinds
        .iter()
        .map(|kind| {
            if *kind == BlockKind::Tone && !split_nuisances {
                make_joint_tone_block(problem, stage.active)
            } else {
                make_block(problem, *kind, stage.active)
            }
        })
        .collect();
    if split_nuisances && (stage.active.image || stage.active.wb) {
        for index in 0..problem_image_count(layout) {
            blocks.push(make_image_block(layout, index, stage.active));
        }
    }
    if split_nuisances && (stage.active.mode || stage.active.clip) {
        blocks.push(make_shared_nuisance_block(problem, layout, stage.active));
    }
    let penalties = blocks
        .iter()
        .map(|block| block_penalty(problem, block))
        .collect();
    (kinds, blocks, penalties)
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
/// SDs: tone 1 on the fitted coordinates (`c0` in J', `eta = softplus_inverse(beta)`),
/// tint 2 J'a'b', log G 0.3, hue 15°, lightness 8 J', residual 0.5 ΔE (docs/02 §4.1).
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

/// `Λ_id · (θ - m)` restricted to the block's coordinates.
pub(crate) fn identity_gradient(layout: &Layout, block: &BlockAcc, theta: &[f64]) -> Vec<f64> {
    let mut out = vec![0.0_f64; block.len()];
    for (local, global) in block.params.iter().enumerate() {
        out[local] = layout.identity_scale
            * kind_of(layout, *global).map_or(0.0, identity_weight)
            * (theta[*global] - layout.identity_mean(*global));
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
    let mut value = 0.0;
    for global in &block.params {
        let delta = theta[*global] - layout.identity_mean(*global);
        let applied =
            layout.identity_scale * kind_of(layout, *global).map_or(0.0, identity_weight) * delta;
        value += 0.5 * delta * applied;
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
            let fixed_gradient: Vec<f64> = block
                .params
                .iter()
                .enumerate()
                .map(|(local, global)| {
                    prior_gradient[local]
                        + identity[local] * (theta0[local] - problem.layout.identity_mean(*global))
                })
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
    let identity = identity_gradient(layout, block, &params.theta);
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
    mask: &ResidualMask,
    origin: CertificateOrigin,
) -> StationarityReport {
    let report = |certificate: StationarityCertificate,
                  work: Option<CertificateWork>,
                  bounds: Option<StationarityBounds>| {
        StationarityReport {
            origin,
            certificate,
            work,
            bounds,
        }
    };
    let model = match JointModel::new(
        layout, problem, params, options, blocks, penalties, lambdas, roles, pass, data_scale,
        false, false, mask,
    ) {
        Ok(model) => model,
        Err(failure) => {
            if fit_diag() {
                eprintln!("fit certificate model failed: {}", failure.label());
            }
            return report(
                StationarityCertificate::Unresolved {
                    termination: certificate_termination(failure),
                },
                None,
                None,
            );
        }
    };
    let solve = match JointSolve::new(&model, 0.0) {
        Ok(solve) => solve,
        Err(failure) => {
            if fit_diag() {
                eprintln!("fit certificate factor failed: {}", failure.label());
            }
            return report(
                StationarityCertificate::Unresolved {
                    termination: certificate_termination(failure),
                },
                None,
                None,
            );
        }
    };
    certified_bound(&model, &solve, warm, origin)
}

/// Full stationarity interval for an already-built undamped solve of `model`.
fn certified_bound(
    model: &JointModel<'_>,
    solve: &JointSolve,
    warm: &mut Option<Vec<f64>>,
    origin: CertificateOrigin,
) -> StationarityReport {
    let report = |certificate: StationarityCertificate,
                  work: Option<CertificateWork>,
                  bounds: Option<StationarityBounds>| {
        StationarityReport {
            origin,
            certificate,
            work,
            bounds,
        }
    };
    let lower_bound = solve.energy_lower_bound(model);
    if lower_bound.is_finite() && lower_bound > STATIONARY_DECREMENT_NATS {
        if fit_diag() {
            eprintln!(
                "fit certificate lower bound {lower_bound:.9e} nats complete true residual_coords {} joint {}",
                model.residual_count, model.dim
            );
        }
        if let Some(lower) = Nats::checked(lower_bound) {
            return report(
                StationarityCertificate::CertifiedNonstationary { lower },
                Some(CertificateWork {
                    iterations: 0,
                    relative_residual: None,
                    dim: model.dim,
                    residual_coordinates: model.residual_count,
                }),
                None,
            );
        }
    }
    let solution = match solve.solve(model, CERTIFICATE_TOLERANCE, None, warm.as_deref()) {
        Ok(solution) => solution,
        Err(failure) => {
            if fit_diag() {
                eprintln!("fit certificate solve failed: {}", failure.label());
            }
            return report(
                StationarityCertificate::Unresolved {
                    termination: certificate_termination(failure),
                },
                None,
                None,
            );
        }
    };
    *warm = Some(solution.reduced.clone());
    let work = CertificateWork {
        iterations: solution.iterations,
        relative_residual: Some(solution.relative_residual),
        dim: model.dim,
        residual_coordinates: model.residual_count,
    };
    let mut quadratic = 0.0_f64;
    let mut first = 0.0_f64;
    let mut residual_correction = 0.0_f64;
    for index in 0..model.dim {
        let residual = model.rhs[index] - solution.curvature[index];
        // `diag(λP)` is not a Loewner lower bound of `λP`; only the prior floor is.
        // A response block contributes `2λ` because its stored penalty is `2P` with `P ⪰ I`.
        let floor = model.floor[index];
        if floor <= 0.0 || !floor.is_finite() {
            if fit_diag() {
                eprintln!("fit certificate has no SPD prior floor at joint coordinate {index}");
            }
            return report(
                StationarityCertificate::Unresolved {
                    termination: CertificateTermination::NoPriorFloor,
                },
                Some(work),
                None,
            );
        }
        quadratic += solution.delta[index] * solution.curvature[index];
        first += solution.delta[index] * residual;
        residual_correction += residual * residual / floor;
    }
    let lower = (0.5 * quadratic + first).max(0.0);
    let upper = lower + 0.5 * residual_correction;
    if !(lower.is_finite() && upper.is_finite()) {
        return report(
            StationarityCertificate::Unresolved {
                termination: CertificateTermination::Nonfinite,
            },
            Some(work),
            None,
        );
    }
    if fit_diag() {
        eprintln!(
            "fit certificate bound {upper:.9e} nats lower {lower:.9e} complete true residual_coords {} joint {}",
            model.residual_count, model.dim
        );
    }
    let certificate = if lower > STATIONARY_DECREMENT_NATS {
        match Nats::checked(lower) {
            Some(lower) => StationarityCertificate::CertifiedNonstationary { lower },
            None => StationarityCertificate::Unresolved {
                termination: CertificateTermination::Nonfinite,
            },
        }
    } else if upper <= STATIONARY_DECREMENT_NATS {
        match Nats::checked(upper) {
            Some(upper) => StationarityCertificate::CertifiedStationary { upper },
            None => StationarityCertificate::Unresolved {
                termination: CertificateTermination::Nonfinite,
            },
        }
    } else {
        StationarityCertificate::Unresolved {
            termination: solution.failure.map_or(
                CertificateTermination::BoundNotDecisive,
                certificate_termination,
            ),
        }
    };
    if fit_diag() {
        let (verdict, kind) = match certificate {
            StationarityCertificate::CertifiedStationary { .. } => ("stationary", "upper"),
            StationarityCertificate::CertifiedNonstationary { .. } => ("nonstationary", "lower"),
            StationarityCertificate::Unresolved { .. } => ("unresolved", "none"),
        };
        let termination = match certificate {
            StationarityCertificate::CertifiedStationary { .. }
            | StationarityCertificate::CertifiedNonstationary { .. } => "resolved",
            StationarityCertificate::Unresolved { termination } => termination.label(),
        };
        eprintln!(
            "fit certificate verdict {verdict} kind {kind} termination {termination} lower {lower:.9e} upper {upper:.9e} residual {:.3e} iterations {} dim {} residual_coords {} complete true",
            solution.relative_residual, solution.iterations, model.dim, model.residual_count
        );
    }
    let bounds = match (Nats::checked(lower), Nats::checked(upper)) {
        (Some(lower), Some(upper)) => Some(StationarityBounds { lower, upper }),
        _ => None,
    };
    report(certificate, Some(work), bounds)
}

/// Full upper-bound certificate for the current linearization, when it is certified stationary.
fn certified_stationary(
    model: &JointModel<'_>,
    warm: &mut Option<Vec<f64>>,
) -> Option<StationarityReport> {
    let solve = JointSolve::new(model, 0.0).ok()?;
    let report = certified_bound(model, &solve, warm, CertificateOrigin::FinalStage);
    report
        .certificate
        .is_certified_stationary()
        .then_some(report)
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

fn certificate_termination(failure: SolveFailure) -> CertificateTermination {
    match failure {
        SolveFailure::WorkLimit => CertificateTermination::WorkLimit,
        SolveFailure::NonpositiveCurvature => CertificateTermination::NonpositiveCurvature,
        SolveFailure::Factorization(_) => CertificateTermination::Factorization,
        SolveFailure::Nonfinite => CertificateTermination::Nonfinite,
    }
}

fn dot3(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn penalty_pair(
    matrix: &[[f64; DOMAIN_AXES]; DOMAIN_AXES],
    left: &[f64; DOMAIN_AXES],
    right: &[f64; DOMAIN_AXES],
) -> f64 {
    let mut sum = 0.0;
    for (a, left) in left.iter().enumerate() {
        if *left == 0.0 {
            continue;
        }
        for (b, right) in right.iter().enumerate() {
            sum += matrix[a][b] * left * right;
        }
    }
    sum
}

struct JointRow {
    columns: Vec<(u32, [f64; 3])>,
    /// Image slot of the row's image columns.
    image: Option<u32>,
    /// Residual-grid columns `(local residual index, derivative, Jᵀr, ∂axis/∂θ)`.
    residual_columns: Vec<(u32, [f64; 3], f64, [f64; DOMAIN_AXES])>,
    weight: f64,
    /// `Jᵀr` per column.
    robust_terms: Vec<f64>,
    /// `−2(ν+3)/q²` times the sample weight and the data scale.
    robust: f64,
    /// `∂axis/∂θ` per column.
    axis_terms: Vec<[f64; DOMAIN_AXES]>,
    /// `weight * P` for the Hessian-vector product.
    penalty: [[f64; DOMAIN_AXES]; DOMAIN_AXES],
    /// `weight * P_gn` for the SPD preconditioner.
    penalty_gn: [[f64; DOMAIN_AXES]; DOMAIN_AXES],
    /// Layer value each column moves, `None` when it mixes layers.
    layers: Vec<Option<Layer>>,
    /// `∂(t, n_a, n_b)/∂θ` in the first three slots and `∂layer/∂θ` of `layers` in
    /// the fourth.
    layer_weights: Vec<[f64; 4]>,
    /// `∂²layer/∂θ²` of a tone column, `∂w_m/∂t` of a tint column, zero otherwise.
    layer_slopes: Vec<f64>,
    /// `α w r·∂p/∂l_k` for `k = 0, 1, 2`, scaled.
    motion: [f64; 3],
    /// `Σ_m w''_m a_m`, `Σ_m w''_m b_m`, `Σ_m w'_m a_m`, `Σ_m w'_m b_m`.
    tint: [f64; 4],
    /// `α w Σ_c r_c ∂²p_c/∂l_k∂l_l`, scaled by `data_scale`.
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
struct JointModel<'a> {
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
    /// Diagonal lower bound on the prior curvature: [`Self::fixed`] plus `2λ` for a
    /// response block, because `block_penalty` stores `2P` with `P ⪰ I`.
    floor: Vec<f64>,
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
    /// Image slot of each joint coordinate, `u32::MAX` outside image blocks.
    joint_image: Vec<u32>,
    /// Local image index of each joint coordinate.
    joint_local: Vec<u32>,
    /// Preconditioner blocks over the outer coordinates.
    groups: Vec<JointGroup>,
    outer_count: usize,
    border_count: usize,
    /// First joint index of the compact free residual block.
    residual_start: usize,
    /// Free residual coordinates in the joint vector, zero when the block is absent.
    residual_count: usize,
    /// Compact local index of each global residual coordinate; `u32::MAX` when pinned.
    residual_local_of: Vec<u32>,
    /// Residual data curvature scale.
    residual_data_scale: f64,
    /// Linearization samples of the residual data block.
    residual_samples: &'a [ResidualSample],
    /// Largest image block width, for scratch sizing.
    image_width: usize,
    dim: usize,
    /// Whether the rows carry the Student-t robust-weight correction.
    robust: bool,
    /// Whether the rows carry the prediction's second derivative.
    second: bool,
}

impl<'a> JointModel<'a> {
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
        pass: &'a PassOutput,
        data_scale: f64,
        robust: bool,
        second: bool,
        mask: &ResidualMask,
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
        let residual_active = blocks.iter().any(|block| block.kind == BlockKind::Residual);
        let residual_start = globals.len();
        let mut residual_count = 0usize;
        let mut residual_ridge = 0.0_f64;
        let mut residual_identity = 0.0_f64;
        let mut residual_local_of: Vec<u32> = Vec::new();
        let mut residual_globals: Vec<usize> = Vec::new();
        let mut residual_penalty_diagonal: Vec<f64> = Vec::new();
        let mut residual_penalty_entries: Vec<(u32, u32, f64)> = Vec::new();
        let mut residual_data_diagonal: Vec<f64> = Vec::new();
        let mut residual_samples: &[ResidualSample] = &[];
        if residual_active {
            residual_ridge = 2.0 * lam(lambdas, BlockKind::Residual);
            residual_identity = identity_weight(BlockKind::Residual);
            let space = ResidualSpace::new(mask, residual_ridge);
            let data = residual_diagonal(&pass.residual_samples);
            for &local in &space.globals {
                joint_of[layout.residual.start + local] = globals.len() as u32;
                globals.push(layout.residual.start + local);
                residual_data_diagonal.push(data[local]);
            }
            for &(row, column, value) in &space.penalty {
                residual_penalty_entries.push((
                    (residual_start + row as usize) as u32,
                    (residual_start + column as usize) as u32,
                    value,
                ));
            }
            residual_count = space.len();
            residual_globals = space.globals;
            residual_penalty_diagonal = space.diagonal;
            residual_local_of = space.local_of;
            residual_samples = &pass.residual_samples;
        }
        let dim = globals.len();

        let mut fixed = vec![0.0_f64; dim];
        let mut floor = vec![0.0_f64; dim];
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
                floor[joint] = fixed[joint];
                if is_response_kind(block.kind) {
                    floor[joint] += 2.0 * lambda;
                }
                data_diagonal[joint] = block.full_diagonal[local].max(0.0);
                let mut regularizer = 0.0_f64;
                for other in 0..n {
                    regularizer +=
                        penalties[index][local * n + other] * params.theta[block.params[other]];
                }
                rhs[joint] = -(block.gradient[local]
                    + prior_gradient[local]
                    + identity[local] * (params.theta[global] - layout.identity_mean(global))
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
        if residual_active {
            let theta = &params.theta[layout.residual.clone()];
            for (compact, &local) in residual_globals.iter().enumerate() {
                let joint = residual_start + compact;
                fixed[joint] = residual_identity;
                floor[joint] = residual_identity;
                data_diagonal[joint] = data_scale * residual_data_diagonal[compact];
                penalty_diagonal[joint] = residual_ridge * residual_penalty_diagonal[local];
                rhs[joint] = pass.residual_gradient[local]
                    - residual_ridge * laplacian_at(theta, local, &residual_penalty_diagonal)
                    - residual_identity * theta[local];
            }
            penalty.extend_from_slice(&residual_penalty_entries);
        }
        if fixed
            .iter()
            .chain(floor.iter())
            .chain(rhs.iter())
            .chain(data_diagonal.iter())
            .any(|value| !value.is_finite())
        {
            if fit_diag() {
                for (label, values) in [
                    ("fixed", &fixed),
                    ("floor", &floor),
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

        let mut joint_image = vec![u32::MAX; dim];
        let mut joint_local = vec![0u32; dim];
        for (slot, globals_of_image) in images.iter().enumerate() {
            for (local, &global) in globals_of_image.iter().enumerate() {
                let joint = joint_of[global] as usize;
                joint_image[joint] = slot as u32;
                joint_local[joint] = local as u32;
            }
        }
        let mut rows = Vec::with_capacity(pass.coupled_rows.len());
        for row in &pass.coupled_rows {
            let mut columns = Vec::with_capacity(row.columns.len());
            let mut robust_terms = Vec::with_capacity(row.columns.len());
            let mut layers = Vec::with_capacity(row.columns.len());
            let mut layer_weights = Vec::with_capacity(row.columns.len());
            let mut layer_slopes = Vec::with_capacity(row.columns.len());
            let mut axis_terms = Vec::with_capacity(row.columns.len());
            for (index, (global, derivative)) in row.columns.iter().enumerate() {
                let joint = joint_of[*global as usize];
                if joint != u32::MAX {
                    columns.push((joint, *derivative));
                    robust_terms.push(row.robust_terms[index]);
                    axis_terms.push(row.axis_terms[index]);
                    layers.push(row.layers[index]);
                    layer_weights.push(row.layer_weights[index]);
                    layer_slopes.push(row.layer_slopes[index]);
                }
            }
            let mut residual_columns = Vec::with_capacity(row.residual_columns.len());
            for (global, derivative, robust_term, axis) in &row.residual_columns {
                let joint = joint_of[*global as usize];
                if joint != u32::MAX {
                    residual_columns.push((
                        joint - residual_start as u32,
                        *derivative,
                        *robust_term,
                        *axis,
                    ));
                }
            }
            if columns.is_empty() && residual_columns.is_empty() {
                continue;
            }
            let image = columns.iter().find_map(|(joint, _)| {
                let index = *joint as usize;
                (index >= outer_count + border_count).then(|| joint_image[index])
            });
            let mut scaled = Box::new([0.0_f64; 81]);
            for (value, source) in scaled.iter_mut().zip(row.second.iter()) {
                *value = source * data_scale;
            }
            let motion = row.motion.map(|value| value * data_scale);
            let penalty = row
                .penalty
                .map(|entry| entry.map(|value| value * data_scale));
            let penalty_gn = row
                .penalty_gn
                .map(|entry| entry.map(|value| value * data_scale));
            rows.push(JointRow {
                columns,
                image,
                residual_columns,
                weight: row.weight * data_scale,
                robust_terms,
                robust: row.robust * data_scale,
                axis_terms,
                penalty,
                penalty_gn,
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
        let mut row_outer: Vec<(usize, [f64; 3], f64, [f64; DOMAIN_AXES])> = Vec::new();
        let mut row_border: Vec<(usize, [f64; 3], f64, [f64; DOMAIN_AXES])> = Vec::new();
        let mut row_image: Vec<(usize, [f64; 3], f64, [f64; DOMAIN_AXES])> = Vec::new();
        for row in &rows {
            row_outer.clear();
            row_border.clear();
            row_image.clear();
            let mut slot = u32::MAX;
            for (position, (joint, derivative)) in row.columns.iter().enumerate() {
                let index = *joint as usize;
                let robust_term = row.robust_terms[position];
                let axis = row.axis_terms[position];
                if index < outer_count {
                    row_outer.push((index, *derivative, robust_term, axis));
                } else if index < outer_count + border_count {
                    row_border.push((index - outer_count, *derivative, robust_term, axis));
                } else {
                    slot = joint_image[index];
                    row_image.push((joint_local[index] as usize, *derivative, robust_term, axis));
                }
            }
            let weight = row.weight;
            let robust = if robust { row.robust } else { 0.0 };
            let penalty = &row.penalty_gn;
            let pair = |left: &(usize, [f64; 3], f64, [f64; DOMAIN_AXES]),
                        right: &(usize, [f64; 3], f64, [f64; DOMAIN_AXES])|
             -> f64 {
                let mut curvature = 0.0;
                for (a, left) in left.3.iter().enumerate() {
                    if *left == 0.0 {
                        continue;
                    }
                    for (b, right) in right.3.iter().enumerate() {
                        curvature += penalty[a][b] * left * right;
                    }
                }
                weight * dot3(left.1, right.1) + robust * left.2 * right.2 + curvature
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
            floor,
            penalty,
            penalty_diagonal,
            data_diagonal,
            image_outer,
            image_border,
            image_data,
            outer_border,
            border_data,
            image_range,
            joint_image,
            joint_local,
            groups,
            outer_count,
            border_count,
            residual_start,
            residual_count,
            residual_local_of,
            residual_data_scale: data_scale,
            residual_samples,
            image_width,
            dim,
            robust,
            second,
        })
    }

    /// Retained-row curvature: GN, the Student-t correction when `self.robust`, the
    /// penalty, and the prediction's second derivative when `self.second`.
    fn apply_rows(&self, v: &[f64], out: &mut [f64]) {
        self.apply_rows_with(v, out, true);
    }

    /// [`Self::apply_rows`] with the second-order term forced on or off.
    fn apply_rows_with(&self, v: &[f64], out: &mut [f64], with_second: bool) {
        let second = self.second && with_second;
        for row in &self.rows {
            let mut projected = [0.0_f64; 3];
            let mut robust_projection = 0.0_f64;
            let mut axis_projection = [0.0_f64; DOMAIN_AXES];
            let mut layer_direction = [0.0_f64; 9];
            let mut tint_a_direction = 0.0_f64;
            let mut tint_b_direction = 0.0_f64;
            for (position, (column, derivative)) in row.columns.iter().enumerate() {
                let value = v[*column as usize];
                projected[0] += derivative[0] * value;
                projected[1] += derivative[1] * value;
                projected[2] += derivative[2] * value;
                robust_projection += row.robust_terms[position] * value;
                for (axis, total) in axis_projection.iter_mut().enumerate() {
                    *total += row.axis_terms[position][axis] * value;
                }
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
            let mut residual_projected = [0.0_f64; 3];
            let mut residual_robust = 0.0_f64;
            let mut residual_axis = [0.0_f64; DOMAIN_AXES];
            for (local, derivative, robust_term, axis) in &row.residual_columns {
                let value = v[self.residual_start + *local as usize];
                residual_projected[0] += derivative[0] * value;
                residual_projected[1] += derivative[1] * value;
                residual_projected[2] += derivative[2] * value;
                residual_robust += robust_term * value;
                for (slot, total) in residual_axis.iter_mut().enumerate() {
                    *total += axis[slot] * value;
                }
            }
            let mut penalty_projection = [0.0_f64; DOMAIN_AXES];
            let mut residual_penalty = [0.0_f64; DOMAIN_AXES];
            for a in 0..DOMAIN_AXES {
                for b in 0..DOMAIN_AXES {
                    penalty_projection[a] += row.penalty[a][b] * axis_projection[b];
                    residual_penalty[a] += row.penalty[a][b] * residual_axis[b];
                }
            }
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
                let axis = row.axis_terms[position];
                let mut penalty = 0.0;
                for (slot, value) in axis.iter().enumerate() {
                    penalty += value * (penalty_projection[slot] + residual_penalty[slot]);
                }
                let mut total = weight * dot3(*derivative, projected)
                    + robust * row.robust_terms[position] * robust_projection
                    + penalty
                    + weight * dot3(*derivative, residual_projected)
                    + robust * row.robust_terms[position] * residual_robust;
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
            for (local, derivative, robust_term, axis) in &row.residual_columns {
                let mut penalty = 0.0;
                for (slot, value) in axis.iter().enumerate() {
                    penalty += value * penalty_projection[slot];
                }
                let mut total = weight * dot3(*derivative, projected)
                    + robust * robust_term * robust_projection
                    + penalty;
                if robust != 0.0 {
                    total += robust * robust_term * residual_robust;
                }
                out[self.residual_start + *local as usize] += total;
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
        if self.residual_count > 0 {
            self.apply_residual_data(
                &v[self.residual_start..self.residual_start + self.residual_count],
                &mut out[self.residual_start..self.residual_start + self.residual_count],
            );
        }
        for (index, slot) in out.iter_mut().enumerate() {
            *slot += self.fixed[index] * v[index];
        }
        for &(row, column, value) in &self.penalty {
            out[row as usize] += value * v[column as usize];
        }
    }

    /// Curvature over the compact free block.
    fn apply_residual_data(&self, v: &[f64], out: &mut [f64]) {
        residual_data_accumulate(
            self.residual_samples,
            v,
            out,
            self.residual_data_scale,
            |global| {
                let local = self.residual_local_of[global];
                (local != u32::MAX).then_some(local as usize)
            },
        );
    }
}

#[derive(Clone, Copy, Debug)]
enum AnalyticFailure {
    MissingPrediction,
    Prediction(crate::predict::CurvatureError),
    Nonfinite,
    InvalidMetric,
    InvalidObjective,
}

impl AnalyticFailure {
    fn label(self) -> &'static str {
        match self {
            Self::MissingPrediction => "missing-prediction",
            Self::Prediction(error) => match error {
                crate::predict::CurvatureError::NonFinite => "prediction-nonfinite",
                crate::predict::CurvatureError::Singular => "prediction-singular",
                crate::predict::CurvatureError::Domain => "prediction-domain",
                crate::predict::CurvatureError::ScratchTooSmall => "prediction-scratch-too-small",
                crate::predict::CurvatureError::Length => "prediction-length",
            },
            Self::Nonfinite => "nonfinite",
            Self::InvalidMetric => "invalid-metric",
            Self::InvalidObjective => "invalid-objective",
        }
    }
}

#[derive(Default)]
struct AnalyticProductScratch {
    direction: Vec<f64>,
    product: Vec<f64>,
    prediction: crate::predict::CurvatureScratch,
}

#[derive(Default)]
struct FinalScratch {
    hessian: AnalyticProductScratch,
    solver: crate::trust_region::TrustRegionScratch,
    gradient: Vec<f64>,
    diagonal: Vec<f64>,
    effective: Vec<f64>,
    product: Vec<f64>,
    cauchy_effective: Vec<f64>,
    cauchy_product: Vec<f64>,
    box_effective: Vec<f64>,
    box_product: Vec<f64>,
    reduced: Vec<f64>,
    reduced_out: Vec<f64>,
    trial: Option<Params>,
    cauchy_trial: Option<Params>,
    box_trial: Option<Params>,
}

struct AnalyticModel<'a, 'b> {
    metric: &'a JointModel<'b>,
    rows: &'a [crate::problem::HessianRow],
    scale: f64,
    total: usize,
    clip: Option<usize>,
    clip_prior: f64,
}

impl<'a, 'b> AnalyticModel<'a, 'b> {
    fn new(
        metric: &'a JointModel<'b>,
        pass: &'a PassOutput,
        params: &Params,
        options: &FitOptions,
        scale: f64,
    ) -> Result<Self, AnalyticFailure> {
        if pass.infeasible || !pass.cost.is_finite() {
            return Err(AnalyticFailure::InvalidObjective);
        }
        if pass.coupled_rows.len() != pass.hessian_rows
            || pass.coupled_rows.iter().any(|row| row.curvature.is_none())
        {
            return Err(AnalyticFailure::MissingPrediction);
        }
        let clip = metric
            .globals
            .iter()
            .position(|global| *global == params.layout.clip);
        let k = params.clip_k();
        let probability = (k - 0.005) / 0.095;
        let second = params.clip_slope() * (1.0 - 2.0 * probability);
        let (mean, sd) = options.clip_prior;
        let clip_prior = (k - mean) * second / (sd * sd);
        if !clip_prior.is_finite() {
            return Err(AnalyticFailure::Nonfinite);
        }
        Ok(Self {
            metric,
            rows: &pass.coupled_rows,
            scale,
            total: params.layout.total,
            clip,
            clip_prior,
        })
    }

    fn prepare(&self, scratch: &mut FinalScratch) -> Result<(), AnalyticFailure> {
        let dim = self.metric.dim;
        scratch.gradient.resize(dim, 0.0);
        scratch.diagonal.resize(dim, 0.0);
        scratch.effective.resize(dim, 0.0);
        scratch.product.resize(dim, 0.0);
        scratch.cauchy_effective.resize(dim, 0.0);
        scratch.cauchy_product.resize(dim, 0.0);
        scratch.box_effective.resize(dim, 0.0);
        scratch.box_product.resize(dim, 0.0);
        let reduced = self.metric.outer_count + self.metric.residual_count;
        scratch.reduced.resize(reduced, 0.0);
        scratch.reduced_out.resize(reduced, 0.0);
        scratch.hessian.direction.resize(self.total, 0.0);
        scratch.hessian.direction.fill(0.0);
        scratch.hessian.product.resize(self.total, 0.0);
        for index in 0..dim {
            scratch.gradient[index] = -self.metric.rhs[index];
            let diagonal = self.metric.fixed[index]
                + self.metric.penalty_diagonal[index]
                + self.metric.data_diagonal[index];
            if !(diagonal.is_finite() && diagonal > 0.0) {
                return Err(AnalyticFailure::InvalidMetric);
            }
            scratch.diagonal[index] = diagonal;
        }
        for row in self.rows {
            row.curvature
                .as_ref()
                .ok_or(AnalyticFailure::MissingPrediction)?
                .prepare_scratch(&mut scratch.hessian.prediction)
                .map_err(AnalyticFailure::Prediction)?;
        }
        Ok(())
    }

    fn apply(
        &self,
        direction: &[f64],
        out: &mut [f64],
        scratch: &mut AnalyticProductScratch,
    ) -> Result<(), AnalyticFailure> {
        self.metric.apply_undamped(direction, out);
        for (local, global) in self.metric.globals.iter().enumerate() {
            scratch.direction[*global] = direction[local];
        }
        scratch.product.fill(0.0);
        for row in self.rows {
            let mut projection = 0.0;
            for ((global, _), term) in row.columns.iter().zip(&row.robust_terms) {
                projection += term * scratch.direction[*global as usize];
            }
            for (global, _, term, _) in &row.residual_columns {
                projection += term * scratch.direction[*global as usize];
            }
            let radial = row.robust * projection;
            for ((global, _), term) in row.columns.iter().zip(&row.robust_terms) {
                scratch.product[*global as usize] += radial * term;
            }
            for (global, _, term, _) in &row.residual_columns {
                scratch.product[*global as usize] += radial * term;
            }
            row.curvature
                .as_ref()
                .ok_or(AnalyticFailure::MissingPrediction)?
                .add_hvp(
                    &scratch.direction,
                    &mut scratch.product,
                    &mut scratch.prediction,
                )
                .map_err(AnalyticFailure::Prediction)?;
        }
        for (local, global) in self.metric.globals.iter().enumerate() {
            out[local] += self.scale * scratch.product[*global];
        }
        if let Some(clip) = self.clip {
            out[clip] += self.clip_prior * direction[clip];
        }
        if out.iter().any(|value| !value.is_finite()) {
            return Err(AnalyticFailure::Nonfinite);
        }
        Ok(())
    }
}

/// Buffers shared by the reduced operator and its preconditioner.
struct JointScratch<'a> {
    joint: &'a mut [f64],
    joint_out: &'a mut [f64],
    image: &'a mut [f64],
    border: &'a mut [f64],
}

fn scratch_buffer(model: &JointModel<'_>) -> Vec<f64> {
    let image = model.images.len() * model.image_width.max(1);
    vec![0.0_f64; 2 * model.dim + image + model.border_count.max(1)]
}

fn split_scratch<'a>(buffer: &'a mut [f64], model: &JointModel<'_>) -> JointScratch<'a> {
    let dim = model.dim;
    let border_count = model.border_count;
    let image = model.images.len() * model.image_width.max(1);
    let (first, rest) = buffer.split_at_mut(dim);
    let (second, rest) = rest.split_at_mut(dim);
    let (image, border) = rest.split_at_mut(image);
    JointScratch {
        joint: first,
        joint_out: second,
        image,
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

/// The branch production selected for one preconditioner group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreconditionerChoice {
    Reduced,
    Block,
    Diagonal,
    Prior,
}

impl PreconditionerChoice {
    pub fn label(self) -> &'static str {
        match self {
            Self::Reduced => "reduced",
            Self::Block => "block",
            Self::Diagonal => "diagonal",
            Self::Prior => "prior",
        }
    }
}

/// The branch selected for one preconditioner group.
#[cfg_attr(not(feature = "synth"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreconditionerGroupChoice {
    pub start: usize,
    pub width: usize,
    pub choice: PreconditionerChoice,
}

/// How the diagonal fallback treats the group block's off-diagonal entries.
#[cfg_attr(not(feature = "synth"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiagonalFallback {
    /// Keep the off-diagonals of the computed diagonal fallback.
    Baseline,
    /// Keep the computed diagonal and zero the off-diagonals.
    ZeroedOffDiagonals,
}

const PRODUCTION_DIAGONAL_FALLBACK: DiagonalFallback = DiagonalFallback::Baseline;

/// Observes the selected group branch without changing the factorization or branch order.
trait PreconditionerObserver {
    fn group_choice(&mut self, _start: usize, _width: usize, _choice: PreconditionerChoice) {}
}

struct NoPreconditionerObserver;

impl PreconditionerObserver for NoPreconditionerObserver {}

/// The damped joint system with the per-image blocks and the border eliminated.
struct JointSolve {
    image_factor: Vec<Vec<f64>>,
    border_factor: Vec<f64>,
    /// Eliminated outer/border coupling, `q × k`.
    outer_border: Vec<f64>,
    /// Eliminated residual/border coupling, `nr × k`.
    residual_border: Vec<f64>,
    /// Reduced right-hand side over `(y, r)`.
    reduced_rhs: Vec<f64>,
    /// Reduced right-hand side over the border, before the `S_ss` solve.
    border_rhs: Vec<f64>,
    /// `(start, width, scaled factor, Jacobi scale)` of each outer preconditioner block.
    groups: Vec<(usize, usize, Vec<f64>, Vec<f64>)>,
    /// Jacobi diagonal of the residual block.
    residual_diagonal: Vec<f64>,
    damping: f64,
    zero_border: Vec<f64>,
}

impl JointSolve {
    fn new(model: &JointModel<'_>, damping: f64) -> Result<Self, SolveFailure> {
        Self::build_observed(
            model,
            damping,
            PRODUCTION_DIAGONAL_FALLBACK,
            &mut NoPreconditionerObserver,
        )
    }

    fn build_observed<O: PreconditionerObserver>(
        model: &JointModel<'_>,
        damping: f64,
        diagonal_fallback: DiagonalFallback,
        observer: &mut O,
    ) -> Result<Self, SolveFailure> {
        let q = model.outer_count;
        let k = model.border_count;
        let nr = model.residual_count;
        let res = model.residual_start;
        let mut rows_by_image: Vec<Vec<usize>> = vec![Vec::new(); model.images.len()];
        for (index, row) in model.rows.iter().enumerate() {
            if let Some(slot) = row.image {
                rows_by_image[slot as usize].push(index);
            }
        }
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

        let mut border_residual = vec![0.0_f64; nr * k];
        if nr > 0 && k > 0 {
            let robust = if model.robust { 1.0 } else { 0.0 };
            for row in &model.rows {
                if row.residual_columns.is_empty() {
                    continue;
                }
                for (index, (joint, derivative)) in row.columns.iter().enumerate() {
                    let column = *joint as usize;
                    if column < q || column >= q + k {
                        continue;
                    }
                    let border = column - q;
                    for (local, residual_derivative, residual_robust, residual_axis) in
                        &row.residual_columns
                    {
                        border_residual[*local as usize * k + border] += row.weight
                            * dot3(*derivative, *residual_derivative)
                            + robust * row.robust * row.robust_terms[index] * residual_robust
                            + penalty_pair(&row.penalty, &row.axis_terms[index], residual_axis);
                    }
                }
            }
            let mut coupling = Vec::new();
            let mut values = vec![0.0_f64; model.image_width.max(1)];
            for index in 0..model.images.len() {
                let m = model.images[index].len();
                if m == 0 || rows_by_image[index].is_empty() {
                    continue;
                }
                coupling.resize(m * nr, 0.0);
                image_residual_coupling(model, &rows_by_image[index], index, &mut coupling);
                for local in 0..nr {
                    for row in 0..m {
                        values[row] = coupling[row * nr + local];
                    }
                    solve_in_place(&image_factor[index], m, &mut values[..m]);
                    for border in 0..k {
                        let mut sum = 0.0_f64;
                        for row in 0..m {
                            sum += model.image_border[index][row * k + border] * values[row];
                        }
                        border_residual[local * k + border] -= sum;
                    }
                }
            }
        }

        let mut outer_rhs = model.rhs[..q].to_vec();
        let mut border_rhs = model.rhs[q..q + k].to_vec();
        let mut residual_rhs = model.rhs[res..res + nr].to_vec();
        let mut image_value = vec![0.0_f64; model.images.len() * model.image_width.max(1)];
        for (index, globals_of_image) in model.images.iter().enumerate() {
            let m = globals_of_image.len();
            if m == 0 {
                continue;
            }
            let (start, _) = model.image_range[index];
            let base = index * model.image_width.max(1);
            image_value[base..base + m].copy_from_slice(&model.rhs[start..start + m]);
            solve_in_place(&image_factor[index], m, &mut image_value[base..base + m]);
        }
        for (index, globals_of_image) in model.images.iter().enumerate() {
            let m = globals_of_image.len();
            if m == 0 {
                continue;
            }
            let base = index * model.image_width.max(1);
            for row in 0..m {
                let value = image_value[base + row];
                for left in 0..q {
                    outer_rhs[left] -= model.image_outer[index][row * q + left] * value;
                }
                for right in 0..k {
                    border_rhs[right] -= model.image_border[index][row * k + right] * value;
                }
            }
        }
        image_residual_product(model, &image_value, &mut residual_rhs, -1.0);
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
            for local in 0..nr {
                let mut sum = 0.0_f64;
                for right in 0..k {
                    sum += border_residual[local * k + right] * border_rhs[right];
                }
                residual_rhs[local] -= sum;
            }
        }
        let mut reduced_rhs = Vec::with_capacity(q + nr);
        reduced_rhs.extend_from_slice(&outer_rhs);
        reduced_rhs.extend_from_slice(&residual_rhs);

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
            let mut choice = PreconditionerChoice::Reduced;
            let mut scaled = scaled_factor(&matrix, width);
            if scaled.is_none() {
                // A principal block of the Schur complement is positive definite only when
                // the full reduced operator is.
                choice = PreconditionerChoice::Block;
                scaled = scaled_factor(&unreduced, width);
            }
            if scaled.is_none() {
                choice = PreconditionerChoice::Diagonal;
                let mut diagonal = match diagonal_fallback {
                    DiagonalFallback::Baseline => group.matrix.clone(),
                    DiagonalFallback::ZeroedOffDiagonals => vec![0.0_f64; width * width],
                };
                for row in 0..width {
                    let joint = start + row;
                    let total = group.matrix[row * width + row].max(0.0)
                        + model.fixed[joint]
                        + damping * model.data_diagonal[joint].max(1e-12);
                    diagonal[row * width + row] = total - schur_diagonal[joint].min(0.5 * total);
                }
                scaled = scaled_factor(&diagonal, width);
            }
            if scaled.is_none() {
                choice = PreconditionerChoice::Prior;
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
            observer.group_choice(start, width, choice);
            if fit_diag() && choice != PreconditionerChoice::Reduced {
                eprintln!(
                    "fit preconditioner group {start}+{width} falls back to {}",
                    choice.label()
                );
            }
            groups.push((start, width, factor, scale));
        }

        let mut residual_diagonal = vec![0.0_f64; nr];
        for local in 0..nr {
            let joint = res + local;
            residual_diagonal[local] = (model.fixed[joint]
                + model.penalty_diagonal[joint]
                + (1.0 + damping) * model.data_diagonal[joint].max(1e-12))
            .max(1e-12);
        }

        Ok(Self {
            image_factor,
            border_factor,
            outer_border,
            residual_border: border_residual,
            reduced_rhs,
            border_rhs: reduced_border,
            groups,
            residual_diagonal,
            damping,
            zero_border: vec![0.0_f64; k],
        })
    }

    /// Builds with an explicit diagonal-fallback policy and records the selected group branch.
    #[cfg(feature = "synth")]
    fn diagnostic(
        model: &JointModel<'_>,
        damping: f64,
        diagonal_fallback: DiagonalFallback,
    ) -> Result<(Self, Vec<PreconditionerGroupChoice>), SolveFailure> {
        struct Recorder(Vec<PreconditionerGroupChoice>);

        impl PreconditionerObserver for Recorder {
            fn group_choice(&mut self, start: usize, width: usize, choice: PreconditionerChoice) {
                self.0.push(PreconditionerGroupChoice {
                    start,
                    width,
                    choice,
                });
            }
        }

        let mut recorder = Recorder(Vec::new());
        let solve = Self::build_observed(model, damping, diagonal_fallback, &mut recorder)?;
        Ok((solve, recorder.0))
    }

    /// `[[S_yy, S_ys, S_yr], [S_sy, S_ss, S_sr], [S_ry, S_rs, S_rr]]` applied to `(v_outer, v_border, v_residual)`, images eliminated.
    fn eliminated(
        &self,
        model: &JointModel<'_>,
        v_outer: &[f64],
        v_border: &[f64],
        v_residual: &[f64],
        out: &mut [f64],
        scratch: &mut JointScratch<'_>,
    ) {
        let q = model.outer_count;
        let k = model.border_count;
        let nr = model.residual_count;
        let res = model.residual_start;
        scratch.joint[..q].copy_from_slice(v_outer);
        scratch.joint[q..q + k].copy_from_slice(v_border);
        scratch.joint[q + k..res].fill(0.0);
        scratch.joint[res..res + nr].copy_from_slice(v_residual);
        scratch.joint_out.fill(0.0);
        model.apply_rows(scratch.joint, scratch.joint_out);
        for index in 0..q + k {
            out[index] = scratch.joint_out[index]
                + (model.fixed[index] + self.damping * model.data_diagonal[index].max(1e-12))
                    * scratch.joint[index];
        }
        if nr > 0 {
            out[res..res + nr].copy_from_slice(&scratch.joint_out[res..res + nr]);
            model.apply_residual_data(v_residual, &mut out[res..res + nr]);
            for local in 0..nr {
                let joint = res + local;
                out[joint] += (model.fixed[joint]
                    + self.damping * model.data_diagonal[joint].max(1e-12))
                    * scratch.joint[joint];
            }
        }
        for &(row, column, value) in &model.penalty {
            out[row as usize] += value * scratch.joint[column as usize];
        }
        let width = model.image_width.max(1);
        scratch.image.fill(0.0);
        for (index, globals_of_image) in model.images.iter().enumerate() {
            let m = globals_of_image.len();
            if m == 0 {
                continue;
            }
            let base = index * width;
            for row in 0..m {
                let mut value = 0.0_f64;
                for left in 0..q {
                    value += model.image_outer[index][row * q + left] * v_outer[left];
                }
                for right in 0..k {
                    value += model.image_border[index][row * k + right] * v_border[right];
                }
                scratch.image[base + row] = value;
            }
        }
        residual_image_product(model, v_residual, scratch.image, 1.0);
        for (index, globals_of_image) in model.images.iter().enumerate() {
            let m = globals_of_image.len();
            if m == 0 {
                continue;
            }
            let base = index * width;
            solve_in_place(
                &self.image_factor[index],
                m,
                &mut scratch.image[base..base + m],
            );
        }
        for (index, globals_of_image) in model.images.iter().enumerate() {
            let m = globals_of_image.len();
            if m == 0 {
                continue;
            }
            let base = index * width;
            for row in 0..m {
                let weight = scratch.image[base + row];
                for left in 0..q {
                    out[left] -= model.image_outer[index][row * q + left] * weight;
                }
                for right in 0..k {
                    out[q + right] -= model.image_border[index][row * k + right] * weight;
                }
            }
        }
        image_residual_product(model, scratch.image, &mut out[res..res + nr], -1.0);
    }

    /// The reduced Schur operator over the outer and residual coordinates.
    fn apply_reduced(
        &self,
        model: &JointModel<'_>,
        v: &[f64],
        out: &mut [f64],
        eliminated: &mut [f64],
        scratch: &mut JointScratch<'_>,
    ) {
        let q = model.outer_count;
        let k = model.border_count;
        let nr = model.residual_count;
        let res = model.residual_start;
        self.eliminated(
            model,
            &v[..q],
            &self.zero_border,
            &v[q..q + nr],
            eliminated,
            scratch,
        );
        out[..q].copy_from_slice(&eliminated[..q]);
        out[q..q + nr].copy_from_slice(&eliminated[res..res + nr]);
        if k == 0 {
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
        for local in 0..nr {
            let mut sum = 0.0_f64;
            for right in 0..k {
                sum += self.residual_border[local * k + right] * scratch.border[right];
            }
            out[q + local] = eliminated[res + local] - sum;
        }
    }

    /// `(gᵀd)² / (2 dᵀBd)` for the preconditioned direction, a lower bound on `E`.
    fn energy_lower_bound(&self, model: &JointModel<'_>) -> f64 {
        if self.reduced_rhs.iter().all(|value| *value == 0.0) {
            return 0.0;
        }
        let q = model.outer_count;
        let nr = model.residual_count;
        let red = q + nr;
        let mut buffer = scratch_buffer(model);
        let mut scratch = split_scratch(&mut buffer, model);
        let mut direction = vec![0.0_f64; red];
        let mut product = vec![0.0_f64; red];
        let mut eliminated = vec![0.0_f64; model.dim];
        self.precondition(&self.reduced_rhs, &mut direction);
        self.apply_reduced(
            model,
            &direction,
            &mut product,
            &mut eliminated,
            &mut scratch,
        );
        let first = crate::solve::dot(&self.reduced_rhs, &direction);
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
        let q = self.reduced_rhs.len() - self.residual_diagonal.len();
        for (local, diagonal) in self.residual_diagonal.iter().enumerate() {
            z[q + local] = residual[q + local] / diagonal.max(1e-12);
        }
    }

    fn precondition_full(
        &self,
        model: &JointModel<'_>,
        residual: &[f64],
        out: &mut [f64],
        reduced: &mut [f64],
        reduced_out: &mut [f64],
    ) -> Result<(), SolveFailure> {
        let q = model.outer_count;
        let k = model.border_count;
        let nr = model.residual_count;
        let res = model.residual_start;
        reduced[..q].copy_from_slice(&residual[..q]);
        reduced[q..q + nr].copy_from_slice(&residual[res..res + nr]);
        self.precondition(reduced, reduced_out);
        out[..q].copy_from_slice(&reduced_out[..q]);
        out[res..res + nr].copy_from_slice(&reduced_out[q..q + nr]);
        out[q..q + k].copy_from_slice(&residual[q..q + k]);
        if k > 0 {
            solve_in_place(&self.border_factor, k, &mut out[q..q + k]);
        }
        for (index, globals) in model.images.iter().enumerate() {
            let width = globals.len();
            if width == 0 {
                continue;
            }
            let (start, _) = model.image_range[index];
            out[start..start + width].copy_from_slice(&residual[start..start + width]);
            solve_in_place(
                &self.image_factor[index],
                width,
                &mut out[start..start + width],
            );
        }
        if out.iter().any(|value| !value.is_finite()) {
            return Err(SolveFailure::Nonfinite);
        }
        Ok(())
    }

    /// Solve the damped joint system, recover the eliminated coordinates and report the
    /// explicit residual of the original joint system.
    fn solve(
        &self,
        model: &JointModel<'_>,
        tolerance: f64,
        radius: Option<f64>,
        warm: Option<&[f64]>,
    ) -> Result<JointSolution, SolveFailure> {
        self.solve_inner(model, tolerance, radius, warm)
    }

    fn solve_inner(
        &self,
        model: &JointModel<'_>,
        tolerance: f64,
        radius: Option<f64>,
        warm: Option<&[f64]>,
    ) -> Result<JointSolution, SolveFailure> {
        let q = model.outer_count;
        let k = model.border_count;
        let nr = model.residual_count;
        let res = model.residual_start;
        let red = q + nr;
        let dim = model.dim;
        let mut x = vec![0.0_f64; red];
        let mut residual = self.reduced_rhs.clone();
        let mut product = vec![0.0_f64; red];
        let mut eliminated = vec![0.0_f64; dim];
        let mut buffer = scratch_buffer(model);
        let mut scratch = split_scratch(&mut buffer, model);

        let norm: f64 = crate::solve::dot(&self.reduced_rhs, &self.reduced_rhs);
        if !norm.is_finite() {
            return Err(SolveFailure::Nonfinite);
        }
        let mut warmed = false;
        if let Some(values) = warm.filter(|values| values.len() == red) {
            x.copy_from_slice(values);
            self.apply_reduced(model, &x, &mut product, &mut eliminated, &mut scratch);
            let mut error = 0.0_f64;
            for index in 0..red {
                residual[index] = self.reduced_rhs[index] - product[index];
                error += residual[index] * residual[index];
            }
            if error.is_finite() && error < norm {
                warmed = true;
            } else {
                x.fill(0.0);
                residual.copy_from_slice(&self.reduced_rhs);
            }
        }
        let mut iterations = 0usize;
        let mut stalled = false;
        if residual.iter().any(|value| *value != 0.0) {
            let mut z = vec![0.0_f64; red];
            let mut direction = vec![0.0_f64; red];
            self.precondition(&residual, &mut z);
            direction.copy_from_slice(&z);
            let mut rz = crate::solve::dot(&residual, &z);
            if !(rz.is_finite() && rz > 0.0) {
                return Err(SolveFailure::Nonfinite);
            }
            let mut target = rz * tolerance * tolerance;
            let mut checked = 0usize;
            let mut stagnant = 0usize;
            let mut previous = f64::INFINITY;
            loop {
                if iterations >= JOINT_CG_LIMIT {
                    stalled = true;
                    break;
                }
                let recursive = rz <= target;
                let chunk = (warmed && iterations == 0)
                    || iterations.saturating_sub(checked) >= JOINT_CG_CHUNK;
                if recursive || chunk {
                    self.apply_reduced(model, &x, &mut product, &mut eliminated, &mut scratch);
                    let mut error = 0.0_f64;
                    for index in 0..red {
                        residual[index] = self.reduced_rhs[index] - product[index];
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
                for index in 0..red {
                    residual[index] -= step * product[index];
                }
                self.precondition(&residual, &mut z);
                let next = crate::solve::dot(&residual, &z);
                if !next.is_finite() {
                    return Err(SolveFailure::Nonfinite);
                }
                let beta = next / rz;
                for index in 0..red {
                    direction[index] = z[index] + beta * direction[index];
                }
                rz = next;
            }
        }

        let mut delta = vec![0.0_f64; dim];
        delta[..q].copy_from_slice(&x[..q]);
        delta[res..res + nr].copy_from_slice(&x[q..]);
        self.eliminated(
            model,
            &x[..q],
            &self.zero_border,
            &x[q..],
            &mut eliminated,
            &mut scratch,
        );
        let mut border_delta = vec![0.0_f64; k];
        for index in 0..k {
            border_delta[index] = self.border_rhs[index] - eliminated[q + index];
        }
        if k > 0 {
            solve_in_place(&self.border_factor, k, &mut border_delta);
        }
        delta[q..q + k].copy_from_slice(&border_delta);
        let width = model.image_width.max(1);
        scratch.image.fill(0.0);
        for (index, globals_of_image) in model.images.iter().enumerate() {
            let m = globals_of_image.len();
            if m == 0 {
                continue;
            }
            let (start, _) = model.image_range[index];
            let base = index * width;
            for row in 0..m {
                let mut value = model.rhs[start + row];
                for left in 0..q {
                    value -= model.image_outer[index][row * q + left] * x[left];
                }
                for right in 0..k {
                    value -= model.image_border[index][row * k + right] * border_delta[right];
                }
                scratch.image[base + row] = value;
            }
        }
        residual_image_product(model, &x[q..], scratch.image, -1.0);
        for (index, globals_of_image) in model.images.iter().enumerate() {
            let m = globals_of_image.len();
            if m == 0 {
                continue;
            }
            let (start, _) = model.image_range[index];
            let base = index * width;
            solve_in_place(
                &self.image_factor[index],
                m,
                &mut scratch.image[base..base + m],
            );
            delta[start..start + m].copy_from_slice(&scratch.image[base..base + m]);
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
        if fit_diag() {
            eprintln!(
                "fit coupled residual relative {relative_residual:.3e} tolerance {tolerance:.3e} iterations {iterations} stalled {stalled} dim {dim} residual_coords {}",
                model.residual_count
            );
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
            if nr > 0 {
                push("residual".to_string(), res, res + nr);
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
        #[cfg(test)]
        let prediction =
            crate::solve::dot(&model.rhs, &delta) - 0.5 * crate::solve::dot(&delta, &curvature);
        Ok(JointSolution {
            delta,
            curvature,
            #[cfg(test)]
            prediction,
            iterations,
            relative_residual,
            failure: stalled.then_some(SolveFailure::WorkLimit),
            reduced: x,
        })
    }
}

/// `B_ur` for one image over the given retained rows, `m × nr`, row-major.
fn image_residual_coupling(model: &JointModel<'_>, rows: &[usize], slot: usize, out: &mut [f64]) {
    let nr = model.residual_count;
    let m = model.images[slot].len();
    out.fill(0.0);
    if nr == 0 || m == 0 {
        return;
    }
    let robust = if model.robust { 1.0 } else { 0.0 };
    for &index in rows {
        let row = &model.rows[index];
        if row.residual_columns.is_empty() {
            continue;
        }
        for (position, (joint, derivative)) in row.columns.iter().enumerate() {
            if model.joint_image[*joint as usize] != slot as u32 {
                continue;
            }
            let local = model.joint_local[*joint as usize] as usize;
            for (residual_local, residual_derivative, residual_robust, residual_axis) in
                &row.residual_columns
            {
                out[local * nr + *residual_local as usize] += row.weight
                    * dot3(*derivative, *residual_derivative)
                    + robust * row.robust * row.robust_terms[position] * residual_robust
                    + penalty_pair(&row.penalty, &row.axis_terms[position], residual_axis);
            }
        }
    }
}

/// `B_ur v` added to the per-image values, scaled by `sign`.
fn residual_image_product(
    model: &JointModel<'_>,
    v_residual: &[f64],
    image: &mut [f64],
    sign: f64,
) {
    let nr = model.residual_count;
    if nr == 0 {
        return;
    }
    let width = model.image_width.max(1);
    let robust = if model.robust { 1.0 } else { 0.0 };
    for row in &model.rows {
        if row.residual_columns.is_empty() {
            continue;
        }
        let Some(slot) = row.image else { continue };
        let mut projected = [0.0_f64; 3];
        let mut residual_robust = 0.0_f64;
        let mut residual_axis = [0.0_f64; DOMAIN_AXES];
        for (local, derivative, robust_term, axis) in &row.residual_columns {
            let value = v_residual[*local as usize];
            projected[0] += derivative[0] * value;
            projected[1] += derivative[1] * value;
            projected[2] += derivative[2] * value;
            residual_robust += robust_term * value;
            for (slot, total) in residual_axis.iter_mut().enumerate() {
                *total += axis[slot] * value;
            }
        }
        let base = slot as usize * width;
        for (position, (joint, derivative)) in row.columns.iter().enumerate() {
            if model.joint_image[*joint as usize] != slot {
                continue;
            }
            let local = model.joint_local[*joint as usize] as usize;
            image[base + local] += sign
                * (row.weight * dot3(*derivative, projected)
                    + robust * row.robust * row.robust_terms[position] * residual_robust
                    + penalty_pair(&row.penalty, &row.axis_terms[position], &residual_axis));
        }
    }
}

/// `B_ru w` added to the residual output, scaled by `sign`.
fn image_residual_product(
    model: &JointModel<'_>,
    image: &[f64],
    out_residual: &mut [f64],
    sign: f64,
) {
    let nr = model.residual_count;
    if nr == 0 {
        return;
    }
    let width = model.image_width.max(1);
    let robust = if model.robust { 1.0 } else { 0.0 };
    for row in &model.rows {
        if row.residual_columns.is_empty() {
            continue;
        }
        let Some(slot) = row.image else { continue };
        let base = slot as usize * width;
        let mut projected = [0.0_f64; 3];
        let mut robust_projection = 0.0_f64;
        let mut axis_projection = [0.0_f64; DOMAIN_AXES];
        for (position, (joint, derivative)) in row.columns.iter().enumerate() {
            if model.joint_image[*joint as usize] != slot {
                continue;
            }
            let value = image[base + model.joint_local[*joint as usize] as usize];
            projected[0] += derivative[0] * value;
            projected[1] += derivative[1] * value;
            projected[2] += derivative[2] * value;
            robust_projection += row.robust_terms[position] * value;
            for (axis, total) in axis_projection.iter_mut().enumerate() {
                *total += row.axis_terms[position][axis] * value;
            }
        }
        for (local, derivative, robust_term, residual_axis) in &row.residual_columns {
            out_residual[*local as usize] += sign
                * (row.weight * dot3(*derivative, projected)
                    + robust * row.robust * robust_term * robust_projection
                    + penalty_pair(&row.penalty, &axis_projection, residual_axis));
        }
    }
}

struct JointSolution {
    /// Joint-coordinate step.
    delta: Vec<f64>,
    /// `B δ` under the undamped joint model.
    curvature: Vec<f64>,
    /// `-g·δ - ½ δᵀBδ`.
    #[cfg(test)]
    prediction: f64,
    iterations: usize,
    /// Relative residual of the damped joint system after nuisance recovery.
    relative_residual: f64,
    /// Set when the iteration limit or the stall detector stopped the solve early.
    failure: Option<SolveFailure>,
    /// Reduced `(outer, residual)` solution, for warm starts.
    reduced: Vec<f64>,
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
    let damping_diag: Vec<f64> = block
        .full_diagonal
        .iter()
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
    debug_assert_eq!(block.full_diagonal.len(), n);
    for i in 0..n {
        a[i * n + i] = diagonal[i]
            + lambda * penalty[i * n + i]
            + damping * block.full_diagonal[i].max(1e-12)
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
            sum += identity[i * n + j]
                * (params.theta[block.params[j]] - layout.identity_mean(block.params[j]));
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

#[cfg(test)]
fn apply_step_unclamped(params: &mut Params, step: &[f64]) {
    for (index, value) in step.iter().enumerate() {
        if value.is_finite() {
            params.theta[index] += value;
        }
    }
}

/// Derivative of the penalized objective in `log sigma` for one contributor.
fn sigma_derivative(t: f64, pairs: &[(f64, f64)], prior: (f64, f64), nu: f64) -> f64 {
    let (a, b) = prior;
    let variance = libm::exp(2.0 * t);
    let mut derivative = 2.0 * a - 2.0 * b * libm::exp(-2.0 * t);
    for (weight, squared) in pairs {
        let u = squared / (nu * variance);
        derivative += weight * (3.0 - (nu + 3.0) * u / (1.0 + u));
    }
    derivative
}

/// Bracket of the monotone `log sigma` root; `None` when an endpoint derivative is non-finite or the root lies outside the search interval.
fn sigma_bracket(pairs: &[(f64, f64)], prior: (f64, f64), nu: f64) -> Option<(f64, f64)> {
    let low_endpoint = sigma_derivative(-SIGMA_RANGE, pairs, prior, nu);
    let high_endpoint = sigma_derivative(SIGMA_RANGE, pairs, prior, nu);
    if !(low_endpoint.is_finite() && high_endpoint.is_finite())
        || low_endpoint > 0.0
        || high_endpoint < 0.0
    {
        return None;
    }
    let mut low = -SIGMA_RANGE;
    let mut high = SIGMA_RANGE;
    for _ in 0..SIGMA_BRACKET_STEPS {
        let mid = 0.5 * (low + high);
        if sigma_derivative(mid, pairs, prior, nu) > 0.0 {
            high = mid;
        } else {
            low = mid;
        }
    }
    Some((low, high))
}

fn update_sigma(problem: &Problem, params: &mut Params, pass: &PassOutput, options: &FitOptions) {
    let nu = problem.nu;
    for index in 0..problem.contributors.len() {
        let pairs = &pass.contributor_pairs[index];
        let Some((low, high)) = sigma_bracket(pairs, options.sigma_prior, nu) else {
            continue;
        };
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

/// Free and pinned residual-grid coordinates at one linearization.
struct ResidualMask {
    free: Vec<bool>,
}

impl ResidualMask {
    fn from_samples(samples: &[ResidualSample]) -> Self {
        let support = residual_support(samples);
        let peak = support.iter().copied().fold(0.0_f64, f64::max);
        Self {
            free: support
                .iter()
                .map(|value| peak > 0.0 && *value >= 1e-4 * peak)
                .collect(),
        }
    }

    fn empty() -> Self {
        Self {
            free: vec![false; RESIDUAL_COEFS],
        }
    }

    fn free(&self) -> &[bool] {
        &self.free
    }

    #[cfg(test)]
    fn count(&self) -> usize {
        self.free.iter().filter(|value| **value).count()
    }

    /// Whether a coordinate outside the free mask is nonzero.
    fn has_pinned_value(&self, theta: &[f64]) -> bool {
        self.free
            .iter()
            .enumerate()
            .any(|(index, free)| !free && theta[index] != 0.0)
    }

    /// Nodes the data does not reach stay at zero (docs/02 §3.5).
    fn pin(&self, theta: &mut [f64]) {
        for (index, free) in self.free.iter().enumerate() {
            if !free {
                theta[index] = 0.0;
            }
        }
    }
}

/// Free residual coordinates in compact order with the `2λ·P` smoothness stencil over them.
struct ResidualSpace {
    /// Compact local index of each global residual coordinate; `u32::MAX` when pinned.
    local_of: Vec<u32>,
    /// Global residual coordinate of each compact local coordinate.
    globals: Vec<usize>,
    /// `(row, column, 2λ·P entry)` over compact coordinates.
    penalty: Vec<(u32, u32, f64)>,
    /// Smoothness penalty diagonal, full.
    diagonal: Vec<f64>,
}

impl ResidualSpace {
    fn new(mask: &ResidualMask, ridge: f64) -> Self {
        let diagonal = residual_penalty_diagonal();
        let mut local_of = vec![u32::MAX; RESIDUAL_COEFS];
        let mut globals = Vec::new();
        for local in 0..RESIDUAL_COEFS {
            if mask.free()[local] {
                local_of[local] = globals.len() as u32;
                globals.push(local);
            }
        }
        let mut penalty = Vec::with_capacity(7 * globals.len());
        for (row, &local) in globals.iter().enumerate() {
            let row = row as u32;
            penalty.push((row, row, ridge * diagonal[local]));
            let node = local / 3;
            let channel = local % 3;
            for axis in 0..3 {
                let (before, after) = neighbours(node, axis);
                for neighbour in [before, after].into_iter().flatten() {
                    let other = neighbour * 3 + channel;
                    let other_local = local_of[other];
                    if other_local != u32::MAX {
                        penalty.push((row, other_local, -ridge));
                    }
                }
            }
        }
        Self {
            local_of,
            globals,
            penalty,
            diagonal,
        }
    }

    fn len(&self) -> usize {
        self.globals.len()
    }

    /// Data curvature, `2λ·P` and the identity prior over the compact block.
    fn apply(
        &self,
        samples: &[ResidualSample],
        v: &[f64],
        out: &mut [f64],
        hessian_scale: f64,
        identity: f64,
    ) {
        out.fill(0.0);
        residual_data_accumulate(samples, v, out, hessian_scale, |global| {
            let local = self.local_of[global];
            (local != u32::MAX).then_some(local as usize)
        });
        for &(row, column, value) in &self.penalty {
            out[row as usize] += value * v[column as usize];
        }
        for (slot, value) in out.iter_mut().enumerate() {
            *value += identity * v[slot];
        }
    }
}

/// State of the conditional `log sigma` optimization at one linearization.
enum SigmaState {
    Settled,
    Refresh,
    Unresolved(CertificateTermination),
}

/// Checks that every stored `log sigma` lies in the bracket of its fresh root.
fn conditional_sigma_state(
    problem: &Problem,
    params: &mut Params,
    pass: &PassOutput,
    options: &FitOptions,
) -> SigmaState {
    let nu = problem.nu;
    let prior = options.sigma_prior;
    let bracket_width = 2.0 * SIGMA_RANGE / (1u64 << SIGMA_BRACKET_STEPS) as f64;
    let mut brackets: Vec<(f64, f64)> = Vec::with_capacity(problem.contributors.len());
    let mut refresh = false;
    for index in 0..problem.contributors.len() {
        let Some(bracket) = sigma_bracket(&pass.contributor_pairs[index], prior, nu) else {
            return SigmaState::Unresolved(CertificateTermination::SigmaUnbracketed);
        };
        let (low, high) = bracket;
        let stored = params.theta[params.layout.sigma_of(index)];
        if !stored.is_finite() {
            return SigmaState::Unresolved(CertificateTermination::Nonfinite);
        }
        let guard = 2.0 * bracket_width + 4.0 * f64::EPSILON * stored.abs().max(1.0);
        if stored < low - guard || stored > high + guard {
            refresh = true;
        }
        brackets.push(bracket);
    }
    if !refresh {
        return SigmaState::Settled;
    }
    for (index, (low, high)) in brackets.into_iter().enumerate() {
        params.theta[params.layout.sigma_of(index)] = 0.5 * (low + high);
    }
    SigmaState::Refresh
}

#[allow(clippy::too_many_arguments)]
fn recertify_native(
    problem: &Problem,
    params: &mut Params,
    options: &FitOptions,
    lambdas: &StageLambdas,
    evaluations: &mut usize,
    sample_evaluations: &mut SampleEvaluations,
    warm: &mut Option<Vec<f64>>,
    operator_rows: &mut usize,
) -> StationarityReport {
    let report =
        |certificate: StationarityCertificate, work: Option<CertificateWork>| StationarityReport {
            origin: CertificateOrigin::PostFitRecertification,
            certificate,
            work,
            bounds: None,
        };
    let layout = problem.layout.clone();
    let roles = prior_roles(problem, &layout);
    let (_, mut blocks, penalties) = stage_blocks(&STAGES[4], problem, options);
    let samples = problem.samples.len();
    for _ in 0..3 {
        if sample_evaluations.get() as f64 + samples as f64 > options.sweep_budget {
            return report(
                StationarityCertificate::Unresolved {
                    termination: CertificateTermination::WorkLimit,
                },
                None,
            );
        }
        for block in &mut blocks {
            block.reset();
        }
        let pass = problem.pass_with_rows(
            params,
            ActiveColumns::ALL,
            &mut blocks,
            false,
            false,
            false,
            IntentFilter::NativeOnly,
            true,
            samples <= FULL_ROW_LIMIT,
        );
        *evaluations += 1;
        *sample_evaluations += samples;
        *operator_rows = pass.hessian_rows;
        if pass.infeasible || !pass.cost.is_finite() {
            return report(
                StationarityCertificate::Unresolved {
                    termination: CertificateTermination::InvalidObjective,
                },
                None,
            );
        }
        let data_scale = pass.active_samples.max(1) as f64 / pass.hessian_rows.max(1) as f64;
        let mask = ResidualMask::from_samples(&pass.residual_samples);
        let residual_active = blocks.iter().any(|block| block.kind == BlockKind::Residual);
        if residual_active && mask.has_pinned_value(&params.theta[layout.residual.clone()]) {
            mask.pin(&mut params.theta[layout.residual.clone()]);
            continue;
        }
        match conditional_sigma_state(problem, params, &pass, options) {
            SigmaState::Settled => {}
            SigmaState::Refresh => continue,
            SigmaState::Unresolved(termination) => {
                return report(StationarityCertificate::Unresolved { termination }, None);
            }
        }
        return stationarity_bound(
            &layout,
            problem,
            params,
            options,
            &blocks,
            &penalties,
            lambdas,
            &roles,
            &pass,
            data_scale,
            warm,
            &mask,
            CertificateOrigin::PostFitRecertification,
        );
    }
    report(
        StationarityCertificate::Unresolved {
            termination: CertificateTermination::WorkLimit,
        },
        None,
    )
}

fn native_params_from_all(
    native: &Problem,
    all: &Problem,
    all_params: &Params,
    positions: &[Option<usize>],
) -> Params {
    let mut params = Params::initial(native.layout.clone(), native.contributors.len());
    let copy =
        |source: core::ops::Range<usize>, target: core::ops::Range<usize>, params: &mut Params| {
            params.theta[target].copy_from_slice(&all_params.theta[source]);
        };
    params.theta[native.layout.tone_c0] = all_params.theta[all.layout.tone_c0];
    copy(
        all.layout.tone_beta.clone(),
        native.layout.tone_beta.clone(),
        &mut params,
    );
    copy(
        all.layout.tint_a.clone(),
        native.layout.tint_a.clone(),
        &mut params,
    );
    copy(
        all.layout.tint_b.clone(),
        native.layout.tint_b.clone(),
        &mut params,
    );
    copy(all.layout.g.clone(), native.layout.g.clone(), &mut params);
    copy(all.layout.r.clone(), native.layout.r.clone(), &mut params);
    copy(
        all.layout.lambda.clone(),
        native.layout.lambda.clone(),
        &mut params,
    );
    copy(
        all.layout.residual.clone(),
        native.layout.residual.clone(),
        &mut params,
    );
    params.theta[native.layout.clip] = all_params.theta[all.layout.clip];
    for (index, contributor) in native.contributors.iter().enumerate() {
        if let Some(position) = all
            .contributors
            .iter()
            .position(|entry| entry == contributor)
        {
            params.theta[native.layout.sigma_of(index)] =
                all_params.theta[all.layout.sigma_of(position)];
        }
    }
    for (index, mode) in native.modes.iter().enumerate() {
        if let Some(position) = all.modes.iter().position(|entry| entry == mode) {
            params.theta[native.layout.mode_of(index)] =
                all_params.theta[all.layout.mode_of(position)];
        }
    }
    if !native.layout.response.is_empty() {
        copy(
            all.layout.response.clone(),
            native.layout.response.clone(),
            &mut params,
        );
    }
    for (original, position) in positions.iter().enumerate() {
        if let Some(position) = position {
            copy(
                all.layout.image(original),
                native.layout.image(*position),
                &mut params,
            );
        }
    }
    params
}

fn propagate_shared(
    native: &Problem,
    native_params: &Params,
    all: &Problem,
    all_params: &mut Params,
) {
    all_params.theta[all.layout.residual.clone()]
        .copy_from_slice(&native_params.theta[native.layout.residual.clone()]);
    for (index, contributor) in native.contributors.iter().enumerate() {
        if let Some(position) = all
            .contributors
            .iter()
            .position(|entry| entry == contributor)
        {
            all_params.theta[all.layout.sigma_of(position)] =
                native_params.theta[native.layout.sigma_of(index)];
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

/// The residual sum of squares excludes the domain barrier.
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
                rhs[*node as usize * 3 + channel] -=
                    weight * (sample.alpha * gradient + sample.penalty_gradient[channel]);
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
                diagonal[*node as usize * 3 + channel] +=
                    weight * weight * (sample.alpha * sum + sample.penalty_gn[channel][channel]);
            }
        }
    }
    diagonal
}

/// Residual data curvature `B_data v` over the coordinates selected by `local`, where `local`
/// maps a global residual coordinate to its index in `v`/`product`.
fn residual_data_accumulate<F: Fn(usize) -> Option<usize>>(
    samples: &[ResidualSample],
    v: &[f64],
    product: &mut [f64],
    hessian_scale: f64,
    local: F,
) {
    for sample in samples {
        let mut y = [0.0_f64; 3];
        let mut residual_projection = [0.0_f64; 3];
        for (m, node) in sample.nodes.iter().enumerate() {
            let weight = sample.weights[m];
            for channel in 0..3 {
                let Some(index) = local(*node as usize * 3 + channel) else {
                    continue;
                };
                let value = v[index];
                residual_projection[channel] += weight * value;
                for (row, slot) in y.iter_mut().enumerate() {
                    *slot += sample.clip.0[row][channel] * weight * value;
                }
            }
        }
        let mut penalty_projection = [0.0_f64; 3];
        for (row, total) in penalty_projection.iter_mut().enumerate() {
            for (column, value) in residual_projection.iter().enumerate() {
                *total += sample.penalty_curvature[row][column] * value;
            }
        }
        for (m, node) in sample.nodes.iter().enumerate() {
            let weight = hessian_scale * sample.weights[m];
            for channel in 0..3 {
                let Some(index) = local(*node as usize * 3 + channel) else {
                    continue;
                };
                let mut gradient = 0.0;
                for (row, value) in y.iter().enumerate() {
                    gradient += sample.clip.0[row][channel] * value;
                }
                product[index] += weight * (sample.alpha * gradient + penalty_projection[channel]);
            }
        }
    }
}

#[cfg(all(test, feature = "synth"))]
fn residual_undamped_accumulate(
    samples: &[ResidualSample],
    v: &[f64],
    product: &mut [f64],
    penalty_diagonal: &[f64],
    ridge: f64,
    identity: f64,
    hessian_scale: f64,
) {
    residual_data_accumulate(samples, v, product, hessian_scale, Some);
    for index in 0..RESIDUAL_COEFS {
        product[index] += ridge * laplacian_at(v, index, penalty_diagonal);
        product[index] += identity * v[index];
    }
}

#[cfg(all(test, feature = "synth"))]
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
    residual_undamped_accumulate(
        samples,
        v,
        product,
        penalty_diagonal,
        ridge,
        identity,
        hessian_scale,
    );
}

fn residual_step(
    pass: &PassOutput,
    theta: &[f64],
    lambda: f64,
    damping: f64,
    hessian_scale: f64,
    step_limit: StepLimit,
    mask: &ResidualMask,
) -> Vec<f64> {
    // The objective is `lambda * theta' P theta`, so the ridge matrix is 2 * lambda * P.
    let ridge = 2.0 * lambda;
    let identity = identity_weight(BlockKind::Residual);
    let space = ResidualSpace::new(mask, ridge);
    let dim = space.len();
    let data = residual_diagonal(&pass.residual_samples);
    let mut data_diagonal = vec![0.0_f64; dim];
    let mut diagonal = vec![0.0_f64; dim];
    let mut rhs = vec![0.0_f64; dim];
    for (compact, &local) in space.globals.iter().enumerate() {
        let base = (data[local] * hessian_scale).max(1e-9);
        data_diagonal[compact] = base;
        diagonal[compact] = base + ridge * space.diagonal[local] + damping * base + identity;
        rhs[compact] = pass.residual_gradient[local]
            - ridge * laplacian_at(theta, local, &space.diagonal)
            - identity * theta[local];
    }
    let mut solution = vec![0.0_f64; dim];
    crate::solve::conjugate_gradient(
        dim,
        |v, product| {
            space.apply(&pass.residual_samples, v, product, hessian_scale, identity);
            for index in 0..dim {
                product[index] += damping * data_diagonal[index] * v[index];
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
    let mut full = vec![0.0_f64; RESIDUAL_COEFS];
    for (compact, &local) in space.globals.iter().enumerate() {
        full[local] = solution[compact];
    }
    full
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
    fn frozen_recert_fails_closed_when_pinning_would_move_theta() {
        let mask = ResidualMask {
            free: vec![true, false],
        };
        let theta = vec![0.0, 1.0];
        assert!(pinned_residual_blocks(&mask, &theta, true));
        assert!(!pinned_residual_blocks(&mask, &theta, false));
        assert_eq!(theta[1], 1.0, "fail-closed path mutated theta");
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
        params.theta[layout.tone_c0] = layout.identity_mean(layout.tone_c0);

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
        let report = stationarity_bound(
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
            &ResidualMask::empty(),
            CertificateOrigin::FinalStage,
        );
        assert!(converged(&[100.0; 5]));
        assert!(!report.certificate.is_certified_stationary());
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
            axis_terms: vec![[0.0; DOMAIN_AXES]; 2],
            penalty: [[0.0; DOMAIN_AXES]; DOMAIN_AXES],
            penalty_gn: [[0.0; DOMAIN_AXES]; DOMAIN_AXES],
            curvature: None,
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
            &ResidualMask::empty(),
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
        block.full_diagonal = (0..n).map(|i| hessian[i * n + i]).collect();
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
        block.full_diagonal = system.block.full_diagonal.clone();
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
        dense_block.full_diagonal = system.block.full_diagonal.clone();
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

    #[cfg(feature = "synth")]
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
    #[cfg(feature = "synth")]
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
                        + penalty_pair(
                            &row.penalty_gn,
                            &row.axis_terms[position],
                            &row.axis_terms[other],
                        )
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
                    + identity[entry] * (params.theta[global] - layout.identity_mean(global));
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

    #[cfg(feature = "synth")]
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

    #[cfg(feature = "synth")]
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
            layout,
            &problem,
            &params,
            &options,
            &blocks,
            &penalties,
            &lambdas,
            &roles,
            &pass,
            1.0,
            robust,
            false,
            &ResidualMask::empty(),
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

    #[cfg(feature = "synth")]
    fn one_coordinate_certificate(identity_scale: f64, rhs: f64) -> StationarityReport {
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
        params.theta[layout.tone_c0] = layout.identity_mean(layout.tone_c0);
        let roles = prior_roles(&problem, layout);
        let mut block = BlockAcc::new(BlockKind::Tone, vec![layout.tone_c0], layout.total);
        block.hessian[0] = 4.0;
        block.full_diagonal[0] = 4.0;
        block.gradient[0] = -rhs;
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
            axis_terms: vec![[0.0; DOMAIN_AXES]; 1],
            penalty: [[0.0; DOMAIN_AXES]; DOMAIN_AXES],
            penalty_gn: [[0.0; DOMAIN_AXES]; DOMAIN_AXES],
            curvature: None,
            residual_columns: Vec::new(),
            layers: vec![None; 1],
            layer_weights: vec![[0.0; 4]; 1],
            layer_slopes: vec![0.0; 1],
            motion: [0.0; 3],
            tint: [0.0; 4],
            second: Box::new([0.0; 81]),
        }];
        stationarity_bound(
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
            &ResidualMask::empty(),
            CertificateOrigin::FinalStage,
        )
    }

    #[cfg(feature = "synth")]
    #[test]
    fn certificate_reports_the_exact_decrement_of_a_resolved_system() {
        let report = one_coordinate_certificate(1.0, 2.0);
        match report.certificate {
            StationarityCertificate::CertifiedNonstationary { lower } => {
                assert!((lower.get() - 0.4).abs() < 1e-9, "lower {}", lower.get());
            }
            other => panic!("expected a nonstationary lower bound, got {other:?}"),
        }
    }

    #[cfg(feature = "synth")]
    #[test]
    fn certificate_rejects_a_missing_prior_floor() {
        let report = one_coordinate_certificate(0.0, 2.0);
        assert!(
            !report.certificate.is_certified_stationary(),
            "zero prior floor certified stationarity: {:?}",
            report.certificate
        );
    }

    #[cfg(feature = "synth")]
    #[test]
    fn zero_reduced_rhs_certifies_an_exact_stationary_state() {
        let report = one_coordinate_certificate(1.0, 0.0);
        match report.certificate {
            StationarityCertificate::CertifiedStationary { upper } => {
                assert_eq!(upper.get(), 0.0);
            }
            other => panic!("expected a stationary upper bound, got {other:?}"),
        }
    }

    #[cfg(feature = "synth")]
    #[test]
    fn zero_reduced_rhs_recovers_nonzero_image_motion() {
        let data = crate::synth::dataset(&crate::synth::SynthConfig {
            images: 1,
            samples_per_image: 8,
            ..crate::synth::SynthConfig::default()
        });
        let options = FitOptions {
            exposure_prior_sd: 1.0,
            ..FitOptions::default()
        };
        let problem = Problem::new_with(&data.images, None, &options).expect("problem");
        let layout = &problem.layout;
        let params = initial_params(&problem, &options);
        let roles = prior_roles(&problem, layout);
        let blocks = [
            BlockAcc::new(BlockKind::Tone, vec![layout.tone_c0], layout.total),
            BlockAcc::new(
                BlockKind::Nuisance,
                vec![layout.image(0).start],
                layout.total,
            ),
        ];
        let pass = problem.pass(
            &params,
            ActiveColumns::ALL,
            &mut [],
            false,
            false,
            false,
            IntentFilter::NativeOnly,
        );
        let mut model = JointModel::new(
            layout,
            &problem,
            &params,
            &options,
            &blocks,
            &[vec![0.0], vec![0.0]],
            &options.initial_lambdas,
            &roles,
            &pass,
            1.0,
            false,
            false,
            &ResidualMask::empty(),
        )
        .expect("joint model");
        model.rhs.fill(0.0);
        model.rhs[model.image_range[0].0] = 2.0;
        let solve = JointSolve::new(&model, 0.0).expect("factor");
        let solution = solve
            .solve(&model, CERTIFICATE_TOLERANCE, None, None)
            .expect("recover image motion");
        assert_eq!(solution.delta, [0.0, 2.0]);
        assert_eq!(solution.relative_residual, 0.0);
    }
}

#[cfg(all(test, feature = "synth"))]
mod coupled_tests;
#[cfg(feature = "synth")]
pub mod fallback_ab;
#[cfg(all(test, feature = "synth"))]
mod tone_prior_tests;

/// Exact-Hessian harness for the joint step model (test only).
#[cfg(all(test, feature = "synth"))]
mod curvature_tests {
    use super::*;

    struct Fixture {
        problem: Problem,
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
            Self {
                problem,
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
}

/// Held-image exposure/WB conditioning with a known Look (test only).
#[cfg(all(test, feature = "synth"))]
mod held_nuisance_tests {
    use super::*;

    struct Held {
        problem: Problem,
        options: FitOptions,
        active: ActiveColumns,
        blocks: Vec<BlockAcc>,
        penalties: Vec<Vec<f64>>,
        roles: Vec<Role>,
        params: Params,
        lambdas: StageLambdas,
    }

    impl Held {
        fn new(
            images: &[FitImage],
            look: &looklab_model::Look,
            options: &FitOptions,
            clip_k: f64,
        ) -> Self {
            let problem = Problem::new_with(images, None, options).expect("problem");
            let layout = problem.layout.clone();
            let mut params = params_from_look(&problem, look);
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
            let blocks: Vec<BlockAcc> = (0..problem.images.len())
                .map(|index| make_image_block(&layout, index, active))
                .collect();
            let penalties: Vec<Vec<f64>> = blocks
                .iter()
                .map(|block| block_penalty(&problem, block))
                .collect();
            let roles = prior_roles(&problem, &layout);
            Self {
                problem,
                options: *options,
                active,
                blocks,
                penalties,
                roles,
                params,
                lambdas: options.initial_lambdas,
            }
        }

        /// Objective and pass at `params`, with the image blocks rebuilt.
        fn objective_at(&mut self, params: &Params) -> (f64, usize, usize, bool) {
            for block in &mut self.blocks {
                block.reset();
            }
            let pass = self.problem.pass(
                params,
                self.active,
                &mut self.blocks,
                false,
                false,
                false,
                IntentFilter::All,
            );
            let cost = pass.cost
                + total_cost(
                    &self.problem,
                    params,
                    &self.blocks,
                    &self.penalties,
                    &self.lambdas,
                    &self.options,
                );
            (
                cost,
                pass.active_samples,
                pass.hessian_rows,
                pass.infeasible,
            )
        }

        fn objective(&mut self) -> (f64, usize, usize, bool) {
            let params = self.params.clone();
            self.objective_at(&params)
        }

        /// One diagnostic row per image block.
        fn probe(&mut self, label: &str) {
            let (cost, active, rows, infeasible) = self.objective();
            let scale = active.max(1) as f64 / rows.max(1) as f64;
            eprintln!(
                "{label}: cost {cost:.12e} samples {active} hessian-rows {rows} infeasible {infeasible} scale {scale:.6e}"
            );
            for index in 0..self.blocks.len() {
                let n = self.blocks[index].len();
                let (
                    gradient,
                    corrected,
                    gn,
                    off,
                    factors,
                    solved,
                    predicted,
                    block_step,
                    globals,
                    model_matrix,
                ) = {
                    let block = &self.blocks[index];
                    let n = block.len();
                    let lambda = block_lambda(block, &self.lambdas);
                    let gradient = block_gradients(
                        &self.problem.layout,
                        &self.problem,
                        &self.params,
                        &self.options,
                        block,
                        &self.penalties[index],
                        lambda,
                        &self.roles,
                    );
                    let (prior_gradient, prior_curvature) = prior_terms(
                        &self.problem,
                        &self.roles,
                        &self.params,
                        &self.options,
                        block,
                    );
                    let corrected: Vec<f64> =
                        (0..n).map(|i| block.hessian[i * n + i] * scale).collect();
                    let gn = block.full_diagonal.clone();
                    let mut factors = Vec::new();
                    for damping in [1e-3, 1e0, 1e3, 1e6] {
                        let (a, _) = block_system(
                            &self.problem.layout,
                            block,
                            &self.penalties[index],
                            &prior_gradient,
                            &prior_curvature,
                            &self.params,
                            lambda,
                            damping,
                            scale,
                            StepLimit::WholeStep,
                        );
                        let mut factor = a.clone();
                        if factor_in_place(&mut factor, n) {
                            factors.push(damping);
                        }
                    }
                    let mut step = vec![0.0_f64; self.problem.layout.total];
                    let outcome = solve_block(
                        &self.problem.layout,
                        block,
                        &mut step,
                        &self.penalties[index],
                        &prior_gradient,
                        &prior_curvature,
                        &self.params,
                        &self.lambdas,
                        1e-3,
                        scale,
                        StepLimit::WholeStep,
                    );
                    let predicted = match outcome {
                        BlockStep::Solved(value) => value,
                        BlockStep::Failed => f64::NAN,
                    };
                    let globals = block.params.clone();
                    let block_step: Vec<f64> = globals.iter().map(|global| step[*global]).collect();
                    let off: Vec<f64> = (0..n)
                        .flat_map(|i| ((i + 1)..n).map(move |j| block.hessian[i * n + j] * scale))
                        .collect();
                    let (model_matrix, _) = block_system(
                        &self.problem.layout,
                        block,
                        &self.penalties[index],
                        &prior_gradient,
                        &prior_curvature,
                        &self.params,
                        lambda,
                        0.0,
                        scale,
                        StepLimit::WholeStep,
                    );
                    (
                        gradient,
                        corrected,
                        gn,
                        off,
                        factors,
                        matches!(outcome, BlockStep::Solved(_)),
                        predicted,
                        block_step,
                        globals,
                        model_matrix,
                    )
                };
                let largest = gradient
                    .iter()
                    .fold(0.0_f64, |largest, value| largest.max(value.abs()));
                let t = if largest > 0.0 { 1e-3 / largest } else { 0.0 };
                let mut descent = self.params.clone();
                for (local, global) in globals.iter().enumerate() {
                    descent.theta[*global] -= t * gradient[local];
                }
                let descent_cost = self.objective_at(&descent).0;
                let mut trial = self.params.clone();
                for (local, global) in globals.iter().enumerate() {
                    trial.theta[*global] += block_step[local];
                }
                let trial_cost = self.objective_at(&trial).0;
                let largest_step = block_step
                    .iter()
                    .fold(0.0_f64, |largest, value| largest.max(value.abs()));
                let h = 1e-3;
                let (model_curvature, true_curvature) = if largest_step > 0.0 {
                    let direction: Vec<f64> = block_step
                        .iter()
                        .map(|value| value / largest_step)
                        .collect();
                    let mut plus = self.params.clone();
                    let mut minus = self.params.clone();
                    for (local, global) in globals.iter().enumerate() {
                        plus.theta[*global] += h * direction[local];
                        minus.theta[*global] -= h * direction[local];
                    }
                    let plus_cost = self.objective_at(&plus).0;
                    let minus_cost = self.objective_at(&minus).0;
                    let mut curvature = 0.0_f64;
                    for i in 0..n {
                        let mut row = 0.0_f64;
                        for j in 0..n {
                            row += model_matrix[i * n + j] * direction[j];
                        }
                        curvature += direction[i] * row;
                    }
                    (curvature, (plus_cost - 2.0 * cost + minus_cost) / (h * h))
                } else {
                    (f64::NAN, f64::NAN)
                };
                eprintln!(
                    "{label} block {index} model-curvature {model_curvature:.6e} true-curvature {true_curvature:.6e} ratio {:.6e}",
                    model_curvature / true_curvature
                );
                eprintln!(
                    "{label} block {index} gradient {gradient:?} corrected-diagonal {corrected:?} gn-diagonal {gn:?} off-diagonal {off:?}"
                );
                eprintln!(
                    "{label} block {index} factor-at {factors:?} solved {solved} predicted {predicted:.6e} step {block_step:?}"
                );
                eprintln!(
                    "{label} block {index} cost {cost:.12e} step-cost {trial_cost:.12e} descent-cost {descent_cost:.12e} t {t:.3e}"
                );
            }
        }
    }

    fn fixture(seed: u64, exposure_sd: f64, wb_sd: f64) -> crate::synth::SyntheticDataset {
        crate::synth::dataset(&crate::synth::SynthConfig {
            seed,
            images: 4,
            samples_per_image: 200,
            exposure_sd,
            wb_sd,
            ..crate::synth::SynthConfig::default()
        })
    }

    fn options() -> FitOptions {
        FitOptions {
            max_samples_per_image: 200,
            hessian_samples: 500,
            max_sweeps: 64,
            sweep_budget: 2_000_000.0,
            ..FitOptions::default()
        }
    }

    #[test]
    fn held_nuisance_conditioning_converges_with_a_known_look() {
        let data = fixture(0x5eed_1234, 0.25, 0.015);
        let options = options();
        let held = &data.images[..2];
        let mut state = Held::new(held, &data.truth, &options, data.clip_k);
        state.probe("known-look");
        let conditioned =
            condition_nuisances(held, &options, &data.truth, None, data.clip_k).expect("condition");
        eprintln!(
            "known-look conditioned converged {} accepted {} attempts {} damping {:.3e} step {:.3e} gradient {:.3e}",
            conditioned.converged,
            conditioned.accepted,
            conditioned.attempts,
            conditioned.damping,
            conditioned.last_step,
            conditioned.gradient_max
        );
        assert!(
            conditioned.converged,
            "conditioning did not converge: accepted {} attempts {} damping {:.3e} gradient {:.3e}",
            conditioned.accepted,
            conditioned.attempts,
            conditioned.damping,
            conditioned.gradient_max
        );
        assert!(
            conditioned.accepted > 0,
            "the injected offsets must need a step: gradient {:.3e}",
            conditioned.gradient_max
        );
        assert!(
            conditioned.gradient_max <= STATIONARY_GRADIENT,
            "converged at gradient {:.3e}",
            conditioned.gradient_max
        );
        let mut tail = Held::new(held, &data.truth, &options, data.clip_k);
        for (index, nuisance) in conditioned.images.iter().enumerate() {
            let range = tail.problem.layout.image(index);
            tail.params.theta[range.start] = nuisance.exposure;
            tail.params.theta[range.start + 1] = nuisance.wb_log_r;
            tail.params.theta[range.start + 2] = nuisance.wb_log_b;
        }
        tail.probe("conditioned");
        for (index, nuisance) in conditioned.images.iter().enumerate() {
            let truth = data.nuisances[index];
            eprintln!(
                "known-look image {index} exposure {:.6e} (truth {:.6e}) wb-r {:.6e} (truth {:.6e}) wb-b {:.6e} (truth {:.6e})",
                nuisance.exposure,
                truth.exposure,
                nuisance.wb_log_r,
                truth.wb_log_r,
                nuisance.wb_log_b,
                truth.wb_log_b
            );
            assert!(
                (nuisance.exposure - truth.exposure).abs() < 0.1,
                "image {index} exposure {} against {}",
                nuisance.exposure,
                truth.exposure
            );
            assert!(
                (nuisance.wb_log_r - truth.wb_log_r).abs() < 0.02
                    && (nuisance.wb_log_b - truth.wb_log_b).abs() < 0.02,
                "image {index} WB {} {} against {} {}",
                nuisance.wb_log_r,
                nuisance.wb_log_b,
                truth.wb_log_r,
                truth.wb_log_b
            );
        }
    }

    #[test]
    fn held_nuisance_conditioning_recovers_a_zero_injection_fixture() {
        let data = crate::synth::dataset(&crate::synth::SynthConfig {
            seed: 0x5eed_1234,
            images: 2,
            samples_per_image: 200,
            noise_ucs: 0.0,
            exposure_sd: 0.0,
            wb_sd: 0.0,
            hint_sd: 0.0,
            ..crate::synth::SynthConfig::default()
        });
        let options = options();
        let mut state = Held::new(&data.images, &data.truth, &options, data.clip_k);
        state.probe("zero-injection");
        let conditioned =
            condition_nuisances(&data.images, &options, &data.truth, None, data.clip_k)
                .expect("condition");
        eprintln!(
            "zero-injection conditioned converged {} accepted {} attempts {} damping {:.3e} step {:.3e} gradient {:.3e}",
            conditioned.converged,
            conditioned.accepted,
            conditioned.attempts,
            conditioned.damping,
            conditioned.last_step,
            conditioned.gradient_max
        );
        assert!(
            conditioned.converged,
            "zero-injection conditioning did not converge: accepted {} attempts {} damping {:.3e} gradient {:.3e}",
            conditioned.accepted,
            conditioned.attempts,
            conditioned.damping,
            conditioned.gradient_max
        );
        assert!(
            conditioned.gradient_max <= STATIONARY_GRADIENT,
            "converged at gradient {:.3e}",
            conditioned.gradient_max
        );
        for nuisance in &conditioned.images {
            assert!(
                nuisance.exposure.abs() <= options.exposure_prior_sd,
                "exposure {} outside one prior sigma",
                nuisance.exposure
            );
            assert!(
                nuisance.wb_log_r.abs() <= options.wb_prior_sd
                    && nuisance.wb_log_b.abs() <= options.wb_prior_sd,
                "WB {} {} outside one prior sigma",
                nuisance.wb_log_r,
                nuisance.wb_log_b
            );
        }
    }
}

/// One-shot diagnostic of the stalled fold-1 joint step (test only).
#[cfg(all(test, feature = "synth"))]
mod fold_probe {
    use super::*;

    struct State {
        problem: Problem,
        layout: Layout,
        options: FitOptions,
        blocks: Vec<BlockAcc>,
        penalties: Vec<Vec<f64>>,
        roles: Vec<Role>,
        params: Params,
        lambdas: StageLambdas,
        pass: PassOutput,
        data_scale: f64,
        cost: f64,
    }

    impl State {
        /// Objective and feasibility at `params`, with the final-stage pass.
        fn objective(&self, params: &Params) -> (f64, bool) {
            let trial = self.problem.pass(
                params,
                STAGES[4].active,
                &mut [],
                false,
                false,
                STAGES[4].neutral,
                IntentFilter::NativeOnly,
            );
            let cost = trial.cost
                + total_cost(
                    &self.problem,
                    params,
                    &self.blocks,
                    &self.penalties,
                    &self.lambdas,
                    &self.options,
                );
            (cost, trial.infeasible)
        }

        /// Objective split into pass cost, prior, penalty plus identity, and the residual block.
        fn components(&self, params: &Params) -> [f64; 4] {
            let trial = self.problem.pass(
                params,
                STAGES[4].active,
                &mut [],
                false,
                false,
                STAGES[4].neutral,
                IntentFilter::NativeOnly,
            );
            let mut penalty = 0.0_f64;
            let mut identity = 0.0_f64;
            let mut residual = 0.0_f64;
            for (index, block) in self.blocks.iter().enumerate() {
                if block.kind == BlockKind::Residual {
                    residual += residual_cost(
                        &self.layout,
                        &params.theta,
                        lam(&self.lambdas, BlockKind::Residual),
                    );
                    continue;
                }
                penalty += block_lambda(block, &self.lambdas)
                    * penalty_value(&params.theta, &block.params, &self.penalties[index]);
                identity += identity_cost(&self.layout, block, &params.theta);
            }
            [
                trial.cost,
                prior_cost(&self.problem, params, &self.options),
                penalty + identity,
                residual,
            ]
        }
    }

    fn fold_one_state() -> State {
        let data = crate::synth::dataset(&crate::synth::SynthConfig {
            images: 4,
            samples_per_image: 200,
            ..crate::synth::SynthConfig::default()
        });
        let options = FitOptions {
            max_samples_per_image: 200,
            hessian_samples: 500,
            max_sweeps: 64,
            sweep_budget: 2_000_000.0,
            ..FitOptions::default()
        };
        let kept: Vec<FitImage> = data.images[..2].to_vec();
        let result = run_quick(&kept, &options, None, None).expect("quick fit");
        eprintln!(
            "fold1 fit converged {} sweeps {} evaluations {} cost {:.12e} excluded {}",
            result.diagnostics.converged,
            result.diagnostics.sweeps,
            result.diagnostics.evaluations,
            result.diagnostics.final_cost,
            result.diagnostics.excluded_samples
        );
        let problem = Problem::new_with(&kept, None, &options).expect("problem");
        let layout = problem.layout.clone();
        let mut params = params_from_look(&problem, &result.look);
        for (index, nuisance) in result.images.iter().enumerate() {
            let range = layout.image(index);
            params.theta[range.start] = nuisance.exposure;
            params.theta[range.start + 1] = nuisance.wb_log_r;
            params.theta[range.start + 2] = nuisance.wb_log_b;
        }
        for (contributor, sigma) in &result.diagnostics.sigma_v {
            if let Some(index) = problem
                .contributors
                .iter()
                .position(|value| value == contributor)
            {
                params.theta[layout.sigma_of(index)] = libm::log(*sigma);
            }
        }
        params.theta[layout.clip] = crate::layout::zeta_from_clip(result.realization.clip_k);
        let lambdas = result.lambdas;
        let roles = prior_roles(&problem, &layout);
        let kinds: Vec<BlockKind> = STAGES[4]
            .blocks
            .iter()
            .copied()
            .filter(|kind| options.fit_residual || *kind != BlockKind::Residual)
            .filter(|kind| !is_response_kind(*kind))
            .collect();
        let mut blocks: Vec<BlockAcc> = kinds
            .iter()
            .map(|kind| {
                if *kind == BlockKind::Tone {
                    make_joint_tone_block(&problem, STAGES[4].active)
                } else {
                    make_block(&problem, *kind, STAGES[4].active)
                }
            })
            .collect();
        let penalties: Vec<Vec<f64>> = blocks
            .iter()
            .map(|block| block_penalty(&problem, block))
            .collect();
        let pass = problem.pass_with_rows(
            &params,
            STAGES[4].active,
            &mut blocks,
            false,
            false,
            STAGES[4].neutral,
            IntentFilter::NativeOnly,
            true,
            problem.samples.len() <= FULL_ROW_LIMIT,
        );
        let data_scale = pass.active_samples.max(1) as f64 / pass.hessian_rows.max(1) as f64;
        let cost =
            pass.cost + total_cost(&problem, &params, &blocks, &penalties, &lambdas, &options);
        State {
            problem,
            layout,
            options,
            blocks,
            penalties,
            roles,
            params,
            lambdas,
            pass,
            data_scale,
            cost,
        }
    }

    fn shifted(state: &State, model: &JointModel<'_>, delta: &[f64], h: f64) -> Params {
        let mut trial = state.params.clone();
        for (index, value) in delta.iter().enumerate() {
            trial.theta[model.globals[index]] += h * value;
        }
        trial
    }

    /// Joint step on the joint globals, the independent residual step, then the pin.
    fn trial_params(
        state: &State,
        model: &JointModel<'_>,
        delta: &[f64],
        residual_delta: &[f64],
    ) -> Params {
        let mut step = vec![0.0_f64; state.layout.total];
        for (index, value) in delta.iter().enumerate() {
            step[model.globals[index]] = *value;
        }
        let mut trial = state.params.clone();
        apply_step_unclamped(&mut trial, &step);
        for (index, value) in residual_delta.iter().enumerate() {
            trial.theta[state.layout.residual.start + index] += value;
        }
        if !residual_delta.is_empty() {
            ResidualMask::from_samples(&state.pass.residual_samples)
                .pin(&mut trial.theta[state.layout.residual.clone()]);
        }
        trial
    }

    fn residual_shifted(state: &State, residual_delta: &[f64], h: f64) -> Params {
        let mut trial = state.params.clone();
        for (index, value) in residual_delta.iter().enumerate() {
            trial.theta[state.layout.residual.start + index] += h * value;
        }
        trial
    }

    fn mixed_shifted(
        state: &State,
        model: &JointModel<'_>,
        delta: &[f64],
        joint_h: f64,
        residual_delta: &[f64],
        residual_h: f64,
    ) -> Params {
        let mut trial = shifted(state, model, delta, joint_h);
        for (index, value) in residual_delta.iter().enumerate() {
            trial.theta[state.layout.residual.start + index] += residual_h * value;
        }
        trial
    }

    fn masked_delta(
        state: &State,
        model: &JointModel<'_>,
        delta: &[f64],
        groups: &[usize],
    ) -> Vec<f64> {
        let mut masked = delta.to_vec();
        for (index, value) in masked.iter_mut().enumerate() {
            let slot = match state.roles[model.globals[index]] {
                Role::None => 0,
                Role::Exposure(_) | Role::Wb(_) => 1,
                Role::Mode(_) => 2,
                Role::Clip => 3,
            };
            if !groups.contains(&slot) {
                *value = 0.0;
            }
        }
        masked
    }

    fn stage_blocks(problem: &Problem, options: &FitOptions) -> (Vec<BlockAcc>, Vec<Vec<f64>>) {
        let kinds: Vec<BlockKind> = STAGES[4]
            .blocks
            .iter()
            .copied()
            .filter(|kind| options.fit_residual || *kind != BlockKind::Residual)
            .filter(|kind| !is_response_kind(*kind))
            .collect();
        let blocks: Vec<BlockAcc> = kinds
            .iter()
            .map(|kind| {
                if *kind == BlockKind::Tone {
                    make_joint_tone_block(problem, STAGES[4].active)
                } else {
                    make_block(problem, *kind, STAGES[4].active)
                }
            })
            .collect();
        let penalties: Vec<Vec<f64>> = blocks
            .iter()
            .map(|block| block_penalty(problem, block))
            .collect();
        (blocks, penalties)
    }

    /// Full objective gradient, accumulated from the stage blocks.
    fn joint_gradient(state: &State, model: &JointModel<'_>, params: &Params) -> Vec<f64> {
        let (mut blocks, penalties) = stage_blocks(&state.problem, &state.options);
        state.problem.pass(
            params,
            STAGES[4].active,
            &mut blocks,
            false,
            false,
            STAGES[4].neutral,
            IntentFilter::NativeOnly,
        );
        let mut joint_of = vec![u32::MAX; state.layout.total];
        for (index, global) in model.globals.iter().enumerate() {
            joint_of[*global] = index as u32;
        }
        let mut gradient = vec![0.0_f64; model.dim];
        for (index, block) in blocks.iter().enumerate() {
            let lambda = block_lambda(block, &state.lambdas);
            let values = block_gradients(
                &state.layout,
                &state.problem,
                params,
                &state.options,
                block,
                &penalties[index],
                lambda,
                &state.roles,
            );
            for (local, global) in block.params.iter().enumerate() {
                let joint = joint_of[*global];
                if joint != u32::MAX {
                    gradient[joint as usize] += values[local];
                }
            }
        }
        gradient
    }

    /// Second-order operator with the symmetrized sparse image and mixed correction, plus LM damping.
    fn corrected_apply(
        second: &JointModel<'_>,
        columns: &[Vec<f64>],
        indices: &[usize],
        damping: f64,
        v: &[f64],
        out: &mut [f64],
    ) {
        second.apply_undamped(v, out);
        for (column, index) in columns.iter().zip(indices) {
            let weight = v[*index];
            if weight != 0.0 {
                for (slot, value) in out.iter_mut().enumerate() {
                    *value += weight * column[slot];
                }
            }
        }
        for (column, index) in columns.iter().zip(indices) {
            out[*index] += crate::solve::dot(column, v);
        }
        for (slot, index) in indices.iter().enumerate() {
            let mut value = 0.0_f64;
            for (other, other_index) in indices.iter().enumerate() {
                let symmetric = 0.5 * (columns[other][*index] + columns[slot][*other_index]);
                value += symmetric * v[*other_index];
            }
            out[*index] -= value;
        }
        for (index, value) in out.iter_mut().enumerate() {
            *value += damping * second.data_diagonal[index].max(1e-12) * v[index];
        }
    }

    fn cold_certificate(state: &State, params: &Params) -> StationarityReport {
        let (mut blocks, penalties) = stage_blocks(&state.problem, &state.options);
        let pass = state.problem.pass_with_rows(
            params,
            STAGES[4].active,
            &mut blocks,
            false,
            false,
            STAGES[4].neutral,
            IntentFilter::NativeOnly,
            true,
            state.problem.samples.len() <= FULL_ROW_LIMIT,
        );
        let data_scale = pass.active_samples.max(1) as f64 / pass.hessian_rows.max(1) as f64;
        let mut warm: Option<Vec<f64>> = None;
        let mask = ResidualMask::from_samples(&pass.residual_samples);
        stationarity_bound(
            &state.layout,
            &state.problem,
            params,
            &state.options,
            &blocks,
            &penalties,
            &state.lambdas,
            &state.roles,
            &pass,
            data_scale,
            &mut warm,
            &mask,
            CertificateOrigin::FinalStage,
        )
    }

    fn reported_bound(report: &StationarityReport) -> f64 {
        match report.certificate {
            StationarityCertificate::CertifiedStationary { upper } => upper.get(),
            StationarityCertificate::CertifiedNonstationary { lower } => lower.get(),
            StationarityCertificate::Unresolved { .. } => f64::NAN,
        }
    }

    #[test]
    fn fold_one_stalled_joint_step_terms() {
        let state = fold_one_state();
        eprintln!(
            "fold1 state cost {:.12e} samples {} hessian-rows {} scale {:.6e} infeasible {}",
            state.cost,
            state.pass.active_samples,
            state.pass.hessian_rows,
            state.data_scale,
            state.pass.infeasible
        );
        let mut warm: Option<Vec<f64>> = None;
        let mask = ResidualMask::from_samples(&state.pass.residual_samples);
        let report = stationarity_bound(
            &state.layout,
            &state.problem,
            &state.params,
            &state.options,
            &state.blocks,
            &state.penalties,
            &state.lambdas,
            &state.roles,
            &state.pass,
            state.data_scale,
            &mut warm,
            &mask,
            CertificateOrigin::FinalStage,
        );
        eprintln!(
            "fold1 certificate bound {:.6e} nats verdict {:?}",
            reported_bound(&report),
            report.certificate
        );
        let residual_active = state
            .blocks
            .iter()
            .any(|block| block.kind == BlockKind::Residual);
        for (damping, detailed) in [
            (8.61e-6, true),
            (2.583e-6, false),
            (1e-3, false),
            (1e-1, false),
        ] {
            let model = match JointModel::new(
                &state.layout,
                &state.problem,
                &state.params,
                &state.options,
                &state.blocks,
                &state.penalties,
                &state.lambdas,
                &state.roles,
                &state.pass,
                state.data_scale,
                true,
                false,
                &ResidualMask::from_samples(&state.pass.residual_samples),
            ) {
                Ok(model) => model,
                Err(failure) => {
                    eprintln!(
                        "fold1 damping {damping:.3e} joint model failed: {}",
                        failure.label()
                    );
                    continue;
                }
            };
            let solve = match JointSolve::new(&model, damping) {
                Ok(solve) => solve,
                Err(failure) => {
                    eprintln!(
                        "fold1 damping {damping:.3e} preconditioner failed: {}",
                        failure.label()
                    );
                    continue;
                }
            };
            let solution = match solve.solve(&model, 0.02, Some(STEP_RADIUS), None) {
                Ok(solution) => solution,
                Err(failure) => {
                    eprintln!(
                        "fold1 damping {damping:.3e} solve failed: {}",
                        failure.label()
                    );
                    continue;
                }
            };
            let diagonal = |values: &[f64]| {
                values
                    .iter()
                    .fold((f64::INFINITY, f64::NEG_INFINITY), |(low, high), value| {
                        (low.min(*value), high.max(*value))
                    })
            };
            let (data_low, data_high) = diagonal(&model.data_diagonal);
            let (fixed_low, fixed_high) = diagonal(&model.fixed);
            let rhs_norm = crate::solve::dot(&model.rhs, &model.rhs).sqrt();
            let linear = crate::solve::dot(&model.rhs, &solution.delta);
            let model_curvature = crate::solve::dot(&solution.delta, &solution.curvature);
            let delta_max = solution
                .delta
                .iter()
                .fold(0.0_f64, |largest, value| largest.max(value.abs()));
            let mut step = vec![0.0_f64; state.layout.total];
            for (index, value) in solution.delta.iter().enumerate() {
                step[model.globals[index]] = *value;
            }
            let residual_delta = if residual_active {
                residual_step(
                    &state.pass,
                    &state.params.theta[state.layout.residual.clone()],
                    lam(&state.lambdas, BlockKind::Residual),
                    damping,
                    state.data_scale,
                    StepLimit::PerBlock,
                    &ResidualMask::from_samples(&state.pass.residual_samples),
                )
            } else {
                Vec::new()
            };
            let residual_max = residual_delta
                .iter()
                .fold(0.0_f64, |largest, value| largest.max(value.abs()));
            let full = trial_params(&state, &model, &solution.delta, &residual_delta);
            let (full_cost, full_infeasible) = state.objective(&full);
            let full_decrease = state.cost - full_cost;
            let mut residual_part = 0.0_f64;
            let mut cross_part = 0.0_f64;
            if residual_active {
                let effective: Vec<f64> = (0..RESIDUAL_COEFS)
                    .map(|index| {
                        full.theta[state.layout.residual.start + index]
                            - state.params.theta[state.layout.residual.start + index]
                    })
                    .collect();
                residual_part = residual_prediction(
                    &state.layout,
                    &state.pass,
                    &state.params,
                    &effective,
                    &state.lambdas,
                    state.data_scale,
                );
                cross_part = residual_cross(
                    &state.pass.coupled_rows,
                    &state.layout.residual,
                    model.robust,
                    &step,
                    &effective,
                    state.data_scale,
                );
            }
            let full_prediction = solution.prediction + residual_part - cross_part;
            let joint = trial_params(&state, &model, &solution.delta, &[]);
            let (joint_cost, joint_infeasible) = state.objective(&joint);
            let joint_decrease = state.cost - joint_cost;
            let joint_ratio = joint_decrease / solution.prediction;
            let implied_curvature = 2.0 * linear - 2.0 * joint_ratio * solution.prediction;
            eprintln!(
                "fold1 damping {damping:.3e} joint-solve dim {} outer {} iterations {} residual {:.3e} failure {:?} delta-max {delta_max:.3e} rhs-norm {rhs_norm:.6e}",
                model.dim,
                model.outer_count,
                solution.iterations,
                solution.relative_residual,
                solution.failure.map(SolveFailure::label),
            );
            eprintln!(
                "fold1 damping {damping:.3e} production prediction {full_prediction:.6e} joint {:.6e} residual {residual_part:.6e} cross {cross_part:.6e} decrease {full_decrease:.6e} ratio {:.6e} infeasible {full_infeasible} residual-delta-max {residual_max:.3e}",
                solution.prediction,
                full_decrease / full_prediction,
            );
            eprintln!(
                "fold1 damping {damping:.3e} joint-only linear {linear:.6e} model-curvature {model_curvature:.6e} prediction {:.6e} decrease {joint_decrease:.6e} ratio {joint_ratio:.6e} implied-curvature {implied_curvature:.6e} infeasible {joint_infeasible}",
                solution.prediction,
            );
            eprintln!(
                "fold1 damping {damping:.3e} damping-terms data-diagonal [{data_low:.6e}, {data_high:.6e}] fixed [{fixed_low:.6e}, {fixed_high:.6e}] damping-scale [{:.6e}, {:.6e}]",
                damping * data_low.max(1e-12),
                damping * data_high.max(1e-12),
            );
            for h in [1.0, 1e-1, 1e-2, 1e-3] {
                let (plus_cost, plus_infeasible) =
                    state.objective(&shifted(&state, &model, &solution.delta, h));
                let (minus_cost, minus_infeasible) =
                    state.objective(&shifted(&state, &model, &solution.delta, -h));
                let curvature = (plus_cost - 2.0 * state.cost + minus_cost) / (h * h);
                let slope = (plus_cost - minus_cost) / (2.0 * h);
                eprintln!(
                    "fold1 damping {damping:.3e} h {h:.1e} fd-curvature {curvature:.6e} fd-slope {slope:.6e} plus-infeasible {plus_infeasible} minus-infeasible {minus_infeasible}"
                );
            }
            if detailed {
                let mut q_gn = 0.0_f64;
                let mut q_robust = 0.0_f64;
                let mut q_domain = 0.0_f64;
                for row in &state.pass.coupled_rows {
                    let mut projected = [0.0_f64; 3];
                    let mut robust_projection = 0.0_f64;
                    let mut axis_projection = [0.0_f64; DOMAIN_AXES];
                    for (position, (column, derivative)) in row.columns.iter().enumerate() {
                        let value = step[*column as usize];
                        projected[0] += derivative[0] * value;
                        projected[1] += derivative[1] * value;
                        projected[2] += derivative[2] * value;
                        robust_projection += row.robust_terms[position] * value;
                        for (axis, total) in axis_projection.iter_mut().enumerate() {
                            *total += row.axis_terms[position][axis] * value;
                        }
                    }
                    q_gn += row.weight * crate::solve::dot(&projected, &projected);
                    q_robust += row.robust * robust_projection * robust_projection;
                    q_domain += penalty_pair(&row.penalty, &axis_projection, &axis_projection);
                }
                let q_fixed: f64 = solution
                    .delta
                    .iter()
                    .enumerate()
                    .map(|(index, value)| model.fixed[index] * value * value)
                    .sum();
                let q_penalty: f64 = model
                    .penalty
                    .iter()
                    .map(|(row, column, value)| {
                        value * solution.delta[*row as usize] * solution.delta[*column as usize]
                    })
                    .sum();
                let decomposed = q_fixed + q_penalty + q_gn + q_robust + q_domain;
                eprintln!(
                    "fold1 curvature-terms fixed {q_fixed:.6e} penalty {q_penalty:.6e} gn {q_gn:.6e} robust {q_robust:.6e} domain {q_domain:.6e} sum {decomposed:.6e} model {model_curvature:.6e} gap {:.3e}",
                    decomposed - model_curvature
                );
                let base_components = state.components(&state.params);
                for h in [1e-1, 1e-3] {
                    let plus = state.components(&shifted(&state, &model, &solution.delta, h));
                    let minus = state.components(&shifted(&state, &model, &solution.delta, -h));
                    let split: [f64; 4] = core::array::from_fn(|index| {
                        (plus[index] - 2.0 * base_components[index] + minus[index]) / (h * h)
                    });
                    eprintln!(
                        "fold1 fd-components h {h:.1e} pass-data-plus-barrier {:.6e} prior {:.6e} penalty-plus-identity {:.6e} residual {:.6e} total {:.6e}",
                        split[0],
                        split[1],
                        split[2],
                        split[3],
                        split[0] + split[1] + split[2] + split[3],
                    );
                }
                let mut share = [0.0_f64; 4];
                let mut largest = (0usize, 0.0_f64);
                for (index, value) in solution.delta.iter().enumerate() {
                    let global = model.globals[index];
                    let squared = value * value;
                    match state.roles[global] {
                        Role::None => share[0] += squared,
                        Role::Exposure(_) | Role::Wb(_) => share[1] += squared,
                        Role::Mode(_) => share[2] += squared,
                        Role::Clip => share[3] += squared,
                    }
                    if value.abs() > largest.1 {
                        largest = (global, value.abs());
                    }
                }
                let total_share = share.iter().sum::<f64>().max(1e-300);
                eprintln!(
                    "fold1 direction-share look {:.6e} image {:.6e} mode {:.6e} clip {:.6e} largest {:.6e} role {:?}",
                    share[0] / total_share,
                    share[1] / total_share,
                    share[2] / total_share,
                    share[3] / total_share,
                    largest.1,
                    state.roles[largest.0],
                );
                let second_model = match JointModel::new(
                    &state.layout,
                    &state.problem,
                    &state.params,
                    &state.options,
                    &state.blocks,
                    &state.penalties,
                    &state.lambdas,
                    &state.roles,
                    &state.pass,
                    state.data_scale,
                    true,
                    true,
                    &ResidualMask::from_samples(&state.pass.residual_samples),
                ) {
                    Ok(second) => {
                        let mut out = vec![0.0_f64; second.dim];
                        second.apply_undamped(&solution.delta, &mut out);
                        let q_second = crate::solve::dot(&solution.delta, &out);
                        eprintln!(
                            "fold1 second-model (analytic acceleration on top of gn+robust+barrier, not an exact Hessian) curvature {q_second:.6e} acceleration {:.6e}",
                            q_second - model_curvature
                        );
                        Some(second)
                    }
                    Err(failure) => {
                        eprintln!("fold1 second-model failed: {}", failure.label());
                        None
                    }
                };
                let look_only: &[usize] = &[0];
                let image_only: &[usize] = &[1];
                let mode_only: &[usize] = &[2];
                let clip_only: &[usize] = &[3];
                let look_image: &[usize] = &[0, 1];
                let groups: [(&str, &[usize]); 5] = [
                    ("look", look_only),
                    ("image", image_only),
                    ("mode", mode_only),
                    ("clip", clip_only),
                    ("look+image", look_image),
                ];
                let mut q_model_group = [0.0_f64; 5];
                let mut q_fd_group = [0.0_f64; 5];
                let mut q_second_group = [0.0_f64; 5];
                let mut slope_fd_group = [0.0_f64; 5];
                let mut linear_model_group = [0.0_f64; 5];
                for (slot, (label, members)) in groups.iter().enumerate() {
                    let masked = masked_delta(&state, &model, &solution.delta, members);
                    let mut model_out = vec![0.0_f64; model.dim];
                    model.apply_undamped(&masked, &mut model_out);
                    q_model_group[slot] = crate::solve::dot(&masked, &model_out);
                    linear_model_group[slot] = crate::solve::dot(&model.rhs, &masked);
                    if let Some(second) = &second_model {
                        let mut second_out = vec![0.0_f64; second.dim];
                        second.apply_undamped(&masked, &mut second_out);
                        q_second_group[slot] = crate::solve::dot(&masked, &second_out);
                    }
                    let h = 1e-3;
                    let plus = state.objective(&shifted(&state, &model, &masked, h)).0;
                    let minus = state.objective(&shifted(&state, &model, &masked, -h)).0;
                    q_fd_group[slot] = (plus - 2.0 * state.cost + minus) / (h * h);
                    slope_fd_group[slot] = (plus - minus) / (2.0 * h);
                    eprintln!(
                        "fold1 group-curvature {label} model {:.6e} fd {:.6e} second {:.6e} acceleration {:.6e} slope-fd {:.6e} linear-model {:.6e} slope-gap {:.3e}",
                        q_model_group[slot],
                        q_fd_group[slot],
                        q_second_group[slot],
                        q_second_group[slot] - q_model_group[slot],
                        slope_fd_group[slot],
                        linear_model_group[slot],
                        slope_fd_group[slot] + linear_model_group[slot],
                    );
                }
                let cross_model = 0.5 * (q_model_group[4] - q_model_group[0] - q_model_group[1]);
                let cross_fd = 0.5 * (q_fd_group[4] - q_fd_group[0] - q_fd_group[1]);
                let cross_second =
                    0.5 * (q_second_group[4] - q_second_group[0] - q_second_group[1]);
                eprintln!(
                    "fold1 look-image-cross model {cross_model:.6e} fd {cross_fd:.6e} second {cross_second:.6e} model-gap {:.6e} acceleration {:.6e}",
                    cross_fd - cross_model,
                    cross_second - cross_model,
                );
                let effective: Vec<f64> = if residual_active {
                    (0..RESIDUAL_COEFS)
                        .map(|index| {
                            full.theta[state.layout.residual.start + index]
                                - state.params.theta[state.layout.residual.start + index]
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let mut unpinned = state.params.clone();
                apply_step_unclamped(&mut unpinned, &step);
                for (index, value) in residual_delta.iter().enumerate() {
                    unpinned.theta[state.layout.residual.start + index] += value;
                }
                let unpinned_cost = state.objective(&unpinned).0;
                let pinned_cost = state.objective(&full).0;
                let residual_unpinned = residual_shifted(&state, &residual_delta, 1.0);
                let mut residual_pinned = residual_unpinned.clone();
                ResidualMask::from_samples(&state.pass.residual_samples)
                    .pin(&mut residual_pinned.theta[state.layout.residual.clone()]);
                let residual_unpinned_cost = state.objective(&residual_unpinned).0;
                let residual_pinned_cost = state.objective(&residual_pinned).0;
                let mut base_pinned = state.params.clone();
                ResidualMask::from_samples(&state.pass.residual_samples)
                    .pin(&mut base_pinned.theta[state.layout.residual.clone()]);
                let residual_range = state.layout.residual.clone();
                let base_pin_change = base_pinned.theta[residual_range.clone()]
                    .iter()
                    .zip(&state.params.theta[residual_range.clone()])
                    .fold(0.0_f64, |largest, (pinned, base)| {
                        largest.max((pinned - base).abs())
                    });
                let step_pin_change = residual_pinned.theta[residual_range.clone()]
                    .iter()
                    .zip(&residual_unpinned.theta[residual_range.clone()])
                    .fold(0.0_f64, |largest, (pinned, unpinned)| {
                        largest.max((pinned - unpinned).abs())
                    });
                eprintln!(
                    "fold1 pin-path base {:.12e} joint-only {joint_cost:.12e} joint+res-unpinned {unpinned_cost:.12e} joint+res-pinned {pinned_cost:.12e} residual-unpinned {residual_unpinned_cost:.12e} residual-pinned {residual_pinned_cost:.12e} base-pin-change {base_pin_change:.3e} step-pin-change {step_pin_change:.3e}",
                    state.cost
                );
                let ridge = 2.0 * lam(&state.lambdas, BlockKind::Residual);
                let identity = identity_weight(BlockKind::Residual);
                let penalty = residual_penalty_diagonal();
                let mut curvature = vec![0.0_f64; RESIDUAL_COEFS];
                residual_undamped_apply(
                    &state.pass.residual_samples,
                    &effective,
                    &mut curvature,
                    &penalty,
                    ridge,
                    identity,
                    state.data_scale,
                );
                let q_residual = crate::solve::dot(&effective, &curvature);
                for h in [1.0, 1e-3] {
                    let (plus_cost, _) = state.objective(&residual_shifted(&state, &effective, h));
                    let (minus_cost, _) =
                        state.objective(&residual_shifted(&state, &effective, -h));
                    let slope = (plus_cost - minus_cost) / (2.0 * h);
                    let curvature_fd = (plus_cost - 2.0 * state.cost + minus_cost) / (h * h);
                    eprintln!(
                        "fold1 residual-fd h {h:.1e} slope {slope:.6e} curvature {curvature_fd:.6e} model-curvature {q_residual:.6e} model-prediction {residual_part:.6e}"
                    );
                }
                let hk = 1e-3;
                let pp = state
                    .objective(&mixed_shifted(
                        &state,
                        &model,
                        &solution.delta,
                        hk,
                        &effective,
                        hk,
                    ))
                    .0;
                let pm = state
                    .objective(&mixed_shifted(
                        &state,
                        &model,
                        &solution.delta,
                        hk,
                        &effective,
                        -hk,
                    ))
                    .0;
                let mp = state
                    .objective(&mixed_shifted(
                        &state,
                        &model,
                        &solution.delta,
                        -hk,
                        &effective,
                        hk,
                    ))
                    .0;
                let mm = state
                    .objective(&mixed_shifted(
                        &state,
                        &model,
                        &solution.delta,
                        -hk,
                        &effective,
                        -hk,
                    ))
                    .0;
                let cross_fd = (pp - pm - mp + mm) / (4.0 * hk * hk);
                eprintln!("fold1 cross-fd-central {cross_fd:.6e} model-cross {cross_part:.6e}");
                if let Some(second) = &second_model {
                    let h_image = 1e-4;
                    let delta_image = masked_delta(&state, &model, &solution.delta, &[1usize]);
                    let mut delta_q_single = 0.0_f64;
                    let mut delta_q_image = 0.0_f64;
                    let mut delta_h_columns: Vec<Vec<f64>> = Vec::new();
                    let mut delta_h_indices: Vec<usize> = Vec::new();
                    let mut image_missing: Vec<(usize, f64, f64, f64, f64)> = Vec::new();
                    for (index, global) in model.globals.iter().enumerate() {
                        if !matches!(state.roles[*global], Role::Exposure(_) | Role::Wb(_)) {
                            continue;
                        }
                        let mut plus = state.params.clone();
                        let mut minus = state.params.clone();
                        plus.theta[*global] += h_image;
                        minus.theta[*global] -= h_image;
                        let g_plus = joint_gradient(&state, &model, &plus);
                        let g_minus = joint_gradient(&state, &model, &minus);
                        let true_col: Vec<f64> = g_plus
                            .iter()
                            .zip(&g_minus)
                            .map(|(p, m)| (p - m) / (2.0 * h_image))
                            .collect();
                        let mut unit = vec![0.0_f64; model.dim];
                        unit[index] = 1.0;
                        let mut model_col = vec![0.0_f64; model.dim];
                        second.apply_undamped(&unit, &mut model_col);
                        let delta_h_col: Vec<f64> = true_col
                            .iter()
                            .zip(&model_col)
                            .map(|(truth, model)| truth - model)
                            .collect();
                        delta_h_columns.push(delta_h_col.clone());
                        delta_h_indices.push(index);
                        let single = crate::solve::dot(&solution.delta, &delta_h_col);
                        let image = crate::solve::dot(&delta_image, &delta_h_col);
                        let weight = solution.delta[index];
                        delta_q_single += weight * single;
                        delta_q_image += weight * image;
                        image_missing.push((
                            index,
                            weight,
                            single,
                            true_col[index],
                            model_col[index],
                        ));
                    }
                    let delta_q = 2.0 * delta_q_single - delta_q_image;
                    let mut out = vec![0.0_f64; model.dim];
                    second.apply_undamped(&solution.delta, &mut out);
                    let q_second = crate::solve::dot(&solution.delta, &out);
                    let q_corrected = q_second + delta_q;
                    for (index, weight, single, true_diagonal, model_diagonal) in &image_missing {
                        eprintln!(
                            "fold1 image-column joint {index} role {:?} delta {weight:.6e} delta-h-single {single:.6e} true-diagonal {true_diagonal:.6e} second-diagonal {model_diagonal:.6e}",
                            state.roles[model.globals[*index]]
                        );
                    }
                    let joint_prediction_corrected = linear - 0.5 * q_corrected;
                    eprintln!(
                        "fold1 corrected joint-curvature {q_corrected:.6e} model {model_curvature:.6e} second {q_second:.6e} delta-q {delta_q:.6e} single {delta_q_single:.6e} image {delta_q_image:.6e} prediction {joint_prediction_corrected:.6e} decrease {joint_decrease:.6e} ratio {:.6e}",
                        joint_decrease / joint_prediction_corrected
                    );
                    for damping in [1e-2, 1e-1] {
                        let old = JointSolve::new(&model, damping)
                            .and_then(|solve| solve.solve(&model, 1e-4, None, None));
                        let mut old_decrease = f64::NAN;
                        let mut old_ok = false;
                        if let Ok(old) = &old {
                            let old_raw_max = old
                                .delta
                                .iter()
                                .fold(0.0_f64, |largest, value| largest.max(value.abs()));
                            let old_clamp = if old_raw_max > STEP_RADIUS {
                                STEP_RADIUS / old_raw_max
                            } else {
                                1.0
                            };
                            let mut old_delta = old.delta.clone();
                            clamp_values(&mut old_delta, STEP_RADIUS);
                            let trial = trial_params(&state, &model, &old_delta, &[]);
                            let (cost, infeasible) = state.objective(&trial);
                            old_decrease = state.cost - cost;
                            old_ok = old.failure.is_none() && old.relative_residual <= 1e-4;
                            let stationarity = cold_certificate(&state, &trial);
                            eprintln!(
                                "fold1 trial old-gn damping {damping:.3e} solver joint-schur explicit-residual {:.3e} failure {:?} raw-max {old_raw_max:.3e} clamp-scale {old_clamp:.3e} decrease {old_decrease:.6e} stationarity-scalar {:.6e} threshold-pass {} infeasible {infeasible}",
                                old.relative_residual,
                                old.failure.map(SolveFailure::label),
                                reported_bound(&stationarity),
                                stationarity.certificate.is_certified_stationary(),
                            );
                        }
                        let diagonal: Vec<f64> = (0..model.dim)
                            .map(|index| {
                                second.data_diagonal[index].max(1e-12) * (1.0 + damping)
                                    + second.fixed[index]
                            })
                            .collect();
                        let mut probe_u = vec![0.0_f64; model.dim];
                        let mut probe_v = vec![0.0_f64; model.dim];
                        for (index, value) in probe_u.iter_mut().enumerate() {
                            *value = ((index * 37 + 11) % 101) as f64 / 101.0 - 0.5;
                        }
                        for (index, value) in probe_v.iter_mut().enumerate() {
                            *value = ((index * 53 + 7) % 97) as f64 / 97.0 - 0.5;
                        }
                        let mut apply_u = vec![0.0_f64; model.dim];
                        let mut apply_v = vec![0.0_f64; model.dim];
                        corrected_apply(
                            second,
                            &delta_h_columns,
                            &delta_h_indices,
                            damping,
                            &probe_u,
                            &mut apply_u,
                        );
                        corrected_apply(
                            second,
                            &delta_h_columns,
                            &delta_h_indices,
                            damping,
                            &probe_v,
                            &mut apply_v,
                        );
                        let bilinear_left = crate::solve::dot(&probe_u, &apply_v);
                        let bilinear_right = crate::solve::dot(&probe_v, &apply_u);
                        eprintln!(
                            "fold1 corrected-operator symmetry damping {damping:.3e} left {bilinear_left:.9e} right {bilinear_right:.9e} gap {:.3e}",
                            (bilinear_left - bilinear_right).abs()
                                / bilinear_left.abs().max(1e-300)
                        );
                        let mut delta = vec![0.0_f64; model.dim];
                        let failing = std::cell::RefCell::new(None::<Vec<f64>>);
                        let failing_curvature = std::cell::Cell::new(f64::NAN);
                        let failing_finite = std::cell::Cell::new(true);
                        let report = crate::solve::conjugate_gradient_preconditioned(
                            model.dim,
                            |v: &[f64], out: &mut [f64]| {
                                corrected_apply(
                                    second,
                                    &delta_h_columns,
                                    &delta_h_indices,
                                    damping,
                                    v,
                                    out,
                                );
                                let value = crate::solve::dot(v, out);
                                if !value.is_finite() || value <= 0.0 {
                                    failing_curvature.set(value);
                                    failing_finite.set(out.iter().all(|entry| entry.is_finite()));
                                    *failing.borrow_mut() = Some(v.to_vec());
                                }
                            },
                            |residual: &[f64], z: &mut [f64]| {
                                for index in 0..model.dim {
                                    z[index] = residual[index] / diagonal[index].max(1e-12);
                                }
                            },
                            &model.rhs,
                            &mut delta,
                            4096,
                            1e-4,
                        );
                        let mut check = vec![0.0_f64; model.dim];
                        corrected_apply(
                            second,
                            &delta_h_columns,
                            &delta_h_indices,
                            damping,
                            &delta,
                            &mut check,
                        );
                        let error: f64 = check
                            .iter()
                            .zip(&model.rhs)
                            .map(|(lhs, rhs)| (lhs - rhs) * (lhs - rhs))
                            .sum::<f64>()
                            .sqrt();
                        let scale: f64 = model
                            .rhs
                            .iter()
                            .map(|value| value * value)
                            .sum::<f64>()
                            .sqrt();
                        let relative = error / scale.max(1e-300);
                        let raw_max = delta
                            .iter()
                            .fold(0.0_f64, |largest, value| largest.max(value.abs()));
                        let clamp_scale = if raw_max > STEP_RADIUS {
                            STEP_RADIUS / raw_max
                        } else {
                            1.0
                        };
                        let failing = failing.into_inner();
                        let failing_curvature = failing_curvature.get();
                        let failing_finite = failing_finite.get();
                        let failing_norm = failing.as_ref().map(|v| crate::solve::dot(v, v).sqrt());
                        let failing_max = failing.as_ref().map(|v| {
                            v.iter()
                                .fold(0.0_f64, |largest, value| largest.max(value.abs()))
                        });
                        let failing_normalized = failing.as_ref().map(|v| {
                            let norm_squared = crate::solve::dot(v, v);
                            if norm_squared > 0.0 {
                                failing_curvature / norm_squared
                            } else {
                                f64::NAN
                            }
                        });
                        let failing_damping = failing.as_ref().map(|v| {
                            let norm_squared = crate::solve::dot(v, v);
                            if norm_squared > 0.0 {
                                let mut total = 0.0_f64;
                                for (index, value) in v.iter().enumerate() {
                                    total += second.data_diagonal[index].max(1e-12) * value * value;
                                }
                                damping * total / norm_squared
                            } else {
                                f64::NAN
                            }
                        });
                        clamp_values(&mut delta, STEP_RADIUS);
                        let trial = trial_params(&state, &model, &delta, &[]);
                        let (cost, infeasible) = state.objective(&trial);
                        let decrease = state.cost - cost;
                        let stationarity = cold_certificate(&state, &trial);
                        let ranked = relative <= 1e-4 && old_ok;
                        eprintln!(
                            "fold1 trial corrected damping {damping:.3e} solver cg-jacobi iterations {} positive-curvature {} failing-curvature {failing_curvature:.6e} failing-finite {failing_finite} failing-norm {failing_norm:?} failing-max {failing_max:?} failing-normalized {failing_normalized:?} failing-damping {failing_damping:?} explicit-residual {relative:.3e} raw-max {raw_max:.3e} clamp-scale {clamp_scale:.3e} decrease {decrease:.6e} stationarity-scalar {:.6e} threshold-pass {} ranked {ranked} infeasible {infeasible}",
                            report.iterations,
                            report.positive_curvature,
                            reported_bound(&stationarity),
                            stationarity.certificate.is_certified_stationary(),
                        );
                        eprintln!(
                            "fold1 ranking damping {damping:.3e} ranked {ranked} corrected-decrease {decrease:.6e} old-decrease {old_decrease:.6e}"
                        );
                    }
                    if residual_active {
                        let mut cross_tensor = 0.0_f64;
                        if state.pass.coupled_rows.len() == state.pass.residual_samples.len() {
                            for (row, sample) in state
                                .pass
                                .coupled_rows
                                .iter()
                                .zip(&state.pass.residual_samples)
                            {
                                if row.residual_columns.is_empty() {
                                    continue;
                                }
                                let mut dl_joint = [0.0_f64; 9];
                                for (position, (column, _)) in row.columns.iter().enumerate() {
                                    let value = step[*column as usize];
                                    if let Some(layer) = row.layers[position] {
                                        let weights = row.layer_weights[position];
                                        for (slot, weight) in
                                            dl_joint.iter_mut().take(3).zip(weights)
                                        {
                                            *slot += weight * value;
                                        }
                                        if layer.index() >= 3 {
                                            dl_joint[layer.index()] += weights[3] * value;
                                        }
                                    }
                                }
                                let mut dl_residual = [0.0_f64; 9];
                                for (m, node) in sample.nodes.iter().enumerate() {
                                    let weight = sample.weights[m];
                                    for (channel, slot) in
                                        dl_residual.iter_mut().skip(6).enumerate()
                                    {
                                        *slot += weight * effective[*node as usize * 3 + channel];
                                    }
                                }
                                let mut value = 0.0_f64;
                                for (k, left) in dl_joint.iter().enumerate() {
                                    let mut row_sum = 0.0_f64;
                                    for (l, right) in dl_residual.iter().enumerate() {
                                        row_sum += row.second[k * 9 + l] * right;
                                    }
                                    value += left * row_sum;
                                }
                                cross_tensor += value;
                            }
                        } else {
                            eprintln!(
                                "fold1 residual-cross-tensor skipped rows {} residual samples {}",
                                state.pass.coupled_rows.len(),
                                state.pass.residual_samples.len()
                            );
                        }
                        eprintln!(
                            "fold1 residual-cross-tensor {cross_tensor:.6e} model-cross {cross_part:.6e}"
                        );
                        for h in [1.0, 1e-1, 1e-2] {
                            let mut plus = state.params.clone();
                            let mut minus = state.params.clone();
                            for (index, value) in solution.delta.iter().enumerate() {
                                plus.theta[model.globals[index]] += h * value;
                                minus.theta[model.globals[index]] -= h * value;
                            }
                            let (mut blocks_plus, _) = stage_blocks(&state.problem, &state.options);
                            let (mut blocks_minus, _) =
                                stage_blocks(&state.problem, &state.options);
                            let pass_plus = state.problem.pass(
                                &plus,
                                STAGES[4].active,
                                &mut blocks_plus,
                                false,
                                false,
                                STAGES[4].neutral,
                                IntentFilter::NativeOnly,
                            );
                            let pass_minus = state.problem.pass(
                                &minus,
                                STAGES[4].active,
                                &mut blocks_minus,
                                false,
                                false,
                                STAGES[4].neutral,
                                IntentFilter::NativeOnly,
                            );
                            let difference: f64 = (0..RESIDUAL_COEFS)
                                .map(|index| {
                                    effective[index]
                                        * (pass_plus.residual_gradient[index]
                                            - pass_minus.residual_gradient[index])
                                })
                                .sum();
                            let cross_gradient_fd = -difference / (2.0 * h);
                            let full_prediction_corrected =
                                (linear - 0.5 * q_corrected) + residual_part - cross_gradient_fd;
                            eprintln!(
                                "fold1 residual-cross-gradient-fd h {h:.1e} cross {cross_gradient_fd:.6e} model-cross {cross_part:.6e} tensor {cross_tensor:.6e} motion-implied {:.6e} full-prediction-corrected {full_prediction_corrected:.6e} full-ratio {:.6e}",
                                cross_gradient_fd - (cross_part + cross_tensor),
                                full_decrease / full_prediction_corrected
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Lifecycle regressions: stationary steps, held residual coefficients and checkpoint history.
#[cfg(all(test, feature = "synth"))]
mod lifecycle_tests {
    use super::*;

    struct Fixture {
        problem: Problem,
        options: FitOptions,
        params: Params,
        roles: Vec<Role>,
        lambdas: StageLambdas,
        blocks: Vec<BlockAcc>,
        penalties: Vec<Vec<f64>>,
        pass: PassOutput,
    }

    impl Fixture {
        fn rebuild(&mut self) {
            for block in &mut self.blocks {
                block.reset();
            }
            self.pass = self.problem.pass_with_analytic_rows(
                &self.params,
                ActiveColumns::ALL,
                &mut self.blocks,
                false,
                false,
                STAGES[4].neutral,
                IntentFilter::NativeOnly,
                crate::problem::HessianRows::All,
            );
        }

        fn cost(&self) -> f64 {
            self.pass.cost
                + total_cost(
                    &self.problem,
                    &self.params,
                    &self.blocks,
                    &self.penalties,
                    &self.lambdas,
                    &self.options,
                )
        }

        fn model(&self) -> JointModel<'_> {
            let mask = ResidualMask::from_samples(&self.pass.residual_samples);
            JointModel::new(
                &self.problem.layout,
                &self.problem,
                &self.params,
                &self.options,
                &self.blocks,
                &self.penalties,
                &self.lambdas,
                &self.roles,
                &self.pass,
                self.pass.active_samples.max(1) as f64 / self.pass.hessian_rows.max(1) as f64,
                false,
                false,
                &mask,
            )
            .expect("joint model")
        }
    }

    fn fixture(images: usize, samples: usize, fit_residual: bool, seed: u64) -> Fixture {
        let data = crate::synth::dataset(&crate::synth::SynthConfig {
            seed,
            images,
            samples_per_image: samples,
            noise_ucs: 0.5,
            ..crate::synth::SynthConfig::default()
        });
        let options = FitOptions {
            seed,
            hessian_samples: usize::MAX,
            run_gcv: false,
            fit_residual,
            ..FitOptions::default()
        };
        let problem = Problem::new_with(&data.images, None, &options).expect("problem");
        let layout = problem.layout.clone();
        let params = initial_params(&problem, &options);
        let roles = prior_roles(&problem, &layout);
        let lambdas = StageLambdas {
            tone: 0.7,
            residual: 0.4,
            ..StageLambdas::default()
        };
        let (_, mut blocks, penalties) = stage_blocks(&STAGES[4], &problem, &options);
        let pass = problem.pass_with_analytic_rows(
            &params,
            ActiveColumns::ALL,
            &mut blocks,
            false,
            false,
            STAGES[4].neutral,
            IntentFilter::NativeOnly,
            crate::problem::HessianRows::All,
        );
        Fixture {
            problem,
            options,
            params,
            roles,
            lambdas,
            blocks,
            penalties,
            pass,
        }
    }

    fn run_step(
        f: &Fixture,
        params: &mut Params,
        mask: &ResidualMask,
        residual_active: bool,
    ) -> FinalOutcome {
        let mut radius = None;
        let mut gradient_scale = None;
        let mut evaluations = 0usize;
        let mut sample_evaluations = SampleEvaluations::default();
        let mut scratch = FinalScratch::default();
        let mut cert_warm = None;
        final_stage_step(
            &f.problem.layout,
            &f.problem,
            &f.problem,
            &STAGES[4],
            &f.options,
            params,
            &f.blocks,
            &f.penalties,
            &f.lambdas,
            &f.roles,
            &f.pass,
            f.pass.active_samples.max(1) as f64 / f.pass.hessian_rows.max(1) as f64,
            f.cost(),
            &mut radius,
            &mut gradient_scale,
            &mut evaluations,
            &mut sample_evaluations,
            f.problem.samples.len(),
            4,
            0,
            0,
            &mut scratch,
            &mut cert_warm,
            mask,
            residual_active,
        )
    }

    fn freeze_stationary(f: &mut Fixture) {
        f.problem.samples.clear();
        f.problem.hessian.clear();
        f.params = initial_params(&f.problem, &f.options);
        for index in 0..f.problem.layout.images.start {
            f.params.theta[index] = f.problem.layout.identity_mean(index);
        }
        let (a, b) = f.options.sigma_prior;
        let root = 0.5 * libm::log(b / a);
        for index in 0..f.problem.contributors.len() {
            f.params.theta[f.problem.layout.sigma_of(index)] = root;
        }
        f.rebuild();
    }

    fn perturb(f: &mut Fixture) {
        f.params.theta[f.problem.layout.tone_c0] += 0.7;
        f.params.theta[f.problem.layout.image(0).start] += 0.4;
        f.params.theta[f.problem.layout.clip] += 0.2;
        f.rebuild();
    }

    /// `B = diag(1e8, 1e-4)`, `rhs = (1, 1e-3)` and an identity preconditioner: the
    /// directional lower bound is ~5e-9 while the energy is ~5e-3.
    fn anisotropic(f: &Fixture) -> JointModel<'_> {
        let mut model = f.model();
        let dim = model.dim;
        model.rows.clear();
        for image in &mut model.images {
            image.clear();
        }
        model.border_count = 0;
        model.residual_start = dim;
        model.residual_count = 0;
        model.residual_local_of.clear();
        model.penalty.clear();
        model.penalty_diagonal = vec![0.0; dim];
        model.data_diagonal = vec![0.0; dim];
        model.outer_border.clear();
        model.border_data.clear();
        model.image_width = 1;
        model.rhs = vec![0.0; dim];
        model.rhs[0] = 1.0;
        model.rhs[1] = 1.0e-3;
        model.fixed = vec![1.0; dim];
        model.fixed[0] = 1.0e8;
        model.fixed[1] = 1.0e-4;
        model.floor = model.fixed.clone();
        model.outer_count = dim;
        model.groups = vec![JointGroup {
            start: 0,
            matrix: vec![1.0 - 1.0e8, 0.0, 0.0, 1.0 - 1.0e-4],
        }];
        model
    }

    #[test]
    fn anisotropic_lower_bound_does_not_certify_stationarity() {
        let f = fixture(1, 8, true, 0x51ae);
        let model = anisotropic(&f);
        let solve = JointSolve::new(&model, 0.0).expect("anisotropic factor");
        let directional = solve.energy_lower_bound(&model);
        assert!(
            directional <= STATIONARY_DECREMENT_NATS,
            "the directional lower bound is not the discriminating case: {directional}"
        );
        let mut warm = None;
        let report = certified_bound(&model, &solve, &mut warm, CertificateOrigin::FinalStage);
        match report.certificate {
            StationarityCertificate::CertifiedNonstationary { lower } => assert!(
                lower.get() > STATIONARY_DECREMENT_NATS,
                "the certificate did not separate the lower bound from the energy"
            ),
            other => panic!("anisotropic state was certified: {other:?}"),
        }
        let mut decision_warm = None;
        assert!(
            certified_stationary(&model, &mut decision_warm).is_none(),
            "a small directional lower bound accepted a nonstationary state"
        );
    }

    #[test]
    fn plateau_history_keeps_signed_objective_values() {
        let f = fixture(1, 8, true, 0x51af);
        let objective = plateau_objective(&f.problem, &f.params, &f.lambdas, 4, &f.options);
        let state = checkpoint_state(
            &f.problem,
            &f.params,
            f.lambdas,
            4,
            0,
            1e-3,
            &f.options,
            &[-2.0, -1.5, -1.0, -0.5, 0.0],
            false,
        );
        assert_eq!(
            resume_history(&state, &objective),
            vec![-2.0, -1.5, -1.0, -0.5, 0.0],
            "a negative objective value was rejected"
        );
    }

    #[test]
    fn stationary_linearization_keeps_a_non_increasing_state() {
        let mut f = fixture(2, 8, true, 0x51a7);
        freeze_stationary(&mut f);
        let mask = ResidualMask::from_samples(&f.pass.residual_samples);
        let mut params = f.params.clone();
        let before = params.theta.clone();
        let outcome = run_step(&f, &mut params, &mask, true);
        assert!(
            matches!(outcome, FinalOutcome::Stationary(_)),
            "a prior-only stationary objective was not stationary"
        );
        assert_eq!(
            params.theta, before,
            "the stationary step changed the state"
        );
    }

    #[test]
    fn data_linearization_is_not_reported_stationary() {
        let mut f = fixture(2, 8, true, 0x51a8);
        perturb(&mut f);
        let mask = ResidualMask::from_samples(&f.pass.residual_samples);
        let mut params = f.params.clone();
        let before = params.theta.clone();
        match run_step(&f, &mut params, &mask, true) {
            FinalOutcome::Stationary(_) => panic!("a data gradient was reported stationary"),
            FinalOutcome::Accepted(_) => {
                assert_ne!(
                    params.theta, before,
                    "an accepted step did not move the state"
                )
            }
            FinalOutcome::Budget
            | FinalOutcome::Stalled
            | FinalOutcome::SigmaRefresh
            | FinalOutcome::Unresolved(_) => {}
        }
    }

    #[test]
    fn box_prediction_lead_yields_to_a_larger_realized_decrease() {
        // Controlled-recovery endpoint: box predicted 4.42980459456707649e2 and realized
        // 7.64977619483199760e1; trust-region predicted 3.55131986310910918e2 and realized
        // 2.77056179438026902e2.
        assert_eq!(
            guarded_selection(
                OrdinaryStep::TrustRegion,
                true,
                2.770_561_794_380_269e2,
                7.649_776_194_831_998e1
            ),
            AppliedStep::TrustRegion
        );
        assert_eq!(
            guarded_selection(
                OrdinaryStep::TrustRegion,
                true,
                2.770_561_794_380_269e2,
                2.770_561_794_380_27e2
            ),
            AppliedStep::Box
        );
        assert_eq!(
            guarded_selection(
                OrdinaryStep::Cauchy,
                true,
                5.750_634_682_022_542e1,
                5.750_634_682_022_542e1
            ),
            AppliedStep::Cauchy
        );
        assert_eq!(
            guarded_selection(OrdinaryStep::Cauchy, false, 5.750_634_682_022_542e1, 9.0e1),
            AppliedStep::Cauchy
        );
    }

    #[test]
    fn held_residual_coefficients_survive_a_step_without_the_residual_block() {
        let mut f = fixture(2, 8, false, 0x51a9);
        let range = f.problem.layout.residual.clone();
        for (offset, value) in f.params.theta[range.clone()].iter_mut().enumerate() {
            *value = 0.05 * (1 + offset % 4) as f64;
        }
        perturb(&mut f);
        assert!(
            !f.blocks
                .iter()
                .any(|block| block.kind == BlockKind::Residual),
            "the fixture fitted the residual block"
        );
        let mask = ResidualMask::empty();
        let mut params = f.params.clone();
        let before = params.theta[range.clone()].to_vec();
        let outcome = run_step(&f, &mut params, &mask, false);
        assert_eq!(
            &params.theta[range],
            before.as_slice(),
            "a held residual coefficient changed"
        );
        assert!(
            matches!(outcome, FinalOutcome::Accepted(_)),
            "the held-residual step did not take a normal step"
        );
    }

    #[test]
    fn recertification_rejects_an_infeasible_objective() {
        let mut f = fixture(1, 8, true, 0x51aa);
        f.params.theta[f.problem.layout.tone_c0] = 1.0e6;
        let mut evaluations = 0usize;
        let mut sample_evaluations = SampleEvaluations::default();
        let mut warm = None;
        let mut operator_rows = 0usize;
        let report = recertify_native(
            &f.problem,
            &mut f.params,
            &f.options,
            &f.lambdas,
            &mut evaluations,
            &mut sample_evaluations,
            &mut warm,
            &mut operator_rows,
        );
        assert_eq!(report.origin, CertificateOrigin::PostFitRecertification);
        assert_eq!(
            report.certificate,
            StationarityCertificate::Unresolved {
                termination: CertificateTermination::InvalidObjective,
            }
        );
    }

    #[test]
    fn recertification_rejects_a_nonfinite_objective() {
        let mut f = fixture(1, 8, true, 0x51ab);
        f.params.theta[f.problem.layout.sigma_of(0)] = -1000.0;
        let mut evaluations = 0usize;
        let mut sample_evaluations = SampleEvaluations::default();
        let mut warm = None;
        let mut operator_rows = 0usize;
        let report = recertify_native(
            &f.problem,
            &mut f.params,
            &f.options,
            &f.lambdas,
            &mut evaluations,
            &mut sample_evaluations,
            &mut warm,
            &mut operator_rows,
        );
        assert_eq!(
            report.certificate,
            StationarityCertificate::Unresolved {
                termination: CertificateTermination::InvalidObjective,
            }
        );
    }

    #[test]
    fn obsolete_generations_are_not_resumed() {
        let f = fixture(1, 8, true, 0x51ad);
        let mut state = checkpoint_state(
            &f.problem,
            &f.params,
            f.lambdas,
            4,
            0,
            1e-3,
            &f.options,
            &[],
            false,
        );
        assert_eq!(state.generation, FitStateGeneration::CURRENT);
        assert!(current_resume(Some(&state)).is_some());
        state.generation = FitStateGeneration::default();
        assert!(
            current_resume(Some(&state)).is_none(),
            "an obsolete objective was resumed"
        );
    }

    #[test]
    fn checkpoint_history_resumes_only_under_the_same_objective() {
        let f = fixture(1, 8, true, 0x51ac);
        let objective = plateau_objective(&f.problem, &f.params, &f.lambdas, 4, &f.options);
        let state = checkpoint_state(
            &f.problem,
            &f.params,
            f.lambdas,
            4,
            7,
            1e-3,
            &f.options,
            &[10.0, 9.0, 8.0, 7.0, 6.0, 5.0],
            true,
        );
        assert_eq!(state.plateau.len(), PLATEAU_SAMPLES);
        assert!(
            state.plateau.pending,
            "the checkpoint lost the pending accepted sample"
        );
        assert_eq!(
            state.plateau.samples().first().map(|sample| sample.get()),
            Some(9.0),
            "the checkpoint kept more than the last five samples"
        );
        assert_eq!(
            resume_history(&state, &objective),
            vec![9.0, 8.0, 7.0, 6.0, 5.0],
            "a checkpoint did not resume its own history"
        );

        let resumed = plateau_objective(&f.problem, &f.params, &state.lambdas, 4, &f.options);
        assert!(
            state.plateau.compatible(&resumed),
            "a checkpoint did not resume its own history"
        );

        let changed = FitOptions {
            fit_residual: false,
            ..f.options
        };
        let changed_objective = plateau_objective(&f.problem, &f.params, &f.lambdas, 4, &changed);
        assert!(
            resume_history(&state, &changed_objective).is_empty(),
            "history crossed a residual-block change"
        );

        let mut moved = f.params.clone();
        moved.theta[f.problem.layout.sigma_of(0)] += 0.5;
        let moved_objective = plateau_objective(&f.problem, &moved, &f.lambdas, 4, &f.options);
        assert!(
            resume_history(&state, &moved_objective).is_empty(),
            "history crossed a sigma change"
        );

        let other_stage = plateau_objective(&f.problem, &f.params, &f.lambdas, 3, &f.options);
        assert!(
            resume_history(&state, &other_stage).is_empty(),
            "history crossed a stage change"
        );
    }
}

/// Finite-difference oracles for [`AnalyticModel`] against the evaluated penalized objective.
#[cfg(all(test, feature = "synth"))]
mod analytic_oracle_tests {
    use super::*;

    const SEED: u64 = 0x5eed_0a71_c0de;
    const FD_STEP: f64 = 1e-6;
    const FD_LADDER: [f64; 10] = [
        1e-3,
        5e-4,
        2.5e-4,
        1.25e-4,
        6.25e-5,
        3.125e-5,
        1.5625e-5,
        7.8125e-6,
        3.90625e-6,
        1.953125e-6,
    ];

    struct Oracle {
        images: Vec<FitImage>,
        response_model: Option<ControlResponseModel>,
        problem: Problem,
        options: FitOptions,
        gradient_problem: Problem,
        gradient_options: FitOptions,
        params: Params,
        roles: Vec<Role>,
        lambdas: StageLambdas,
        blocks: Vec<BlockAcc>,
        penalties: Vec<Vec<f64>>,
        pass: PassOutput,
        data_scale: f64,
        mask: ResidualMask,
    }

    impl Oracle {
        fn model(&self) -> JointModel<'_> {
            JointModel::new(
                &self.problem.layout,
                &self.problem,
                &self.params,
                &self.options,
                &self.blocks,
                &self.penalties,
                &self.lambdas,
                &self.roles,
                &self.pass,
                self.data_scale,
                false,
                false,
                &self.mask,
            )
            .expect("joint model")
        }
    }

    fn build_response_model(data: &mut crate::synth::SyntheticDataset) -> ControlResponseModel {
        let schema = looklab_schema::validate_control_schema(include_str!(
            "../../../data/bodies/fujifilm-x-t5.yaml"
        ))
        .expect("body schema validates");
        let sim = looklab_schema::SimKey::new("classic_chrome").expect("sim key");
        let mut model = ControlResponseModel::zeroed(&schema, 1, &sim, 1).expect("zeroed model");
        let index = model
            .controls
            .iter()
            .position(|control| !control.values.is_empty())
            .expect("a layer control with a non-default value");
        model.controls = vec![model.controls.swap_remove(index)];
        model.controls[0].values.truncate(1);
        model.controls[0].affects = vec![looklab_model::realization::Affect::Tone];
        model.wb.controls.clear();
        model.exposure.modes.clear();
        model.exposure.steps.clear();
        let key = model.controls[0].key.clone();
        let value = model.controls[0].values[0].value.clone();
        for image in &mut data.images {
            image.settings.set(key.clone(), value.clone());
        }
        model
    }

    fn oracle(
        images: usize,
        samples: usize,
        fit_residual: bool,
        response: bool,
        hessian_cap: Option<usize>,
        seed: u64,
    ) -> Oracle {
        let mut data = crate::synth::dataset(&crate::synth::SynthConfig {
            seed,
            images,
            samples_per_image: samples,
            noise_ucs: 0.5,
            ..crate::synth::SynthConfig::default()
        });
        let response_model = response.then(|| build_response_model(&mut data));
        let options = FitOptions {
            seed,
            hessian_samples: hessian_cap.unwrap_or(usize::MAX),
            run_gcv: false,
            fit_residual,
            ..FitOptions::default()
        };
        let problem =
            Problem::new_with(&data.images, response_model.as_ref(), &options).expect("problem");
        if response {
            assert!(
                !problem.layout.response.is_empty(),
                "the response fixture has no response coordinates"
            );
        }
        let gradient_options = FitOptions {
            hessian_samples: 1,
            ..options
        };
        let gradient_problem =
            Problem::new_with(&data.images, response_model.as_ref(), &gradient_options)
                .expect("gradient problem");
        assert_eq!(
            gradient_problem.samples.len(),
            problem.samples.len(),
            "the gradient problem changed the sample set"
        );
        let layout = problem.layout.clone();
        let mut params = initial_params(&problem, &options);
        params.theta[layout.clip] = zeta_from_clip(0.05);
        for index in 0..problem.images.len() {
            let range = layout.image(index);
            params.theta[range.start] += 0.05;
            params.theta[range.start + 1] += 0.02;
            params.theta[range.start + 2] -= 0.015;
        }
        for index in layout.modes.clone() {
            params.theta[index] += 0.05;
        }
        params.theta[layout.tint_a.start] += 0.002;
        params.theta[layout.g.start] += 0.002;
        params.theta[layout.r.start] -= 0.002;
        params.theta[layout.lambda.start] += 0.002;
        for (offset, value) in params.theta[layout.residual.clone()].iter_mut().enumerate() {
            *value = 0.01 * (((offset as i64 * 13) % 7) - 3) as f64;
        }
        let roles = prior_roles(&problem, &layout);
        let lambdas = StageLambdas {
            tone: 0.7,
            tint: 0.5,
            grl: 0.3,
            residual: 0.4,
            response_tone: RESPONSE_LAMBDA_FLOOR,
            response_tint: RESPONSE_LAMBDA_FLOOR,
            response_chroma: RESPONSE_LAMBDA_FLOOR,
            response_hue: RESPONSE_LAMBDA_FLOOR,
            response_lightness: RESPONSE_LAMBDA_FLOOR,
            response_wb: RESPONSE_LAMBDA_FLOOR,
        };
        let (_, mut blocks, penalties) = stage_blocks(&STAGES[4], &problem, &options);
        let rows = if hessian_cap.is_some() {
            crate::problem::HessianRows::Retained
        } else {
            crate::problem::HessianRows::All
        };
        let pass = problem.pass_with_analytic_rows(
            &params,
            ActiveColumns::ALL,
            &mut blocks,
            false,
            false,
            STAGES[4].neutral,
            IntentFilter::NativeOnly,
            rows,
        );
        assert!(
            !pass.infeasible && pass.cost.is_finite(),
            "oracle fixture is infeasible"
        );
        let data_scale = pass.active_samples.max(1) as f64 / pass.hessian_rows.max(1) as f64;
        if hessian_cap.is_some() {
            assert_eq!(pass.coupled_rows.len(), pass.hessian_rows);
        }
        let mask = ResidualMask::from_samples(&pass.residual_samples);
        Oracle {
            images: data.images,
            response_model,
            problem,
            options,
            gradient_problem,
            gradient_options,
            params,
            roles,
            lambdas,
            blocks,
            penalties,
            pass,
            data_scale,
            mask,
        }
    }

    fn prepared<'a, 'b>(
        model: &'a JointModel<'b>,
        pass: &'a PassOutput,
        params: &'a Params,
        options: &'a FitOptions,
    ) -> (AnalyticModel<'a, 'b>, FinalScratch) {
        let full = AnalyticModel::new(model, pass, params, options, model.residual_data_scale)
            .expect("analytic model");
        let mut scratch = FinalScratch::default();
        full.prepare(&mut scratch).expect("analytic workspace");
        (full, scratch)
    }

    fn evaluated_gradient_checked(
        problem: &Problem,
        options: &FitOptions,
        roles: &[Role],
        lambdas: &StageLambdas,
        model: &JointModel<'_>,
        params: &Params,
    ) -> Option<Vec<f64>> {
        let (_, mut blocks, penalties) = stage_blocks(&STAGES[4], problem, options);
        let pass = problem.pass(
            params,
            STAGES[4].active,
            &mut blocks,
            false,
            false,
            STAGES[4].neutral,
            IntentFilter::NativeOnly,
        );
        if pass.infeasible || !pass.cost.is_finite() {
            return None;
        }
        let layout = &problem.layout;
        let mut joint_of = vec![u32::MAX; layout.total];
        for (joint, &global) in model.globals.iter().enumerate() {
            joint_of[global] = joint as u32;
        }
        let mut gradient = vec![0.0_f64; model.dim];
        for (index, block) in blocks.iter().enumerate() {
            if block.kind == BlockKind::Residual {
                continue;
            }
            let lambda = block_lambda(block, lambdas);
            let values = block_gradients(
                layout,
                problem,
                params,
                options,
                block,
                &penalties[index],
                lambda,
                roles,
            );
            for (local, &global) in block.params.iter().enumerate() {
                let joint = joint_of[global];
                if joint != u32::MAX {
                    gradient[joint as usize] += values[local];
                }
            }
        }
        let ridge = 2.0 * lam(lambdas, BlockKind::Residual);
        let identity = identity_weight(BlockKind::Residual);
        let diagonal = residual_penalty_diagonal();
        let theta = &params.theta[layout.residual.clone()];
        for compact in 0..model.residual_count {
            let joint = model.residual_start + compact;
            let full = model.globals[joint] - layout.residual.start;
            gradient[joint] = ridge * laplacian_at(theta, full, &diagonal) + identity * theta[full]
                - pass.residual_gradient[full];
        }
        Some(gradient)
    }

    fn evaluated_gradient(
        problem: &Problem,
        options: &FitOptions,
        roles: &[Role],
        lambdas: &StageLambdas,
        model: &JointModel<'_>,
        params: &Params,
    ) -> Vec<f64> {
        evaluated_gradient_checked(problem, options, roles, lambdas, model, params)
            .expect("evaluated state is infeasible")
    }

    fn objective(
        problem: &Problem,
        options: &FitOptions,
        lambdas: &StageLambdas,
        params: &Params,
    ) -> f64 {
        let (_, mut blocks, penalties) = stage_blocks(&STAGES[4], problem, options);
        let pass = problem.pass(
            params,
            STAGES[4].active,
            &mut blocks,
            false,
            false,
            STAGES[4].neutral,
            IntentFilter::NativeOnly,
        );
        assert!(
            !pass.infeasible && pass.cost.is_finite(),
            "objective state is infeasible"
        );
        pass.cost + total_cost(problem, params, &blocks, &penalties, lambdas, options)
    }

    fn shifted(params: &Params, model: &JointModel<'_>, direction: &[f64], step: f64) -> Params {
        let mut trial = params.clone();
        for (joint, &global) in model.globals.iter().enumerate() {
            trial.theta[global] += step * direction[joint];
        }
        trial
    }

    fn central_difference(g_plus: &[f64], g_minus: &[f64], step: f64) -> Vec<f64> {
        g_plus
            .iter()
            .zip(g_minus)
            .map(|(plus, minus)| (plus - minus) / (2.0 * step))
            .collect()
    }

    struct LadderWindow {
        steps: [f64; 3],
        estimate: Vec<f64>,
        level: Vec<f64>,
    }

    /// Richardson estimates and level differences over consecutive `FD_LADDER` triples.
    fn gradient_finite_difference(
        oracle: &Oracle,
        problem: &Problem,
        options: &FitOptions,
        model: &JointModel<'_>,
        params: &Params,
        direction: &[f64],
    ) -> Vec<LadderWindow> {
        let differences: Vec<Vec<f64>> = FD_LADDER
            .iter()
            .map(|step| {
                let plus = shifted(params, model, direction, *step);
                let minus = shifted(params, model, direction, -*step);
                let g_plus = evaluated_gradient(
                    problem,
                    options,
                    &oracle.roles,
                    &oracle.lambdas,
                    model,
                    &plus,
                );
                let g_minus = evaluated_gradient(
                    problem,
                    options,
                    &oracle.roles,
                    &oracle.lambdas,
                    model,
                    &minus,
                );
                central_difference(&g_plus, &g_minus, *step)
            })
            .collect();
        differences
            .windows(3)
            .enumerate()
            .map(|(offset, window)| {
                let mut estimate = Vec::with_capacity(window[0].len());
                let mut level = Vec::with_capacity(window[0].len());
                for index in 0..window[0].len() {
                    let coarse = window[0][index];
                    let fine = window[1][index];
                    let finest = window[2][index];
                    let first = (4.0 * fine - coarse) / 3.0;
                    let second = (4.0 * finest - fine) / 3.0;
                    let third = (16.0 * second - first) / 15.0;
                    estimate.push(third);
                    level.push((second - third).abs());
                }
                LadderWindow {
                    steps: [
                        FD_LADDER[offset],
                        FD_LADDER[offset + 1],
                        FD_LADDER[offset + 2],
                    ],
                    estimate,
                    level,
                }
            })
            .collect()
    }

    fn assert_ladder(
        label: &str,
        windows: &[LadderWindow],
        joint: usize,
        global: usize,
        expected: f64,
    ) {
        let tolerance = 1e-6 * (1.0 + expected.abs());
        let mut worst_normalized = (0.0_f64, 0.0_f64, 0.0_f64, [0.0_f64; 3]);
        let mut worst_absolute = (0.0_f64, 0.0_f64, 0.0_f64, [0.0_f64; 3]);
        let mut diagnostics = Vec::new();
        for window in windows {
            let estimate = window.estimate[joint];
            let level = window.level[joint];
            let error = (estimate - expected).abs();
            if level <= tolerance && error <= tolerance {
                return;
            }
            let normalized = error / tolerance;
            if normalized > worst_normalized.0 {
                worst_normalized = (normalized, level, error, window.steps);
            }
            if error > worst_absolute.0 {
                worst_absolute = (error, level, estimate, window.steps);
            }
            diagnostics.push(format!(
                "steps {:?} level {level:.3e} estimate {estimate:.9e} error {error:.3e}",
                window.steps
            ));
        }
        panic!(
            "{label} joint {joint} global {global}: no window both resolved and accurate; tolerance {tolerance:.3e}; worst normalized {:.3e} level {:.3e} error {:.3e} steps {:?}; worst absolute {:.3e} level {:.3e} estimate {:.9e} steps {:?}; {diagnostics:?}",
            worst_normalized.0,
            worst_normalized.1,
            worst_normalized.2,
            worst_normalized.3,
            worst_absolute.0,
            worst_absolute.1,
            worst_absolute.2,
            worst_absolute.3,
        );
    }

    fn reweighted_problem(oracle: &Oracle) -> (Problem, FitOptions) {
        let options = FitOptions {
            hessian_samples: 1,
            ..oracle.options
        };
        let mut problem =
            Problem::new_with(&oracle.images, oracle.response_model.as_ref(), &options)
                .expect("reweighted problem");
        assert_eq!(
            problem.samples.len(),
            oracle.problem.samples.len(),
            "the reweighted problem changed the sample set"
        );
        let mut selected = vec![false; problem.samples.len()];
        for &index in &oracle.problem.hessian {
            selected[index as usize] = true;
        }
        for (index, sample) in problem.samples.iter_mut().enumerate() {
            sample.weight = if selected[index] {
                sample.weight * oracle.data_scale
            } else {
                0.0
            };
        }
        (problem, options)
    }

    fn direction_masks(oracle: &Oracle, model: &JointModel<'_>) -> Vec<(&'static str, Vec<f64>)> {
        let layout = &oracle.problem.layout;
        let image_count = oracle.problem.images.len();
        let mixed: Vec<f64> = (0..model.dim)
            .map(|joint| {
                let value = ((model.globals[joint] as u64 * 37 + 11) % 19) as f64 - 9.0;
                if value == 0.0 { 1.0 } else { value }
            })
            .collect();
        let mask = |keep: &dyn Fn(usize) -> bool| -> Vec<f64> {
            mixed
                .iter()
                .enumerate()
                .map(|(joint, value)| {
                    if keep(model.globals[joint]) {
                        *value
                    } else {
                        0.0
                    }
                })
                .collect()
        };
        let is_image =
            |global: usize| (0..image_count).any(|index| layout.image(index).contains(&global));
        let mut directions = vec![("mixed", mixed.clone())];
        directions.push(("clip", mask(&|global| global == layout.clip)));
        directions.push((
            "residual",
            mask(&|global| layout.residual.contains(&global)),
        ));
        directions.push(("image", mask(&|global| is_image(global))));
        directions.push((
            "native",
            mask(&|global| {
                !layout.response.contains(&global)
                    && !layout.residual.contains(&global)
                    && global != layout.clip
                    && !is_image(global)
            }),
        ));
        if !layout.response.is_empty() {
            directions.push((
                "response",
                mask(&|global| layout.response.contains(&global)),
            ));
        }
        directions
    }

    fn richardson_check(
        oracle: &Oracle,
        problem: &Problem,
        options: &FitOptions,
        model: &JointModel<'_>,
        full: &AnalyticModel<'_, '_>,
        scratch: &mut FinalScratch,
    ) {
        const WINDOWS: [[f64; 3]; 2] = [[1e-5, 5e-6, 2.5e-6], [1e-3, 5e-4, 2.5e-4]];
        const MAX_REPORTED: usize = 8;
        let base = evaluated_gradient(
            problem,
            options,
            &oracle.roles,
            &oracle.lambdas,
            model,
            &oracle.params,
        );
        let mut failures = Vec::new();
        let mut failure_count = 0usize;
        for (label, direction) in direction_masks(oracle, model) {
            assert!(
                direction.iter().any(|value| *value != 0.0),
                "{label} direction is zero"
            );
            let direction_scale = direction
                .iter()
                .fold(0.0_f64, |scale, value| scale.max(value.abs()));
            let mut windows = Vec::with_capacity(WINDOWS.len());
            for steps in &WINDOWS {
                let mut differences = Vec::with_capacity(steps.len());
                let mut feasible = true;
                for step in steps {
                    let plus = shifted(&oracle.params, model, &direction, *step);
                    let minus = shifted(&oracle.params, model, &direction, -*step);
                    let Some(g_plus) = evaluated_gradient_checked(
                        problem,
                        options,
                        &oracle.roles,
                        &oracle.lambdas,
                        model,
                        &plus,
                    ) else {
                        feasible = false;
                        break;
                    };
                    let Some(g_minus) = evaluated_gradient_checked(
                        problem,
                        options,
                        &oracle.roles,
                        &oracle.lambdas,
                        model,
                        &minus,
                    ) else {
                        feasible = false;
                        break;
                    };
                    differences.push(central_difference(&g_plus, &g_minus, *step));
                }
                windows.push((*steps, feasible.then_some(differences)));
            }
            let mut product = vec![0.0_f64; model.dim];
            full.apply(&direction, &mut product, &mut scratch.hessian)
                .expect("analytic product");
            for joint in 0..model.dim {
                let expected = product[joint];
                let tolerance = 1e-6 * (1.0 + expected.abs());
                let mut matched = false;
                for (_, differences) in &windows {
                    let Some(differences) = differences else {
                        continue;
                    };
                    let first = (4.0 * differences[1][joint] - differences[0][joint]) / 3.0;
                    let second = (4.0 * differences[2][joint] - differences[1][joint]) / 3.0;
                    let third = (16.0 * second - first) / 15.0;
                    if (second - third).abs() <= tolerance && (third - expected).abs() <= tolerance
                    {
                        matched = true;
                        break;
                    }
                }
                if matched {
                    continue;
                }
                failure_count += 1;
                if failures.len() >= MAX_REPORTED {
                    continue;
                }
                let mut message = format!(
                    "{label} joint {joint} global {}: analytic {expected:.9e} tolerance {tolerance:.3e}; base gradient {:.9e}; direction scale {direction_scale:.3e}",
                    model.globals[joint], base[joint]
                );
                for (index, (steps, differences)) in windows.iter().enumerate() {
                    let Some(differences) = differences else {
                        message.push_str(&format!("; window {index} steps {steps:?} infeasible"));
                        continue;
                    };
                    let first = (4.0 * differences[1][joint] - differences[0][joint]) / 3.0;
                    let second = (4.0 * differences[2][joint] - differences[1][joint]) / 3.0;
                    let third = (16.0 * second - first) / 15.0;
                    let level = (second - third).abs();
                    let error = (third - expected).abs();
                    message.push_str(&format!(
                        "; window {index} steps {steps:?} level {level:.3e} estimate {third:.9e} error {error:.3e} converged {} matches {}",
                        level <= tolerance,
                        error <= tolerance
                    ));
                }
                failures.push(message);
            }
        }
        assert!(
            failure_count == 0,
            "analytic Hessian finite-difference diagnostics ({failure_count} components):\n{}",
            failures.join("\n")
        );
    }

    fn assert_required_terms(
        oracle: &Oracle,
        model: &JointModel<'_>,
        full: &AnalyticModel<'_, '_>,
    ) {
        let layout = &oracle.problem.layout;
        assert!(
            model.globals.contains(&layout.clip),
            "clip coordinate is not free"
        );
        assert!(model.residual_count > 0, "no free residual coordinates");
        assert!(
            model
                .globals
                .iter()
                .any(|global| layout.residual.contains(global)),
            "residual coordinates are not in the joint model"
        );
        assert!(
            model.globals.contains(&layout.tone_c0),
            "tone coordinate is not free"
        );
        assert!(
            model
                .globals
                .iter()
                .any(|global| layout.tint_a.contains(global)),
            "tint coordinate is not free"
        );
        assert!(
            model.globals.iter().any(|global| layout.g.contains(global)),
            "G coordinate is not free"
        );
        assert!(
            model.globals.iter().any(|global| layout.r.contains(global)),
            "R coordinate is not free"
        );
        assert!(
            model
                .globals
                .iter()
                .any(|global| layout.lambda.contains(global)),
            "lambda coordinate is not free"
        );
        assert!(
            (0..oracle.problem.images.len())
                .any(|index| model.globals.contains(&layout.image(index).start)),
            "image exposure coordinate is not free"
        );
        assert!(
            (0..oracle.problem.images.len())
                .any(|index| model.globals.contains(&(layout.image(index).start + 1))),
            "image WB coordinate is not free"
        );
        if !layout.response.is_empty() {
            assert!(
                model
                    .globals
                    .iter()
                    .any(|global| layout.response.contains(global)),
                "response coordinates are not in the joint model"
            );
        }
        assert!(
            model.rows.iter().any(|row| row.robust.abs() > 1e-12),
            "no Student-t radial curvature in the retained rows"
        );
        assert!(
            model.rows.iter().any(|row| row
                .penalty_gn
                .iter()
                .flatten()
                .any(|value| value.abs() > 1e-12)),
            "no domain penalty curvature in the retained rows"
        );
        assert!(
            model
                .rows
                .iter()
                .any(|row| !row.residual_columns.is_empty()),
            "no residual cross columns in the retained rows"
        );
        assert!(
            full.clip_prior.abs() > 1e-12,
            "the nonlinear clip prior correction is zero"
        );
        assert!(
            oracle.params.theta[layout.residual.clone()]
                .iter()
                .any(|value| value.abs() > 1e-6),
            "the fixture residual state is zero"
        );
    }

    fn assert_close(
        actual: f64,
        expected: f64,
        tolerance: f64,
        label: &str,
        joint: usize,
        global: usize,
    ) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "{label} joint {joint} global {global}: {actual:.9e} vs {expected:.9e} (tolerance {tolerance:.3e})"
        );
    }

    #[test]
    fn analytic_gradient_matches_the_evaluated_penalized_gradient() {
        for response in [false, true] {
            let oracle = oracle(2, 8, true, response, None, SEED ^ response as u64);
            let model = oracle.model();
            let (full, scratch) = prepared(&model, &oracle.pass, &oracle.params, &oracle.options);
            assert_required_terms(&oracle, &model, &full);
            let gradient = evaluated_gradient(
                &oracle.gradient_problem,
                &oracle.gradient_options,
                &oracle.roles,
                &oracle.lambdas,
                &model,
                &oracle.params,
            );
            for joint in 0..model.dim {
                let expected = -model.rhs[joint];
                let tolerance = 1e-9 * expected.abs().max(1.0);
                assert_close(
                    gradient[joint],
                    expected,
                    tolerance,
                    &format!("response {response} evaluated gradient"),
                    joint,
                    model.globals[joint],
                );
                assert_close(
                    scratch.gradient[joint],
                    expected,
                    tolerance,
                    &format!("response {response} prepared gradient"),
                    joint,
                    model.globals[joint],
                );
            }
        }
    }

    #[test]
    fn finite_difference_of_the_evaluated_gradient_matches_the_analytic_hessian() {
        for response in [false, true] {
            let oracle = oracle(2, 8, true, response, None, SEED ^ 0x101 ^ response as u64);
            let model = oracle.model();
            let (full, mut scratch) =
                prepared(&model, &oracle.pass, &oracle.params, &oracle.options);
            assert_required_terms(&oracle, &model, &full);
            let base = evaluated_gradient(
                &oracle.gradient_problem,
                &oracle.gradient_options,
                &oracle.roles,
                &oracle.lambdas,
                &model,
                &oracle.params,
            );
            for (label, direction) in direction_masks(&oracle, &model) {
                assert!(
                    direction.iter().any(|value| *value != 0.0),
                    "response {response} {label} direction is zero"
                );
                let plus = shifted(&oracle.params, &model, &direction, FD_STEP);
                let minus = shifted(&oracle.params, &model, &direction, -FD_STEP);
                let estimate = gradient_finite_difference(
                    &oracle,
                    &oracle.gradient_problem,
                    &oracle.gradient_options,
                    &model,
                    &oracle.params,
                    &direction,
                );
                let mut product = vec![0.0_f64; model.dim];
                full.apply(&direction, &mut product, &mut scratch.hessian)
                    .expect("analytic product");
                for joint in 0..model.dim {
                    assert_ladder(
                        &format!("response {response} {label} gradient finite difference"),
                        &estimate,
                        joint,
                        model.globals[joint],
                        product[joint],
                    );
                }
                let f_plus = objective(
                    &oracle.gradient_problem,
                    &oracle.gradient_options,
                    &oracle.lambdas,
                    &plus,
                );
                let f_minus = objective(
                    &oracle.gradient_problem,
                    &oracle.gradient_options,
                    &oracle.lambdas,
                    &minus,
                );
                let slope = (f_plus - f_minus) / (2.0 * FD_STEP);
                let directional = crate::solve::dot(&base, &direction);
                let tolerance = 1e-5 * (1.0 + slope.abs().max(directional.abs()));
                assert!(
                    (slope - directional).abs() <= tolerance,
                    "response {response} {label}: objective finite difference {slope:.9e} vs gradient {directional:.9e} (tolerance {tolerance:.3e})"
                );
            }
        }
    }

    #[test]
    fn hessian_finite_difference_richardson_converges_to_the_analytic_operator() {
        for response in [false, true] {
            let oracle = oracle(2, 8, true, response, None, SEED ^ 0x101 ^ response as u64);
            let model = oracle.model();
            let (full, mut scratch) =
                prepared(&model, &oracle.pass, &oracle.params, &oracle.options);
            assert_required_terms(&oracle, &model, &full);
            richardson_check(
                &oracle,
                &oracle.gradient_problem,
                &oracle.gradient_options,
                &model,
                &full,
                &mut scratch,
            );
        }
        let oracle = oracle(2, 8, true, false, Some(9), SEED ^ 0x404);
        let model = oracle.model();
        let (full, mut scratch) = prepared(&model, &oracle.pass, &oracle.params, &oracle.options);
        assert_required_terms(&oracle, &model, &full);
        let (reweighted, reweighted_options) = reweighted_problem(&oracle);
        richardson_check(
            &oracle,
            &reweighted,
            &reweighted_options,
            &model,
            &full,
            &mut scratch,
        );
    }

    #[test]
    fn analytic_hessian_is_symmetric_on_free_native_response_and_residual_coordinates() {
        for response in [false, true] {
            let oracle = oracle(2, 8, true, response, None, SEED ^ 0x202 ^ response as u64);
            let model = oracle.model();
            let (full, mut scratch) =
                prepared(&model, &oracle.pass, &oracle.params, &oracle.options);
            let probes: Vec<Vec<f64>> = (0..4u64)
                .map(|seed| {
                    (0..model.dim)
                        .map(|joint| {
                            let value =
                                ((model.globals[joint] as u64 * (17 + 2 * seed) + 31 * seed + 7)
                                    % 23) as f64
                                    - 11.0;
                            if value == 0.0 { 0.5 } else { value }
                        })
                        .collect()
                })
                .collect();
            let mut products = Vec::with_capacity(probes.len());
            for probe in &probes {
                let mut product = vec![0.0_f64; model.dim];
                full.apply(probe, &mut product, &mut scratch.hessian)
                    .expect("analytic product");
                products.push(product);
            }
            for left in 0..probes.len() {
                for right in left..probes.len() {
                    let forward = crate::solve::dot(&probes[left], &products[right]);
                    let reverse = crate::solve::dot(&probes[right], &products[left]);
                    let tolerance = 1e-7 * forward.abs().max(reverse.abs()).max(1.0);
                    assert!(
                        (forward - reverse).abs() <= tolerance,
                        "response {response} Hessian asymmetry on probes {left}/{right}: {forward:.9e} vs {reverse:.9e}"
                    );
                }
            }
        }
    }

    #[test]
    fn preconditioner_is_symmetric_and_positive_on_free_coordinates() {
        for response in [false, true] {
            let oracle = oracle(2, 8, true, response, None, SEED ^ 0x303 ^ response as u64);
            let model = oracle.model();
            let solve = JointSolve::new(&model, 0.0).expect("preconditioner factorization");
            let reduced = model.outer_count + model.residual_count;
            let mut reduced_in = vec![0.0_f64; reduced];
            let mut reduced_out = vec![0.0_f64; reduced];
            let mut precondition = |probe: &[f64]| -> Vec<f64> {
                let mut out = vec![0.0_f64; model.dim];
                solve
                    .precondition_full(&model, probe, &mut out, &mut reduced_in, &mut reduced_out)
                    .expect("preconditioner");
                out
            };
            let layout = &oracle.problem.layout;
            let mut targets = vec![layout.clip];
            if model.residual_count > 0 {
                targets.push(model.globals[model.residual_start]);
            }
            targets.push(layout.tone_c0);
            targets.push(layout.tint_a.start);
            targets.push(layout.g.start);
            targets.push(layout.r.start);
            targets.push(layout.lambda.start);
            targets.push(layout.image(0).start);
            targets.push(layout.image(0).start + 1);
            targets.push(layout.image(0).start + 2);
            if !layout.response.is_empty() {
                targets.push(layout.response.start);
            }
            for target in targets {
                let Some(joint) = model.globals.iter().position(|global| *global == target) else {
                    continue;
                };
                let mut probe = vec![0.0_f64; model.dim];
                probe[joint] = 1.0;
                let applied = precondition(&probe);
                let energy = crate::solve::dot(&probe, &applied);
                assert!(
                    energy.is_finite() && energy > 0.0,
                    "response {response} preconditioner is not positive at global {target}: {energy:.9e}"
                );
            }
            let probes: Vec<Vec<f64>> = (0..4u64)
                .map(|seed| {
                    (0..model.dim)
                        .map(|joint| {
                            let value =
                                ((joint as u64 * (7 + 2 * seed) + 5 * seed + 3) % 11) as f64 - 5.0;
                            if value == 0.0 { 0.5 } else { value }
                        })
                        .collect()
                })
                .collect();
            let mut images = Vec::with_capacity(probes.len());
            for probe in &probes {
                let applied = precondition(probe);
                let energy = crate::solve::dot(probe, &applied);
                assert!(
                    energy.is_finite() && energy > 0.0,
                    "response {response} preconditioner is not positive on a dense probe: {energy:.9e}"
                );
                images.push(applied);
            }
            for left in 0..probes.len() {
                for right in left..probes.len() {
                    let forward = crate::solve::dot(&probes[left], &images[right]);
                    let reverse = crate::solve::dot(&probes[right], &images[left]);
                    let tolerance = 1e-9 * forward.abs().max(reverse.abs()).max(1.0);
                    assert!(
                        (forward - reverse).abs() <= tolerance,
                        "response {response} preconditioner is asymmetric on probes {left}/{right}: {forward:.9e} vs {reverse:.9e}"
                    );
                }
            }
        }
    }

    #[test]
    fn nonunit_data_scale_matches_a_reweighted_full_objective_gradient_fd() {
        let oracle = oracle(2, 8, true, false, Some(9), SEED ^ 0x404);
        assert!(
            oracle.data_scale > 1.0 + 1e-9,
            "the fixture data scale is unit: {}",
            oracle.data_scale
        );
        let model = oracle.model();
        let (full, mut scratch) = prepared(&model, &oracle.pass, &oracle.params, &oracle.options);
        assert_required_terms(&oracle, &model, &full);
        let (reweighted, reweighted_options) = reweighted_problem(&oracle);
        let base = evaluated_gradient(
            &reweighted,
            &reweighted_options,
            &oracle.roles,
            &oracle.lambdas,
            &model,
            &oracle.params,
        );
        for (label, direction) in direction_masks(&oracle, &model) {
            assert!(
                direction.iter().any(|value| *value != 0.0),
                "{label} direction is zero"
            );
            let plus = shifted(&oracle.params, &model, &direction, FD_STEP);
            let minus = shifted(&oracle.params, &model, &direction, -FD_STEP);
            let estimate = gradient_finite_difference(
                &oracle,
                &reweighted,
                &reweighted_options,
                &model,
                &oracle.params,
                &direction,
            );
            let mut product = vec![0.0_f64; model.dim];
            full.apply(&direction, &mut product, &mut scratch.hessian)
                .expect("analytic product");
            for joint in 0..model.dim {
                assert_ladder(
                    &format!("nonunit scale {label} gradient finite difference"),
                    &estimate,
                    joint,
                    model.globals[joint],
                    product[joint],
                );
            }
            let f_plus = objective(&reweighted, &reweighted_options, &oracle.lambdas, &plus);
            let f_minus = objective(&reweighted, &reweighted_options, &oracle.lambdas, &minus);
            let slope = (f_plus - f_minus) / (2.0 * FD_STEP);
            let directional = crate::solve::dot(&base, &direction);
            let tolerance = 1e-5 * (1.0 + slope.abs().max(directional.abs()));
            assert!(
                (slope - directional).abs() <= tolerance,
                "nonunit scale {label}: objective finite difference {slope:.9e} vs gradient {directional:.9e} (tolerance {tolerance:.3e})"
            );
        }
    }
}

#[cfg(all(test, feature = "synth"))]
mod fallback_tests {
    use super::*;

    fn group_model(
        matrix: [f64; 4],
        fixed: [f64; 2],
        data_diagonal: [f64; 2],
        image_outer: [f64; 2],
        image_data: f64,
    ) -> JointModel<'static> {
        JointModel {
            images: vec![vec![2]],
            globals: vec![0, 1, 2],
            rows: Vec::new(),
            rhs: vec![1.0, 1.0, 0.0],
            fixed: vec![fixed[0], fixed[1], 0.0],
            floor: vec![0.0; 3],
            penalty: Vec::new(),
            penalty_diagonal: vec![0.0; 3],
            data_diagonal: vec![data_diagonal[0], data_diagonal[1], 1.0],
            image_outer: vec![image_outer.to_vec()],
            image_border: vec![Vec::new()],
            image_data: vec![vec![image_data]],
            outer_border: Vec::new(),
            border_data: Vec::new(),
            image_range: vec![(2, 1)],
            joint_image: vec![u32::MAX, u32::MAX, 0],
            joint_local: vec![0, 0, 0],
            groups: vec![JointGroup {
                start: 0,
                matrix: matrix.to_vec(),
            }],
            outer_count: 2,
            border_count: 0,
            residual_start: 3,
            residual_count: 0,
            residual_local_of: Vec::new(),
            residual_data_scale: 1.0,
            residual_samples: &[],
            image_width: 1,
            dim: 3,
            robust: false,
            second: false,
        }
    }

    fn apply(solve: &JointSolve, residual: [f64; 2]) -> [f64; 2] {
        let mut z = [0.0_f64; 2];
        solve.precondition(&residual, &mut z);
        z
    }

    #[test]
    fn reduced_and_block_paths_are_unchanged() {
        let reduced = group_model(
            [4.0, 1.0, 1.0, 4.0],
            [2.0, 2.0],
            [3.0, 3.0],
            [0.0, 0.0],
            1.0,
        );
        let (baseline, baseline_groups) =
            JointSolve::diagnostic(&reduced, 1.0, DiagonalFallback::Baseline)
                .expect("baseline reduced");
        let (candidate, candidate_groups) =
            JointSolve::diagnostic(&reduced, 1.0, DiagonalFallback::ZeroedOffDiagonals)
                .expect("candidate reduced");
        assert_eq!(baseline_groups[0].choice, PreconditionerChoice::Reduced);
        assert_eq!(candidate_groups[0].choice, PreconditionerChoice::Reduced);
        assert_eq!(apply(&baseline, [1.0, 0.0]), apply(&candidate, [1.0, 0.0]));

        let block = group_model(
            [-10.0, 17.0, 17.0, -10.0],
            [10.0, 10.0],
            [20.0, 20.0],
            [4.0, 8.0],
            1.0,
        );
        let (baseline, baseline_groups) =
            JointSolve::diagnostic(&block, 1.0, DiagonalFallback::Baseline)
                .expect("baseline block");
        let (candidate, candidate_groups) =
            JointSolve::diagnostic(&block, 1.0, DiagonalFallback::ZeroedOffDiagonals)
                .expect("candidate block");
        assert_eq!(baseline_groups[0].choice, PreconditionerChoice::Block);
        assert_eq!(candidate_groups[0].choice, PreconditionerChoice::Block);
        assert_eq!(apply(&baseline, [1.0, 0.0]), apply(&candidate, [1.0, 0.0]));
    }

    #[test]
    fn applied_diagonal_preserves_computed_diagonal_and_zeroes_cross_response() {
        let model = group_model(
            [-10.0, 17.0, 17.0, -10.0],
            [10.0, 10.0],
            [10.0, 10.0],
            [1.0, 2.0],
            1.0,
        );
        let (baseline, baseline_groups) =
            JointSolve::diagnostic(&model, 1.0, DiagonalFallback::Baseline)
                .expect("baseline diagonal");
        let (candidate, candidate_groups) =
            JointSolve::diagnostic(&model, 1.0, DiagonalFallback::ZeroedOffDiagonals)
                .expect("candidate diagonal");
        assert_eq!(baseline_groups[0].choice, PreconditionerChoice::Diagonal);
        assert_eq!(candidate_groups[0].choice, PreconditionerChoice::Diagonal);

        let baseline_x = apply(&baseline, [1.0, 0.0]);
        let baseline_y = apply(&baseline, [0.0, 1.0]);
        let candidate_x = apply(&candidate, [1.0, 0.0]);
        let candidate_y = apply(&candidate, [0.0, 1.0]);

        // The retained fallback corrections are (1/2)² and (2/2)².
        let determinant = 19.75 * 19.0 - 17.0 * 17.0;
        assert!((baseline_x[0] - 19.0 / determinant).abs() <= 1e-12);
        assert!((baseline_x[1] + 17.0 / determinant).abs() <= 1e-12);
        assert!((baseline_y[0] + 17.0 / determinant).abs() <= 1e-12);
        assert!((baseline_y[1] - 19.75 / determinant).abs() <= 1e-12);

        assert!((candidate_x[0] - 1.0 / 19.75).abs() <= 1e-12);
        assert_eq!(candidate_x[1], 0.0);
        assert_eq!(candidate_y[0], 0.0);
        assert!((candidate_y[1] - 1.0 / 19.0).abs() <= 1e-12);
    }
}

/// A stage boundary must not adopt a step that no pass has evaluated.
#[cfg(all(test, feature = "synth"))]
mod look_only_domain_tests {
    use super::*;

    const CLIP_K: f64 = 0.03;
    const SEED: u64 = 0x5eed_0b1e;

    fn frozen() -> ActiveColumns {
        ActiveColumns {
            image: false,
            mode: false,
            wb: false,
            clip: false,
            response: false,
        }
    }

    /// One image whose black-target rows pull the tone black point down.
    fn fixture() -> (Vec<FitImage>, Vec<RowModel<'static>>, FitOptions) {
        let data = crate::synth::dataset(&crate::synth::SynthConfig {
            seed: SEED,
            images: 1,
            samples_per_image: 96,
            noise_ucs: 0.5,
            clip_k: CLIP_K,
            ..crate::synth::SynthConfig::default()
        });
        let image = &data.images[0];
        let mut header = image.samples.header().clone();
        let mut in_rgb = image.samples.in_rgb().to_vec();
        let mut out_rgb = image.samples.out_rgb().to_vec();
        let mut density = image.samples.density_w().to_vec();
        let mut flags = image.samples.flags().to_vec();
        for _ in 0..24 {
            in_rgb.push([1.0e-6, 1.04e-6, 1.04e-6]);
            out_rgb.push([0.0, 0.0, 0.0]);
            density.push(1.0);
            flags.push(looklab_samples::SampleFlags::NONE);
        }
        header.n = in_rgb.len() as u32;
        let samples = looklab_samples::SampleSet::new(header, in_rgb, out_rgb, density, flags)
            .expect("fixture samples");
        let mut image = image.clone();
        image.samples = samples;
        let rows = vec![RowModel {
            group: 0,
            model: None,
            clip_k: CLIP_K,
            firmware: [0.0; 2],
            eligible_transform: image.transform,
            eligible_hint: image.exposure_hint,
        }];
        let options = FitOptions {
            seed: SEED,
            max_samples_per_image: usize::MAX,
            hessian_samples: usize::MAX,
            run_gcv: false,
            fit_residual: true,
            ..FitOptions::default()
        };
        (vec![image], rows, options)
    }

    fn assert_feasible(
        images: &[FitImage],
        rows: &[RowModel<'_>],
        options: &FitOptions,
        theta: &[f64],
    ) {
        let problem = Problem::new_with_rows(images, rows, options).expect("problem");
        let params = Params {
            layout: problem.layout.clone(),
            theta: theta.to_vec(),
        };
        let pass = problem.pass(
            &params,
            frozen(),
            &mut Vec::new(),
            false,
            false,
            false,
            IntentFilter::NativeOnly,
        );
        assert!(
            !pass.infeasible,
            "the returned look state must stay inside the evaluator domain"
        );
    }

    #[test]
    fn a_stage_exit_step_that_leaves_the_domain_is_dropped() {
        let (images, rows, options) = fixture();
        let problem = Problem::new_with_rows(&images, &rows, &options).expect("problem");
        assert!(
            problem
                .samples
                .iter()
                .any(|sample| sample.target.ucs.j < 1e-6),
            "the fixture must retain a black-target row"
        );
        let mut remaining = f64::MAX;
        let result = run_look_only_rows(
            &images,
            &options,
            None,
            CLIP_K,
            &rows,
            None,
            None,
            None,
            &[],
            &mut remaining,
        )
        .expect("the look solve must reject the domain-exiting step instead of failing");
        assert_feasible(&images, &rows, &options, &result.theta);
    }

    #[test]
    fn an_infeasible_seed_state_still_reports_the_row_context() {
        let (images, rows, options) = fixture();
        let problem = Problem::new_with_rows(&images, &rows, &options).expect("problem");
        let mut theta = initial_params(&problem, &options).theta;
        theta[problem.layout.tone_c0] += 1.0e6;
        let seed =
            seed_look_only_params(&problem, &options, None, CLIP_K, &rows, Some(&theta), &[])
                .expect("seed params");
        let pass = problem.pass(
            &seed,
            frozen(),
            &mut Vec::new(),
            false,
            false,
            false,
            IntentFilter::NativeOnly,
        );
        assert!(
            pass.infeasible,
            "the control seed state must be outside the evaluator domain"
        );
        let mut remaining = f64::MAX;
        let error = match run_look_only_rows(
            &images,
            &options,
            None,
            CLIP_K,
            &rows,
            None,
            Some(&theta),
            None,
            &[],
            &mut remaining,
        ) {
            Ok(_) => panic!("a state that is infeasible before any step must still fail"),
            Err(error) => error,
        };
        assert!(
            matches!(error, FitError::RowContext(_)),
            "unexpected error: {error:?}"
        );
    }
}
