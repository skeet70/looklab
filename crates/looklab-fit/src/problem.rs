// SPDX-License-Identifier: AGPL-3.0-or-later
//! The fit's sample set, block structure and one objective/Jacobian pass.

use core::ops::Range;
use std::borrow::Cow;

use looklab_color::{Mat3, Rgb, RgbSpace};
use looklab_model::Look;
use looklab_model::basis::{RESIDUAL_COEFS, TONE_COEFS};
use looklab_model::realization::{
    ControlResponseModel, ControlValue, Delta, ResponseComponent, ResponseParamId, Settings,
};
use looklab_model::solve::ucs_of_xyz;
use looklab_samples::OutputSpace;

use crate::layout::{BlockKind, Layout, Params, softplus_slope};
use crate::predict::{ColumnMap, ImageCtx, Jac, Layer, ResponseTerm, evaluate_sample};
use crate::types::{
    ContributorId, DomainExclusion, ExposureModeId, FitError, FitImage, FitOptions, ImageIntent,
    Result,
};

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
fn eligible(image: &FitImage, rgb: [f64; 3], hint: f64, options: &FitOptions) -> bool {
    for exposure in [-ELIGIBILITY_SIGMAS, 0.0, ELIGIBILITY_SIGMAS] {
        let scale = libm::pow(
            2.0,
            (hint + exposure * options.exposure_prior_sd).clamp(-8.0, 8.0),
        );
        for gain_shift in [-ELIGIBILITY_SIGMAS, ELIGIBILITY_SIGMAS] {
            let gain = libm::exp(gain_shift * options.wb_prior_sd);
            let cam = [rgb[0] * gain * scale, rgb[1] * scale, rgb[2] * gain * scale];
            let scene = image.transform.mul_vec(cam);
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
    /// `(local column, derivative, Jᵀr, d margin/dθ)`.
    columns: Vec<(u32, [f64; 3], f64, f64)>,
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
    buffer: &[(u32, [f64; 3], f64, f64)],
    contribution: f64,
    residual: [f64; 3],
    robust: f64,
    barrier: f64,
) {
    for (a, (left, left_derivative, left_term, left_margin)) in buffer.iter().enumerate() {
        for (right, right_derivative, right_term, right_margin) in buffer.iter().skip(a) {
            let dot = left_derivative[0] * right_derivative[0]
                + left_derivative[1] * right_derivative[1]
                + left_derivative[2] * right_derivative[2];
            let data = contribution * dot;
            let weighted =
                data + robust * left_term * right_term + barrier * left_margin * right_margin;
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
}

pub(crate) struct HessianRow {
    pub columns: Vec<(u32, [f64; 3])>,
    pub weight: f64,
    /// `Jᵀr` per column, aligned with `columns`.
    pub robust_terms: Vec<f64>,
    /// Coefficient of `−(Jᵀr)(Jᵀr)ᵀ`, already carrying the sample weight.
    pub robust: f64,
    /// `d margin / dθ` per column, aligned with `columns`.
    pub margin_terms: Vec<f64>,
    /// `barrier''(margin)`, already carrying the sample weight.
    pub barrier: f64,
    /// `prediction − target` over the three likelihood channels.
    #[cfg(test)]
    pub residual: [f64; 3],
    /// Residual-grid columns `(global, derivative, Jᵀr, d margin)`; kept out of `columns` so
    /// the joint operator excludes them while the cross curvature can still be formed.
    pub residual_columns: Vec<(u32, [f64; 3], f64, f64)>,
    /// Layer value each retained column moves, `None` when it mixes layers.
    pub layers: Vec<Option<Layer>>,
    /// `∂(t, n_a, n_b)/∂θ` in the first three slots and `∂layer/∂θ` of `layers` in
    /// the fourth.
    pub layer_weights: Vec<[f64; 4]>,
    /// `∂²layer/∂θ²` of a tone column, `∂w_m/∂t` of a tint column, zero otherwise.
    pub layer_slopes: Vec<f64>,
    /// `α w r·∂p/∂l_k + barrier' ∇margin·∂(J'a'b')/∂l_k` for `k = 0, 1, 2`.
    pub motion: [f64; 3],
    /// `Σ_m w''_m a_m`, `Σ_m w''_m b_m`, `Σ_m w'_m a_m`, `Σ_m w'_m b_m`.
    pub tint: [f64; 4],
    /// `α w Σ_c r_c ∂²p_c/∂l_k∂l_l + barrier'(margin) ∂²margin/∂l_k∂l_l`.
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
        if images.is_empty() {
            return Err(FitError::NoImages);
        }
        let parameter_ids = model
            .map(ControlResponseModel::parameter_ids)
            .unwrap_or_default();
        let terms: Vec<Vec<ResponseTerm>> = match model {
            Some(model) => images
                .iter()
                .map(|image| {
                    model
                        .active_parameters(&image.settings)
                        .into_iter()
                        .filter_map(|(param, phi)| {
                            let id = parameter_ids.get(param)?;
                            Some(ResponseTerm {
                                index: param,
                                phi,
                                component: id.component,
                                feature: id.feature,
                            })
                        })
                        .collect()
                })
                .collect(),
            None => vec![Vec::new(); images.len()],
        };
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
        let mut rng = Xorshift::new(options.seed ^ 0x5bf0_3635_1c9a_2e11);
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
                None => shuffle(&mut order, &mut rng, take),
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
            for index in order {
                let index = index as usize;
                let rgb = in_rgb[index].map(|value| value as f64);
                if !eligible(image, rgb, image.exposure_hint, options) {
                    let input = looklab_model::LookInput::new(looklab_color::Xyz::from_array(
                        image.transform.mul_vec([
                            rgb[0] * libm::pow(2.0, image.exposure_hint.clamp(-8.0, 8.0)),
                            rgb[1] * libm::pow(2.0, image.exposure_hint.clamp(-8.0, 8.0)),
                            rgb[2] * libm::pow(2.0, image.exposure_hint.clamp(-8.0, 8.0)),
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
                let hint_scale = libm::pow(2.0, image.exposure_hint.clamp(-8.0, 8.0));
                let scene = image.transform.mul_vec([
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
                known_exposure: model.map_or(0.0, |model| {
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
        let defaults = model.map_or_else(Vec::new, |model| {
            model
                .controls
                .iter()
                .map(|control| (control.key.clone(), control.default.clone()))
                .chain(
                    model
                        .wb
                        .controls
                        .iter()
                        .map(|control| (control.key.clone(), control.default.clone())),
                )
                .collect()
        });
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

    pub(crate) fn realized_look(&self, params: &Params, index: usize) -> Look {
        let base = params.look();
        self.effective_look(params, &base, index).into_owned()
    }

    fn tone_slope(&self, params: &Params) -> [f64; TONE_COEFS] {
        let mut slope = [0.0_f64; TONE_COEFS];
        for (k, value) in slope.iter_mut().enumerate() {
            *value = softplus_slope(params.theta[self.layout.tone_beta.start + k]);
        }
        slope
    }

    fn effective_look<'a>(&self, params: &Params, base: &'a Look, index: usize) -> Cow<'a, Look> {
        let terms = &self.terms[index];
        if terms.is_empty() {
            return Cow::Borrowed(base);
        }
        let mut delta = Delta::default();
        for term in terms {
            if let Some(layer) = term.component.layer() {
                let value = params.theta[self.layout.response.start + term.index];
                delta.add(layer, term.feature, value * term.phi);
            }
        }
        match delta.apply(base) {
            Ok(realized) => Cow::Owned(realized),
            Err(_) => Cow::Borrowed(base),
        }
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
    ) -> PassOutput {
        let base = params.look();
        let slope = self.tone_slope(params);
        let looks: Vec<Cow<'_, Look>> = (0..self.images.len())
            .map(|index| self.effective_look(params, &base, index))
            .collect();
        let clip_k = params.clip_k();
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
        let mut jac = Jac::default();
        let contexts: Vec<(ImageCtx, ColumnMap)> = self
            .images
            .iter()
            .enumerate()
            .map(|(index, _)| {
                let mut columns = self.column_map(params, index, active);
                columns.second = retain_rows;
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
        let mut margin_terms: Vec<f64> = Vec::new();

        for (index, sample) in self.samples.iter().enumerate() {
            if neutral_only && !sample.neutral {
                continue;
            }
            let image = sample.image as usize;
            let image_entry = &self.images[image];
            if !intents.includes(image_entry.intent) {
                continue;
            }
            let is_hessian = all_rows || hessian_set[index];
            let (context, columns) = &contexts[image];
            let look: &Look = &looks[image];
            let terms: &[ResponseTerm] = if active.response {
                &self.terms[image]
            } else {
                &[]
            };
            let (prediction, margin, fitting, evaluated) = if needs_jacobian {
                let evaluated = evaluate_sample(
                    look,
                    &self.layout,
                    context,
                    columns,
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
                    evaluated.margin,
                    evaluated.residual,
                    Some(evaluated),
                )
            } else {
                let (prediction, fitting, margin) =
                    crate::predict::predict_only(look, context, sample.rgb, clip_k, &sample.target);
                (prediction, margin, fitting, None)
            };
            let (barrier_slope, barrier_curvature) = if margin > 0.0 && margin.is_finite() {
                looklab_model::look::barrier_derivatives(margin)
            } else {
                (0.0, 0.0)
            };
            let feasible = margin > 0.0
                && margin.is_finite()
                && barrier_slope.is_finite()
                && barrier_curvature.is_finite();
            let mut usable = feasible;
            if !feasible {
                output.infeasible = true;
            }
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
            if feasible {
                output.cost += weight * looklab_model::look::barrier(margin);
            } else {
                output.cost = f64::INFINITY;
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
            let robust = if q > 0.0 && q.is_finite() {
                weight * (-2.0 * nu_plus_three / (q * q))
            } else {
                0.0
            };
            let barrier = weight * barrier_curvature;
            let barrier_gradient = weight * barrier_slope;
            output.rss += contribution * squared;
            if is_hessian {
                output.hessian_rss += contribution * squared;
            }
            if let Some(evaluated) = &evaluated {
                column_terms.clear();
                column_norms.clear();
                margin_terms.clear();
                for (index, derivative) in jac.derivs.iter().enumerate() {
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
                    margin_terms.push(evaluated.margin_terms[index]);
                }
                // A finite margin does not imply finite derivatives: the CAM16 inverse and
                // the clip chain can still fail at a contracted or extreme point.
                usable = column_terms.iter().all(|value| value.is_finite())
                    && column_norms.iter().all(|value| value.is_finite())
                    && margin_terms.iter().all(|value| value.is_finite())
                    && jac
                        .derivs
                        .iter()
                        .all(|derivative| derivative.iter().all(|value| value.is_finite()))
                    && evaluated.second.as_ref().is_none_or(|order| {
                        order
                            .prediction
                            .iter()
                            .chain(core::iter::once(&order.margin))
                            .all(|values| values.iter().all(|value| value.is_finite()))
                    });
                if !usable {
                    output.infeasible = true;
                    output.cost = f64::INFINITY;
                }
                if usable && retain_rows && is_hessian {
                    let mut columns = Vec::with_capacity(jac.cols.len());
                    let mut robust_terms = Vec::with_capacity(jac.cols.len());
                    let mut retained_margins = Vec::with_capacity(jac.cols.len());
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
                                    margin_terms[index],
                                ));
                            }
                            continue;
                        }
                        if !retained_columns[*column as usize] {
                            continue;
                        }
                        columns.push((*column, *derivative));
                        robust_terms.push(column_terms[index]);
                        retained_margins.push(margin_terms[index]);
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
                            *value = contribution * data + barrier_gradient * order.margin[index];
                        }
                        for (slot, value) in motion.iter_mut().enumerate() {
                            let mut data = 0.0;
                            for (channel, residual) in fitting.iter().enumerate() {
                                data += residual * order.gradient[slot][channel];
                            }
                            *value =
                                contribution * data + barrier_gradient * order.margin_value[slot];
                        }
                        tint = [
                            order.tint_curvature[0],
                            order.tint_curvature[1],
                            order.tint_slope[0],
                            order.tint_slope[1],
                        ];
                    }
                    output.coupled_rows.push(HessianRow {
                        columns,
                        weight: contribution,
                        robust_terms,
                        robust,
                        margin_terms: retained_margins,
                        barrier,
                        #[cfg(test)]
                        residual: fitting,
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
                        let margin_term = margin_terms[column_index];
                        block.gradient[local as usize] += contribution * column_terms[column_index]
                            + barrier_gradient * margin_term;
                        block.full_diagonal[local as usize] += contribution
                            * column_norms[column_index]
                            + barrier * margin_term * margin_term;
                        if is_hessian {
                            buffer.columns.push((
                                local,
                                *derivative,
                                column_terms[column_index],
                                margin_term,
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
                for (m, node) in evaluated.residual_indices.iter().enumerate() {
                    let weight = evaluated.residual_weights[m];
                    for channel in 0..3 {
                        output.residual_gradient[*node * 3 + channel] -=
                            contribution * weight * clip_residual[channel];
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
                        barrier,
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
                        barrier,
                    );
                }
            }
            if wants_residual_block && usable {
                let evaluated = evaluated
                    .as_ref()
                    .expect("Hessian samples carry the Jacobian");
                output.residual_samples.push(ResidualSample {
                    nodes: evaluated.residual_indices.map(|index| index as u32),
                    weights: evaluated.residual_weights,
                    clip: evaluated.clip_jacobian,
                    alpha: contribution,
                    residual: fitting,
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

    /// Every fixture contribution is smaller than this value's ULP.
    const ABSORBING: f64 = 1e300;

    fn mixed_block(problem: &Problem) -> BlockAcc {
        let mut params: Vec<usize> = problem.layout.tone_beta.clone().rev().collect();
        params.push(problem.layout.tone_c0);
        params.push(problem.layout.tint_a.start);
        BlockAcc::new(BlockKind::Tone, params, problem.layout.total)
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
}
