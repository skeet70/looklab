// SPDX-License-Identifier: AGPL-3.0-or-later
//! The fit's sample set, block structure and one objective/Jacobian pass.

use core::ops::Range;
use looklab_color::ucs::Ucs;
use looklab_color::{Mat3, Rgb, RgbSpace};
use looklab_model::Look;
use looklab_model::basis::TONE_COEFS;
use looklab_model::solve::ucs_of_xyz;
use looklab_samples::OutputSpace;

use crate::layout::{BlockKind, Layout, Params, softplus_slope};
use crate::predict::{ColumnMap, ImageCtx, Jac, evaluate_sample};
use crate::types::{ContributorId, ExposureModeId, FitError, FitImage, FitOptions, Result};

pub(crate) struct ImageEntry {
    pub transform: Mat3,
    pub space: RgbSpace,
    pub exposure_hint: f64,
    pub trust: f64,
    pub mode: Option<usize>,
    pub contributor: usize,
    pub samples: Range<usize>,
}

pub(crate) struct Sample {
    pub rgb: [f64; 3],
    pub target: Ucs,
    pub image: u32,
    pub weight: f64,
    pub density: f64,
    pub neutral: bool,
}

pub(crate) struct Problem {
    pub layout: Layout,
    pub images: Vec<ImageEntry>,
    pub modes: Vec<ExposureModeId>,
    pub contributors: Vec<ContributorId>,
    pub samples: Vec<Sample>,
    pub hessian: Vec<u32>,
    pub nu: f64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ActiveColumns {
    pub image: bool,
    pub mode: bool,
    pub wb: bool,
    pub clip: bool,
}

impl ActiveColumns {
    pub const ALL: Self = Self {
        image: true,
        mode: true,
        wb: true,
        clip: true,
    };
}

pub(crate) struct BlockAcc {
    pub kind: BlockKind,
    pub params: Vec<usize>,
    pub local: Vec<u32>,
    pub hessian: Vec<f64>,
    pub gradient: Vec<f64>,
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
            hessian: vec![0.0; n * n],
            gradient: vec![0.0; n],
        }
    }

    pub fn len(&self) -> usize {
        self.params.len()
    }

    pub fn reset(&mut self) {
        self.hessian.fill(0.0);
        self.gradient.fill(0.0);
    }
}

/// Per-sample data for the matrix-free residual block.
pub(crate) struct ResidualSample {
    pub nodes: [u32; 8],
    pub weights: [f64; 8],
    pub clip: Mat3,
    pub alpha: f64,
    pub residual: [f64; 3],
}

pub(crate) struct PassOutput {
    pub cost: f64,
    pub rss: f64,
    pub gradient: Vec<f64>,
    pub contributor_rss: Vec<f64>,
    pub contributor_weight: Vec<f64>,
    pub contributor_pairs: Vec<Vec<(f64, f64)>>,
    pub residuals: Vec<[f64; 3]>,
    pub image_rss: Vec<f64>,
    pub image_weight: Vec<f64>,
    pub per_image_gradient: Vec<Vec<f64>>,
    pub residual_samples: Vec<ResidualSample>,
    pub active_samples: usize,
    pub hessian_rows: usize,
}

fn space_of(output: OutputSpace) -> RgbSpace {
    match output {
        OutputSpace::Srgb => RgbSpace::Srgb,
        OutputSpace::AdobeRgb => RgbSpace::AdobeRgb,
    }
}

impl Problem {
    pub fn new(images: &[FitImage], options: &FitOptions) -> Result<Self> {
        if images.is_empty() {
            return Err(FitError::NoImages);
        }
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
        let mut rng = Xorshift::new(options.seed ^ 0x5bf0_3635_1c9a_2e11);
        for (image_index, image) in images.iter().enumerate() {
            if image.samples.n() == 0 {
                return Err(FitError::EmptyImage { image: image_index });
            }
            let n = image.samples.n();
            let take = n.min(options.max_samples_per_image);
            let mut order: Vec<u32> = (0..n as u32).collect();
            for i in 0..take {
                let j = i + rng.below(n - i);
                order.swap(i, j);
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
                let target =
                    ucs_of_xyz(space.rgb_linear_to_xyz(Rgb::from_array(
                        out_rgb[index].map(|value| value as f64),
                    )));
                // Neutrality is an input property (docs/02 §4.1 stage 1), taken at the prior nuisance.
                let scene = image.transform.mul_vec([
                    in_rgb[index][0] as f64 * libm::pow(2.0, image.exposure_hint.clamp(-8.0, 8.0)),
                    in_rgb[index][1] as f64 * libm::pow(2.0, image.exposure_hint.clamp(-8.0, 8.0)),
                    in_rgb[index][2] as f64 * libm::pow(2.0, image.exposure_hint.clamp(-8.0, 8.0)),
                ]);
                let neutral = looklab_model::LookInput::new(looklab_color::Xyz::from_array(scene))
                    .c_in
                    < 12.0;
                samples.push(Sample {
                    rgb: in_rgb[index].map(|value| value as f64),
                    target,
                    image: image_index as u32,
                    weight: density[index] as f64 / mean_density.max(1e-12) * trust * temper,
                    density: density[index] as f64,
                    neutral,
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
            });
        }
        if samples.is_empty() {
            return Err(FitError::NoSamples);
        }
        let hessian = hessian_subset(samples.len(), options.hessian_samples, options.seed);
        let layout = Layout::new(entries.len(), mode_list.len(), contributors.len());
        Ok(Self {
            layout,
            images: entries,
            modes: mode_list,
            contributors,
            samples,
            hessian,
            nu: options.nu,
        })
    }

    pub fn look_and_slope(&self, params: &Params) -> (Look, [f64; TONE_COEFS]) {
        let look = params.look();
        let mut slope = [0.0_f64; TONE_COEFS];
        for (k, value) in slope.iter_mut().enumerate() {
            *value = softplus_slope(params.theta[self.layout.tone_beta.start + k]);
        }
        (look, slope)
    }

    pub fn image_ctx(&self, params: &Params, index: usize) -> ImageCtx {
        let image = &self.images[index];
        let nuisance = params.image(index);
        ImageCtx {
            transform: image.transform,
            space: image.space,
            exposure: nuisance[0] + image.mode.map_or(0.0, |mode| params.mode_offset(mode)),
            wb_r: libm::exp(nuisance[1]),
            wb_b: libm::exp(nuisance[2]),
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
        }
    }

    /// Student-t log-likelihood increase and IRLS weight for one residual.
    fn irls(&self, residual_squared: f64, sigma_squared: f64) -> (f64, f64) {
        let nu = self.nu;
        let ratio = residual_squared / (nu * sigma_squared);
        let weight = (nu + 3.0) / (nu * sigma_squared + residual_squared);
        (0.5 * (nu + 3.0) * libm::log1p(ratio), weight)
    }

    pub fn pass(
        &self,
        params: &Params,
        active: ActiveColumns,
        blocks: &mut [BlockAcc],
        want_residuals: bool,
        want_per_image: bool,
        neutral_only: bool,
    ) -> PassOutput {
        let (look, slope) = self.look_and_slope(params);
        let clip_k = params.clip_k();
        let clip_slope = params.clip_slope();
        let total = self.layout.total;
        let mut output = PassOutput {
            cost: 0.0,
            rss: 0.0,
            gradient: vec![0.0_f64; total],
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
            residual_samples: Vec::new(),
            active_samples: 0,
            hessian_rows: 0,
        };
        let mut jac = Jac::default();
        let contexts: Vec<(ImageCtx, ColumnMap)> = self
            .images
            .iter()
            .enumerate()
            .map(|(index, _)| {
                (
                    self.image_ctx(params, index),
                    self.column_map(params, index, active),
                )
            })
            .collect();
        let sigma: Vec<f64> = (0..self.contributors.len())
            .map(|index| params.sigma(index) * params.sigma(index))
            .collect();
        let mut hessian_set = vec![false; self.samples.len()];
        for index in &self.hessian {
            hessian_set[*index as usize] = true;
        }
        let mut block_buffers: Vec<Vec<(u32, [f64; 3])>> =
            blocks.iter().map(|_| Vec::new()).collect();
        let wants_residual_block = blocks.iter().any(|block| block.kind == BlockKind::Residual);

        for (index, sample) in self.samples.iter().enumerate() {
            if neutral_only && !sample.neutral {
                continue;
            }
            let image = sample.image as usize;
            let (context, columns) = &contexts[image];
            let evaluated = evaluate_sample(
                &look,
                &self.layout,
                context,
                columns,
                sample.rgb,
                clip_k,
                clip_slope,
                &slope,
                &mut jac,
            );
            let target = &sample.target;
            let residual = [
                evaluated.prediction.j - target.j,
                evaluated.prediction.a - target.a,
                evaluated.prediction.b - target.b,
            ];
            let squared =
                residual[0] * residual[0] + residual[1] * residual[1] + residual[2] * residual[2];
            let variance = sigma[self.images[image].contributor];
            let (log_likelihood, alpha) = self.irls(squared, variance);
            let weight = sample.weight;
            output.active_samples += 1;
            if hessian_set[index] {
                output.hessian_rows += 1;
            }
            output.cost += weight * (log_likelihood + 1.5 * libm::log(variance));
            output.image_rss[image] += weight * squared;
            output.image_weight[image] += weight;
            let contributor = self.images[image].contributor;
            output.contributor_rss[contributor] += weight * squared;
            output.contributor_weight[contributor] += weight;
            output.contributor_pairs[contributor].push((weight, squared));
            if want_residuals {
                output.residuals[index] = residual;
            }
            let contribution = weight * alpha;
            output.rss += contribution * squared;
            for (column, derivative) in jac.cols.iter().zip(jac.derivs.iter()) {
                let term = derivative[0] * residual[0]
                    + derivative[1] * residual[1]
                    + derivative[2] * residual[2];
                output.gradient[*column as usize] += contribution * term;
                if want_per_image {
                    output.per_image_gradient[image][*column as usize] += contribution * term;
                }
            }

            if !hessian_set[index] {
                continue;
            }
            for (slot, block) in blocks.iter_mut().enumerate() {
                let buffer = &mut block_buffers[slot];
                buffer.clear();
                for (column, derivative) in jac.cols.iter().zip(jac.derivs.iter()) {
                    let local = block.local[*column as usize];
                    if local != u32::MAX {
                        buffer.push((local, *derivative));
                    }
                }
                let n = block.len();
                for (a, (left, left_derivative)) in buffer.iter().enumerate() {
                    let base = *left as usize * n;
                    for (right, right_derivative) in buffer.iter().skip(a) {
                        let dot = left_derivative[0] * right_derivative[0]
                            + left_derivative[1] * right_derivative[1]
                            + left_derivative[2] * right_derivative[2];
                        block.hessian[base + *right as usize] += contribution * dot;
                        if right != left {
                            block.hessian[*right as usize * n + *left as usize] +=
                                contribution * dot;
                        }
                    }
                    let term = left_derivative[0] * residual[0]
                        + left_derivative[1] * residual[1]
                        + left_derivative[2] * residual[2];
                    block.gradient[*left as usize] += contribution * term;
                }
            }
            if wants_residual_block {
                output.residual_samples.push(ResidualSample {
                    nodes: evaluated.residual_indices.map(|index| index as u32),
                    weights: evaluated.residual_weights,
                    clip: evaluated.clip_jacobian,
                    alpha: contribution,
                    residual,
                });
            }
        }
        output
    }
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
