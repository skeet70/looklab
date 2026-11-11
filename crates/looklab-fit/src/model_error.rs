// SPDX-License-Identifier: AGPL-3.0-or-later
//! Body-held-out model error for the published uncertainty path (docs/02 §5).

use looklab_color::RgbSpace;
use looklab_color::linalg::Mat3;
use looklab_color::ucs::{Ucs, delta_e_ucs};
use looklab_model::basis::C_MAX;
use looklab_model::realization::ControlResponseModel;
use looklab_model::{Look, LookInput};

use crate::hierarchical::BodyGroup;
use crate::problem::Problem;
use crate::types::{FitImage, FitOptions, ProbeUncertainty, Result};
use crate::uncertainty::student_t_95;

pub const MODEL_HUE_BINS: usize = 12;
pub const MODEL_S_BINS: usize = 6;
pub const MODEL_S_MIN: f64 = -12.0;
pub const MODEL_S_MAX: f64 = 6.0;
pub const MODEL_C_BINS: usize = 4;
const MODEL_IMAGE_FOLDS: usize = 8;
const MODEL_ENCODINGS: usize = RgbSpace::ALL.len();

type CellMoment = [[f64; 3]; 3];

fn zero_moment() -> CellMoment {
    [[0.0; 3]; 3]
}

fn identity_moment(value: f64) -> CellMoment {
    let mut out = zero_moment();
    for (index, row) in out.iter_mut().enumerate() {
        row[index] = value;
    }
    out
}

fn outer_moment(vector: [f64; 3]) -> CellMoment {
    core::array::from_fn(|i| core::array::from_fn(|j| vector[i] * vector[j]))
}

fn moment_add(left: CellMoment, right: CellMoment) -> CellMoment {
    core::array::from_fn(|i| core::array::from_fn(|j| left[i][j] + right[i][j]))
}

fn moment_sub(left: CellMoment, right: CellMoment) -> CellMoment {
    core::array::from_fn(|i| core::array::from_fn(|j| left[i][j] - right[i][j]))
}

fn moment_scale(moment: CellMoment, scale: f64) -> CellMoment {
    core::array::from_fn(|i| core::array::from_fn(|j| moment[i][j] * scale))
}

fn moment_trace(moment: CellMoment) -> f64 {
    moment[0][0] + moment[1][1] + moment[2][2]
}

fn sample_covariance(vectors: &[[f64; 3]], mean: [f64; 3]) -> Option<CellMoment> {
    if vectors.len() < 2 {
        return None;
    }
    let count = vectors.len() as f64;
    let mut out = zero_moment();
    for vector in vectors {
        let delta = [
            vector[0] - mean[0],
            vector[1] - mean[1],
            vector[2] - mean[2],
        ];
        for i in 0..3 {
            for j in 0..3 {
                out[i][j] += delta[i] * delta[j];
            }
        }
    }
    for row in &mut out {
        for value in row {
            *value /= count - 1.0;
        }
    }
    Some(out)
}

/// Jacobi rotations for a real symmetric 3x3 matrix (Golub & Van Loan, Matrix
/// Computations, 4th ed., §8.5; eqs. 8.5.1 and 8.5.3; max-offdiagonal selection §8.5.3).
fn project_psd(moment: CellMoment) -> CellMoment {
    let mut a: CellMoment =
        core::array::from_fn(|i| core::array::from_fn(|j| 0.5 * (moment[i][j] + moment[j][i])));
    let mut vectors = identity_moment(1.0);
    for _ in 0..20 {
        let candidates: [(usize, usize); 3] = [(0, 1), (0, 2), (1, 2)];
        let (p, q) = candidates
            .into_iter()
            .max_by(|left, right| {
                a[left.0][left.1]
                    .abs()
                    .total_cmp(&a[right.0][right.1].abs())
            })
            .unwrap_or((0, 1));
        let off = a[p][q];
        if off.abs() <= 1e-14 * (a[p][p].abs() + a[q][q].abs() + 1.0) {
            break;
        }
        let theta = 0.5 * libm::atan2(2.0 * off, a[q][q] - a[p][p]);
        let cosine = libm::cos(theta);
        let sine = libm::sin(theta);
        let mut rotation = Mat3::IDENTITY;
        rotation.0[p][p] = cosine;
        rotation.0[p][q] = sine;
        rotation.0[q][p] = -sine;
        rotation.0[q][q] = cosine;
        a = rotation.transpose().mul(&Mat3(a)).mul(&rotation).0;
        vectors = Mat3(vectors).mul(&rotation).0;
    }
    let diagonal = Mat3::diag([a[0][0].max(0.0), a[1][1].max(0.0), a[2][2].max(0.0)]);
    Mat3(vectors)
        .mul(&diagonal)
        .mul(&Mat3(vectors).transpose())
        .0
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelErrorField {
    /// Post-clip UCS second moment of the mean Look response, per encoding and cell.
    pub post: Vec<[[f64; 3]; 3]>,
    /// True where the cell has independent held-out mean-response evidence.
    pub evidence: Vec<bool>,
    /// Diagnostic root of the mean held-out body mean-response energy per encoding and cell.
    pub held_out_mean_rms: Vec<f64>,
    /// Independent body estimates contributing to each encoding/cell.
    pub bodies: Vec<usize>,
    /// Independent image units contributing to each encoding/cell.
    pub repeats: Vec<usize>,
}

impl ModelErrorField {
    pub fn empty() -> Self {
        let entries = MODEL_ENCODINGS * MODEL_HUE_BINS * MODEL_S_BINS * MODEL_C_BINS;
        Self {
            post: vec![zero_moment(); entries],
            evidence: vec![false; entries],
            held_out_mean_rms: vec![0.0; entries],
            bodies: vec![0; entries],
            repeats: vec![0; entries],
        }
    }

    pub fn at(&self, input: &LookInput, space: RgbSpace) -> [[f64; 3]; 3] {
        self.post[post_index(input, space)]
    }

    pub fn evidence_at(&self, input: &LookInput, space: RgbSpace) -> bool {
        self.evidence[post_index(input, space)]
    }

    pub fn radial(&self, input: &LookInput, space: RgbSpace) -> f64 {
        moment_trace(self.post[post_index(input, space)])
    }
}

fn encoding_index(space: RgbSpace) -> usize {
    match space {
        RgbSpace::Srgb => 0,
        RgbSpace::AdobeRgb => 1,
        RgbSpace::Rec2020 => 2,
        RgbSpace::DisplayP3 => 3,
        RgbSpace::ProPhoto => 4,
    }
}

pub fn post_index(input: &LookInput, space: RgbSpace) -> usize {
    encoding_index(space) * MODEL_HUE_BINS * MODEL_S_BINS * MODEL_C_BINS + cell(input)
}

pub fn cell(input: &LookInput) -> usize {
    region(input.s, input.h_in) * MODEL_C_BINS + color_cell(input.c_in)
}

/// Region index: 12 hue bins of 30 degrees, 6 exposure bins over [-12, 6].
pub fn region(s: f64, hue_degrees: f64) -> usize {
    let hue = hue_degrees % 360.0;
    let hue = if hue < 0.0 { hue + 360.0 } else { hue };
    let hue_bin = ((hue / 30.0).floor() as usize).min(MODEL_HUE_BINS - 1);
    let span = (MODEL_S_MAX - MODEL_S_MIN) / MODEL_S_BINS as f64;
    let s_bin =
        (((s - MODEL_S_MIN) / span).floor() as isize).clamp(0, MODEL_S_BINS as isize - 1) as usize;
    hue_bin * MODEL_S_BINS + s_bin
}

pub fn region_center(index: usize) -> (f64, f64) {
    let hue = (index / MODEL_S_BINS) as f64 * 30.0 + 15.0;
    let span = (MODEL_S_MAX - MODEL_S_MIN) / MODEL_S_BINS as f64;
    let s = MODEL_S_MIN + (index % MODEL_S_BINS) as f64 * span + 0.5 * span;
    (s, hue)
}

fn color_cell(c: f64) -> usize {
    (((c / C_MAX) * MODEL_C_BINS as f64).floor() as isize).clamp(0, MODEL_C_BINS as isize - 1)
        as usize
}

fn equivalent_images(a: &FitImage, b: &FitImage) -> bool {
    a.transform == b.transform
        && a.exposure_hint.to_bits() == b.exposure_hint.to_bits()
        && a.trust_weight.to_bits() == b.trust_weight.to_bits()
        && a.sample_trust == b.sample_trust
        && a.exposure_mode == b.exposure_mode
        && a.contributor == b.contributor
        && a.intent == b.intent
        && a.settings == b.settings
        && a.samples == b.samples
}

fn deduplicate_copies(images: &[FitImage]) -> Vec<FitImage> {
    let mut out: Vec<FitImage> = Vec::new();
    for image in images {
        let group = image.samples.header().rerender_group;
        if group.is_some()
            && out.iter().any(|kept| {
                kept.samples.header().rerender_group == group && equivalent_images(image, kept)
            })
        {
            continue;
        }
        out.push(image.clone());
    }
    out
}

#[derive(Clone, Copy)]
struct ImageRegion {
    mean: [f64; 3],
    within: Option<CellMoment>,
}

#[derive(Clone, Copy)]
struct BodyRegion {
    mean: [f64; 3],
    /// From independently repeated image units.
    mean_covariance: Option<CellMoment>,
    /// Used when there is no independent repeat.
    within_image: Option<CellMoment>,
    repeats: usize,
}

impl BodyRegion {
    fn from_units(units: &[ImageRegion]) -> Option<Self> {
        if units.is_empty() {
            return None;
        }
        let count = units.len() as f64;
        let mean: [f64; 3] = core::array::from_fn(|channel| {
            units.iter().map(|unit| unit.mean[channel]).sum::<f64>() / count
        });
        if units.len() < 2 {
            return Some(Self {
                mean,
                mean_covariance: None,
                within_image: units[0].within,
                repeats: 1,
            });
        }
        let covariance = sample_covariance(
            &units.iter().map(|unit| unit.mean).collect::<Vec<_>>(),
            mean,
        )
        .map(|covariance| moment_scale(covariance, 1.0 / count));
        Some(Self {
            mean,
            mean_covariance: covariance,
            within_image: None,
            repeats: units.len(),
        })
    }
}

#[derive(Clone, Copy, Default)]
struct RegionAccum {
    sum: [f64; 3],
    outer: CellMoment,
    weight: f64,
    weight_sq: f64,
}

impl RegionAccum {
    fn add(&mut self, residual: [f64; 3], weight: f64) {
        if weight <= 0.0 {
            return;
        }
        for i in 0..3 {
            self.sum[i] += weight * residual[i];
            for j in 0..3 {
                self.outer[i][j] += weight * residual[i] * residual[j];
            }
        }
        self.weight += weight;
        self.weight_sq += weight * weight;
    }

    fn finish(&self) -> Option<ImageRegion> {
        if self.weight <= 0.0 {
            return None;
        }
        let weight = self.weight;
        let mean = self.sum.map(|sum| sum / weight);
        let denominator = weight - self.weight_sq / weight;
        let within = if denominator > 1e-12 {
            let covariance: CellMoment = core::array::from_fn(|i| {
                core::array::from_fn(|j| {
                    (self.outer[i][j] - self.sum[i] * self.sum[j] / weight) / denominator
                })
            });
            Some(project_psd(covariance))
        } else {
            None
        };
        Some(ImageRegion { mean, within })
    }
}

/// Groups images whose rerender group says they share one input.
fn image_units(images: &[FitImage]) -> Vec<Vec<usize>> {
    let mut units: Vec<(Option<looklab_samples::RerenderGroupId>, Vec<usize>)> = Vec::new();
    for (index, image) in images.iter().enumerate() {
        match image.samples.header().rerender_group {
            Some(group) => {
                if let Some((_, members)) = units
                    .iter_mut()
                    .find(|(candidate, _)| *candidate == Some(group))
                {
                    members.push(index);
                } else {
                    units.push((Some(group), vec![index]));
                }
            }
            None => units.push((None, vec![index])),
        }
    }
    units.into_iter().map(|(_, members)| members).collect()
}

fn image_folds(images: &[FitImage]) -> Vec<Vec<usize>> {
    let units = image_units(images);
    if units.is_empty() {
        return Vec::new();
    }
    // Three units per fold when possible, two for smaller pools; one-unit folds
    // only when the pool cannot give a held-out fit and an independent repeat.
    let count = if units.len() >= 6 {
        (units.len() / 3).min(MODEL_IMAGE_FOLDS)
    } else if units.len() >= 4 {
        (units.len() / 2).min(MODEL_IMAGE_FOLDS)
    } else {
        units.len()
    };
    let mut folds = Vec::with_capacity(count);
    for index in 0..count {
        let start = index * units.len() / count;
        let end = (index + 1) * units.len() / count;
        if start == end {
            continue;
        }
        let mut fold_images = Vec::new();
        for unit in &units[start..end] {
            fold_images.extend(unit.iter().copied());
        }
        folds.push(fold_images);
    }
    folds
}

/// Normal-body-mean delta method: `(B-1)S²/σ² ~ χ²_{B-1}`, `Var(S²)≈2σ⁴/(B-1)`;
/// the Student-t multiplier is finite-B inflation, approximate for heavy tails.
fn variance_upper_factor(count: f64) -> f64 {
    1.0 + student_t_95(count - 1.0) * (2.0 / (count - 1.0)).sqrt()
}

fn body_point(bodies: &[BodyRegion]) -> f64 {
    let count = bodies.len() as f64;
    bodies
        .iter()
        .map(|body| moment_trace(outer_moment(body.mean)))
        .sum::<f64>()
        / count
}

#[derive(Clone, Copy)]
enum CellClass {
    Supported(CellMoment),
    Local(CellMoment),
    Unknown,
}

fn cell_estimate(bodies: &[BodyRegion]) -> CellClass {
    let count = bodies.len() as f64;
    if bodies.len() == 1 {
        let body = bodies[0];
        let coherent = outer_moment(body.mean);
        if let Some(covariance) = body.mean_covariance {
            let t = student_t_95(body.repeats as f64 - 1.0);
            CellClass::Supported(moment_add(coherent, moment_scale(covariance, t * t)))
        } else if let Some(within) = body.within_image {
            CellClass::Local(moment_add(coherent, within))
        } else {
            CellClass::Unknown
        }
    } else {
        let mean: [f64; 3] = core::array::from_fn(|channel| {
            bodies.iter().map(|body| body.mean[channel]).sum::<f64>() / count
        });
        let between = sample_covariance(
            &bodies.iter().map(|body| body.mean).collect::<Vec<_>>(),
            mean,
        )
        .unwrap_or_else(zero_moment);
        let sampling = bodies.iter().fold(zero_moment(), |mut sum, body| {
            if let Some(covariance) = body.mean_covariance {
                sum = moment_add(sum, covariance);
            }
            sum
        });
        let sampling = moment_scale(sampling, 1.0 / count);
        let t = student_t_95(count - 1.0);
        let factor = variance_upper_factor(count);
        let coherent = outer_moment(mean);
        let excess = project_psd(moment_sub(moment_scale(between, factor), sampling));
        // Sampling covariance of the grand mean, separate from heterogeneity.
        let mean_uncertainty = moment_scale(sampling, t * t / count);
        CellClass::Supported(project_psd(moment_add(
            moment_add(coherent, excess),
            mean_uncertainty,
        )))
    }
}

fn model_error_diag() -> bool {
    std::env::var_os("LOOKLAB_MODEL_ERROR_DIAG").is_some()
}

/// K-fold over whole bodies or whole images, conditioning held nuisances on the
/// kept Look; variance is squared coherent mean plus inflated body-to-body heterogeneity.
pub fn estimate_model_error(
    images: &[FitImage],
    groups: &[BodyGroup],
    model: Option<&ControlResponseModel>,
    options: &FitOptions,
    clip_k: f64,
) -> Result<ModelErrorField> {
    if images.is_empty() {
        return Err(crate::types::FitError::NoImages);
    }
    let mut field = ModelErrorField::empty();
    let bins = MODEL_HUE_BINS * MODEL_S_BINS;
    let cells = bins * MODEL_C_BINS;
    let body_holdouts = groups
        .iter()
        .filter(|group| !group.images.is_empty())
        .count()
        >= 2;
    let folds = if body_holdouts {
        groups
            .iter()
            .filter(|group| !group.images.is_empty())
            .map(|group| group.images.clone())
            .collect()
    } else {
        image_folds(images)
    };
    if folds.is_empty() {
        return Err(crate::types::FitError::NoModelErrorEvidence);
    }
    let mut per_cell: Vec<Vec<BodyRegion>> = vec![Vec::new(); MODEL_ENCODINGS * cells];
    let mut single_body_units = (!body_holdouts).then(|| vec![Vec::new(); MODEL_ENCODINGS * cells]);
    let mut evidence = false;
    let mut failed = false;
    for (fold_index, held) in folds.iter().enumerate() {
        let kept_raw: Vec<FitImage> = images
            .iter()
            .enumerate()
            .filter(|(index, _)| !held.contains(index))
            .map(|(_, image)| image.clone())
            .collect();
        let kept = deduplicate_copies(&kept_raw);
        if kept.is_empty() {
            if model_error_diag() {
                eprintln!("model_error fold {fold_index}: kept set empty");
            }
            failed = true;
            continue;
        }
        let Ok(result) = crate::fit::run_quick(&kept, options, None, model) else {
            if model_error_diag() {
                eprintln!("model_error fold {fold_index}: kept quick fit failed");
            }
            failed = true;
            continue;
        };
        if model_error_diag() {
            eprintln!(
                "model_error fold {fold_index}: kept converged={} cost={} evaluations={}",
                result.diagnostics.converged,
                result.diagnostics.final_cost,
                result.diagnostics.evaluations,
            );
        }
        if !result.diagnostics.converged {
            failed = true;
            continue;
        }
        let learned = result
            .control_response
            .as_ref()
            .map(|fit| &fit.model)
            .or(model);
        let held_raw: Vec<FitImage> = held.iter().map(|index| images[*index].clone()).collect();
        let held_images = deduplicate_copies(&held_raw);
        if held_images.is_empty() {
            continue;
        }
        let Ok(problem) = Problem::new_with(&held_images, learned, options) else {
            if model_error_diag() {
                eprintln!("model_error fold {fold_index}: held problem construction failed");
            }
            failed = true;
            continue;
        };
        let image_encoding: Vec<usize> = problem
            .images
            .iter()
            .map(|image| encoding_index(image.space))
            .collect();
        let Ok(conditioned) =
            crate::fit::condition_nuisances(&held_images, options, &result.look, learned, clip_k)
        else {
            if model_error_diag() {
                eprintln!("model_error fold {fold_index}: nuisance conditioning failed");
            }
            failed = true;
            continue;
        };
        if !conditioned.converged {
            if model_error_diag() {
                eprintln!(
                    "model_error fold {fold_index}: nuisance conditioning not converged accepted={} attempts={} damping={} last_step={} gradient_max={}",
                    conditioned.accepted,
                    conditioned.attempts,
                    conditioned.damping,
                    conditioned.last_step,
                    conditioned.gradient_max
                );
            }
            failed = true;
            continue;
        }
        let nuisances = conditioned.images;
        let mut params = crate::fit::params_from_look(&problem, &result.look);
        if let Some(model) = learned {
            crate::fit::seed_response_params(&problem, &mut params, model);
            crate::fit::seed_mode_params(&problem, &mut params, model);
        }
        for (index, nuisance) in nuisances.iter().enumerate() {
            let range = params.layout.image(index);
            params.theta[range.start] = nuisance.exposure;
            params.theta[range.start + 1] = nuisance.wb_log_r;
            params.theta[range.start + 2] = nuisance.wb_log_b;
        }
        params.theta[params.layout.clip] = crate::layout::zeta_from_clip(clip_k);
        let realized: Vec<Look> = (0..held_images.len())
            .map(|index| problem.realized_look(&params, index))
            .collect::<crate::types::Result<_>>()?;
        let mut accum: Vec<Vec<RegionAccum>> = (0..held_images.len())
            .map(|_| vec![RegionAccum::default(); cells])
            .collect();
        for sample in &problem.samples {
            let index = sample.image as usize;
            let context = problem.image_ctx(&params, index);
            let xyz = looklab_color::Xyz::from_array(context.transform.mul_vec([
                sample.rgb[0] * context.wb_r * context.exposure_scale,
                sample.rgb[1] * context.exposure_scale,
                sample.rgb[2] * context.wb_b * context.exposure_scale,
            ]));
            let input = LookInput::new(xyz);
            let (prediction, _, penalty) = crate::predict::predict_only(
                &realized[index],
                &context,
                sample.rgb,
                clip_k,
                &sample.target,
            );
            if penalty.is_none() {
                if model_error_diag() {
                    eprintln!("model_error fold {fold_index}: held sample {index} left the domain");
                }
                failed = true;
                continue;
            }
            let target = &sample.target.ucs;
            let residual = [
                prediction.j - target.j,
                prediction.a - target.a,
                prediction.b - target.b,
            ];
            let cell = region(input.s, input.h_in) * MODEL_C_BINS + color_cell(input.c_in);
            accum[index][cell].add(residual, sample.weight.max(0.0));
        }
        let image_regions: Vec<Vec<Option<ImageRegion>>> = accum
            .iter()
            .map(|regions| regions.iter().map(RegionAccum::finish).collect())
            .collect();
        let units = image_units(&held_images);
        for indexed in 0..MODEL_ENCODINGS * cells {
            let encoding = indexed / cells;
            let cell = indexed % cells;
            let mut unit_regions = Vec::new();
            for unit in &units {
                let mut means = Vec::new();
                let mut within: Option<CellMoment> = None;
                for image_index in unit {
                    if image_encoding[*image_index] != encoding {
                        continue;
                    }
                    if let Some(region) = image_regions[*image_index][cell] {
                        means.push(region.mean);
                        if let Some(value) = region.within
                            && within
                                .is_none_or(|current| moment_trace(value) > moment_trace(current))
                        {
                            within = Some(value);
                        }
                    }
                }
                if means.is_empty() {
                    continue;
                }
                let count = means.len() as f64;
                let mean: [f64; 3] = core::array::from_fn(|channel| {
                    means.iter().map(|value| value[channel]).sum::<f64>() / count
                });
                unit_regions.push(ImageRegion { mean, within });
            }
            if let Some(all_units) = single_body_units.as_mut() {
                all_units[indexed].extend(unit_regions);
            } else if let Some(body) = BodyRegion::from_units(&unit_regions) {
                per_cell[indexed].push(body);
                evidence = true;
            }
        }
    }
    if failed {
        return Err(crate::types::FitError::NoModelErrorEvidence);
    }
    if let Some(units_by_cell) = single_body_units {
        for (indexed, units) in units_by_cell.iter().enumerate() {
            if let Some(body) = BodyRegion::from_units(units) {
                per_cell[indexed].push(body);
                evidence = true;
            }
        }
    }
    if !evidence {
        return Err(crate::types::FitError::NoModelErrorEvidence);
    }

    let entries = MODEL_ENCODINGS * cells;
    let mut post = vec![zero_moment(); entries];
    let mut held_rms = vec![0.0_f64; entries];
    let mut bodies_count = vec![0usize; entries];
    let mut repeats = vec![0usize; entries];
    let mut evidence_cells = vec![false; entries];
    let mut classes = vec![CellClass::Unknown; entries];
    let mut bin_supported = vec![0.0_f64; MODEL_ENCODINGS * bins];
    let mut encoding_supported = [false; MODEL_ENCODINGS];
    let mut encoding_global = [0.0_f64; MODEL_ENCODINGS];
    let mut any_supported = false;
    for indexed in 0..entries {
        let bodies = &per_cell[indexed];
        if bodies.is_empty() {
            continue;
        }
        let encoding = indexed / cells;
        let cell = indexed % cells;
        let bin = cell / MODEL_C_BINS;
        bodies_count[indexed] = bodies.len();
        repeats[indexed] = bodies.iter().map(|body| body.repeats).sum();
        held_rms[indexed] = body_point(bodies).max(0.0).sqrt();
        let estimate = cell_estimate(bodies);
        classes[indexed] = estimate;
        match estimate {
            CellClass::Supported(moment) => {
                post[indexed] = moment;
                evidence_cells[indexed] = true;
                let radial = moment_trace(moment);
                bin_supported[encoding * bins + bin] =
                    bin_supported[encoding * bins + bin].max(radial);
                encoding_global[encoding] = encoding_global[encoding].max(radial);
                encoding_supported[encoding] = true;
                any_supported = true;
            }
            CellClass::Local(moment) => post[indexed] = moment,
            CellClass::Unknown => {}
        }
    }
    if !any_supported {
        return Err(crate::types::FitError::NoModelErrorEvidence);
    }
    let global_all = encoding_global.iter().copied().fold(0.0_f64, f64::max);
    for indexed in 0..entries {
        let encoding = indexed / cells;
        let cell = indexed % cells;
        let bin = cell / MODEL_C_BINS;
        let region_supported = bin_supported[encoding * bins + bin];
        let encoding_fallback = if encoding_supported[encoding] {
            encoding_global[encoding]
        } else {
            global_all
        };
        let fallback = if region_supported > 0.0 {
            region_supported
        } else {
            encoding_fallback
        };
        match classes[indexed] {
            CellClass::Supported(_) => {}
            CellClass::Local(moment) => {
                post[indexed] = identity_moment(moment_trace(moment).max(fallback));
            }
            CellClass::Unknown => post[indexed] = identity_moment(fallback),
        }
    }
    field.post = post;
    field.evidence = evidence_cells;
    field.held_out_mean_rms = held_rms;
    field.bodies = bodies_count;
    field.repeats = repeats;
    if model_error_diag()
        && let Some((indexed, moment)) = field
            .post
            .iter()
            .enumerate()
            .max_by(|left, right| moment_trace(*left.1).total_cmp(&moment_trace(*right.1)))
    {
        let encoding = indexed / cells;
        let cell = indexed % cells;
        eprintln!(
            "model_error peak encoding={encoding} cell={cell} radial={} rms={} repeats={} evidence={} class={}",
            moment_trace(*moment),
            field.held_out_mean_rms[indexed],
            field.repeats[indexed],
            field.evidence[indexed],
            match classes[indexed] {
                CellClass::Supported(_) => "supported",
                CellClass::Local(_) => "local",
                CellClass::Unknown => "unknown",
            }
        );
        for body in &per_cell[indexed] {
            eprintln!(
                "model_error cell={cell} repeats={} mean={:?} mean_covariance={:?} within={:?}",
                body.repeats, body.mean, body.mean_covariance, body.within_image,
            );
        }
    }
    Ok(field)
}

/// Add measured post-clip model moments; reliable probes also get inverse-transported pre-clip.
#[allow(clippy::too_many_arguments)]
pub fn apply_model_error(
    probes: &mut ProbeUncertainty,
    field: &ModelErrorField,
    look: &Look,
    density: &crate::types::Density,
    space: RgbSpace,
    options: &FitOptions,
    clip_k: f64,
    previous: Option<&crate::types::PreviousSupport>,
) {
    let probes_v1 = looklab_model::probes::v1();
    let mut supported = 0usize;
    for (index, probe) in probes.probes.iter_mut().enumerate() {
        let input = looklab_model::LookInput::new(probes_v1.probes[index].xyz);
        let has_evidence = field.evidence_at(&input, space);
        let prediction = look.eval(probes_v1.probes[index].xyz);
        let chain = crate::predict::clip_jacobian(prediction, space, clip_k);
        let model_post = field.at(&input, space);
        for a in 0..3 {
            for b in 0..3 {
                probe.post_covariance[a][b] += model_post[a][b];
            }
        }
        let mut pre_clip_reliable =
            crate::predict::clip_derivative_min(prediction, space, clip_k) >= 0.25;
        if pre_clip_reliable {
            if let Some(inverse) = chain.inverse() {
                let transported = inverse.mul(&Mat3(model_post)).mul(&inverse.transpose()).0;
                let transported: CellMoment = core::array::from_fn(|i| {
                    core::array::from_fn(|j| 0.5 * (transported[i][j] + transported[j][i]))
                });
                if transported.iter().flatten().all(|value| value.is_finite()) {
                    for a in 0..3 {
                        for b in 0..3 {
                            probe.covariance[a][b] += transported[a][b];
                        }
                    }
                } else {
                    pre_clip_reliable = false;
                }
            } else {
                pre_clip_reliable = false;
            }
        }
        probe.sigma_de = (probe.covariance[0][0] + probe.covariance[1][1] + probe.covariance[2][2])
            .max(0.0)
            .sqrt();
        crate::uncertainty::floor_post_covariance(&probe.covariance, &mut probe.post_covariance);
        probe.post_sigma_de = (probe.post_covariance[0][0]
            + probe.post_covariance[1][1]
            + probe.post_covariance[2][2])
            .max(0.0)
            .sqrt();
        probe.post_ucs =
            crate::predict::post_clip(look, probes_v1.probes[index].xyz, space, clip_k);
        if !probe
            .post_ucs
            .to_array()
            .iter()
            .all(|value| value.is_finite())
        {
            probe.post_ucs = looklab_model::output_encode_clip(prediction, space, clip_k);
        }
        probe.pre_clip_reliable = pre_clip_reliable && !probe.support_withheld;
        probe.gamut_edge = probes_v1.probes[index].id.as_str().split('-').nth(2) == Some("80");
        let previous_sigma = previous
            .and_then(|state| state.sigma.get(index).copied())
            .unwrap_or(false);
        let previous_bias = previous
            .and_then(|state| state.bias.get(index).copied())
            .unwrap_or(false);
        let flags = crate::uncertainty::mark_support(
            probe.post_sigma_de,
            probe.bias_de,
            probe.density,
            probe.clusters,
            density.floor,
            previous_sigma,
            previous_bias,
            options,
        );
        probe.supported_sigma = flags.sigma && !probe.support_withheld;
        probe.supported_density = flags.density;
        probe.supported_clusters = flags.clusters;
        probe.supported_bias = flags.bias;
        probe.supported_evidence = has_evidence;
        probe.supported = flags.all() && has_evidence && !probe.support_withheld;
        if probe.supported {
            supported += 1;
        }
    }
    probes.supported_fraction = supported as f64 / probes.probes.len().max(1) as f64;
}

/// Per-sample ΔE and target for an arbitrary Look over `images`.
pub fn image_errors(
    look: &Look,
    images: &[FitImage],
    options: &FitOptions,
) -> Result<Vec<(f64, Ucs)>> {
    let problem = Problem::new_with(images, None, options)?;
    let mut out = Vec::with_capacity(problem.samples.len());
    for sample in &problem.samples {
        let index = sample.image as usize;
        let image = &images[index];
        let scale = libm::pow(2.0, image.exposure_hint);
        let xyz = looklab_color::Xyz::from_array(image.transform.mul_vec([
            sample.rgb[0] * scale,
            sample.rgb[1] * scale,
            sample.rgb[2] * scale,
        ]));
        out.push((
            delta_e_ucs(look.eval(xyz), sample.target.ucs),
            sample.target.ucs,
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod review_tests {
    use super::*;
    use crate::types::{Density, ProbeEstimate, ProbeUncertainty};

    fn density() -> Density {
        Density {
            s_bins: 36,
            hue_bins: 24,
            c_bins: 12,
            counts: vec![1.0; 36 * 24 * 12],
            total: 1.0,
            floor: 0.0,
        }
    }

    fn probes() -> ProbeUncertainty {
        let ids = looklab_model::probes::v1();
        let probes = ids
            .probes
            .iter()
            .enumerate()
            .map(|(index, _)| ProbeEstimate {
                id_hint: index,
                ucs: looklab_color::ucs::Ucs::new(50.0, 0.0, 0.0),
                covariance: [[0.0; 3]; 3],
                sigma_de: 0.0,
                post_ucs: looklab_color::ucs::Ucs::new(50.0, 0.0, 0.0),
                post_covariance: [[0.0; 3]; 3],
                post_sigma_de: 0.0,
                bias_de: 0.0,
                pre_clip_reliable: true,
                gamut_edge: false,
                density: 1.0,
                supported_sigma: false,
                supported_density: false,
                supported_clusters: false,
                supported_bias: false,
                supported_evidence: false,
                supported: false,
                support_withheld: false,
                clusters: 5,
            })
            .collect();
        ProbeUncertainty {
            probes,
            ids: ids.probes.iter().map(|probe| probe.id.clone()).collect(),
            supported_fraction: 0.0,
        }
    }

    #[test]
    fn directional_post_moment_is_not_divided_or_clipped_twice() {
        let look = Look::colorimetric_spline().expect("look");
        let mut field = ModelErrorField::empty();
        let moment = [[4.0, 0.0, 0.0], [0.0, 0.0, 0.0], [0.0, 0.0, 0.0]];
        field.post.fill(moment);
        let mut estimates = probes();
        apply_model_error(
            &mut estimates,
            &field,
            &look,
            &density(),
            RgbSpace::Srgb,
            &FitOptions {
                density_floor: 0.0,
                ..FitOptions::default()
            },
            0.03,
            None,
        );
        let post = estimates.probes[0].post_covariance;
        assert!(post[0][0] >= 4.0 - 1e-12);
        assert!(estimates.probes[0].post_sigma_de >= 2.0 - 1e-12);
    }

    #[test]
    fn pre_clip_transport_only_for_reliable_probes() {
        let mut look = Look::colorimetric_spline().expect("look");
        look.color.g.plane.fill(0.5);
        look.color.g.volume.fill(0.5);
        let mut field = ModelErrorField::empty();
        let moment = [[4.0, 0.0, 0.0], [0.0, 0.0, 0.0], [0.0, 0.0, 0.0]];
        field.post.fill(moment);
        let mut estimates = probes();
        apply_model_error(
            &mut estimates,
            &field,
            &look,
            &density(),
            RgbSpace::Srgb,
            &FitOptions {
                density_floor: 0.0,
                ..FitOptions::default()
            },
            0.03,
            None,
        );
        let mut reliable = false;
        let mut unreliable = false;
        for probe in &estimates.probes {
            let pre = moment_trace(probe.covariance);
            let post = moment_trace(probe.post_covariance);
            if probe.pre_clip_reliable {
                reliable = true;
                assert!(pre > 1e-12, "reliable probe got no pre-clip model term");
            } else {
                unreliable = true;
                assert!(pre < 1e-12, "unreliable probe invented a pre-clip term");
            }
            assert!(post >= 4.0 - 1e-12, "post model moment missing");
        }
        assert!(reliable, "fixture has no reliable probe");
        assert!(unreliable, "fixture has no unreliable probe");
    }

    #[test]
    fn withheld_probes_stay_unsupported_after_model_error() {
        const MID_GRAY: usize = 30;
        let look = Look::colorimetric_spline().expect("look");
        let mut field = ModelErrorField::empty();
        let moment = [[0.25, 0.0, 0.0], [0.0, 0.0, 0.0], [0.0, 0.0, 0.0]];
        field.post.fill(moment);
        field.evidence.fill(true);
        let mut estimates = probes();
        estimates.probes[MID_GRAY].support_withheld = true;
        estimates.probes[MID_GRAY].pre_clip_reliable = false;
        apply_model_error(
            &mut estimates,
            &field,
            &look,
            &density(),
            RgbSpace::Srgb,
            &FitOptions {
                density_floor: 0.0,
                ..FitOptions::default()
            },
            0.03,
            None,
        );
        crate::uncertainty::unsupport_unreliable(&mut estimates);
        let withheld = &estimates.probes[MID_GRAY];
        assert!(withheld.support_withheld);
        assert!(!withheld.supported);
        assert!(!withheld.supported_sigma);
        assert!(!withheld.pre_clip_reliable);
        assert!(
            estimates.probes[1..].iter().any(|probe| probe.supported),
            "full evidence made no probe supported"
        );
        let mut control = probes();
        apply_model_error(
            &mut control,
            &field,
            &look,
            &density(),
            RgbSpace::Srgb,
            &FitOptions {
                density_floor: 0.0,
                ..FitOptions::default()
            },
            0.03,
            None,
        );
        crate::uncertainty::unsupport_unreliable(&mut control);
        assert!(
            control.probes[MID_GRAY].supported,
            "a mid-gray probe fails support without the withheld bit"
        );
    }

    #[test]
    fn large_body_variance_keeps_mean_uncertainty() {
        let body = |mean: f64| BodyRegion {
            mean: [mean, 0.0, 0.0],
            mean_covariance: Some(identity_moment(100.0)),
            within_image: None,
            repeats: 2,
        };
        let CellClass::Supported(moment) = cell_estimate(&[body(1.0), body(-1.0)]) else {
            panic!("two independent bodies are supported");
        };
        assert!(
            moment_trace(moment) > 1.0,
            "large body variance erased mean uncertainty"
        );
    }

    #[cfg(feature = "synth")]
    #[test]
    fn single_body_pools_use_image_folds() {
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
        let field =
            estimate_model_error(&data.images, &[], None, &options, 0.03).expect("image folds");
        assert!(field.held_out_mean_rms.iter().any(|value| *value > 0.0));
    }
}
