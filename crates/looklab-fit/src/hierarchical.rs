// SPDX-License-Identifier: AGPL-3.0-or-later
//! Global hierarchical fit across bodies (docs/02 §4.3): block coordinate descent
//! between per-body parameters and the shared Look, then a recipe stage with the
//! Look frozen.

use std::collections::{BTreeMap, BTreeSet};

use looklab_body::{
    A0Source, AnchorOptions, BodyId, Delta6, InputTransform, TransformVersion, chart_anchor_terms,
};
use looklab_color::Mat3;
use looklab_model::basis::TONE_COEFS;
use looklab_model::{Look, Tone};

use crate::fit;
use crate::gauge::{ComponentGauge, WbModeClass};
use crate::layout::{Layout, softplus_inverse, softplus_slope};
use crate::predict::{ColumnMap, ImageCtx, Jac, evaluate_sample};
use crate::problem::Problem;
use crate::solve::{factor_in_place, solve_in_place};
use crate::types::{
    ContributorId, Density, ExposureModeId, ExposureOffset, FitError, FitImage, FitOptions,
    FitResult, ImageIntent, ImageNuisances, ProbeUncertainty, Result,
};

/// Two firmware offset coordinates: a constant `T` shift and a constant `log G` shift.
pub const FIRMWARE_PARAMETERS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FirmwareId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LookFamilyId(pub u32);

#[derive(Clone, Debug)]
pub struct HierImage {
    pub image: FitImage,
    pub look: LookFamilyId,
    pub firmware: FirmwareId,
    pub wb_mode: WbModeClass,
}

#[derive(Clone, Debug)]
pub struct BodyData {
    pub id: BodyId,
    pub transform: InputTransform,
    pub default_mode: ExposureModeId,
    pub default_wb_mode: WbModeClass,
    pub images: Vec<HierImage>,
    pub charts: Vec<looklab_body::ChartSession>,
}

impl BodyData {
    /// Chart- and SSF-sourced priors are tight enough to define the global `Δ` gauge.
    pub fn anchored(&self) -> bool {
        matches!(self.transform.source(), A0Source::Ssf | A0Source::Chart)
    }

    fn native_samples(&self) -> usize {
        self.images
            .iter()
            .filter(|image| image.image.intent == ImageIntent::Native)
            .map(|image| image.image.samples.n())
            .sum()
    }
}

#[derive(Clone, Debug)]
pub struct HierOptions {
    pub fit: FitOptions,
    pub outer_iterations: usize,
    pub body_iterations: usize,
    pub firmware_prior_sd: [f64; FIRMWARE_PARAMETERS],
    pub anchor: AnchorOptions,
}

impl Default for HierOptions {
    fn default() -> Self {
        Self {
            fit: FitOptions::default(),
            outer_iterations: 3,
            body_iterations: 6,
            firmware_prior_sd: [0.5, 0.03],
            anchor: AnchorOptions::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct BodyParameters {
    pub id: BodyId,
    /// Fitted `Δ` with the source prior covariance.
    pub transform: InputTransform,
    pub exposure: Vec<ExposureOffset>,
    pub wb: Vec<(WbModeClass, [f64; 2])>,
    pub clip_k: f64,
    pub images: Vec<ImageNuisances>,
    pub recipe_fitted: bool,
}

#[derive(Clone, Debug)]
pub struct LookProvenance {
    pub body: BodyId,
    pub transform_version: TransformVersion,
    pub delta: Delta6,
    pub exposure: Vec<ExposureOffset>,
    pub wb: Vec<(WbModeClass, [f64; 2])>,
}

/// Image indices of one body inside a pooled Look fit.
#[derive(Clone, Debug)]
pub struct BodyGroup {
    pub body: BodyId,
    pub images: Vec<usize>,
}

#[derive(Clone, Debug)]
pub struct LookParameters {
    pub id: LookFamilyId,
    pub look: Look,
    pub firmware: Vec<(FirmwareId, [f64; FIRMWARE_PARAMETERS])>,
    pub probes: ProbeUncertainty,
    pub density: Density,
    pub provenance: Vec<LookProvenance>,
    /// Native images with the published body parameters folded in.
    pub pool: Vec<FitImage>,
    pub groups: Vec<BodyGroup>,
}

#[derive(Clone, Debug)]
pub struct HierDiagnostics {
    pub evaluations: usize,
    pub outer_iterations: usize,
    pub sigma_v: Vec<(ContributorId, f64)>,
}

#[derive(Clone, Debug)]
pub struct HierarchicalFit {
    pub looks: Vec<LookParameters>,
    pub bodies: Vec<BodyParameters>,
    pub components: Vec<ComponentGauge>,
    pub diagnostics: HierDiagnostics,
}

impl HierarchicalFit {
    /// Every Look whose native body set intersects `dirty`.
    pub fn refit_cascade(&self, dirty: &[BodyId]) -> Vec<LookFamilyId> {
        let dirty: std::collections::HashSet<&BodyId> = dirty.iter().collect();
        let mut out: Vec<LookFamilyId> = self
            .looks
            .iter()
            .filter(|look| {
                look.provenance
                    .iter()
                    .any(|entry| dirty.contains(&entry.body))
            })
            .map(|look| look.id)
            .collect();
        out.sort();
        out
    }

    pub fn look(&self, id: LookFamilyId) -> Option<&LookParameters> {
        self.looks.iter().find(|look| look.id == id)
    }
}

/// A constant `T` offset and a constant `log G` offset over the whole layer.
pub fn firmware_look(look: &Look, offset: [f64; FIRMWARE_PARAMETERS]) -> Look {
    let mut out = look.clone();
    if let Tone::Spline { c0, beta } = &look.tone
        && let Ok(tone) = Tone::spline(c0 + offset[0], *beta)
    {
        out.tone = tone;
    }
    for value in &mut out.color.g.plane {
        *value += offset[1];
    }
    for value in &mut out.color.g.volume {
        *value += offset[1];
    }
    out
}

#[derive(Clone, Debug)]
struct BodyState {
    delta: Delta6,
    xi: Vec<(ExposureModeId, f64)>,
    omega: Vec<(WbModeClass, [f64; 2])>,
    clip: f64,
    nuisances: Vec<[f64; 3]>,
}

impl BodyState {
    fn initial(body: &BodyData, clip: f64, warm: Option<&BodyState>) -> Self {
        match warm {
            Some(state) => state.clone(),
            None => Self {
                delta: body.transform.delta(),
                xi: Vec::new(),
                omega: Vec::new(),
                clip,
                nuisances: vec![[0.0; 3]; body.images.len()],
            },
        }
    }

    fn xi(&self, mode: ExposureModeId) -> f64 {
        self.xi
            .iter()
            .find(|(candidate, _)| *candidate == mode)
            .map_or(0.0, |(_, value)| *value)
    }

    fn omega(&self, class: WbModeClass) -> [f64; 2] {
        self.omega
            .iter()
            .find(|(candidate, _)| *candidate == class)
            .map_or([0.0; 2], |(_, value)| *value)
    }
}

struct BodyLayout {
    delta: [usize; 6],
    xi: Vec<Option<usize>>,
    omega: Vec<[Option<usize>; 2]>,
    mode_ids: Vec<ExposureModeId>,
    wb_ids: Vec<WbModeClass>,
    clip: usize,
    image: Vec<[usize; 3]>,
    total: usize,
    offset: usize,
}

struct BodyPass {
    gradient: Vec<f64>,
    hessian: Vec<f64>,
    cost: f64,
    pairs: Vec<Vec<(f64, f64)>>,
}

fn contributor_index(contributors: &[ContributorId], id: ContributorId) -> usize {
    contributors
        .iter()
        .position(|candidate| *candidate == id)
        .unwrap_or(0)
}

fn unique_modes(images: &[HierImage], active: &[usize]) -> Vec<ExposureModeId> {
    let mut modes = Vec::new();
    for index in active {
        let mode = images[*index].image.exposure_mode;
        if !modes.contains(&mode) {
            modes.push(mode);
        }
    }
    modes
}

fn unique_wb_modes(images: &[HierImage], active: &[usize]) -> Vec<WbModeClass> {
    let mut modes = Vec::new();
    for index in active {
        let mode = images[*index].wb_mode;
        if !modes.contains(&mode) {
            modes.push(mode);
        }
    }
    modes
}

#[allow(clippy::too_many_arguments)]
fn body_layout(
    active: &[usize],
    modes: &[ExposureModeId],
    wb_modes: &[WbModeClass],
    gauge: &ComponentGauge,
    designated: bool,
    offset: usize,
) -> BodyLayout {
    let mut next = 0usize;
    let mut alloc = || {
        let index = next;
        next += 1;
        index
    };
    let delta = core::array::from_fn(|_| alloc());
    let xi: Vec<Option<usize>> = modes
        .iter()
        .map(|mode| {
            if designated && *mode == gauge.exposure_mode {
                None
            } else {
                Some(alloc())
            }
        })
        .collect();
    let omega: Vec<[Option<usize>; 2]> = wb_modes
        .iter()
        .map(|mode| {
            if designated && *mode == gauge.wb_mode {
                [None, None]
            } else {
                [Some(alloc()), Some(alloc())]
            }
        })
        .collect();
    let clip = alloc();
    let image = active.iter().map(|_| [alloc(), alloc(), alloc()]).collect();
    BodyLayout {
        delta,
        xi,
        omega,
        mode_ids: modes.to_vec(),
        wb_ids: wb_modes.to_vec(),
        clip,
        image,
        total: next,
        offset,
    }
}

fn tone_slopes(look: &Look) -> [f64; TONE_COEFS] {
    let mut slopes = [1.0_f64; TONE_COEFS];
    if let Tone::Spline { beta, .. } = &look.tone {
        for (slope, value) in slopes.iter_mut().zip(beta.iter()) {
            *slope = softplus_slope(softplus_inverse(*value));
        }
    }
    slopes
}

#[allow(clippy::too_many_arguments)]
fn body_pass(
    body: &BodyData,
    active: &[usize],
    samples: &[crate::problem::Sample],
    problem_layout: &Layout,
    spaces: &[looklab_color::RgbSpace],
    looks: &BTreeMap<LookFamilyId, Look>,
    firmware: &BTreeMap<FirmwareId, [f64; FIRMWARE_PARAMETERS]>,
    layout: &BodyLayout,
    state: &BodyState,
    sigma: &[f64],
    contributors: &[ContributorId],
    options: &HierOptions,
    delta_precision: &[[f64; 6]; 6],
) -> Result<BodyPass> {
    let total = layout.total;
    let mut gradient = vec![0.0_f64; total];
    let mut hessian = vec![0.0_f64; total * total];
    let mut cost = 0.0;
    let mut pairs: Vec<Vec<(f64, f64)>> = vec![Vec::new(); contributors.len()];
    let mut map = vec![u32::MAX; problem_layout.total + total];
    for (local, global) in local_indices(layout).iter().enumerate() {
        map[layout.offset + global] = local as u32;
    }

    let a0 = *body.transform.a0_d65();
    let transform = a0.mul(&state.delta.transform());
    let modes = unique_modes(&body.images, active);
    let wb_modes = unique_wb_modes(&body.images, active);
    let active_looks: Vec<(Look, [f64; TONE_COEFS])> = active
        .iter()
        .map(|index| {
            let tag = &body.images[*index];
            let base = looks
                .get(&tag.look)
                .cloned()
                .unwrap_or_else(Look::colorimetric);
            let offset = firmware.get(&tag.firmware).copied().unwrap_or([0.0; 2]);
            let look = firmware_look(&base, offset);
            let slopes = tone_slopes(&look);
            (look, slopes)
        })
        .collect();

    let mut jac = Jac::default();
    for sample in samples {
        let position = sample.image as usize;
        let body_index = active[position];
        let tag = &body.images[body_index];
        let nuisance = state.nuisances[body_index];
        let mode_position = modes
            .iter()
            .position(|mode| *mode == tag.image.exposure_mode)
            .unwrap_or(0);
        let wb_position = wb_modes
            .iter()
            .position(|mode| *mode == tag.wb_mode)
            .unwrap_or(0);
        let xi = state.xi(tag.image.exposure_mode);
        let omega = state.omega(tag.wb_mode);
        let (look, slopes) = &active_looks[position];
        let context = ImageCtx {
            transform,
            a0,
            space: spaces[position],
            exposure: xi + nuisance[0],
            wb_r: libm::exp(omega[0] + nuisance[1]),
            wb_b: libm::exp(omega[1] + nuisance[2]),
        };
        let columns = ColumnMap {
            mode: None,
            image: Some(layout.image[position][0] + layout.offset),
            wb_r: Some(layout.image[position][1] + layout.offset),
            wb_b: Some(layout.image[position][2] + layout.offset),
            clip: None,
            exposure_extra: layout.xi[mode_position].map(|index| index + layout.offset),
            wb_r_extra: layout.omega[wb_position][0].map(|index| index + layout.offset),
            wb_b_extra: layout.omega[wb_position][1].map(|index| index + layout.offset),
            clip_extra: Some(layout.clip + layout.offset),
            delta: Some(layout.delta.map(|index| index + layout.offset)),
        };
        let evaluated = evaluate_sample(
            look,
            problem_layout,
            &context,
            &columns,
            sample.rgb,
            state.clip,
            1.0,
            slopes,
            &[],
            &mut jac,
        );
        let target = sample.target;
        let residual = [
            evaluated.prediction.j - target.j,
            evaluated.prediction.a - target.a,
            evaluated.prediction.b - target.b,
        ];
        let squared =
            residual[0] * residual[0] + residual[1] * residual[1] + residual[2] * residual[2];
        let contributor = contributor_index(contributors, tag.image.contributor);
        let variance = libm::exp(2.0 * sigma[contributor]);
        let nu = options.fit.nu;
        let ratio = squared / (nu * variance);
        let alpha = (nu + 3.0) / (nu * variance + squared);
        let weight = sample.weight;
        cost += weight * (0.5 * (nu + 3.0) * libm::log1p(ratio) + 1.5 * libm::log(variance));
        pairs[contributor].push((weight, squared));
        let contribution = weight * alpha;

        let mut buffer: Vec<(u32, [f64; 3])> = Vec::with_capacity(jac.cols.len());
        for (column, derivative) in jac.cols.iter().zip(jac.derivs.iter()) {
            let local = map[*column as usize];
            if local != u32::MAX {
                buffer.push((local, *derivative));
            }
        }
        for (a, (left, left_derivative)) in buffer.iter().enumerate() {
            let base = *left as usize * total;
            for (right, right_derivative) in buffer.iter().skip(a) {
                let dot = left_derivative[0] * right_derivative[0]
                    + left_derivative[1] * right_derivative[1]
                    + left_derivative[2] * right_derivative[2];
                hessian[base + *right as usize] += contribution * dot;
                if right != left {
                    hessian[*right as usize * total + *left as usize] += contribution * dot;
                }
            }
            let term = left_derivative[0] * residual[0]
                + left_derivative[1] * residual[1]
                + left_derivative[2] * residual[2];
            gradient[*left as usize] += contribution * term;
        }
    }

    let exposure_sd = options.fit.exposure_prior_sd;
    let wb_sd = options.fit.wb_prior_sd;
    let mode_sd = options.fit.exposure_mode_prior_sd;
    let delta_params = state.delta.params();
    for (index, value) in delta_params.iter().enumerate() {
        let mut quadratic = 0.0;
        for (other, precision) in delta_precision[index].iter().enumerate() {
            gradient[layout.delta[index]] += precision * delta_params[other];
            hessian[layout.delta[index] * total + layout.delta[other]] += precision;
            quadratic += precision * delta_params[other];
        }
        cost += 0.5 * value * quadratic;
    }
    for (position, mode) in modes.iter().enumerate() {
        if let Some(index) = layout.xi[position] {
            let value = state.xi(*mode);
            gradient[index] += value / (mode_sd * mode_sd);
            hessian[index * total + index] += 1.0 / (mode_sd * mode_sd);
            cost += 0.5 * value * value / (mode_sd * mode_sd);
        }
    }
    for (position, class) in wb_modes.iter().enumerate() {
        let values = state.omega(*class);
        for channel in 0..2 {
            if let Some(index) = layout.omega[position][channel] {
                let value = values[channel];
                gradient[index] += value / (wb_sd * wb_sd);
                hessian[index * total + index] += 1.0 / (wb_sd * wb_sd);
                cost += 0.5 * value * value / (wb_sd * wb_sd);
            }
        }
    }
    let (clip_mean, clip_sd) = options.fit.clip_prior;
    {
        let value = state.clip - clip_mean;
        gradient[layout.clip] += value / (clip_sd * clip_sd);
        hessian[layout.clip * total + layout.clip] += 1.0 / (clip_sd * clip_sd);
        cost += 0.5 * value * value / (clip_sd * clip_sd);
    }
    for (position, body_index) in active.iter().enumerate() {
        let nuisance = state.nuisances[*body_index];
        let hint = body.images[*body_index]
            .image
            .exposure_hint
            .clamp(-8.0, 8.0);
        let columns = layout.image[position];
        let delta = nuisance[0] - hint;
        gradient[columns[0]] += delta / (exposure_sd * exposure_sd);
        hessian[columns[0] * total + columns[0]] += 1.0 / (exposure_sd * exposure_sd);
        cost += 0.5 * delta * delta / (exposure_sd * exposure_sd);
        for channel in 0..2 {
            let value = nuisance[1 + channel];
            gradient[columns[1 + channel]] += value / (wb_sd * wb_sd);
            hessian[columns[1 + channel] * total + columns[1 + channel]] += 1.0 / (wb_sd * wb_sd);
            cost += 0.5 * value * value / (wb_sd * wb_sd);
        }
    }

    if !body.charts.is_empty() {
        let transform = body
            .transform
            .with_delta(state.delta, body.transform.covariance());
        let terms = chart_anchor_terms(&transform, &body.charts, &options.anchor);
        cost += terms.value;
        for (index, column) in terms.gradient.iter().enumerate() {
            gradient[layout.delta[index]] += column;
            for (other, value) in terms.hessian[index].iter().enumerate() {
                hessian[layout.delta[index] * total + layout.delta[other]] += value;
            }
        }
    }

    Ok(BodyPass {
        gradient,
        hessian,
        cost,
        pairs,
    })
}

fn local_indices(layout: &BodyLayout) -> Vec<usize> {
    let mut indices: Vec<usize> = layout.delta.to_vec();
    indices.extend(layout.xi.iter().flatten().copied());
    for pair in &layout.omega {
        indices.extend(pair.iter().flatten().copied());
    }
    indices.push(layout.clip);
    for image in &layout.image {
        indices.extend(image.iter().copied());
    }
    indices
}

#[allow(clippy::too_many_arguments)]
fn solve_body_step(pass: &BodyPass, layout: &BodyLayout, damping: f64) -> Option<Vec<f64>> {
    let n = layout.total;
    let mut matrix = pass.hessian.clone();
    for index in 0..n {
        let diagonal = pass.hessian[index * n + index].max(1e-12);
        matrix[index * n + index] += damping * diagonal;
    }
    let mut rhs: Vec<f64> = pass.gradient.iter().map(|value| -value).collect();
    if !factor_in_place(&mut matrix, n) {
        return None;
    }
    solve_in_place(&matrix, n, &mut rhs);
    Some(rhs)
}

fn apply_body_step_values(state: &mut BodyState, layout: &BodyLayout, step: &[f64]) -> bool {
    let largest = step.iter().fold(0.0_f64, |acc, value| {
        if value.is_finite() {
            acc.max(value.abs())
        } else {
            acc
        }
    });
    let factor = if largest > 2.0 { 2.0 / largest } else { 1.0 };
    let mut params = state.delta.params();
    for index in 0..6 {
        params[index] += factor * step[layout.delta[index]];
    }
    let Ok(delta) = Delta6::new(params) else {
        return false;
    };
    state.delta = delta;
    for (position, id) in layout.mode_ids.iter().enumerate() {
        if let Some(index) = layout.xi[position]
            && let Some((_, value)) = state.xi.iter_mut().find(|(candidate, _)| candidate == id)
        {
            *value += factor * step[index];
        }
    }
    for (position, id) in layout.wb_ids.iter().enumerate() {
        if let Some((_, value)) = state
            .omega
            .iter_mut()
            .find(|(candidate, _)| candidate == id)
        {
            for channel in 0..2 {
                if let Some(index) = layout.omega[position][channel] {
                    value[channel] += factor * step[index];
                }
            }
        }
    }
    state.clip = (state.clip + factor * step[layout.clip]).clamp(0.005, 0.1);
    for (position, columns) in layout.image.iter().enumerate() {
        for channel in 0..3 {
            state.nuisances[position][channel] += factor * step[columns[channel]];
        }
    }
    true
}

fn ensure_modes(state: &mut BodyState, images: &[HierImage], active: &[usize]) {
    for mode in unique_modes(images, active) {
        if !state.xi.iter().any(|(candidate, _)| *candidate == mode) {
            state.xi.push((mode, 0.0));
        }
    }
    for class in unique_wb_modes(images, active) {
        if !state.omega.iter().any(|(candidate, _)| *candidate == class) {
            state.omega.push((class, [0.0; 2]));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn fit_body(
    body: &BodyData,
    looks: &BTreeMap<LookFamilyId, Look>,
    firmware: &BTreeMap<FirmwareId, [f64; FIRMWARE_PARAMETERS]>,
    gauge: &ComponentGauge,
    designated: bool,
    sigma: &[f64],
    contributors: &[ContributorId],
    options: &HierOptions,
    warm: Option<&BodyState>,
    include_recipe: bool,
) -> Result<(BodyState, BodyPass, usize)> {
    let active: Vec<usize> = body
        .images
        .iter()
        .enumerate()
        .filter(|(_, tag)| include_recipe || tag.image.intent == ImageIntent::Native)
        .map(|(index, _)| index)
        .collect();
    let mut state = BodyState::initial(
        body,
        looks.values().next().map_or(0.03, |look| look.clip.k),
        warm,
    );
    if state.nuisances.len() != body.images.len() {
        state.nuisances = vec![[0.0; 3]; body.images.len()];
    }
    ensure_modes(&mut state, &body.images, &active);
    if active.is_empty() {
        return Ok((
            state,
            BodyPass {
                gradient: Vec::new(),
                hessian: Vec::new(),
                cost: 0.0,
                pairs: vec![Vec::new(); contributors.len()],
            },
            0,
        ));
    }

    let filtered: Vec<FitImage> = active
        .iter()
        .map(|index| {
            let mut image = body.images[*index].image.clone();
            image.exposure_hint =
                state.xi(image.exposure_mode) + image.exposure_hint.clamp(-8.0, 8.0);
            image
        })
        .collect();
    let problem = Problem::new_with(&filtered, None, &options.fit)?;
    let problem_layout = problem.layout.clone();
    let spaces: Vec<looklab_color::RgbSpace> =
        problem.images.iter().map(|image| image.space).collect();

    let modes = unique_modes(&body.images, &active);
    let wb_modes = unique_wb_modes(&body.images, &active);
    let layout = body_layout(
        &active,
        &modes,
        &wb_modes,
        gauge,
        designated,
        problem_layout.total,
    );
    let precision = body
        .transform
        .covariance()
        .precision()
        .map_err(|error| FitError::Hierarchy(format!("delta prior: {error}")))?;

    let mut damping = 1e-3;
    let mut evaluations = 0usize;
    let mut pass = body_pass(
        body,
        &active,
        &problem.samples,
        &problem_layout,
        &spaces,
        looks,
        firmware,
        &layout,
        &state,
        sigma,
        contributors,
        options,
        &precision,
    )?;
    evaluations += 1;
    for _ in 0..options.body_iterations.max(1) {
        let Some(step) = solve_body_step(&pass, &layout, damping) else {
            damping = (damping * 10.0).min(1e6);
            if damping >= 1e6 {
                break;
            }
            continue;
        };
        let mut trial = state.clone();
        if !apply_body_step_values(&mut trial, &layout, &step) {
            damping = (damping * 10.0).min(1e6);
            continue;
        }
        let trial_pass = body_pass(
            body,
            &active,
            &problem.samples,
            &problem_layout,
            &spaces,
            looks,
            firmware,
            &layout,
            &trial,
            sigma,
            contributors,
            options,
            &precision,
        )?;
        evaluations += 1;
        if trial_pass.cost <= pass.cost + 1e-12 {
            state = trial;
            pass = trial_pass;
            damping = (damping * 0.4).max(1e-9);
        } else {
            damping = (damping * 10.0).min(1e6);
            if damping >= 1e6 {
                break;
            }
        }
    }
    Ok((state, pass, evaluations))
}

fn apply_delta_gauge(states: &mut [BodyState], active: &[Vec<usize>]) {
    let total: f64 = active.iter().map(|indices| indices.len() as f64).sum();
    if total <= 0.0 {
        return;
    }
    let mut mean = [0.0_f64; 6];
    for (state, indices) in states.iter().zip(active.iter()) {
        let weight = indices.len() as f64;
        for (slot, value) in mean.iter_mut().zip(state.delta.params().iter()) {
            *slot += weight * value / total;
        }
    }
    for state in states.iter_mut() {
        let mut params = state.delta.params();
        for (value, shift) in params.iter_mut().zip(mean.iter()) {
            *value -= shift;
        }
        if let Ok(delta) = Delta6::new(params) {
            state.delta = delta;
        }
    }
}

fn update_sigma(sigma: &mut [f64], pairs: &[Vec<(f64, f64)>], options: &FitOptions) {
    let (a, b) = options.sigma_prior;
    let nu = options.nu;
    for (index, list) in pairs.iter().enumerate() {
        if list.is_empty() {
            continue;
        }
        let mut low = -6.0_f64;
        let mut high = 6.0_f64;
        for _ in 0..48 {
            let mid = 0.5 * (low + high);
            let variance = libm::exp(2.0 * mid);
            let mut derivative = 2.0 * a - 2.0 * b * libm::exp(-2.0 * mid);
            for (weight, squared) in list {
                let u = squared / (nu * variance);
                derivative += weight * (3.0 - (nu + 3.0) * u / (1.0 + u));
            }
            if derivative > 0.0 {
                high = mid;
            } else {
                low = mid;
            }
        }
        sigma[index] = 0.5 * (low + high);
    }
}

/// Per-image projections of the residual onto the firmware-offset basis, for
/// [`crate::detect_split`].
pub fn firmware_images(
    look: &Look,
    images: &[FitImage],
    nuisances: &[ImageNuisances],
    firmware: &[FirmwareId],
    space: looklab_color::RgbSpace,
    clip_k: f64,
    options: &FitOptions,
) -> Result<Vec<crate::epoch::ImageProjection>> {
    if images.len() != nuisances.len() || images.len() != firmware.len() {
        return Err(FitError::Hierarchy(
            "images, nuisances and firmware tags must have equal length".to_string(),
        ));
    }
    let Ok(problem) = Problem::new_with(images, None, options) else {
        return Err(FitError::Hierarchy(
            "firmware projection pool is empty".to_string(),
        ));
    };
    let layout = problem.layout.clone();
    let slopes = tone_slopes(look);
    let mut per_image: Vec<Vec<crate::epoch::SampleProjection>> = vec![Vec::new(); images.len()];
    let mut jac = Jac::default();
    for sample in &problem.samples {
        let index = sample.image as usize;
        let nuisance = nuisances[index];
        let context = ImageCtx {
            transform: images[index].transform,
            a0: images[index].transform,
            space,
            exposure: nuisance.exposure,
            wb_r: libm::exp(nuisance.wb_log_r),
            wb_b: libm::exp(nuisance.wb_log_b),
        };
        let evaluated = evaluate_sample(
            look,
            &layout,
            &context,
            &ColumnMap::default(),
            sample.rgb,
            clip_k,
            1.0,
            &slopes,
            &[],
            &mut jac,
        );
        let target = sample.target;
        let residual = [
            evaluated.prediction.j - target.j,
            evaluated.prediction.a - target.a,
            evaluated.prediction.b - target.b,
        ];
        let mut basis = [[0.0_f64; 3]; crate::epoch::PROJECTION_DIM];
        for (column, derivative) in jac.cols.iter().zip(jac.derivs.iter()) {
            let column = *column as usize;
            if column == layout.tone_c0 {
                for channel in 0..3 {
                    basis[0][channel] += derivative[channel];
                }
            } else if layout.g.contains(&column) {
                for channel in 0..3 {
                    basis[1][channel] += derivative[channel];
                }
            }
        }
        per_image[index].push(crate::epoch::SampleProjection {
            residual,
            basis,
            weight: sample.weight,
        });
    }
    Ok(images
        .iter()
        .enumerate()
        .map(|(index, _)| crate::epoch::ImageProjection {
            group: firmware[index].0,
            samples: per_image[index].clone(),
        })
        .collect())
}

fn pool_images(
    family: LookFamilyId,
    bodies: &[&BodyData],
    states: &[&BodyState],
    include_recipe: bool,
) -> (Vec<FitImage>, Vec<BodyGroup>, Vec<FirmwareId>) {
    let mut pool = Vec::new();
    let mut groups = Vec::new();
    let mut firmware = Vec::new();
    for (body, state) in bodies.iter().zip(states.iter()) {
        let mut indices = Vec::new();
        for (index, tag) in body.images.iter().enumerate() {
            if tag.look != family {
                continue;
            }
            if !include_recipe && tag.image.intent != ImageIntent::Native {
                continue;
            }
            let omega = state.omega(tag.wb_mode);
            let nuisance = state.nuisances[index];
            let gain = Mat3::diag([
                libm::exp(omega[0] + nuisance[1]),
                1.0,
                libm::exp(omega[1] + nuisance[2]),
            ]);
            let mut image = tag.image.clone();
            image.transform = body
                .transform
                .a0_d65()
                .mul(&state.delta.transform())
                .mul(&gain);
            image.exposure_hint = state.xi(image.exposure_mode) + nuisance[0].clamp(-8.0, 8.0);
            image.exposure_mode = ExposureModeId(0);
            indices.push(pool.len());
            pool.push(image);
            firmware.push(tag.firmware);
        }
        if !indices.is_empty() {
            groups.push(BodyGroup {
                body: body.id.clone(),
                images: indices,
            });
        }
    }
    (pool, groups, firmware)
}

#[allow(clippy::too_many_arguments)]
fn estimate_firmware(
    base_look: &Look,
    pool: &[FitImage],
    firmware: &[FirmwareId],
    fit_result: &FitResult,
    sigma: &[f64],
    contributors: &[ContributorId],
    options: &FitOptions,
    firmware_prior_sd: [f64; FIRMWARE_PARAMETERS],
) -> Vec<(FirmwareId, [f64; FIRMWARE_PARAMETERS])> {
    if pool.is_empty() {
        return Vec::new();
    }
    let Ok(problem) = Problem::new_with(pool, None, options) else {
        return Vec::new();
    };
    let layout = problem.layout.clone();
    let slopes = tone_slopes(base_look);
    let mut blocks: BTreeMap<FirmwareId, ([[f64; 2]; 2], [f64; 2])> = BTreeMap::new();
    let mut jac = Jac::default();
    for sample in &problem.samples {
        let index = sample.image as usize;
        let Some(result) = fit_result.images.get(index) else {
            continue;
        };
        let context = ImageCtx {
            transform: pool[index].transform,
            a0: pool[index].transform,
            space: problem.images[index].space,
            exposure: result.exposure,
            wb_r: libm::exp(result.wb_log_r),
            wb_b: libm::exp(result.wb_log_b),
        };
        let columns = ColumnMap::default();
        let evaluated = evaluate_sample(
            base_look,
            &layout,
            &context,
            &columns,
            sample.rgb,
            fit_result.realization.clip_k,
            1.0,
            &slopes,
            &[],
            &mut jac,
        );
        let target = sample.target;
        let residual = [
            evaluated.prediction.j - target.j,
            evaluated.prediction.a - target.a,
            evaluated.prediction.b - target.b,
        ];
        let squared =
            residual[0] * residual[0] + residual[1] * residual[1] + residual[2] * residual[2];
        let contributor = contributor_index(contributors, pool[index].contributor);
        let variance = libm::exp(2.0 * sigma[contributor]);
        let alpha = (options.nu + 3.0) / (options.nu * variance + squared);
        let weight = sample.weight * alpha;
        let mut derivative = [0.0_f64; FIRMWARE_PARAMETERS];
        for (column, value) in jac.cols.iter().zip(jac.derivs.iter()) {
            let column = *column as usize;
            if column == layout.tone_c0 {
                for channel in 0..3 {
                    derivative[0] += value[channel] * residual[channel];
                }
            } else if layout.g.contains(&column) {
                for channel in 0..3 {
                    derivative[1] += value[channel] * residual[channel];
                }
            }
        }
        let entry = blocks
            .entry(firmware[index])
            .or_insert(([[0.0; 2]; 2], [0.0; 2]));
        for a in 0..2 {
            entry.1[a] += weight * derivative[a];
            for b in 0..2 {
                entry.0[a][b] += weight * derivative[a] * derivative[b];
            }
        }
    }
    let mut out = Vec::new();
    for (id, (hessian, gradient)) in blocks {
        let mut matrix = hessian;
        let rhs = gradient;
        for (index, sd) in firmware_prior_sd.iter().enumerate() {
            matrix[index][index] += 1.0 / (sd * sd);
        }
        if let Some(solution) = solve_2x2(matrix, rhs) {
            out.push((id, solution));
        }
    }
    out
}

fn solve_2x2(matrix: [[f64; 2]; 2], rhs: [f64; 2]) -> Option<[f64; 2]> {
    let determinant = matrix[0][0] * matrix[1][1] - matrix[0][1] * matrix[1][0];
    if determinant.abs() < 1e-18 {
        return None;
    }
    Some([
        (matrix[1][1] * rhs[0] - matrix[0][1] * rhs[1]) / determinant,
        (matrix[0][0] * rhs[1] - matrix[1][0] * rhs[0]) / determinant,
    ])
}

fn contributors_of(bodies: &[BodyData]) -> Vec<ContributorId> {
    let mut contributors = Vec::new();
    for body in bodies {
        for tag in &body.images {
            let id = tag.image.contributor;
            if !contributors.contains(&id) {
                contributors.push(id);
            }
        }
    }
    contributors
}

fn components(bodies: &[BodyData]) -> Vec<Vec<usize>> {
    let n = bodies.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], index: usize) -> usize {
        let mut root = index;
        while parent[root] != root {
            root = parent[root];
        }
        let mut current = index;
        while parent[current] != current {
            let next = parent[current];
            parent[current] = root;
            current = next;
        }
        root
    }
    let mut by_look: BTreeMap<LookFamilyId, usize> = BTreeMap::new();
    for (index, body) in bodies.iter().enumerate() {
        for tag in &body.images {
            match by_look.get(&tag.look) {
                Some(first) => {
                    let a = find(&mut parent, *first);
                    let b = find(&mut parent, index);
                    if a != b {
                        parent[a] = b;
                    }
                }
                None => {
                    by_look.insert(tag.look, index);
                }
            }
        }
    }
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for index in 0..n {
        let root = find(&mut parent, index);
        groups.entry(root).or_default().push(index);
    }
    groups.into_values().collect()
}

fn designated_body(bodies: &[BodyData], members: &[usize]) -> usize {
    let anchored: Vec<usize> = members
        .iter()
        .copied()
        .filter(|index| bodies[*index].anchored())
        .collect();
    let candidates = if anchored.is_empty() {
        members
    } else {
        &anchored
    };
    candidates
        .iter()
        .copied()
        .max_by_key(|index| (bodies[*index].native_samples(), usize::MAX - *index))
        .unwrap_or(members[0])
}

fn body_parameter(body: &BodyData, state: &BodyState, recipe_fitted: bool) -> BodyParameters {
    let mut modes: Vec<ExposureModeId> = state.xi.iter().map(|(mode, _)| *mode).collect();
    modes.sort();
    modes.dedup();
    let exposure = modes
        .iter()
        .map(|mode| ExposureOffset {
            mode: *mode,
            offset: state.xi(*mode),
        })
        .collect();
    let mut classes: Vec<WbModeClass> = state.omega.iter().map(|(class, _)| *class).collect();
    classes.sort();
    classes.dedup();
    let wb = classes
        .iter()
        .map(|class| (*class, state.omega(*class)))
        .collect();
    let images = body
        .images
        .iter()
        .enumerate()
        .map(|(index, tag)| ImageNuisances {
            exposure: state.nuisances[index][0],
            wb_log_r: state.nuisances[index][1],
            wb_log_b: state.nuisances[index][2],
            exposure_mode: tag.image.exposure_mode,
            contributor: tag.image.contributor,
            weight: tag.image.trust_weight,
        })
        .collect();
    BodyParameters {
        id: body.id.clone(),
        transform: body
            .transform
            .with_delta(state.delta, body.transform.covariance()),
        exposure,
        wb,
        clip_k: state.clip,
        images,
        recipe_fitted,
    }
}

/// Fit every connected component: native stage alternating bodies and Looks,
/// then a recipe stage with every `θ_L` frozen.
pub fn fit_hierarchical(bodies: &[BodyData], options: &HierOptions) -> Result<HierarchicalFit> {
    if bodies.is_empty() {
        return Err(FitError::NoImages);
    }
    for body in bodies {
        if body.images.is_empty() {
            return Err(FitError::NoImages);
        }
    }
    let contributors = contributors_of(bodies);
    let mut sigma = vec![libm::log(1.5); contributors.len()];
    let mut states: Vec<BodyState> = bodies
        .iter()
        .map(|body| BodyState::initial(body, 0.03, None))
        .collect();
    let groups = components(bodies);
    let mut gauges = Vec::new();
    let mut designated = vec![false; bodies.len()];
    for members in &groups {
        let chosen = designated_body(bodies, members);
        designated[chosen] = true;
        gauges.push(ComponentGauge {
            designated: bodies[chosen].id.clone(),
            exposure_mode: bodies[chosen].default_mode,
            wb_mode: bodies[chosen].default_wb_mode,
            anchored_delta: members.iter().any(|index| bodies[*index].anchored()),
        });
    }
    let mut look_map: BTreeMap<LookFamilyId, Look> = BTreeMap::new();
    let mut look_firmware: BTreeMap<LookFamilyId, Vec<(FirmwareId, [f64; FIRMWARE_PARAMETERS])>> =
        BTreeMap::new();
    let mut firmware: BTreeMap<FirmwareId, [f64; FIRMWARE_PARAMETERS]> = BTreeMap::new();
    let mut family_ids: Vec<LookFamilyId> = bodies
        .iter()
        .flat_map(|body| body.images.iter().map(|tag| tag.look))
        .collect();
    family_ids.sort();
    family_ids.dedup();

    // Initial Look fit with the prior body transforms.
    for family in &family_ids {
        let component = groups
            .iter()
            .find(|members| {
                members
                    .iter()
                    .any(|index| bodies[*index].images.iter().any(|tag| tag.look == *family))
            })
            .cloned()
            .unwrap_or_default();
        let members: Vec<&BodyData> = component.iter().map(|index| &bodies[*index]).collect();
        let member_states: Vec<&BodyState> =
            component.iter().map(|index| &states[*index]).collect();
        let (pool, _groups, _fw) = pool_images(*family, &members, &member_states, false);
        if pool.is_empty() {
            continue;
        }
        let result = fit::run_quick(&pool, &options.fit, None)?;
        look_map.insert(*family, result.look);
    }

    let mut evaluations = 0usize;
    let mut outer_iterations = 0usize;
    for _ in 0..options.outer_iterations.max(1) {
        outer_iterations += 1;
        // (a) bodies, given the Looks.
        let mut all_pairs: Vec<Vec<(f64, f64)>> = vec![Vec::new(); contributors.len()];
        for (component, members) in groups.iter().enumerate() {
            let gauge = &gauges[component];
            for index in members {
                let body = &bodies[*index];
                let (state, pass, count) = fit_body(
                    body,
                    &look_map,
                    &firmware,
                    gauge,
                    designated[*index],
                    &sigma,
                    &contributors,
                    options,
                    Some(&states[*index]),
                    false,
                )?;
                states[*index] = state;
                evaluations += count;
                for (slot, list) in all_pairs.iter_mut().enumerate() {
                    list.extend(pass.pairs[slot].iter().copied());
                }
            }
            if !gauge.anchored_delta {
                let member_states: Vec<BodyState> =
                    members.iter().map(|index| states[*index].clone()).collect();
                let active: Vec<Vec<usize>> = members
                    .iter()
                    .map(|index| {
                        bodies[*index]
                            .images
                            .iter()
                            .enumerate()
                            .filter(|(_, tag)| tag.image.intent == ImageIntent::Native)
                            .map(|(position, _)| position)
                            .collect()
                    })
                    .collect();
                let mut local_states = member_states;
                apply_delta_gauge(&mut local_states, &active);
                for (position, index) in members.iter().enumerate() {
                    states[*index] = local_states[position].clone();
                }
            }
        }
        update_sigma(&mut sigma, &all_pairs, &options.fit);

        // (b) Looks, given the bodies.
        for family in &family_ids {
            let component = groups
                .iter()
                .find(|members| {
                    members
                        .iter()
                        .any(|index| bodies[*index].images.iter().any(|tag| tag.look == *family))
                })
                .cloned()
                .unwrap_or_default();
            let members: Vec<&BodyData> = component.iter().map(|index| &bodies[*index]).collect();
            let member_states: Vec<&BodyState> =
                component.iter().map(|index| &states[*index]).collect();
            let (pool, _groups, firmware_tags) =
                pool_images(*family, &members, &member_states, false);
            if pool.is_empty() {
                continue;
            }
            let warm = look_map.get(family).cloned();
            let result = fit::run_quick(&pool, &options.fit, warm.as_ref())?;
            evaluations += result.diagnostics.evaluations;
            let offsets = estimate_firmware(
                &result.look,
                &pool,
                &firmware_tags,
                &result,
                &sigma,
                &contributors,
                &options.fit,
                options.firmware_prior_sd,
            );
            for (id, offset) in &offsets {
                firmware.insert(*id, *offset);
            }
            look_firmware.insert(*family, offsets);
            look_map.insert(*family, result.look);
        }
    }

    // Recipe stage: every `θ_L` stays at its native-stage value.
    let has_recipe = bodies.iter().any(|body| {
        body.images
            .iter()
            .any(|tag| tag.image.intent == ImageIntent::Recipe)
    });
    for (component, members) in groups.iter().enumerate() {
        if !has_recipe {
            continue;
        }
        let gauge = &gauges[component];
        for index in members {
            let body = &bodies[*index];
            let (state, _pass, count) = fit_body(
                body,
                &look_map,
                &firmware,
                gauge,
                designated[*index],
                &sigma,
                &contributors,
                options,
                Some(&states[*index]),
                true,
            )?;
            states[*index] = state;
            evaluations += count;
        }
        if !gauge.anchored_delta {
            let active: Vec<Vec<usize>> = members
                .iter()
                .map(|index| {
                    bodies[*index]
                        .images
                        .iter()
                        .enumerate()
                        .map(|(position, _)| position)
                        .collect()
                })
                .collect();
            let mut local: Vec<BodyState> =
                members.iter().map(|index| states[*index].clone()).collect();
            apply_delta_gauge(&mut local, &active);
            for (position, index) in members.iter().enumerate() {
                states[*index] = local[position].clone();
            }
        }
    }

    let mut looks = Vec::new();
    for family in &family_ids {
        let component = groups
            .iter()
            .find(|members| {
                members
                    .iter()
                    .any(|index| bodies[*index].images.iter().any(|tag| tag.look == *family))
            })
            .cloned()
            .unwrap_or_default();
        let members: Vec<&BodyData> = component.iter().map(|index| &bodies[*index]).collect();
        let member_states: Vec<&BodyState> =
            component.iter().map(|index| &states[*index]).collect();
        let (pool, groups, _firmware) = pool_images(*family, &members, &member_states, false);
        let fallback = look_map
            .get(family)
            .cloned()
            .unwrap_or_else(Look::colorimetric);
        let result = if pool.is_empty() {
            None
        } else {
            fit::run(&pool, &options.fit, None).ok()
        };
        let look = result
            .as_ref()
            .map_or(fallback, |result| result.look.clone());
        let provenance = members
            .iter()
            .zip(member_states.iter())
            .flat_map(|(body, state)| {
                (body.images.iter().any(|tag| tag.look == *family)).then(|| LookProvenance {
                    body: body.id.clone(),
                    transform_version: body.transform.version(),
                    delta: state.delta,
                    exposure: body
                        .images
                        .iter()
                        .map(|tag| tag.image.exposure_mode)
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .map(|mode| ExposureOffset {
                            mode,
                            offset: state.xi(mode),
                        })
                        .collect(),
                    wb: state.omega.clone(),
                })
            })
            .collect();
        let (probes, density) = match &result {
            Some(result) => (result.probes.clone(), result.density.clone()),
            None => (
                ProbeUncertainty {
                    probes: Vec::new(),
                    ids: Vec::new(),
                    supported_fraction: 0.0,
                },
                Density {
                    s_bins: 0,
                    hue_bins: 0,
                    c_bins: 0,
                    counts: Vec::new(),
                    total: 0.0,
                    floor: 0.0,
                },
            ),
        };
        looks.push(LookParameters {
            id: *family,
            look,
            firmware: look_firmware.get(family).cloned().unwrap_or_default(),
            probes,
            density,
            provenance,
            pool,
            groups,
        });
    }

    let bodies_out = bodies
        .iter()
        .zip(states.iter())
        .map(|(body, state)| body_parameter(body, state, true))
        .collect();

    Ok(HierarchicalFit {
        looks,
        bodies: bodies_out,
        components: gauges,
        diagnostics: HierDiagnostics {
            evaluations,
            outer_iterations,
            sigma_v: contributors
                .iter()
                .zip(sigma.iter())
                .map(|(id, log)| (*id, libm::exp(*log)))
                .collect(),
        },
    })
}
