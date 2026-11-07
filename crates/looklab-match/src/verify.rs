// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_color::ucs::{Ucs, delta_e_ucs};
use looklab_color::{Rgb, RgbSpace, Xyz};
use looklab_model::LookInput;
use looklab_model::realization::{ControlResponseModel, Settings};
use looklab_model::solve::ucs_of_xyz;
use looklab_samples::{OutputSpace, SampleSet};
use looklab_schema::ControlKey;

use crate::error::{MatchError, Result};
use crate::model::MatchModel;
use crate::optimizer::{MatchOptions, Suggestion, optimize, quantile};
use crate::settings::Plan;
use crate::target::PreparedTarget;

/// `in_rgb` is camera RGB before the body realization effects; `A_c · in_rgb`
/// is the scene the target is evaluated on. `out_rgb` is linear output RGB.
#[derive(Clone, Debug, PartialEq)]
pub struct ObservedRender {
    pub samples: SampleSet,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FitObservedOptions {
    pub max_samples: usize,
    pub iterations: usize,
    pub ridge: f64,
    pub min_factor: f64,
    pub max_factor: f64,
}

impl Default for FitObservedOptions {
    fn default() -> Self {
        Self {
            max_samples: 400,
            iterations: 2,
            ridge: 1e-3,
            min_factor: 0.05,
            max_factor: 20.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeltaStats {
    pub count: usize,
    pub median: f64,
    pub p95: f64,
    pub max: f64,
}

impl DeltaStats {
    fn new(mut values: Vec<f64>) -> Self {
        values.sort_by(f64::total_cmp);
        Self {
            count: values.len(),
            median: quantile(&values, 0.5),
            p95: quantile(&values, 0.95),
            max: values.last().copied().unwrap_or(0.0),
        }
    }
}

pub trait ObservedFitter {
    fn fit(
        &self,
        model: &MatchModel,
        plan: &Plan,
        settings: &Settings,
        observed: &[ObservedRender],
        options: &FitObservedOptions,
    ) -> Result<ControlResponseModel>;
}

pub trait TruthRenderer {
    fn render(&self, model: &MatchModel, settings: &Settings, xyz: Xyz) -> Result<Ucs>;
}

impl TruthRenderer for MatchModel {
    fn render(&self, _model: &MatchModel, settings: &Settings, xyz: Xyz) -> Result<Ucs> {
        self.predict(settings, xyz)
    }
}

/// A few Gauss-Newton iterations on one scale factor per non-default control.
pub struct ScaleFitter;

impl ScaleFitter {
    /// # Errors
    /// Reports a failed solve or an invalid control-response model.
    pub fn fit_with_scales(
        &self,
        model: &MatchModel,
        plan: &Plan,
        settings: &Settings,
        observed: &[ObservedRender],
        options: &FitObservedOptions,
    ) -> Result<(ControlResponseModel, Vec<(ControlKey, f64)>)> {
        let mut crm = model.crm.clone();
        let mut scales: Vec<(ControlKey, f64)> = Vec::new();
        for _ in 0..options.iterations {
            let candidate = MatchModel {
                crm: crm.clone(),
                ..model.clone()
            };
            let active: Vec<usize> = plan
                .stage_controls()
                .filter(|control| {
                    !plan.is_default(*control, settings) && has_response(&crm, plan.key(*control))
                })
                .collect();
            if active.is_empty() {
                break;
            }
            let size = active.len();
            let mut hessian = vec![vec![0.0_f64; size]; size];
            let mut gradient = vec![0.0_f64; size];
            for render in observed {
                let realization = candidate.realize(settings)?;
                let space = output_space(render.samples.header().output_space);
                let n = render.samples.n();
                let stride = (n / options.max_samples.max(1)).max(1);
                for sample in (0..n).step_by(stride) {
                    let rgb = render.samples.in_rgb()[sample].map(f64::from);
                    let scene = candidate.scene_from_camera(rgb);
                    let input_xyz = candidate.input_xyz(&realization, scene);
                    let prediction = realization.look.eval(input_xyz);
                    let observed_ucs = observed_ucs(&render.samples, sample, space)?;
                    let residual = [
                        prediction.j - observed_ucs.j,
                        prediction.a - observed_ucs.a,
                        prediction.b - observed_ucs.b,
                    ];
                    let jacobian = realization.look.coef_jacobian(&LookInput::new(input_xyz));
                    let derivatives = derivatives_at(&candidate, &realization, scene);
                    let mut effects = Vec::with_capacity(size);
                    for &control in &active {
                        let key = plan.key(control);
                        let value = plan.value(control, settings);
                        let (delta, wb, _) = candidate.value_row(key, &value)?;
                        let mut effect = delta.project(&jacobian);
                        for (channel, value) in effect.iter_mut().enumerate() {
                            *value +=
                                wb[0] * derivatives.0[channel] + wb[1] * derivatives.1[channel];
                        }
                        effects.push(effect);
                    }
                    for row in 0..size {
                        gradient[row] -= effects[row][0] * residual[0]
                            + effects[row][1] * residual[1]
                            + effects[row][2] * residual[2];
                        for column in 0..size {
                            hessian[row][column] += effects[row][0] * effects[column][0]
                                + effects[row][1] * effects[column][1]
                                + effects[row][2] * effects[column][2];
                        }
                    }
                }
            }
            for (index, row) in hessian.iter_mut().enumerate() {
                row[index] += options.ridge;
            }
            let correction = solve(&mut hessian, &mut gradient)?;
            for (slot, &control) in active.iter().enumerate() {
                let factor = (1.0 + correction[slot]).clamp(options.min_factor, options.max_factor);
                let key = plan.key(control);
                scale_response(&mut crm, key, factor);
                match scales
                    .iter_mut()
                    .find(|(known, _)| known.as_str() == key.as_str())
                {
                    Some((_, total)) => *total *= factor,
                    None => scales.push((key.clone(), factor)),
                }
            }
        }
        scales.sort_by(|left, right| left.0.cmp(&right.0));
        Ok((crm, scales))
    }
}

impl ObservedFitter for ScaleFitter {
    fn fit(
        &self,
        model: &MatchModel,
        plan: &Plan,
        settings: &Settings,
        observed: &[ObservedRender],
        options: &FitObservedOptions,
    ) -> Result<ControlResponseModel> {
        Ok(self
            .fit_with_scales(model, plan, settings, observed, options)?
            .0)
    }
}

fn has_response(crm: &ControlResponseModel, key: &looklab_schema::ControlKey) -> bool {
    crm.controls.iter().any(|control| &control.key == key)
        || crm.wb.controls.iter().any(|control| &control.key == key)
}

pub(crate) fn scale_response(crm: &mut ControlResponseModel, key: &ControlKey, factor: f64) {
    for control in &mut crm.controls {
        if &control.key != key {
            continue;
        }
        for value in &mut control.values {
            for layer in &mut value.delta.layers {
                for (_, coefficient) in &mut layer.entries {
                    *coefficient *= factor;
                }
            }
        }
    }
    for control in &mut crm.wb.controls {
        if &control.key != key {
            continue;
        }
        for value in &mut control.values {
            value.log_gain[0] *= factor;
            value.log_gain[1] *= factor;
        }
    }
}

fn derivatives_at(
    model: &MatchModel,
    realization: &looklab_model::realization::Realization,
    xyz: Xyz,
) -> ([f64; 3], [f64; 3]) {
    let step = 1e-3;
    let base = model.input_ucs(realization, xyz, [0.0, 0.0], 0.0);
    let red = model.input_ucs(realization, xyz, [step, 0.0], 0.0);
    let blue = model.input_ucs(realization, xyz, [0.0, step], 0.0);
    (
        [
            (red.j - base.j) / step,
            (red.a - base.a) / step,
            (red.b - base.b) / step,
        ],
        [
            (blue.j - base.j) / step,
            (blue.a - base.a) / step,
            (blue.b - base.b) / step,
        ],
    )
}

fn output_space(space: OutputSpace) -> RgbSpace {
    match space {
        OutputSpace::Srgb => RgbSpace::Srgb,
        OutputSpace::AdobeRgb => RgbSpace::AdobeRgb,
    }
}

fn observed_ucs(samples: &SampleSet, index: usize, space: RgbSpace) -> Result<Ucs> {
    let rgb = samples.out_rgb()[index].map(f64::from);
    let xyz = space.rgb_linear_to_xyz(Rgb::new(rgb[0], rgb[1], rgb[2]));
    Ok(ucs_of_xyz(xyz))
}

fn solve(matrix: &mut [Vec<f64>], rhs: &mut [f64]) -> Result<Vec<f64>> {
    let n = rhs.len();
    for column in 0..n {
        let mut pivot = column;
        for row in column + 1..n {
            if matrix[row][column].abs() > matrix[pivot][column].abs() {
                pivot = row;
            }
        }
        if matrix[pivot][column].abs() < 1e-12 {
            return Err(MatchError::SingularSystem);
        }
        matrix.swap(column, pivot);
        rhs.swap(column, pivot);
        let divisor = matrix[column][column];
        for entry in matrix[column].iter_mut().skip(column) {
            *entry /= divisor;
        }
        rhs[column] /= divisor;
        for row in 0..n {
            if row == column {
                continue;
            }
            let factor = matrix[row][column];
            if factor == 0.0 {
                continue;
            }
            let pivot_row = matrix[column].clone();
            for (index, value) in matrix[row].iter_mut().enumerate().skip(column) {
                *value -= factor * pivot_row[index];
            }
            rhs[row] -= factor * rhs[column];
        }
    }
    Ok(rhs.to_vec())
}

pub fn measure_target_error(
    target: &PreparedTarget<'_>,
    model: &MatchModel,
    _settings: &Settings,
    observed: &[ObservedRender],
) -> Result<DeltaStats> {
    let mut values = Vec::new();
    for render in observed {
        let space = output_space(render.samples.header().output_space);
        for sample in 0..render.samples.n() {
            let rgb = render.samples.in_rgb()[sample].map(f64::from);
            let scene = model.scene_from_camera(rgb);
            let expected = target.function.predict(scene);
            let observed_ucs = observed_ucs(&render.samples, sample, space)?;
            values.push(delta_e_ucs(expected, observed_ucs));
        }
    }
    Ok(DeltaStats::new(values))
}

pub fn select_verification_samples(
    model: &MatchModel,
    settings: &Settings,
    candidates: &[SampleSet],
    count: usize,
) -> Result<Vec<usize>> {
    let realization = model.realize(settings)?;
    let mut ranked: Vec<(f64, usize)> = Vec::with_capacity(candidates.len());
    for (index, samples) in candidates.iter().enumerate() {
        let n = samples.n();
        let stride = (n / 100).max(1);
        let mut total = 0.0_f64;
        let mut seen = 0_usize;
        for sample in (0..n).step_by(stride) {
            let rgb = samples.in_rgb()[sample].map(f64::from);
            let scene = model.scene_from_camera(rgb);
            total += crate::covariance::trace_with(model, &realization, settings, scene)?;
            seen += 1;
        }
        ranked.push((total / seen.max(1) as f64, index));
    }
    ranked.sort_by(|left, right| right.0.total_cmp(&left.0));
    let take = count.clamp(2, 4).min(ranked.len());
    Ok(ranked
        .into_iter()
        .take(take)
        .map(|(_, index)| index)
        .collect())
}

pub struct VerifyRequest<'a> {
    pub target: &'a PreparedTarget<'a>,
    pub plan: &'a Plan,
    pub model: &'a MatchModel,
    pub observed: &'a [ObservedRender],
    pub options: &'a MatchOptions,
    pub fit_options: FitObservedOptions,
    pub truth: Option<&'a dyn TruthRenderer>,
}

pub struct VerifyOutcome {
    pub first: Suggestion,
    pub updated: MatchModel,
    pub second: Suggestion,
    pub first_measured: DeltaStats,
    pub second_measured: Option<DeltaStats>,
    pub reduction: Option<f64>,
}

pub fn verify_once(
    request: &VerifyRequest<'_>,
    fitter: &dyn ObservedFitter,
) -> Result<VerifyOutcome> {
    if request.observed.is_empty() {
        return Err(MatchError::EmptyObserved);
    }
    let first_outcome = optimize(
        std::slice::from_ref(request.model),
        request.target,
        request.plan,
        request.options,
    )?;
    let first = first_outcome
        .results
        .into_iter()
        .next()
        .ok_or(MatchError::NoSupportedProbes)?;
    let updated_crm = fitter.fit(
        request.model,
        request.plan,
        &first.settings,
        request.observed,
        &request.fit_options,
    )?;
    let updated = request.model.with_crm(updated_crm)?;
    let second_outcome = optimize(
        std::slice::from_ref(&updated),
        request.target,
        request.plan,
        request.options,
    )?;
    let second = second_outcome
        .results
        .into_iter()
        .next()
        .ok_or(MatchError::NoSupportedProbes)?;
    let first_measured = measure_target_error(
        request.target,
        request.model,
        &first.settings,
        request.observed,
    )?;
    let second_measured = match request.truth {
        Some(truth) => Some(measure_truth(
            request.target,
            request.model,
            truth,
            &second.settings,
            request.observed,
        )?),
        None => None,
    };
    let reduction = second_measured.and_then(|second| {
        if first_measured.median > 0.0 {
            Some(1.0 - second.median / first_measured.median)
        } else {
            None
        }
    });
    Ok(VerifyOutcome {
        first,
        updated,
        second,
        first_measured,
        second_measured,
        reduction,
    })
}

fn measure_truth(
    target: &PreparedTarget<'_>,
    model: &MatchModel,
    truth: &dyn TruthRenderer,
    second: &Settings,
    observed: &[ObservedRender],
) -> Result<DeltaStats> {
    let mut values = Vec::new();
    for render in observed {
        for sample in 0..render.samples.n() {
            let rgb = render.samples.in_rgb()[sample].map(f64::from);
            let scene = model.scene_from_camera(rgb);
            let expected = target.function.predict(scene);
            let rendered = truth.render(model, second, scene)?;
            values.push(delta_e_ucs(expected, rendered));
        }
    }
    Ok(DeltaStats::new(values))
}
