// SPDX-License-Identifier: AGPL-3.0-or-later
//! Global hierarchical fit across bodies (docs/02 §4.3): block coordinate descent
//! between per-body parameters and the shared Look, then a recipe stage with the
//! Look frozen.

use std::collections::{BTreeMap, BTreeSet};

use looklab_body::{
    A0Source, AnchorOptions, BodyId, Delta6, InputTransform, TransformVersion, chart_anchor_terms,
};
use looklab_color::{Mat3, RgbSpace};
use looklab_model::basis::TONE_COEFS;
use looklab_model::realization::{ControlResponseModel, Delta, ResponseComponent};
use looklab_model::{Look, Tone};

use crate::fit;
use crate::gauge::{ComponentGauge, WbModeClass};
use crate::layout::{Layout, Params, softplus_inverse, softplus_slope};
use crate::predict::{ColumnMap, ImageCtx, Jac, evaluate_sample};
use crate::problem::Problem;
use crate::solve::{factor_in_place, solve_in_place};
use crate::types::{
    ContributorId, Density, ExposureModeId, ExposureOffset, FitError, FitImage, FitOptions,
    ImageIntent, ImageNuisances, PreviousSupport, ProbeUncertainty, Result,
};

/// Two firmware offset coordinates: a constant `T` shift and a constant `log G` shift.
pub const FIRMWARE_PARAMETERS: usize = 2;

const RESPONSE_PRIOR_PRECISION: f64 = 10.0;

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
    /// Control response for this body's native sim, fitted in the body block.
    pub model: Option<ControlResponseModel>,
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
    /// Persisted component gauges; a designated body in the member set is kept.
    pub gauges: Vec<ComponentGauge>,
    /// Previous published support state per Look family, for hysteresis.
    pub previous_support: BTreeMap<LookFamilyId, PreviousSupport>,
}

impl Default for HierOptions {
    fn default() -> Self {
        Self {
            fit: FitOptions::default(),
            outer_iterations: 3,
            body_iterations: 6,
            firmware_prior_sd: [0.5, 0.03],
            anchor: AnchorOptions::default(),
            gauges: Vec::new(),
            previous_support: BTreeMap::new(),
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
    /// Fitted control response, if the body supplied a model.
    pub model: Option<ControlResponseModel>,
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
    /// Clip parameter of the published realization.
    pub clip_k: f64,
    /// Output encoding of the post-clip probe predictions.
    pub output_space: RgbSpace,
    pub provenance: Vec<LookProvenance>,
    /// Native images with the native-stage body parameters the Look was fitted against.
    pub pool: Vec<FitImage>,
    pub groups: Vec<BodyGroup>,
}

#[derive(Clone, Debug)]
pub struct HierDiagnostics {
    pub evaluations: usize,
    pub outer_iterations: usize,
    pub sigma_v: Vec<(ContributorId, f64)>,
    pub converged: bool,
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
    response: Option<ControlResponseModel>,
    coefficients: Vec<f64>,
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
                response: body.model.clone(),
                coefficients: Vec::new(),
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
    active: Vec<usize>,
    response: Vec<usize>,
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
    response_count: usize,
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
    let response = (0..response_count).map(|_| alloc()).collect();
    BodyLayout {
        delta,
        xi,
        omega,
        mode_ids: modes.to_vec(),
        wb_ids: wb_modes.to_vec(),
        clip,
        image,
        active: active.to_vec(),
        response,
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
    terms: &[Vec<crate::predict::ResponseTerm>],
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
    let mut local_of = vec![u32::MAX; total];
    for (local, global) in local_indices(layout).iter().enumerate() {
        map[layout.offset + global] = local as u32;
        local_of[*global] = local as u32;
    }
    for (index, global) in layout.response.iter().enumerate() {
        map[problem_layout.response.start + index] = local_of[*global];
    }

    let a0 = *body.transform.a0_d65();
    let transform = a0.mul(&state.delta.transform());
    let modes = unique_modes(&body.images, active);
    let wb_modes = unique_wb_modes(&body.images, active);
    let active_looks: Vec<(Look, [f64; TONE_COEFS])> = active
        .iter()
        .enumerate()
        .map(|(position, index)| {
            let tag = &body.images[*index];
            let base = looks
                .get(&tag.look)
                .cloned()
                .unwrap_or_else(Look::colorimetric);
            let offset = firmware.get(&tag.firmware).copied().unwrap_or([0.0; 2]);
            let mut look = firmware_look(&base, offset);
            let mut delta = Delta::default();
            for term in &terms[position] {
                if let Some(layer) = term.component.layer() {
                    let value = state.coefficients.get(term.index).copied().unwrap_or(0.0);
                    delta.add(layer, term.feature, value * term.phi);
                }
            }
            look = delta.apply(&look).unwrap_or(look);
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
        let mut log_r = omega[0] + nuisance[1];
        let mut log_b = omega[1] + nuisance[2];
        for term in &terms[position] {
            let value = state.coefficients.get(term.index).copied().unwrap_or(0.0) * term.phi;
            match term.component {
                ResponseComponent::WbR => log_r += value,
                ResponseComponent::WbB => log_b += value,
                _ => {}
            }
        }
        let (look, slopes) = &active_looks[position];
        let context = ImageCtx {
            transform,
            a0,
            space: spaces[position],
            exposure_scale: libm::pow(2.0, xi + nuisance[0]),
            wb_r: libm::exp(log_r),
            wb_b: libm::exp(log_b),
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
            &terms[position],
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

    for (local, value) in state.coefficients.iter().enumerate() {
        if local >= layout.response.len() {
            break;
        }
        let slot = layout.response[local];
        gradient[slot] += RESPONSE_PRIOR_PRECISION * value;
        hessian[slot * total + slot] += RESPONSE_PRIOR_PRECISION;
        cost += 0.5 * RESPONSE_PRIOR_PRECISION * value * value;
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
    indices.extend(layout.response.iter().copied());
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
    for (position, local) in layout.response.iter().enumerate() {
        if position < state.coefficients.len() {
            state.coefficients[position] += factor * step[*local];
        }
    }
    for (position, columns) in layout.image.iter().enumerate() {
        let image = layout.active[position];
        for channel in 0..3 {
            state.nuisances[image][channel] += factor * step[columns[channel]];
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
    let problem = Problem::new_with(&filtered, body.model.as_ref(), &options.fit)?;
    let problem_layout = problem.layout.clone();
    let spaces: Vec<looklab_color::RgbSpace> =
        problem.images.iter().map(|image| image.space).collect();
    let response_count = problem.parameter_ids.len();
    if state.coefficients.len() != response_count {
        let mut params = Params::initial(problem.layout.clone(), problem.contributors.len());
        if let Some(model) = &body.model {
            fit::seed_response_params(&problem, &mut params, model);
        }
        state.coefficients = params.theta[problem.layout.response.clone()].to_vec();
    }

    let modes = unique_modes(&body.images, &active);
    let wb_modes = unique_wb_modes(&body.images, &active);
    let layout = body_layout(
        &active,
        &modes,
        &wb_modes,
        gauge,
        designated,
        response_count,
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
        &problem.terms,
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
            &problem.terms,
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
    if let Some(template) = &body.model {
        let mut params = Params::initial(problem.layout.clone(), problem.contributors.len());
        for (index, value) in state.coefficients.iter().enumerate() {
            params.theta[problem.layout.response.start + index] = *value;
        }
        state.response = Some(
            fit::build_control_response(
                template.clone(),
                &problem,
                &params,
                problem.parameter_ids.clone(),
                Vec::new(),
            )
            .model,
        );
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
            exposure_scale: libm::pow(2.0, nuisance.exposure),
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

/// Firmware basis at every v1 probe, for the predicted-difference effect gate.
pub fn firmware_probe_basis(look: &Look) -> Vec<[[f64; 3]; FIRMWARE_PARAMETERS]> {
    let layout = Layout::with_response(1, 0, 1, 0);
    let slopes = tone_slopes(look);
    let probes = looklab_model::probes::v1();
    let mut jac = Jac::default();
    let mut out = Vec::with_capacity(probes.probes.len());
    for probe in &probes.probes {
        let _ = crate::predict::evaluate_probe(look, &layout, probe.xyz, &slopes, &mut jac);
        let mut basis = [[0.0_f64; 3]; FIRMWARE_PARAMETERS];
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
        out.push(basis);
    }
    out
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
    sigma: &[f64],
    contributors: &[ContributorId],
    options: &FitOptions,
    clip_k: f64,
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
        let context = ImageCtx {
            transform: pool[index].transform,
            a0: pool[index].transform,
            space: problem.images[index].space,
            exposure_scale: libm::pow(2.0, pool[index].exposure_hint),
            wb_r: 1.0,
            wb_b: 1.0,
        };
        let columns = ColumnMap::default();
        let evaluated = evaluate_sample(
            base_look,
            &layout,
            &context,
            &columns,
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
        let squared =
            residual[0] * residual[0] + residual[1] * residual[1] + residual[2] * residual[2];
        let contributor = contributor_index(contributors, pool[index].contributor);
        let variance = libm::exp(2.0 * sigma[contributor]);
        let alpha = (options.nu + 3.0) / (options.nu * variance + squared);
        let weight = sample.weight * alpha;
        let mut basis = [[0.0_f64; 3]; FIRMWARE_PARAMETERS];
        for (column, value) in jac.cols.iter().zip(jac.derivs.iter()) {
            let column = *column as usize;
            if column == layout.tone_c0 {
                for channel in 0..3 {
                    basis[0][channel] += value[channel];
                }
            } else if layout.g.contains(&column) {
                for channel in 0..3 {
                    basis[1][channel] += value[channel];
                }
            }
        }
        let mut derivative = [0.0_f64; FIRMWARE_PARAMETERS];
        for (index, row) in basis.iter().enumerate() {
            for channel in 0..3 {
                derivative[index] += row[channel] * residual[channel];
            }
        }
        let entry = blocks
            .entry(firmware[index])
            .or_insert(([[0.0; 2]; 2], [0.0; 2]));
        for a in 0..2 {
            entry.1[a] += weight * derivative[a];
            for b in 0..2 {
                let mut dot = 0.0;
                for channel in 0..3 {
                    dot += basis[a][channel] * basis[b][channel];
                }
                entry.0[a][b] += weight * dot;
            }
        }
    }
    let mut out = Vec::new();
    for (id, (hessian, gradient)) in blocks {
        let mut matrix = hessian;
        let rhs = [-gradient[0], -gradient[1]];
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
        model: state.response.clone(),
    }
}

fn contribution_clip(members: &[&BodyState]) -> f64 {
    let sum: f64 = members.iter().map(|state| state.clip).sum();
    sum / members.len().max(1) as f64
}

fn shared_model(states: &[&BodyState]) -> Option<ControlResponseModel> {
    let first = states.iter().find_map(|state| state.response.as_ref())?;
    if states
        .iter()
        .all(|state| state.response.as_ref() == Some(first))
    {
        Some(first.clone())
    } else {
        None
    }
}

fn look_distance(a: &Look, b: &Look) -> f64 {
    let left = a.coef_vector();
    let right = b.coef_vector();
    if left.len() != right.len() {
        return f64::INFINITY;
    }
    left.iter()
        .zip(right.iter())
        .map(|(x, y)| (x - y).abs())
        .fold(0.0_f64, f64::max)
}

fn body_distance(a: &BodyState, b: &BodyState) -> f64 {
    let mut change = 0.0_f64;
    for (left, right) in a.delta.params().iter().zip(b.delta.params().iter()) {
        change = change.max((left - right).abs());
    }
    for ((_, left), (_, right)) in a.xi.iter().zip(b.xi.iter()) {
        change = change.max((left - right).abs());
    }
    for ((_, left), (_, right)) in a.omega.iter().zip(b.omega.iter()) {
        for channel in 0..2 {
            change = change.max((left[channel] - right[channel]).abs());
        }
    }
    change = change.max((a.clip - b.clip).abs());
    for (left, right) in a.nuisances.iter().zip(b.nuisances.iter()) {
        for channel in 0..3 {
            change = change.max((left[channel] - right[channel]).abs());
        }
    }
    for (left, right) in a.coefficients.iter().zip(b.coefficients.iter()) {
        change = change.max((left - right).abs());
    }
    change
}

fn family_members(bodies: &[BodyData], groups: &[Vec<usize>], family: LookFamilyId) -> Vec<usize> {
    groups
        .iter()
        .find(|members| {
            members
                .iter()
                .any(|index| bodies[*index].images.iter().any(|tag| tag.look == family))
        })
        .cloned()
        .unwrap_or_default()
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
        let persisted = options.gauges.iter().find(|gauge| {
            members
                .iter()
                .any(|index| bodies[*index].id == gauge.designated)
        });
        let gauge = match persisted {
            Some(gauge) => gauge.clone(),
            None => {
                let chosen = designated_body(bodies, members);
                ComponentGauge {
                    designated: bodies[chosen].id.clone(),
                    exposure_mode: bodies[chosen].default_mode,
                    wb_mode: bodies[chosen].default_wb_mode,
                    anchored_delta: members.iter().any(|index| bodies[*index].anchored()),
                }
            }
        };
        for index in members {
            if bodies[*index].id == gauge.designated {
                designated[*index] = true;
            }
        }
        gauges.push(gauge);
    }
    let mut family_ids: Vec<LookFamilyId> = bodies
        .iter()
        .flat_map(|body| body.images.iter().map(|tag| tag.look))
        .collect();
    family_ids.sort();
    family_ids.dedup();

    let mut look_map: BTreeMap<LookFamilyId, Look> = BTreeMap::new();
    let mut look_results: BTreeMap<LookFamilyId, fit::LookOnlyResult> = BTreeMap::new();
    let mut look_firmware: BTreeMap<LookFamilyId, Vec<(FirmwareId, [f64; FIRMWARE_PARAMETERS])>> =
        BTreeMap::new();
    let mut firmware: BTreeMap<FirmwareId, [f64; FIRMWARE_PARAMETERS]> = BTreeMap::new();
    let mut evaluations = 0usize;

    for family in &family_ids {
        let members = family_members(bodies, &groups, *family);
        let member_bodies: Vec<&BodyData> = members.iter().map(|index| &bodies[*index]).collect();
        let member_states: Vec<&BodyState> = members.iter().map(|index| &states[*index]).collect();
        let (pool, _groups, _tags) = pool_images(*family, &member_bodies, &member_states, false);
        if pool.is_empty() {
            continue;
        }
        let clip = contribution_clip(&member_states);
        let model = shared_model(&member_states);
        let result = fit::run_look_only(
            &pool,
            &options.fit,
            None,
            clip,
            model.as_ref(),
            options.previous_support.get(family),
        )?;
        evaluations += result.evaluations;
        look_map.insert(*family, result.look.clone());
        look_results.insert(*family, result);
    }

    let mut converged = false;
    let mut outer_iterations = 0usize;
    for _ in 0..options.outer_iterations.max(1) {
        outer_iterations += 1;
        let previous_states = states.clone();
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
                let mut local: Vec<BodyState> =
                    members.iter().map(|index| states[*index].clone()).collect();
                apply_delta_gauge(&mut local, &active);
                for (position, index) in members.iter().enumerate() {
                    states[*index] = local[position].clone();
                }
            }
        }
        update_sigma(&mut sigma, &all_pairs, &options.fit);

        let mut change = states
            .iter()
            .zip(previous_states.iter())
            .map(|(current, previous)| body_distance(current, previous))
            .fold(0.0_f64, f64::max);
        for family in &family_ids {
            let members = family_members(bodies, &groups, *family);
            let member_bodies: Vec<&BodyData> =
                members.iter().map(|index| &bodies[*index]).collect();
            let member_states: Vec<&BodyState> =
                members.iter().map(|index| &states[*index]).collect();
            let (pool, _groups, firmware_tags) =
                pool_images(*family, &member_bodies, &member_states, false);
            if pool.is_empty() {
                continue;
            }
            let clip = contribution_clip(&member_states);
            let model = shared_model(&member_states);
            let warm = look_map.get(family);
            let result = fit::run_look_only(
                &pool,
                &options.fit,
                warm,
                clip,
                model.as_ref(),
                options.previous_support.get(family),
            )?;
            evaluations += result.evaluations;
            if let Some(previous) = warm {
                change = change.max(look_distance(previous, &result.look));
            }
            let offsets = estimate_firmware(
                &result.look,
                &pool,
                &firmware_tags,
                &sigma,
                &contributors,
                &options.fit,
                result.clip_k,
                options.firmware_prior_sd,
            );
            for (id, offset) in &offsets {
                firmware.insert(*id, *offset);
            }
            look_firmware.insert(*family, offsets);
            look_map.insert(*family, result.look.clone());
            look_results.insert(*family, result);
        }
        if change < 1e-5 {
            converged = true;
            break;
        }
    }

    let native_states = states.clone();
    let native_looks = look_map.clone();
    let native_results = look_results.clone();

    let has_recipe = bodies.iter().any(|body| {
        body.images
            .iter()
            .any(|tag| tag.image.intent == ImageIntent::Recipe)
    });
    if has_recipe {
        for (component, members) in groups.iter().enumerate() {
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
    }

    let mut looks = Vec::new();
    for family in &family_ids {
        let members = family_members(bodies, &groups, *family);
        let member_bodies: Vec<&BodyData> = members.iter().map(|index| &bodies[*index]).collect();
        let native_member_states: Vec<&BodyState> =
            members.iter().map(|index| &native_states[*index]).collect();
        let (pool, groups, _tags) =
            pool_images(*family, &member_bodies, &native_member_states, false);
        let Some(result) = native_results.get(family) else {
            continue;
        };
        let provenance = member_bodies
            .iter()
            .zip(native_member_states.iter())
            .filter(|(body, _)| {
                body.images
                    .iter()
                    .any(|tag| tag.look == *family && tag.image.intent == ImageIntent::Native)
            })
            .map(|(body, state)| LookProvenance {
                body: body.id.clone(),
                transform_version: body.transform.version(),
                delta: state.delta,
                exposure: body
                    .images
                    .iter()
                    .filter(|tag| tag.look == *family && tag.image.intent == ImageIntent::Native)
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
            .collect();
        looks.push(LookParameters {
            id: *family,
            look: native_looks
                .get(family)
                .cloned()
                .unwrap_or_else(Look::colorimetric),
            firmware: look_firmware.get(family).cloned().unwrap_or_default(),
            probes: result.probes.clone(),
            density: result.density.clone(),
            clip_k: result.clip_k,
            output_space: result.output_space,
            provenance,
            pool,
            groups,
        });
    }

    let bodies_out = bodies
        .iter()
        .zip(states.iter())
        .map(|(body, state)| body_parameter(body, state, has_recipe))
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
            converged,
        },
    })
}

#[cfg(test)]
mod review_tests {
    use super::*;

    #[test]
    fn body_steps_write_to_original_image_indices() {
        let layout = BodyLayout {
            delta: [0, 1, 2, 3, 4, 5],
            xi: Vec::new(),
            omega: Vec::new(),
            mode_ids: Vec::new(),
            wb_ids: Vec::new(),
            clip: 6,
            image: vec![[7, 8, 9]],
            active: vec![1],
            response: Vec::new(),
            total: 10,
            offset: 0,
        };
        let mut state = BodyState {
            delta: Delta6::ZERO,
            xi: Vec::new(),
            omega: Vec::new(),
            clip: 0.03,
            nuisances: vec![[0.0; 3]; 2],
            response: None,
            coefficients: Vec::new(),
        };
        let mut step = vec![0.0_f64; 10];
        step[7] = 1.0;
        assert!(apply_body_step_values(&mut state, &layout, &step));
        assert!((state.nuisances[1][0] - 1.0).abs() < 1e-12);
        assert_eq!(state.nuisances[0][0], 0.0);
    }
}
