// SPDX-License-Identifier: AGPL-3.0-or-later
//! The fit's sample set, block structure and one objective/Jacobian pass.

use core::ops::Range;
use std::borrow::Cow;

use looklab_color::{Mat3, Rgb, RgbSpace};
use looklab_model::Look;
use looklab_model::basis::{RESIDUAL_COEFS, TONE_COEFS};
use looklab_model::domain_penalty::DOMAIN_AXES;
use looklab_model::realization::{
    ControlResponseModel, ControlValue, Delta, ResponseComponent, ResponseParamId, Settings,
};
use looklab_model::solve::ucs_of_xyz;
use looklab_samples::OutputSpace;

use crate::layout::{BlockKind, Layout, Params, softplus, softplus_slope};
use crate::predict::{ColumnMap, ImageCtx, Jac, Layer, ResponseTerm, ToneClamp, evaluate_sample};
use crate::types::{
    ContributorId, DomainExclusion, ExposureModeId, FitError, FitImage, FitOptions, ImageIntent,
    Result,
};

pub(crate) fn response_tone_clamp(
    mut betas: [f64; TONE_COEFS],
    terms: &[ResponseTerm],
    coefficients: &[f64],
) -> [ToneClamp; TONE_COEFS] {
    for term in terms {
        if term.component == ResponseComponent::Tone && term.feature > 0 {
            betas[term.feature as usize - 1] += coefficients[term.index] * term.phi;
        }
    }
    betas.map(|beta| {
        if beta > 0.0 {
            ToneClamp::Interior
        } else if beta < 0.0 {
            ToneClamp::Clamped
        } else {
            ToneClamp::Boundary
        }
    })
}

pub(crate) struct ImageEntry {
    pub transform: Mat3,
    pub space: RgbSpace,
    pub exposure_hint: f64,
    pub trust: f64,
    pub mode: Option<usize>,
    pub contributor: usize,
    pub samples: Range<usize>,
    pub intent: ImageIntent,
    pub known_exposure: f64,
}

/// Per-image realization context: rows sharing `group` share one clip and one control response.
#[derive(Clone, Copy)]
pub(crate) struct RowModel<'a> {
    pub group: usize,
    pub model: Option<&'a ControlResponseModel>,
    pub clip_k: f64,
    /// Constant `(ΔT, Δlog G)` firmware offset applied to this row's Look.
    pub firmware: [f64; 2],
    /// Authoritative input transform for the retained-row eligibility probe.
    pub eligible_transform: Mat3,
    /// Authoritative exposure hint for the retained-row eligibility probe.
    pub eligible_hint: f64,
}

/// One realization's response-coefficient block inside `layout.response`.
#[derive(Clone, Debug)]
pub(crate) struct ResponseGroup {
    /// Offset of the group's first coefficient relative to `layout.response.start`.
    pub start: usize,
    pub ids: Vec<ResponseParamId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntentFilter {
    NativeOnly,
    All,
}

impl IntentFilter {
    fn includes(self, intent: ImageIntent) -> bool {
        match self {
            Self::NativeOnly => intent == ImageIntent::Native,
            Self::All => true,
        }
    }
}

#[derive(Clone, Copy)]
enum CurvatureMode {
    GaussNewton,
    Analytic,
}

#[derive(Clone, Copy)]
pub(crate) enum HessianRows {
    Retained,
    All,
}

pub(crate) struct Sample {
    pub rgb: [f64; 3],
    pub target: crate::predict::Target,
    pub image: u32,
    pub weight: f64,
    pub neutral: bool,
    /// The stored output is away from the clip shoulders, so the sample carries unclipped data.
    pub informative: bool,
}

/// Exposures and WB gains the eligibility probe covers, in prior standard deviations.
const ELIGIBILITY_SIGMAS: f64 = 3.0;

/// `true` when the sample's prediction stays inside the evaluator's domain for every probe
/// nuisance: exposure at `0` and `±3σ`, crossed with both WB gain corners.
fn eligible(transform: Mat3, rgb: [f64; 3], hint: f64, options: &FitOptions) -> bool {
    for exposure in [-ELIGIBILITY_SIGMAS, 0.0, ELIGIBILITY_SIGMAS] {
        let scale = libm::pow(
            2.0,
            (hint + exposure * options.exposure_prior_sd).clamp(-8.0, 8.0),
        );
        for gain_shift in [-ELIGIBILITY_SIGMAS, ELIGIBILITY_SIGMAS] {
            let gain = libm::exp(gain_shift * options.wb_prior_sd);
            let cam = [rgb[0] * gain * scale, rgb[1] * scale, rgb[2] * gain * scale];
            let scene = transform.mul_vec(cam);
            let margin = looklab_model::look::domain_margin(&looklab_model::LookInput::new(
                looklab_color::Xyz::from_array(scene),
            ));
            if margin <= 0.0 || !margin.is_finite() {
                return false;
            }
        }
    }
    true
}

fn exclusion_bin(s: f64, hue: f64, chroma: f64) -> (i32, i32, i32) {
    (
        (s * 4.0).round() as i32,
        (hue / core::f64::consts::PI * 12.0).round() as i32,
        chroma.round() as i32,
    )
}

pub(crate) struct Problem {
    pub layout: Layout,
    pub images: Vec<ImageEntry>,
    pub modes: Vec<ExposureModeId>,
    pub contributors: Vec<ContributorId>,
    pub samples: Vec<Sample>,
    pub hessian: Vec<u32>,
    hessian_set: Vec<bool>,
    pub nu: f64,
    /// Images that carry non-zero weight and therefore count towards the effective sample size.
    pub active_images: usize,
    pub parameter_ids: Vec<ResponseParamId>,
    pub response_groups: Vec<ResponseGroup>,
    /// Per-image clip when rows carry their own realization clip.
    pub row_clip: Option<Vec<f64>>,
    /// Per-image constant firmware offset on `(T, log G)`.
    pub firmware: Vec<[f64; 2]>,
    pub terms: Vec<Vec<ResponseTerm>>,
    pub settings: Vec<Settings>,
    pub defaults: Vec<(looklab_schema::ControlKey, ControlValue)>,
    /// Samples dropped by the fixed eligibility probe, by colour/exposure bin.
    pub excluded_regions: Vec<DomainExclusion>,
    /// Total samples dropped by the fixed eligibility probe.
    pub excluded_samples: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ActiveColumns {
    pub image: bool,
    pub mode: bool,
    pub wb: bool,
    pub clip: bool,
    pub response: bool,
}

impl ActiveColumns {
    pub const ALL: Self = Self {
        image: true,
        mode: true,
        wb: true,
        clip: true,
        response: true,
    };
}

pub(crate) struct BlockAcc {
    pub kind: BlockKind,
    pub params: Vec<usize>,
    pub local: Vec<u32>,
    /// `Some(index)` for a block whose columns belong to image `index` alone.
    pub image: Option<usize>,
    pub hessian: Vec<f64>,
    /// Gauss-Newton data curvature `Σ α JᵀJ`, without the Student-t rank-1 or barrier terms.
    /// GCV fits its data-RSS model on this matrix; `hessian` is the step curvature.
    pub gn_hessian: Vec<f64>,
    /// Full-data gradient.
    pub gradient: Vec<f64>,
    /// Gradient over the Hessian rows.
    pub hessian_gradient: Vec<f64>,
    /// Exact diagonal of the data Hessian over all active samples.
    pub full_diagonal: Vec<f64>,
}

impl BlockAcc {
    pub fn new(kind: BlockKind, params: Vec<usize>, total: usize) -> Self {
        let mut local = vec![u32::MAX; total];
        for (index, global) in params.iter().enumerate() {
            local[*global] = index as u32;
        }
        let n = params.len();
        Self {
            kind,
            params,
            local,
            image: None,
            hessian: vec![0.0; n * n],
            gn_hessian: vec![0.0; n * n],
            gradient: vec![0.0; n],
            hessian_gradient: vec![0.0; n],
            full_diagonal: vec![0.0; n],
        }
    }

    pub fn len(&self) -> usize {
        self.params.len()
    }

    pub fn reset(&mut self) {
        self.hessian.fill(0.0);
        self.gn_hessian.fill(0.0);
        self.gradient.fill(0.0);
        self.hessian_gradient.fill(0.0);
        self.full_diagonal.fill(0.0);
    }
}

#[derive(Default, PartialEq, Eq)]
enum MirrorState {
    #[default]
    Unchecked,
    /// Upper-triangle updates followed by mirroring.
    Canonical,
    Asymmetric,
}

#[derive(Default)]
struct HessianBuffer {
    /// `(local column, derivative, Jᵀr, ∂axis/∂θ)`.
    columns: Vec<(u32, [f64; 3], f64, [f64; DOMAIN_AXES])>,
    mirror: MirrorState,
}

impl MirrorState {
    fn resolve(&mut self, hessian: &[f64], n: usize) -> bool {
        if *self == Self::Unchecked {
            *self = if bitwise_symmetric(hessian, n) {
                Self::Canonical
            } else {
                Self::Asymmetric
            };
        }
        *self == Self::Canonical
    }
}

fn bitwise_symmetric(hessian: &[f64], n: usize) -> bool {
    (0..n).all(|row| {
        (row + 1..n).all(|column| {
            hessian[row * n + column].to_bits() == hessian[column * n + row].to_bits()
        })
    })
}

/// Each entry receives contributions in the original pair order.
#[allow(clippy::too_many_arguments)]
fn accumulate_block<const CANONICAL: bool>(
    hessian: &mut [f64],
    gn_hessian: &mut [f64],
    gradient: &mut [f64],
    n: usize,
    buffer: &[(u32, [f64; 3], f64, [f64; DOMAIN_AXES])],
    contribution: f64,
    residual: [f64; 3],
    robust: f64,
    penalty: &[[f64; DOMAIN_AXES]; DOMAIN_AXES],
) {
    for (a, (left, left_derivative, left_term, left_axis)) in buffer.iter().enumerate() {
        for (right, right_derivative, right_term, right_axis) in buffer.iter().skip(a) {
            let dot = left_derivative[0] * right_derivative[0]
                + left_derivative[1] * right_derivative[1]
                + left_derivative[2] * right_derivative[2];
            let data = contribution * dot;
            let mut penalty_term = 0.0;
            for axis in 0..DOMAIN_AXES {
                if left_axis[axis] == 0.0 {
                    continue;
                }
                for other in 0..DOMAIN_AXES {
                    penalty_term += penalty[axis][other] * left_axis[axis] * right_axis[other];
                }
            }
            let weighted = data + robust * left_term * right_term + penalty_term;
            if CANONICAL {
                let (row, column) = if left <= right {
                    (*left, *right)
                } else {
                    (*right, *left)
                };
                hessian[row as usize * n + column as usize] += weighted;
                gn_hessian[row as usize * n + column as usize] += data;
            } else {
                hessian[*left as usize * n + *right as usize] += weighted;
                gn_hessian[*left as usize * n + *right as usize] += data;
                if right != left {
                    hessian[*right as usize * n + *left as usize] += weighted;
                    gn_hessian[*right as usize * n + *left as usize] += data;
                }
            }
        }
        let term = left_derivative[0] * residual[0]
            + left_derivative[1] * residual[1]
            + left_derivative[2] * residual[2];
        gradient[*left as usize] += contribution * term;
    }
}

fn mirror_triangle(hessian: &mut [f64], n: usize) {
    for row in 0..n {
        for column in row + 1..n {
            hessian[column * n + row] = hessian[row * n + column];
        }
    }
}

/// Per-sample data for the matrix-free residual block.
pub(crate) struct ResidualSample {
    pub nodes: [u32; 64],
    pub weights: [f64; 64],
    pub clip: Mat3,
    pub alpha: f64,
    pub residual: [f64; 3],
    /// `weight * ∂B/∂(res_a, res_b, res_c)`.
    pub penalty_gradient: [f64; 3],
    /// `weight * Aᵀ P A` for the Hessian-vector product.
    pub penalty_curvature: [[f64; 3]; 3],
    /// `weight * Aᵀ P_gn A` for the SPD preconditioner.
    pub penalty_gn: [[f64; 3]; 3],
}

/// Projection onto residual coordinates `(res_a, res_b, res_c)`.
fn residual_penalty_matrix(matrix: &[[f64; DOMAIN_AXES]; DOMAIN_AXES]) -> [[f64; 3]; 3] {
    const SLOTS: [[usize; 2]; 3] = [[1, usize::MAX], [2, 4], [3, 5]];
    let mut out = [[0.0_f64; 3]; 3];
    for (row, left) in SLOTS.iter().enumerate() {
        for (column, right) in SLOTS.iter().enumerate() {
            for a in left.iter().copied().filter(|slot| *slot != usize::MAX) {
                for b in right.iter().copied().filter(|slot| *slot != usize::MAX) {
                    out[row][column] += matrix[a][b];
                }
            }
        }
    }
    out
}

pub(crate) struct HessianRow {
    pub columns: Vec<(u32, [f64; 3])>,
    pub weight: f64,
    /// `Jᵀr` per column, aligned with `columns`.
    pub robust_terms: Vec<f64>,
    /// Coefficient of `(Jᵀr)(Jᵀr)ᵀ`, including sample weight.
    pub robust: f64,
    /// `∂axis/∂θ` per column, aligned with `columns`.
    pub axis_terms: Vec<[f64; DOMAIN_AXES]>,
    /// `weight * P` for the Hessian-vector product; zero where the curvature
    /// provider carries the exact penalty.
    pub penalty: [[f64; DOMAIN_AXES]; DOMAIN_AXES],
    /// `weight * P_gn` for the SPD preconditioner.
    pub penalty_gn: [[f64; DOMAIN_AXES]; DOMAIN_AXES],
    pub curvature: Option<crate::predict::SampleCurvature>,
    /// Residual-grid columns `(global, derivative, Jᵀr, ∂axis/∂θ)`, excluded from `columns`.
    pub residual_columns: Vec<(u32, [f64; 3], f64, [f64; DOMAIN_AXES])>,
    /// Layer value each retained column moves, `None` when it mixes layers.
    pub layers: Vec<Option<Layer>>,
    /// `∂(t, n_a, n_b)/∂θ` in the first three slots and `∂layer/∂θ` of `layers` in
    /// the fourth.
    pub layer_weights: Vec<[f64; 4]>,
    /// `∂²layer/∂θ²` of a tone column, `∂w_m/∂t` of a tint column, zero otherwise.
    pub layer_slopes: Vec<f64>,
    /// `α w r·∂p/∂l_k` for `k = 0, 1, 2`.
    pub motion: [f64; 3],
    /// `Σ_m w''_m a_m`, `Σ_m w''_m b_m`, `Σ_m w'_m a_m`, `Σ_m w'_m b_m`.
    pub tint: [f64; 4],
    /// `α w Σ_c r_c ∂²p_c/∂l_k∂l_l`.
    pub second: Box<[f64; 81]>,
}

pub(crate) struct PassOutput {
    pub cost: f64,
    pub rss: f64,
    /// RSS over the rows that contribute to the block Hessians.
    pub hessian_rss: f64,
    pub contributor_rss: Vec<f64>,
    pub contributor_weight: Vec<f64>,
    pub contributor_pairs: Vec<Vec<(f64, f64)>>,
    pub residuals: Vec<[f64; 3]>,
    pub image_rss: Vec<f64>,
    pub image_weight: Vec<f64>,
    /// Per-image negative log-likelihood plus domain penalty, excluding priors.
    pub image_cost: Vec<f64>,
    pub per_image_gradient: Vec<Vec<f64>>,
    pub residual_samples: Vec<ResidualSample>,
    /// Full-data residual gradient.
    pub residual_gradient: Vec<f64>,
    pub active_samples: usize,
    pub hessian_rows: usize,
    pub coupled_rows: Vec<HessianRow>,
    /// A retained observation or prediction left the evaluator's domain.
    pub infeasible: bool,
}

struct LikelihoodScale {
    nu_variance: f64,
    log_scale: f64,
}

fn space_of(output: OutputSpace) -> RgbSpace {
    match output {
        OutputSpace::Srgb => RgbSpace::Srgb,
        OutputSpace::AdobeRgb => RgbSpace::AdobeRgb,
    }
}

impl Problem {
    pub fn new_with(
        images: &[FitImage],
        model: Option<&ControlResponseModel>,
        options: &FitOptions,
    ) -> Result<Self> {
        let rows: Vec<RowModel<'_>> = images
            .iter()
            .map(|image| RowModel {
                group: 0,
                model,
                clip_k: 0.0,
                firmware: [0.0; 2],
                eligible_transform: image.transform,
                eligible_hint: image.exposure_hint,
            })
            .collect();
        Self::new_inner(images, &rows, false, options)
    }

    /// One response block and one clip per row group.
    pub fn new_with_rows(
        images: &[FitImage],
        rows: &[RowModel<'_>],
        options: &FitOptions,
    ) -> Result<Self> {
        Self::new_inner(images, rows, true, options)
    }

    fn new_inner(
        images: &[FitImage],
        rows: &[RowModel<'_>],
        per_row_clip: bool,
        options: &FitOptions,
    ) -> Result<Self> {
        if images.is_empty() {
            return Err(FitError::NoImages);
        }
        if rows.len() != images.len() {
            return Err(FitError::RowContext(format!(
                "{} row contexts for {} images",
                rows.len(),
                images.len()
            )));
        }
        let group_count = rows
            .iter()
            .map(|row| row.group)
            .max()
            .map_or(0, |max| max + 1);
        let mut group_models: Vec<Option<&ControlResponseModel>> = vec![None; group_count];
        for row in rows {
            let Some(model) = row.model else {
                continue;
            };
            match group_models[row.group] {
                Some(existing)
                    if existing.body != model.body
                        || existing.epoch != model.epoch
                        || existing.base_sim != model.base_sim
                        || existing.version != model.version =>
                {
                    return Err(FitError::RowContext(format!(
                        "row group {} mixes control-response models",
                        row.group
                    )));
                }
                Some(_) => {}
                None => group_models[row.group] = Some(model),
            }
        }
        let mut response_groups = Vec::with_capacity(group_count);
        let mut response_count = 0usize;
        for model in &group_models {
            let ids = model
                .map(ControlResponseModel::parameter_ids)
                .unwrap_or_default();
            response_count += ids.len();
            response_groups.push(ResponseGroup {
                start: response_count - ids.len(),
                ids,
            });
        }
        let parameter_ids: Vec<ResponseParamId> = response_groups
            .iter()
            .flat_map(|group| group.ids.iter().cloned())
            .collect();
        let terms: Vec<Vec<ResponseTerm>> = images
            .iter()
            .zip(rows.iter())
            .map(|(image, row)| {
                let group = &response_groups[row.group];
                row.model.map_or_else(Vec::new, |model| {
                    model
                        .active_parameters(&image.settings)
                        .into_iter()
                        .filter_map(|(param, phi)| {
                            let id = group.ids.get(param)?;
                            Some(ResponseTerm {
                                index: group.start + param,
                                phi,
                                component: id.component,
                                feature: id.feature,
                            })
                        })
                        .collect()
                })
            })
            .collect();
        let row_clip = per_row_clip.then(|| rows.iter().map(|row| row.clip_k).collect());
        let firmware: Vec<[f64; 2]> = rows.iter().map(|row| row.firmware).collect();
        let default_mode = ExposureModeId(0);
        let mut modes: Vec<ExposureModeId> = Vec::new();
        let mut contributors: Vec<ContributorId> = Vec::new();
        for image in images {
            if !modes.contains(&image.exposure_mode) {
                modes.push(image.exposure_mode);
            }
            if !contributors.contains(&image.contributor) {
                contributors.push(image.contributor);
            }
        }
        let mode_list: Vec<ExposureModeId> = modes
            .into_iter()
            .filter(|mode| *mode != default_mode)
            .collect();
        let mut entries = Vec::with_capacity(images.len());
        let mut samples = Vec::new();
        let mut excluded_samples = 0usize;
        let mut excluded_regions: Vec<DomainExclusion> = Vec::new();
        for (image_index, image) in images.iter().enumerate() {
            if image.samples.n() == 0 {
                return Err(FitError::EmptyImage { image: image_index });
            }
            let n = image.samples.n();
            if let Some(trust) = &image.sample_trust
                && trust.len() != n
            {
                return Err(FitError::SampleTrustLength {
                    image: image_index,
                    found: trust.len(),
                    expected: n,
                });
            }
            let take = n.min(options.max_samples_per_image);
            let mut order: Vec<u32> = (0..n as u32).collect();
            // Re-render groups share one pixel set, so one order keeps their rows aligned.
            match image.samples.header().rerender_group {
                Some(group) => {
                    let mut group_rng = Xorshift::new(group_seed(*group.as_bytes()));
                    shuffle(&mut order, &mut group_rng, take);
                }
                None => {
                    let mut image_rng = Xorshift::new(image_sample_seed(options.seed, image));
                    shuffle(&mut order, &mut image_rng, take);
                }
            }
            order.truncate(take);
            let start = samples.len();
            let trust = image.trust_weight.max(0.0);
            let temper = options.n_eff / take as f64;
            let in_rgb = image.samples.in_rgb();
            let out_rgb = image.samples.out_rgb();
            let density = image.samples.density_w();
            let mean_density =
                density.iter().map(|value| *value as f64).sum::<f64>() / density.len() as f64;
            let space = space_of(image.samples.header().output_space);
            let eligible_transform = rows[image_index].eligible_transform;
            let eligible_hint = rows[image_index].eligible_hint;
            for index in order {
                let index = index as usize;
                let rgb = in_rgb[index].map(|value| value as f64);
                if !eligible(eligible_transform, rgb, eligible_hint, options) {
                    let input = looklab_model::LookInput::new(looklab_color::Xyz::from_array(
                        eligible_transform.mul_vec([
                            rgb[0] * libm::pow(2.0, eligible_hint.clamp(-8.0, 8.0)),
                            rgb[1] * libm::pow(2.0, eligible_hint.clamp(-8.0, 8.0)),
                            rgb[2] * libm::pow(2.0, eligible_hint.clamp(-8.0, 8.0)),
                        ]),
                    ));
                    excluded_samples += 1;
                    let bin = exclusion_bin(input.s, input.h_in, input.c_in.sqrt());
                    match excluded_regions
                        .iter_mut()
                        .find(|entry| (entry.s_bin, entry.hue_bin, entry.chroma_bin) == bin)
                    {
                        Some(entry) => entry.samples += 1,
                        None => excluded_regions.push(DomainExclusion {
                            s_bin: bin.0,
                            hue_bin: bin.1,
                            chroma_bin: bin.2,
                            samples: 1,
                        }),
                    }
                    continue;
                }
                let target =
                    ucs_of_xyz(space.rgb_linear_to_xyz(Rgb::from_array(
                        out_rgb[index].map(|value| value as f64),
                    )));
                // Neutrality is an input property (docs/02 §4.1 stage 1), taken at the prior nuisance.
                let hint_scale = libm::pow(2.0, eligible_hint.clamp(-8.0, 8.0));
                let scene = eligible_transform.mul_vec([
                    rgb[0] * hint_scale,
                    rgb[1] * hint_scale,
                    rgb[2] * hint_scale,
                ]);
                let neutral = looklab_model::LookInput::new(looklab_color::Xyz::from_array(scene))
                    .c_in
                    < 12.0;
                samples.push(Sample {
                    rgb: in_rgb[index].map(|value| value as f64),
                    target: crate::predict::Target::new(target),
                    image: image_index as u32,
                    weight: density[index] as f64 / mean_density.max(1e-12)
                        * image
                            .sample_trust
                            .as_ref()
                            .map_or(1.0, |weights| weights[index] as f64)
                        * trust
                        * temper,
                    neutral,
                    informative: out_rgb[index]
                        .iter()
                        .all(|value| *value > 0.06 && *value < 0.94),
                });
            }
            entries.push(ImageEntry {
                transform: image.transform,
                space,
                exposure_hint: image.exposure_hint,
                trust,
                mode: mode_list
                    .iter()
                    .position(|mode| *mode == image.exposure_mode),
                contributor: contributors
                    .iter()
                    .position(|contributor| *contributor == image.contributor)
                    .unwrap_or(0),
                samples: start..samples.len(),
                intent: image.intent,
                known_exposure: rows[image_index].model.map_or(0.0, |model| {
                    model
                        .exposure
                        .steps
                        .iter()
                        .filter_map(|step| match image.settings.get(&step.key) {
                            Some(looklab_model::realization::ControlValue::Step(value)) => {
                                Some((*value - step.default) as f64 * step.ev_per_step)
                            }
                            _ => None,
                        })
                        .sum()
                }),
            });
        }
        if samples.is_empty() {
            return Err(FitError::NoSamples);
        }
        let hessian = hessian_subset(samples.len(), options.hessian_samples, options.seed);
        let mut hessian_set = vec![false; samples.len()];
        for index in &hessian {
            hessian_set[*index as usize] = true;
        }
        let active_images = entries
            .iter()
            .filter(|entry| {
                samples[entry.samples.clone()]
                    .iter()
                    .any(|sample| sample.weight > 0.0)
            })
            .count()
            .max(1);
        let mut layout = Layout::with_response(
            entries.len(),
            mode_list.len(),
            contributors.len(),
            parameter_ids.len(),
        );
        layout.identity_scale = options.identity_scale;
        let settings = images.iter().map(|image| image.settings.clone()).collect();
        let mut defaults: Vec<(looklab_schema::ControlKey, ControlValue)> = Vec::new();
        for model in group_models.iter().flatten() {
            for (key, value) in model
                .controls
                .iter()
                .map(|control| (&control.key, &control.default))
                .chain(
                    model
                        .wb
                        .controls
                        .iter()
                        .map(|control| (&control.key, &control.default)),
                )
            {
                if !defaults.iter().any(|(known, _)| known == key) {
                    defaults.push((key.clone(), value.clone()));
                }
            }
        }
        Ok(Self {
            layout,
            images: entries,
            modes: mode_list,
            contributors,
            samples,
            hessian,
            hessian_set,
            nu: options.nu,
            active_images,
            parameter_ids,
            response_groups,
            row_clip,
            firmware,
            terms,
            settings,
            defaults,
            excluded_regions,
            excluded_samples,
        })
    }

    pub fn look_and_slope(&self, params: &Params) -> (Look, [f64; TONE_COEFS]) {
        (params.look(), self.tone_slope(params))
    }

    pub(crate) fn realized_look(&self, params: &Params, index: usize) -> Result<Look> {
        let base = params.look();
        Ok(self.effective_look(params, &base, index)?.into_owned())
    }

    fn tone_slope(&self, params: &Params) -> [f64; TONE_COEFS] {
        let mut slope = [0.0_f64; TONE_COEFS];
        for (k, value) in slope.iter_mut().enumerate() {
            *value = softplus_slope(params.theta[self.layout.tone_beta.start + k]);
        }
        slope
    }

    fn effective_look<'a>(
        &self,
        params: &Params,
        base: &'a Look,
        index: usize,
    ) -> Result<Cow<'a, Look>> {
        let base = if self.firmware[index] == [0.0; 2] {
            Cow::Borrowed(base)
        } else {
            Cow::Owned(crate::hierarchical::firmware_look(
                base,
                self.firmware[index],
            ))
        };
        let terms = &self.terms[index];
        if terms.is_empty() {
            return Ok(base);
        }
        let mut delta = Delta::default();
        for term in terms {
            if let Some(layer) = term.component.layer() {
                let value = params.theta[self.layout.response.start + term.index];
                delta.add(layer, term.feature, value * term.phi);
            }
        }
        Ok(Cow::Owned(delta.apply(base.as_ref())?))
    }

    pub fn image_ctx(&self, params: &Params, index: usize) -> ImageCtx {
        let image = &self.images[index];
        let nuisance = params.image(index);
        let mut log_r = nuisance[1];
        let mut log_b = nuisance[2];
        for term in &self.terms[index] {
            let value = params.theta[self.layout.response.start + term.index] * term.phi;
            match term.component {
                ResponseComponent::WbR => log_r += value,
                ResponseComponent::WbB => log_b += value,
                _ => {}
            }
        }
        let tone_clamp = if self.terms[index]
            .iter()
            .any(|term| term.component == ResponseComponent::Tone && term.feature > 0)
        {
            response_tone_clamp(
                core::array::from_fn(|k| softplus(params.theta[self.layout.tone_beta.start + k])),
                &self.terms[index],
                &params.theta[self.layout.response.clone()],
            )
        } else {
            [ToneClamp::Interior; TONE_COEFS]
        };
        let exposure = nuisance[0]
            + image.mode.map_or(0.0, |mode| params.mode_offset(mode))
            + image.known_exposure;
        ImageCtx {
            transform: image.transform,
            a0: image.transform,
            space: image.space,
            exposure_scale: libm::pow(2.0, exposure),
            wb_r: libm::exp(log_r),
            wb_b: libm::exp(log_b),
            tone_clamp,
        }
    }

    pub fn column_map(&self, params: &Params, index: usize, active: ActiveColumns) -> ColumnMap {
        let range = params.layout.image(index);
        ColumnMap {
            image: active.image.then_some(range.start),
            mode: match (active.mode, self.images[index].mode) {
                (true, Some(mode)) => Some(params.layout.mode_of(mode)),
                _ => None,
            },
            wb_r: active.wb.then_some(range.start + 1),
            wb_b: active.wb.then_some(range.start + 2),
            clip: active.clip.then_some(params.layout.clip),
            // Body-level columns do not exist in the single-body layout.
            exposure_extra: None,
            wb_r_extra: None,
            wb_b_extra: None,
            clip_extra: None,
            delta: None,
            second: false,
            curvature: false,
            clip_geometry: crate::predict::ClipGeometry::Logistic,
        }
    }

    /// Student-t log-likelihood increase and IRLS weight for one residual.
    fn irls(residual_squared: f64, nu_sigma: f64, nu_plus_three: f64) -> (f64, f64) {
        let ratio = residual_squared / nu_sigma;
        let weight = nu_plus_three / (nu_sigma + residual_squared);
        (0.5 * nu_plus_three * libm::log1p(ratio), weight)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn pass(
        &self,
        params: &Params,
        active: ActiveColumns,
        blocks: &mut [BlockAcc],
        want_residuals: bool,
        want_per_image: bool,
        neutral_only: bool,
        intents: IntentFilter,
    ) -> PassOutput {
        self.pass_with_rows(
            params,
            active,
            blocks,
            want_residuals,
            want_per_image,
            neutral_only,
            intents,
            false,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn pass_with_rows(
        &self,
        params: &Params,
        active: ActiveColumns,
        blocks: &mut [BlockAcc],
        want_residuals: bool,
        want_per_image: bool,
        neutral_only: bool,
        intents: IntentFilter,
        retain_rows: bool,
        all_rows: bool,
    ) -> PassOutput {
        self.pass_inner(
            params,
            active,
            blocks,
            want_residuals,
            want_per_image,
            neutral_only,
            intents,
            retain_rows,
            all_rows,
            CurvatureMode::GaussNewton,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn pass_with_analytic_rows(
        &self,
        params: &Params,
        active: ActiveColumns,
        blocks: &mut [BlockAcc],
        want_residuals: bool,
        want_per_image: bool,
        neutral_only: bool,
        intents: IntentFilter,
        rows: HessianRows,
    ) -> PassOutput {
        self.pass_inner(
            params,
            active,
            blocks,
            want_residuals,
            want_per_image,
            neutral_only,
            intents,
            true,
            matches!(rows, HessianRows::All),
            CurvatureMode::Analytic,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn pass_inner(
        &self,
        params: &Params,
        active: ActiveColumns,
        blocks: &mut [BlockAcc],
        want_residuals: bool,
        want_per_image: bool,
        neutral_only: bool,
        intents: IntentFilter,
        retain_rows: bool,
        all_rows: bool,
        curvature: CurvatureMode,
    ) -> PassOutput {
        let base = params.look();
        let slope = self.tone_slope(params);
        let looks: Result<Vec<Cow<'_, Look>>> = (0..self.images.len())
            .map(|index| self.effective_look(params, &base, index))
            .collect();
        let clip_slope = params.clip_slope();
        let total = self.layout.total;
        let mut retained_columns = vec![false; if retain_rows { total } else { 0 }];
        if retain_rows {
            for block in blocks
                .iter()
                .filter(|block| block.kind != BlockKind::Residual)
            {
                for &global in &block.params {
                    retained_columns[global] = true;
                }
            }
        }
        let wants_residual_block = blocks.iter().any(|block| block.kind == BlockKind::Residual);
        let needs_jacobian = !blocks.is_empty() || want_per_image;
        let mut output = PassOutput {
            cost: 0.0,
            rss: 0.0,
            hessian_rss: 0.0,
            contributor_rss: vec![0.0_f64; self.contributors.len()],
            contributor_weight: vec![0.0_f64; self.contributors.len()],
            contributor_pairs: vec![Vec::new(); self.contributors.len()],
            residuals: vec![
                [0.0_f64; 3];
                if want_residuals {
                    self.samples.len()
                } else {
                    0
                }
            ],
            image_rss: vec![0.0_f64; self.images.len()],
            image_weight: vec![0.0_f64; self.images.len()],
            image_cost: vec![0.0_f64; self.images.len()],
            per_image_gradient: if want_per_image {
                vec![vec![0.0_f64; total]; self.images.len()]
            } else {
                Vec::new()
            },
            residual_samples: if wants_residual_block {
                Vec::with_capacity(self.hessian.len())
            } else {
                Vec::new()
            },
            residual_gradient: if wants_residual_block {
                vec![0.0_f64; RESIDUAL_COEFS]
            } else {
                Vec::new()
            },
            coupled_rows: if retain_rows {
                Vec::with_capacity(self.hessian.len())
            } else {
                Vec::new()
            },
            active_samples: 0,
            hessian_rows: 0,
            infeasible: false,
        };
        let looks = match looks {
            Ok(looks) => looks,
            Err(_) => {
                output.infeasible = true;
                output.cost = f64::INFINITY;
                return output;
            }
        };
        let mut jac = Jac::default();
        let contexts: Vec<(ImageCtx, ColumnMap)> = self
            .images
            .iter()
            .enumerate()
            .map(|(index, _)| {
                let columns = self.column_map(params, index, active);
                (self.image_ctx(params, index), columns)
            })
            .collect();
        let nu = self.nu;
        let nu_plus_three = nu + 3.0;
        let likelihood: Vec<LikelihoodScale> = (0..self.contributors.len())
            .map(|index| {
                let variance = params.sigma(index) * params.sigma(index);
                LikelihoodScale {
                    nu_variance: nu * variance,
                    log_scale: 1.5 * libm::log(variance),
                }
            })
            .collect();
        let hessian_set = &self.hessian_set;
        let mut block_buffers: Vec<HessianBuffer> =
            blocks.iter().map(|_| HessianBuffer::default()).collect();
        let mut column_terms: Vec<f64> = Vec::new();
        let mut column_norms: Vec<f64> = Vec::new();

        for (index, sample) in self.samples.iter().enumerate() {
            if neutral_only && !sample.neutral {
                continue;
            }
            let image = sample.image as usize;
            let clip_k = self
                .row_clip
                .as_ref()
                .map_or(params.clip_k(), |clips| clips[image]);
            let image_entry = &self.images[image];
            if !intents.includes(image_entry.intent) {
                continue;
            }
            let is_hessian = all_rows || hessian_set[index];
            let (context, template_columns) = &contexts[image];
            let mut columns = *template_columns;
            columns.second = false;
            columns.curvature =
                retain_rows && is_hessian && matches!(curvature, CurvatureMode::Analytic);
            let look: &Look = &looks[image];
            let terms: &[ResponseTerm] = if active.response {
                &self.terms[image]
            } else {
                &[]
            };
            let (prediction, fitting, penalty, mut evaluated) = if needs_jacobian {
                let evaluated = evaluate_sample(
                    look,
                    &self.layout,
                    context,
                    &columns,
                    sample.rgb,
                    clip_k,
                    clip_slope,
                    &slope,
                    terms,
                    &sample.target,
                    &mut jac,
                );
                (
                    evaluated.prediction,
                    evaluated.residual,
                    evaluated.penalty,
                    Some(evaluated),
                )
            } else {
                let (prediction, fitting, penalty) =
                    crate::predict::predict_only(look, context, sample.rgb, clip_k, &sample.target);
                (prediction, fitting, penalty, None)
            };
            let feasible = penalty.as_ref().is_some_and(|penalty| {
                penalty.value.is_finite()
                    && penalty.gradient.iter().all(|value| value.is_finite())
                    && penalty
                        .hessian
                        .iter()
                        .flatten()
                        .all(|value| value.is_finite())
                    && penalty
                        .gn_hessian
                        .iter()
                        .flatten()
                        .all(|value| value.is_finite())
            });
            let mut usable = feasible;
            if !feasible {
                output.infeasible = true;
            }
            let penalty_value = penalty.as_ref().map_or(0.0, |penalty| penalty.value);
            let penalty_gradient = penalty
                .as_ref()
                .map_or([0.0_f64; DOMAIN_AXES], |penalty| penalty.gradient);
            let penalty_gn = penalty
                .as_ref()
                .map_or([[0.0_f64; DOMAIN_AXES]; DOMAIN_AXES], |penalty| {
                    penalty.gn_hessian
                });
            let crossing = evaluated
                .as_ref()
                .is_some_and(|evaluated| evaluated.crossing);
            let axis_terms = evaluated.as_mut().map_or_else(Vec::new, |evaluated| {
                core::mem::take(&mut evaluated.axis_terms)
            });
            let target = &sample.target.ucs;
            let residual = [
                prediction.j - target.j,
                prediction.a - target.a,
                prediction.b - target.b,
            ];
            let squared =
                fitting[0] * fitting[0] + fitting[1] * fitting[1] + fitting[2] * fitting[2];
            let contributor = image_entry.contributor;
            let scale = &likelihood[contributor];
            let (log_likelihood, alpha) = Self::irls(squared, scale.nu_variance, nu_plus_three);
            let weight = sample.weight;
            output.active_samples += 1;
            if is_hessian {
                output.hessian_rows += 1;
            }
            output.cost += weight * (log_likelihood + scale.log_scale);
            output.image_cost[image] += weight * (log_likelihood + scale.log_scale);
            if feasible {
                output.cost += weight * penalty_value;
                output.image_cost[image] += weight * penalty_value;
            } else {
                output.cost = f64::INFINITY;
                output.image_cost[image] = f64::INFINITY;
            }
            output.image_rss[image] += weight * squared;
            output.image_weight[image] += weight;
            output.contributor_rss[contributor] += weight * squared;
            output.contributor_weight[contributor] += weight;
            output.contributor_pairs[contributor].push((weight, squared));
            if want_residuals {
                output.residuals[index] = residual;
            }
            let contribution = weight * alpha;
            let q = scale.nu_variance + squared;
            let robust = if !crossing && q > 0.0 && q.is_finite() {
                weight * (-2.0 * nu_plus_three / (q * q))
            } else {
                0.0
            };
            let penalty_gradient: [f64; DOMAIN_AXES] = penalty_gradient.map(|value| weight * value);
            let penalty_gn: [[f64; DOMAIN_AXES]; DOMAIN_AXES] =
                penalty_gn.map(|row| row.map(|value| weight * value));
            output.rss += contribution * squared;
            if is_hessian {
                output.hessian_rss += contribution * squared;
            }
            if let Some(evaluated) = &mut evaluated {
                column_terms.clear();
                column_norms.clear();
                for derivative in jac.derivs.iter() {
                    column_terms.push(
                        derivative[0] * fitting[0]
                            + derivative[1] * fitting[1]
                            + derivative[2] * fitting[2],
                    );
                    column_norms.push(
                        derivative[0] * derivative[0]
                            + derivative[1] * derivative[1]
                            + derivative[2] * derivative[2],
                    );
                }
                // A finite penalty does not imply finite derivatives: the CAM16 inverse
                // and the clip chain can still fail at a contracted or extreme point.
                usable = usable
                    && column_terms.iter().all(|value| value.is_finite())
                    && column_norms.iter().all(|value| value.is_finite())
                    && axis_terms
                        .iter()
                        .all(|axis| axis.iter().all(|value| value.is_finite()))
                    && jac
                        .derivs
                        .iter()
                        .all(|derivative| derivative.iter().all(|value| value.is_finite()))
                    && evaluated.second.as_ref().is_none_or(|order| {
                        order
                            .prediction
                            .iter()
                            .all(|values| values.iter().all(|value| value.is_finite()))
                    });
                if !usable {
                    output.infeasible = true;
                    output.cost = f64::INFINITY;
                }
                if usable && retain_rows && is_hessian {
                    let analytic = matches!(curvature, CurvatureMode::Analytic);
                    let curvature = if analytic {
                        let contracted = evaluated.curvature.take().map(|prediction| {
                            prediction.contract(fitting.map(|value| contribution * value), weight)
                        });
                        match contracted {
                            Some(Ok(curvature)) => Some(curvature),
                            _ => {
                                output.infeasible = true;
                                output.cost = f64::INFINITY;
                                continue;
                            }
                        }
                    } else {
                        None
                    };
                    let mut columns = Vec::with_capacity(jac.cols.len());
                    let mut robust_terms = Vec::with_capacity(jac.cols.len());
                    let mut retained_axes = Vec::with_capacity(jac.cols.len());
                    let mut layers = Vec::with_capacity(jac.cols.len());
                    let mut layer_weights = Vec::with_capacity(jac.cols.len());
                    let mut layer_slopes = Vec::with_capacity(jac.cols.len());
                    let mut residual_columns = Vec::new();
                    for (index, (column, derivative)) in
                        jac.cols.iter().zip(&jac.derivs).enumerate()
                    {
                        if self.layout.residual.contains(&(*column as usize)) {
                            if wants_residual_block {
                                residual_columns.push((
                                    *column,
                                    *derivative,
                                    column_terms[index],
                                    axis_terms[index],
                                ));
                            }
                            continue;
                        }
                        if !retained_columns[*column as usize] {
                            continue;
                        }
                        columns.push((*column, *derivative));
                        robust_terms.push(column_terms[index]);
                        retained_axes.push(axis_terms[index]);
                        layers.push(jac.layers[index]);
                        layer_weights.push(jac.layer_weights[index]);
                        layer_slopes.push(jac.layer_slopes[index]);
                    }
                    let mut second = Box::new([0.0_f64; 81]);
                    let mut motion = [0.0_f64; 3];
                    let mut tint = [0.0_f64; 4];
                    if let Some(order) = &evaluated.second {
                        for (index, value) in second.iter_mut().enumerate() {
                            let mut data = 0.0;
                            for (channel, residual) in fitting.iter().enumerate() {
                                data += residual * order.prediction[channel][index];
                            }
                            *value = contribution * data;
                        }
                        for (slot, value) in motion.iter_mut().enumerate() {
                            let mut data = 0.0;
                            for (channel, residual) in fitting.iter().enumerate() {
                                data += residual * order.gradient[slot][channel];
                            }
                            *value = contribution * data;
                        }
                        tint = [
                            order.tint_curvature[0],
                            order.tint_curvature[1],
                            order.tint_slope[0],
                            order.tint_slope[1],
                        ];
                    }
                    let row_penalty = if analytic {
                        [[0.0_f64; DOMAIN_AXES]; DOMAIN_AXES]
                    } else {
                        penalty_gn
                    };
                    output.coupled_rows.push(HessianRow {
                        columns,
                        weight: contribution,
                        robust_terms,
                        robust,
                        axis_terms: retained_axes,
                        penalty: row_penalty,
                        penalty_gn,
                        curvature,
                        residual_columns,
                        layers,
                        layer_weights,
                        layer_slopes,
                        motion,
                        tint,
                        second,
                    });
                }
                if usable && want_per_image {
                    for (column, term) in jac.cols.iter().zip(column_terms.iter()) {
                        output.per_image_gradient[image][*column as usize] += contribution * term;
                    }
                }
                if is_hessian {
                    for buffer in &mut block_buffers {
                        buffer.columns.clear();
                    }
                }
                for (slot, block) in blocks.iter_mut().enumerate() {
                    if !usable || block.image.is_some_and(|other| other != image) {
                        continue;
                    }
                    let buffer = &mut block_buffers[slot];
                    for (column_index, (column, derivative)) in
                        jac.cols.iter().zip(jac.derivs.iter()).enumerate()
                    {
                        let local = block.local[*column as usize];
                        if local == u32::MAX {
                            continue;
                        }
                        let axis = axis_terms[column_index];
                        let mut gradient = 0.0;
                        let mut diagonal = 0.0;
                        for (a, left) in axis.iter().enumerate() {
                            if *left == 0.0 {
                                continue;
                            }
                            gradient += penalty_gradient[a] * left;
                            for (b, right) in axis.iter().enumerate() {
                                diagonal += penalty_gn[a][b] * left * right;
                            }
                        }
                        block.gradient[local as usize] +=
                            contribution * column_terms[column_index] + gradient;
                        block.full_diagonal[local as usize] +=
                            contribution * column_norms[column_index] + diagonal;
                        if is_hessian {
                            buffer.columns.push((
                                local,
                                *derivative,
                                column_terms[column_index],
                                axis,
                            ));
                        }
                    }
                }
            }

            if wants_residual_block && usable {
                let evaluated = evaluated
                    .as_ref()
                    .expect("every active sample carries the Jacobian");
                let mut clip_residual = [0.0_f64; 3];
                for (channel, value) in clip_residual.iter_mut().enumerate() {
                    let mut gradient = 0.0;
                    for row in 0..3 {
                        gradient += evaluated.clip_jacobian.0[row][channel] * fitting[row];
                    }
                    *value = gradient;
                }
                let residual_penalty = [
                    penalty_gradient[1],
                    penalty_gradient[2] + penalty_gradient[4],
                    penalty_gradient[3] + penalty_gradient[5],
                ];
                for (m, node) in evaluated.residual_indices.iter().enumerate() {
                    let weight = evaluated.residual_weights[m];
                    for channel in 0..3 {
                        output.residual_gradient[*node * 3 + channel] -=
                            contribution * weight * clip_residual[channel]
                                + weight * residual_penalty[channel];
                    }
                }
            }

            if !is_hessian {
                continue;
            }
            for (slot, block) in blocks.iter_mut().enumerate() {
                if !usable || block.image.is_some_and(|other| other != image) {
                    continue;
                }
                let n = block.len();
                let entry = &mut block_buffers[slot];
                if entry.columns.is_empty() {
                    continue;
                }
                if entry.mirror.resolve(&block.hessian, n) {
                    accumulate_block::<true>(
                        &mut block.hessian,
                        &mut block.gn_hessian,
                        &mut block.hessian_gradient,
                        n,
                        &entry.columns,
                        contribution,
                        fitting,
                        robust,
                        &penalty_gn,
                    );
                } else {
                    accumulate_block::<false>(
                        &mut block.hessian,
                        &mut block.gn_hessian,
                        &mut block.hessian_gradient,
                        n,
                        &entry.columns,
                        contribution,
                        fitting,
                        robust,
                        &penalty_gn,
                    );
                }
            }
            if wants_residual_block && usable {
                let evaluated = evaluated
                    .as_ref()
                    .expect("Hessian samples carry the Jacobian");
                let analytic = matches!(curvature, CurvatureMode::Analytic);
                let chosen = if analytic {
                    [[0.0_f64; DOMAIN_AXES]; DOMAIN_AXES]
                } else {
                    penalty_gn
                };
                output.residual_samples.push(ResidualSample {
                    nodes: evaluated.residual_indices.map(|index| index as u32),
                    weights: evaluated.residual_weights,
                    clip: evaluated.clip_jacobian,
                    alpha: contribution,
                    residual: fitting,
                    penalty_gradient: [
                        penalty_gradient[1],
                        penalty_gradient[2] + penalty_gradient[4],
                        penalty_gradient[3] + penalty_gradient[5],
                    ],
                    penalty_curvature: residual_penalty_matrix(&chosen),
                    penalty_gn: residual_penalty_matrix(&penalty_gn),
                });
            }
        }
        for (slot, block) in blocks.iter_mut().enumerate() {
            if block_buffers[slot].mirror == MirrorState::Canonical {
                let n = block.len();
                mirror_triangle(&mut block.hessian, n);
                mirror_triangle(&mut block.gn_hessian, n);
            }
        }
        output
    }
}

fn shuffle(order: &mut [u32], rng: &mut Xorshift, take: usize) {
    for i in 0..take {
        let j = i + rng.below(order.len() - i);
        order.swap(i, j);
    }
}

fn group_seed(group: [u8; 16]) -> u64 {
    let mut state = 0xcbf2_9ce4_8422_2325_u64;
    for byte in group {
        state ^= u64::from(byte);
        state = state.wrapping_mul(0x0000_0100_0000_01b3);
    }
    state
}

/// Retained-row order seed that depends only on the image's stable sample identity.
fn image_sample_seed(seed: u64, image: &FitImage) -> u64 {
    let header = image.samples.header();
    let mut state = seed ^ 0x5bf0_3635_1c9a_2e11;
    for byte in header.prep_params_hash.as_bytes() {
        state ^= u64::from(*byte);
        state = state.wrapping_mul(0x0000_0100_0000_01b3);
    }
    if let Some(group) = header.rerender_group {
        state ^= group_seed(*group.as_bytes());
        state = state.wrapping_mul(0x0000_0100_0000_01b3);
    }
    state ^ u64::from(header.n)
}

/// Uniform draw of at most `cap` sample indices.
pub(crate) fn hessian_subset(n: usize, cap: usize, seed: u64) -> Vec<u32> {
    let take = n.min(cap.max(1));
    let mut order: Vec<u32> = (0..n as u32).collect();
    let mut rng = Xorshift::new(seed ^ 0xa076_1d64_78bd_642f);
    for i in 0..take {
        let j = i + rng.below(n - i);
        order.swap(i, j);
    }
    order.truncate(take);
    order
}

pub(crate) struct Xorshift {
    state: u64,
}

impl Xorshift {
    pub fn new(seed: u64) -> Self {
        Self { state: seed | 1 }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut state = self.state;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.state = state;
        state
    }

    /// Uniform in `0..bound`.
    pub fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() % bound as u64) as usize
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(all(test, feature = "synth"))]
mod tests {
    use super::*;
    use crate::synth::{SynthConfig, dataset};
    use looklab_color::Xyz;
    use looklab_color::ucs::{Ucs, UcsSpace, ucs_to_jmh};
    use looklab_model::LookInput;

    /// Every fixture contribution is smaller than this value's ULP.
    const ABSORBING: f64 = 1e300;

    fn mixed_block(problem: &Problem) -> BlockAcc {
        let mut params: Vec<usize> = problem.layout.tone_beta.clone().rev().collect();
        params.push(problem.layout.tone_c0);
        params.push(problem.layout.tint_a.start);
        BlockAcc::new(BlockKind::Tone, params, problem.layout.total)
    }

    fn active_margin(pre: Ucs) -> (f64, Option<usize>) {
        let model = looklab_model::solve::reference_model();
        let j_limit = model.inverse_j_prime_limit();
        let mut margin = (j_limit - pre.j) / j_limit;
        let mut channel = None;
        let jmh = ucs_to_jmh(pre, UcsSpace::Ucs);
        let responses = model.inverse_responses(jmh);
        for (index, response) in responses.iter().enumerate() {
            let signed = *response - 0.1;
            let distance = (looklab_color::cam16::Cam16Model::RESPONSE_LIMIT - signed.abs())
                / looklab_color::cam16::Cam16Model::RESPONSE_LIMIT;
            if distance < margin {
                margin = distance;
                channel = Some(index);
            }
        }
        (margin, channel)
    }

    fn sample_pre(problem: &Problem, at: &Params, sample: &Sample) -> Ucs {
        let image = sample.image as usize;
        let context = problem.image_ctx(at, image);
        let cam = [
            sample.rgb[0] * context.wb_r * context.exposure_scale,
            sample.rgb[1] * context.exposure_scale,
            sample.rgb[2] * context.wb_b * context.exposure_scale,
        ];
        let xyz = Xyz::from_array(context.transform.mul_vec(cam));
        let input = LookInput::new(xyz);
        let look = at.look();
        let values = look.layer_values(&input);
        look.chroma_paths(&input, &values).ucs
    }

    /// Independent FD of the residual RHS; returns `(worst smooth gap, checked, skipped)`.
    fn residual_fd_report(problem: &Problem, params: &Params, label: &str) -> (f64, usize, usize) {
        let layout = &problem.layout;
        let mut blocks = vec![BlockAcc::new(
            BlockKind::Residual,
            layout.residual.clone().collect(),
            layout.total,
        )];
        let pass = problem.pass(
            params,
            ActiveColumns::ALL,
            &mut blocks,
            false,
            false,
            false,
            IntentFilter::NativeOnly,
        );
        assert!(!pass.infeasible, "{label}: fixture left the feasible set");
        let cost = |at: &Params| {
            problem
                .pass(
                    at,
                    ActiveColumns::ALL,
                    &mut [],
                    false,
                    false,
                    false,
                    IntentFilter::NativeOnly,
                )
                .cost
        };
        let base = cost(params);
        let scale = base.abs().max(pass.cost.abs()).max(1.0);
        eprintln!(
            "{label}: value cost {base:.12e} jacobian cost {:.12e} delta {:.3e}",
            pass.cost,
            base - pass.cost
        );
        assert!(
            (base - pass.cost).abs() <= 64.0 * f64::EPSILON * scale.max(1.0),
            "{label}: value and Jacobian objective modes disagree"
        );
        let step = 1e-6;
        let mut barrier = vec![0.0_f64; layout.residual.len()];
        for sample in &pass.residual_samples {
            for (m, node) in sample.nodes.iter().enumerate() {
                let weight = sample.weights[m];
                for channel in 0..3 {
                    barrier[*node as usize * 3 + channel] +=
                        weight * sample.penalty_gradient[channel];
                }
            }
        }
        let mut checked = 0usize;
        let mut skipped = 0usize;
        let mut tiny = 0usize;
        let mut worst_smooth = 0.0_f64;
        let mut smooth_barrier = 0.0_f64;
        for index in 0..layout.residual.len() {
            let global = layout.residual.start + index;
            let mut plus = params.clone();
            let mut minus = params.clone();
            plus.theta[global] += step;
            minus.theta[global] -= step;
            let forward = cost(&plus);
            let backward = cost(&minus);
            assert!(
                forward.is_finite() && backward.is_finite(),
                "{label}: residual coefficient {index} perturbed cost left the domain"
            );
            let left = (base - backward) / step;
            let right = (forward - base) / step;
            let central = 0.5 * (left + right);
            let analytic = -pass.residual_gradient[index];
            assert!(
                central.is_finite() && analytic.is_finite(),
                "{label}: residual coefficient {index} non-finite derivative {analytic} vs {central}"
            );
            let scale = central.abs().max(analytic.abs());
            if scale <= 1e-6 {
                tiny += 1;
            }
            let roundoff = f64::EPSILON * base.abs().max(forward.abs()).max(backward.abs()) / step;
            let allowed = 32.0 * roundoff + 16.0 * f64::EPSILON * analytic.abs();
            let excess = (central - analytic).abs() - allowed;
            if (left - right).abs() > 1e-2 * scale.max(1.0) {
                skipped += 1;
                if skipped <= 8 {
                    eprintln!(
                        "{label}: residual coefficient {index} nonsmooth left {left:.6e} right {right:.6e}"
                    );
                }
                continue;
            }
            checked += 1;
            smooth_barrier = smooth_barrier.max(barrier[index].abs());
            worst_smooth = worst_smooth.max(excess);
        }
        eprintln!("{label}: checked {checked} skipped {skipped} tiny {tiny}");
        assert!(checked > 0, "{label}: no smooth residual coefficient");
        assert!(
            smooth_barrier > 1e-6,
            "{label}: no smooth coefficient carries the barrier term: {smooth_barrier:.3e}"
        );
        let (barrier_index, barrier_peak) =
            barrier
                .iter()
                .enumerate()
                .fold((0usize, 0.0_f64), |best, (index, value)| {
                    if value.abs() > best.1 {
                        (index, value.abs())
                    } else {
                        best
                    }
                });
        if barrier_peak > 0.0 {
            let global = layout.residual.start + barrier_index;
            let mut plus = params.clone();
            let mut minus = params.clone();
            plus.theta[global] += step;
            minus.theta[global] -= step;
            let forward = cost(&plus);
            let backward = cost(&minus);
            let central = (forward - backward) / (2.0 * step);
            let analytic = -pass.residual_gradient[barrier_index];
            let step2 = 0.5 * step;
            let mut plus2 = params.clone();
            let mut minus2 = params.clone();
            plus2.theta[global] += step2;
            minus2.theta[global] -= step2;
            let forward2 = cost(&plus2);
            let backward2 = cost(&minus2);
            let central2 = (forward2 - backward2) / (2.0 * step2);
            let truncation = (central - central2) / 3.0;
            let roundoff = f64::EPSILON
                * base
                    .abs()
                    .max(forward.abs())
                    .max(backward.abs())
                    .max(forward2.abs())
                    .max(backward2.abs())
                / step;
            let allowed =
                32.0 * roundoff + 16.0 * f64::EPSILON * analytic.abs() + 4.0 * truncation.abs();
            eprintln!(
                "{label}: high-signal barrier coefficient {barrier_index} peak {barrier_peak:.6e} central {central:.9e} analytic {analytic:.9e} truncation {truncation:.3e} allowed {allowed:.3e}"
            );
            assert!(
                (central - analytic).abs() <= allowed,
                "{label}: high-signal barrier coefficient exceeds the FD budget"
            );
            assert!(
                barrier_peak > 8.0 * allowed,
                "{label}: barrier term {barrier_peak:.3e} is below the FD budget {allowed:.3e}"
            );
        }
        (worst_smooth, checked, skipped)
    }

    #[test]
    fn eligibility_is_fixed_once_and_reported() {
        let data = dataset(&SynthConfig {
            images: 3,
            samples_per_image: 24,
            ..SynthConfig::default()
        });
        let options = FitOptions::default();
        let first = Problem::new_with(&data.images, None, &options).expect("problem");
        let second = Problem::new_with(&data.images, None, &options).expect("problem");
        let take: usize = data
            .images
            .iter()
            .map(|image| image.samples.n().min(options.max_samples_per_image))
            .sum();
        assert_eq!(first.samples.len(), second.samples.len());
        assert_eq!(first.excluded_samples, second.excluded_samples);
        assert_eq!(first.excluded_regions, second.excluded_regions);
        assert_eq!(
            first.samples.len() + first.excluded_samples,
            take,
            "eligibility did not partition the sampled rows exactly once"
        );
    }

    #[test]
    fn objective_gradient_matches_finite_differences() {
        let data = dataset(&SynthConfig {
            images: 2,
            samples_per_image: 24,
            ..SynthConfig::default()
        });
        let options = FitOptions {
            hessian_samples: usize::MAX,
            ..FitOptions::default()
        };
        let problem = Problem::new_with(&data.images, None, &options).expect("problem");
        let layout = problem.layout.clone();
        let mut params = Params::initial(layout.clone(), problem.contributors.len());
        params.theta[layout.image(0).start] = 0.2;
        let globals = [
            layout.tone_c0,
            layout.tone_beta.start + 3,
            layout.image(0).start,
            layout.image(0).start + 1,
            layout.clip,
        ];
        let mut blocks = vec![BlockAcc::new(
            BlockKind::Tone,
            globals.to_vec(),
            layout.total,
        )];
        let _ = problem.pass(
            &params,
            ActiveColumns::ALL,
            &mut blocks,
            false,
            false,
            false,
            IntentFilter::NativeOnly,
        );
        let gradient = blocks[0].gradient.clone();
        for (local, global) in globals.iter().enumerate() {
            let h = 1e-5 * params.theta[*global].abs().max(1.0);
            let mut plus = params.clone();
            plus.theta[*global] += h;
            let mut minus = params.clone();
            minus.theta[*global] -= h;
            let forward = problem.pass(
                &plus,
                ActiveColumns::ALL,
                &mut [],
                false,
                false,
                false,
                IntentFilter::NativeOnly,
            );
            let backward = problem.pass(
                &minus,
                ActiveColumns::ALL,
                &mut [],
                false,
                false,
                false,
                IntentFilter::NativeOnly,
            );
            let numeric = (forward.cost - backward.cost) / (2.0 * h);
            let analytic = gradient[local];
            assert!(
                (numeric - analytic).abs() <= 1e-4 * analytic.abs().max(1.0),
                "parameter {global}: analytic {analytic:.9e} numeric {numeric:.9e}"
            );
        }
    }

    #[test]
    fn prefilled_hessians_keep_their_triangles_and_mirror_the_rest() {
        let data = dataset(&SynthConfig {
            images: 1,
            samples_per_image: 32,
            ..SynthConfig::default()
        });
        let options = FitOptions::default();
        let problem = Problem::new_with(&data.images, None, &options).expect("problem");
        let params = Params::initial(problem.layout.clone(), problem.contributors.len());
        let image = problem.layout.image(0);
        let mut untouched = BlockAcc::new(
            BlockKind::Nuisance,
            vec![image.start, image.start + 1],
            problem.layout.total,
        );
        untouched.image = Some(7);
        untouched.hessian[1] = 3.0;
        untouched.hessian[2] = -4.0;
        let mut blocks = vec![mixed_block(&problem), untouched];
        let n = blocks[0].len();
        let _ = problem.pass(
            &params,
            ActiveColumns::ALL,
            &mut blocks,
            false,
            false,
            false,
            IntentFilter::NativeOnly,
        );
        let canonical = blocks[0].hessian.clone();
        let mut probe = None;
        for row in 0..n {
            for column in row + 1..n {
                assert_eq!(
                    canonical[row * n + column].to_bits(),
                    canonical[column * n + row].to_bits(),
                    "entries ({row}, {column}) and ({column}, {row}) differ"
                );
                if probe.is_none() && canonical[row * n + column] != 0.0 {
                    probe = Some((row, column));
                }
            }
        }
        let (row, column) = probe.expect("no off-diagonal entry carries a contribution");
        assert_eq!(blocks[1].hessian[1].to_bits(), 3.0_f64.to_bits());
        assert_eq!(blocks[1].hessian[2].to_bits(), (-4.0_f64).to_bits());

        blocks[0].reset();
        blocks[0].hessian[row * n + column] = ABSORBING;
        let _ = problem.pass(
            &params,
            ActiveColumns::ALL,
            &mut blocks,
            false,
            false,
            false,
            IntentFilter::NativeOnly,
        );
        assert_eq!(
            blocks[0].hessian[row * n + column].to_bits(),
            ABSORBING.to_bits()
        );
        assert_eq!(
            blocks[0].hessian[column * n + row].to_bits(),
            canonical[column * n + row].to_bits()
        );
    }

    #[test]
    fn residual_grid_rhs_matches_the_objective_derivative() {
        let data = dataset(&SynthConfig {
            images: 2,
            samples_per_image: 24,
            ..SynthConfig::default()
        });
        let options = FitOptions {
            hessian_samples: usize::MAX,
            ..FitOptions::default()
        };
        let problem = Problem::new_with(&data.images, None, &options).expect("problem");
        let layout = problem.layout.clone();
        let mut params = Params::initial(layout.clone(), problem.contributors.len());
        params.theta[layout.image(0).start] = 0.2;
        let mut blocks = vec![BlockAcc::new(
            BlockKind::Residual,
            layout.residual.clone().collect(),
            layout.total,
        )];
        let pass = problem.pass(
            &params,
            ActiveColumns::ALL,
            &mut blocks,
            false,
            false,
            false,
            IntentFilter::NativeOnly,
        );
        assert!(
            !pass.infeasible,
            "fixture left the feasible set before differencing"
        );
        let cost = |at: &Params| {
            problem
                .pass(
                    at,
                    ActiveColumns::ALL,
                    &mut [],
                    false,
                    false,
                    false,
                    IntentFilter::NativeOnly,
                )
                .cost
        };
        let mut barrier = vec![0.0_f64; layout.residual.len()];
        for sample in &pass.residual_samples {
            for (m, node) in sample.nodes.iter().enumerate() {
                let weight = sample.weights[m];
                for channel in 0..3 {
                    barrier[*node as usize * 3 + channel] +=
                        weight * sample.penalty_gradient[channel];
                }
            }
        }
        let peak = barrier
            .iter()
            .fold(0.0_f64, |peak, value| peak.max(value.abs()));
        assert!(
            peak > 1e-6,
            "barrier contribution is not discriminating: {peak:.3e}"
        );
        let base = cost(&params);
        let step = 1e-6;
        let mut checked = 0usize;
        let mut skipped = 0usize;
        let mut worst = (0.0_f64, usize::MAX);
        let mut worst_smooth = 0.0_f64;
        let mut smooth_barrier = 0.0_f64;
        for index in 0..layout.residual.len() {
            let global = layout.residual.start + index;
            let mut plus = params.clone();
            let mut minus = params.clone();
            plus.theta[global] += step;
            minus.theta[global] -= step;
            let forward = cost(&plus);
            let backward = cost(&minus);
            assert!(
                forward.is_finite() && backward.is_finite(),
                "residual coefficient {index}: perturbed cost left the domain"
            );
            let left = (base - backward) / step;
            let right = (forward - base) / step;
            let central = 0.5 * (left + right);
            let analytic = -pass.residual_gradient[index];
            assert!(
                central.is_finite() && analytic.is_finite(),
                "residual coefficient {index}: non-finite derivative {analytic} vs {central}"
            );
            let scale = central.abs().max(analytic.abs());
            let roundoff = f64::EPSILON * base.abs().max(forward.abs()).max(backward.abs()) / step;
            let allowed = 32.0 * roundoff + 16.0 * f64::EPSILON * analytic.abs();
            let excess = (central - analytic).abs() - allowed;
            assert!(
                excess.is_finite(),
                "residual coefficient {index}: non-finite gap"
            );
            if excess > worst.0 {
                worst = (excess, index);
            }
            if (left - right).abs() > 1e-2 * scale.max(1.0) {
                skipped += 1;
                eprintln!(
                    "2x24 residual coefficient {index} nonsmooth left {left:.6e} right {right:.6e}"
                );
                continue;
            }
            checked += 1;
            smooth_barrier = smooth_barrier.max(barrier[index].abs());
            worst_smooth = worst_smooth.max(excess);
        }
        assert!(
            checked > 0,
            "no residual coefficient carried a smooth discriminating derivative"
        );
        assert!(
            smooth_barrier > 1e-6,
            "no smooth coefficient carries the barrier term: {smooth_barrier:.3e}"
        );
        assert_eq!(
            skipped, 0,
            "nonsmooth residual coefficients must not be skipped"
        );
        let (barrier_index, barrier_peak) =
            barrier
                .iter()
                .enumerate()
                .fold((0usize, 0.0_f64), |best, (index, value)| {
                    if value.abs() > best.1 {
                        (index, value.abs())
                    } else {
                        best
                    }
                });
        let global = layout.residual.start + barrier_index;
        let mut plus = params.clone();
        let mut minus = params.clone();
        plus.theta[global] += step;
        minus.theta[global] -= step;
        let forward = cost(&plus);
        let backward = cost(&minus);
        let central = (forward - backward) / (2.0 * step);
        let analytic = -pass.residual_gradient[barrier_index];
        let step2 = 0.5 * step;
        let mut plus2 = params.clone();
        let mut minus2 = params.clone();
        plus2.theta[global] += step2;
        minus2.theta[global] -= step2;
        let forward2 = cost(&plus2);
        let backward2 = cost(&minus2);
        let central2 = (forward2 - backward2) / (2.0 * step2);
        let truncation = (central - central2) / 3.0;
        let roundoff = f64::EPSILON
            * base
                .abs()
                .max(forward.abs())
                .max(backward.abs())
                .max(forward2.abs())
                .max(backward2.abs())
            / step;
        let allowed =
            32.0 * roundoff + 16.0 * f64::EPSILON * analytic.abs() + 4.0 * truncation.abs();
        assert!(
            (central - analytic).abs() <= allowed,
            "high-signal barrier coefficient {barrier_index}: central {central:.9e} analytic {analytic:.9e} truncation {truncation:.3e} allowed {allowed:.3e}"
        );
        assert!(
            barrier_peak > 8.0 * allowed,
            "barrier term {barrier_peak:.3e} is below the FD budget {allowed:.3e}"
        );
        assert!(
            worst_smooth <= 0.0,
            "smooth residual coefficient {}: FD exceeds the budget by {:.3e}",
            worst.1,
            worst_smooth
        );
    }

    #[test]
    #[ignore = "residual FD diagnostic; run explicitly"]
    fn residual_fd_gap_diagnostic() {
        let data = dataset(&SynthConfig {
            images: 1,
            samples_per_image: 16,
            ..SynthConfig::default()
        });
        let options = FitOptions {
            hessian_samples: usize::MAX,
            ..FitOptions::default()
        };
        let problem = Problem::new_with(&data.images, None, &options).expect("problem");
        let params = Params::initial(problem.layout.clone(), problem.contributors.len());
        let (worst, checked, skipped) = residual_fd_report(&problem, &params, "1x16");
        eprintln!(
            "1x16 residual gradient worst smooth {worst:.3e} checked {checked} skipped {skipped}"
        );
    }

    #[test]
    fn value_and_jacobian_passes_agree_on_the_objective() {
        let data = dataset(&SynthConfig {
            images: 2,
            samples_per_image: 24,
            ..SynthConfig::default()
        });
        let options = FitOptions {
            hessian_samples: usize::MAX,
            ..FitOptions::default()
        };
        let problem = Problem::new_with(&data.images, None, &options).expect("problem");
        let layout = problem.layout.clone();
        let mut params = Params::initial(layout.clone(), problem.contributors.len());
        params.theta[layout.image(0).start] = 0.2;
        for (k, index) in layout.residual.clone().enumerate() {
            params.theta[index] = 0.002 * ((k % 5) as f64 - 2.0);
        }
        let value = problem.pass(
            &params,
            ActiveColumns::ALL,
            &mut [],
            false,
            false,
            false,
            IntentFilter::NativeOnly,
        );
        let mut blocks = vec![BlockAcc::new(
            BlockKind::Residual,
            layout.residual.clone().collect(),
            layout.total,
        )];
        let jacobian = problem.pass(
            &params,
            ActiveColumns::ALL,
            &mut blocks,
            false,
            false,
            false,
            IntentFilter::NativeOnly,
        );
        let scale = value.cost.abs().max(jacobian.cost.abs()).max(1.0);
        assert!(
            (value.cost - jacobian.cost).abs() <= 64.0 * f64::EPSILON * scale.max(1.0),
            "value cost {:.12e} jacobian cost {:.12e}",
            value.cost,
            jacobian.cost
        );
        assert_eq!(value.active_samples, jacobian.active_samples);
        for (index, sample) in problem.samples.iter().enumerate() {
            let image = sample.image as usize;
            let context = problem.image_ctx(&params, image);
            let (prediction_value, residual_value, penalty_value) = crate::predict::predict_only(
                &params.look(),
                &context,
                sample.rgb,
                params.clip_k(),
                &sample.target,
            );
            let columns = problem.column_map(&params, image, ActiveColumns::ALL);
            let (look, slope) = problem.look_and_slope(&params);
            let mut jac = crate::predict::Jac::default();
            let out = crate::predict::evaluate_sample(
                &look,
                &problem.layout,
                &context,
                &columns,
                sample.rgb,
                params.clip_k(),
                params.clip_slope(),
                &slope,
                &problem.terms[image],
                &sample.target,
                &mut jac,
            );
            let prediction = prediction_value.to_array();
            let evaluated = out.prediction.to_array();
            for channel in 0..3 {
                let scale = prediction[channel]
                    .abs()
                    .max(evaluated[channel].abs())
                    .max(1e-9);
                assert!(
                    (prediction[channel] - evaluated[channel]).abs() <= 1e-9 * scale,
                    "sample {index} channel {channel} prediction {} vs {}",
                    prediction[channel],
                    evaluated[channel]
                );
                let scale = residual_value[channel]
                    .abs()
                    .max(out.residual[channel].abs())
                    .max(1e-9);
                assert!(
                    (residual_value[channel] - out.residual[channel]).abs() <= 1e-9 * scale,
                    "sample {index} channel {channel} residual {} vs {}",
                    residual_value[channel],
                    out.residual[channel]
                );
            }
            let penalty_value = penalty_value.expect("predict_only sample is feasible");
            let penalty = out.penalty.expect("evaluated sample is feasible");
            let scale = penalty_value.value.abs().max(penalty.value.abs()).max(1e-9);
            assert!(
                (penalty_value.value - penalty.value).abs() <= 1e-9 * scale,
                "sample {index} penalty {} vs {}",
                penalty_value.value,
                penalty.value
            );
        }
    }

    #[test]
    fn smooth_objective_gradient_with_residual_field_and_nuisance() {
        let data = dataset(&SynthConfig {
            images: 2,
            samples_per_image: 24,
            ..SynthConfig::default()
        });
        let options = FitOptions {
            hessian_samples: usize::MAX,
            ..FitOptions::default()
        };
        let problem = Problem::new_with(&data.images, None, &options).expect("problem");
        let layout = problem.layout.clone();
        let mut params = Params::initial(layout.clone(), problem.contributors.len());
        params.theta[layout.image(0).start] = 0.2;
        params.theta[layout.image(1).start] = 0.2;
        for (k, index) in layout.residual.clone().enumerate() {
            params.theta[index] = 0.002 * ((k % 5) as f64 - 2.0);
        }
        let image = layout.image(0);
        let mut blocks = vec![
            BlockAcc::new(
                BlockKind::Residual,
                layout.residual.clone().collect(),
                layout.total,
            ),
            BlockAcc::new(BlockKind::Nuisance, image.clone().collect(), layout.total),
        ];
        let pass = problem.pass(
            &params,
            ActiveColumns::ALL,
            &mut blocks,
            false,
            false,
            false,
            IntentFilter::NativeOnly,
        );
        assert!(!pass.infeasible, "fixture left the feasible set");
        let cost = |at: &Params| {
            problem
                .pass(
                    at,
                    ActiveColumns::ALL,
                    &mut [],
                    false,
                    false,
                    false,
                    IntentFilter::NativeOnly,
                )
                .cost
        };
        let base = cost(&params);
        let step = 1e-6;
        let mut checked = 0usize;
        for (local, global) in image.clone().enumerate() {
            let analytic = blocks[1].gradient[local];
            let mut plus = params.clone();
            let mut minus = params.clone();
            plus.theta[global] += step;
            minus.theta[global] -= step;
            let forward = cost(&plus);
            let backward = cost(&minus);
            assert!(
                forward.is_finite() && backward.is_finite(),
                "nuisance {global}: perturbed cost left the domain"
            );
            let numerical = (forward - backward) / (2.0 * step);
            assert!(
                numerical.is_finite() && analytic.is_finite(),
                "nuisance {global}: non-finite derivative {analytic} vs {numerical}"
            );
            let scale = numerical.abs().max(analytic.abs());
            if scale < 1e-9 {
                continue;
            }
            let stable = problem.samples.iter().all(|sample| {
                let base_piece = active_margin(sample_pre(&problem, &params, sample)).1;
                let plus_piece = active_margin(sample_pre(&problem, &plus, sample)).1;
                let minus_piece = active_margin(sample_pre(&problem, &minus, sample)).1;
                base_piece == plus_piece && base_piece == minus_piece
            });
            assert!(
                stable,
                "nuisance {global}: active margin piece changes across the step"
            );
            let roundoff = f64::EPSILON * forward.abs().max(backward.abs()) / step;
            let allowed = 8.0 * roundoff + 1e-5 * scale.max(1.0);
            assert!(
                (numerical - analytic).abs() <= allowed,
                "nuisance {global}: analytic {analytic:.9e} numeric {numerical:.9e} allowed {allowed:.3e}"
            );
            checked += 1;
        }
        let _ = base;
        assert!(checked > 0, "no nuisance direction carried a gradient");
    }
}
