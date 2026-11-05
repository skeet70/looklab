// SPDX-License-Identifier: AGPL-3.0-or-later
//! Body-held-out model error for the published uncertainty path (docs/02 §5).

use looklab_color::RgbSpace;
use looklab_color::ucs::{Ucs, delta_e_ucs};
use looklab_model::Look;

use crate::hierarchical::BodyGroup;
use crate::problem::Problem;
use crate::types::{FitImage, FitOptions, ProbeUncertainty, Result};
use crate::uncertainty::student_t_95;

pub const MODEL_HUE_BINS: usize = 12;
pub const MODEL_S_BINS: usize = 6;
pub const MODEL_S_MIN: f64 = -12.0;
pub const MODEL_S_MAX: f64 = 6.0;
/// Tentative inflation of the cross-validated excess for a new body.
pub const MODEL_SAFETY: f64 = 1.3;

#[derive(Clone, Debug, PartialEq)]
pub struct ModelErrorField {
    pub variance: Vec<f64>,
    /// Held-out RMS ΔE per region, zero where no sample fell.
    pub held_out_rms: Vec<f64>,
    /// Reference (pre-model) RMS σ per region.
    pub reference_rms: Vec<f64>,
}

impl ModelErrorField {
    pub fn empty() -> Self {
        Self {
            variance: vec![0.0; MODEL_HUE_BINS * MODEL_S_BINS],
            held_out_rms: vec![0.0; MODEL_HUE_BINS * MODEL_S_BINS],
            reference_rms: vec![0.0; MODEL_HUE_BINS * MODEL_S_BINS],
        }
    }

    pub fn at(&self, s: f64, hue_degrees: f64) -> f64 {
        self.variance[region(s, hue_degrees)]
    }
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

fn smooth(values: &[f64]) -> Vec<f64> {
    let mut out = values.to_vec();
    for _ in 0..2 {
        let mut next = out.clone();
        for hue in 0..MODEL_HUE_BINS {
            for s in 0..MODEL_S_BINS {
                let mut sum = 0.0;
                let mut count = 0.0;
                for dh in [-1i64, 0, 1] {
                    let h = ((hue as i64 + dh + MODEL_HUE_BINS as i64) % MODEL_HUE_BINS as i64)
                        as usize;
                    for ds in [-1i64, 0, 1] {
                        let bin = s as i64 + ds;
                        if !(0..MODEL_S_BINS as i64).contains(&bin) {
                            continue;
                        }
                        sum += out[h * MODEL_S_BINS + bin as usize];
                        count += 1.0;
                    }
                }
                next[hue * MODEL_S_BINS + s] = sum / count;
            }
        }
        out = next;
    }
    out.into_iter().map(|value| value.max(0.0)).collect()
}

/// K-fold over bodies, or over images for a single-body pool.
pub fn estimate_model_error(
    images: &[FitImage],
    groups: &[BodyGroup],
    reference_variance: &[f64],
    options: &FitOptions,
    clip_k: f64,
) -> Result<ModelErrorField> {
    let mut field = ModelErrorField::empty();
    let folds: Vec<BodyGroup> = if groups.len() >= 2 {
        groups.to_vec()
    } else {
        let chunk_size = images.len().div_ceil(5).max(1);
        let mut folds = Vec::new();
        for (index, slice) in images.chunks(chunk_size).enumerate() {
            let start = index * chunk_size;
            let id = looklab_body::BodyId::new(format!("image-fold-{index}"))
                .map_err(|error| crate::types::FitError::Hierarchy(error.to_string()))?;
            folds.push(BodyGroup {
                body: id,
                images: (start..start + slice.len()).collect(),
            });
        }
        folds
    };
    if folds.len() < 2 {
        return Ok(field);
    }
    let mut fold_squared: Vec<Vec<f64>> = Vec::new();
    let mut fold_counts: Vec<Vec<f64>> = Vec::new();
    for held in &folds {
        let kept: Vec<FitImage> = images
            .iter()
            .enumerate()
            .filter(|(index, _)| !held.images.contains(index))
            .map(|(_, image)| image.clone())
            .collect();
        if kept.is_empty() {
            continue;
        }
        let Ok(result) = crate::fit::run_quick(&kept, options, None) else {
            continue;
        };
        let held_images: Vec<FitImage> = held
            .images
            .iter()
            .map(|index| images[*index].clone())
            .collect();
        let Ok(problem) = Problem::new_with(&held_images, None, options) else {
            continue;
        };
        let slopes = tone_slopes(&result.look);
        let mut jac = crate::predict::Jac::default();
        let mut squared = vec![0.0_f64; field.variance.len()];
        let mut counts = vec![0.0_f64; field.variance.len()];
        for sample in &problem.samples {
            let index = sample.image as usize;
            let image = &held_images[index];
            let scale = libm::pow(2.0, image.exposure_hint);
            let xyz = looklab_color::Xyz::from_array(image.transform.mul_vec([
                sample.rgb[0] * scale,
                sample.rgb[1] * scale,
                sample.rgb[2] * scale,
            ]));
            let input = looklab_model::LookInput::new(xyz);
            let context = crate::predict::ImageCtx {
                transform: image.transform,
                a0: image.transform,
                space: problem.images[index].space,
                exposure: image.exposure_hint,
                wb_r: 1.0,
                wb_b: 1.0,
            };
            let evaluated = crate::predict::evaluate_sample(
                &result.look,
                &problem.layout,
                &context,
                &crate::predict::ColumnMap::default(),
                sample.rgb,
                clip_k,
                1.0,
                &slopes,
                &[],
                &mut jac,
            );
            let error = delta_e_ucs(evaluated.prediction, sample.target);
            let bin = region(input.s, input.h_in);
            let weight = sample.weight.max(0.0);
            squared[bin] += weight * error * error;
            counts[bin] += weight;
        }
        fold_squared.push(squared);
        fold_counts.push(counts);
    }
    let mut raw = vec![0.0_f64; field.variance.len()];
    for bin in 0..raw.len() {
        let mut values = Vec::new();
        for (squared, counts) in fold_squared.iter().zip(fold_counts.iter()) {
            if counts[bin] > 0.0 {
                values.push(squared[bin] / counts[bin]);
            }
        }
        if values.is_empty() {
            continue;
        }
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        field.held_out_rms[bin] = mean.sqrt();
        let mut excess = (mean - reference_variance[bin]).max(0.0);
        if values.len() > 1 {
            let variance = values
                .iter()
                .map(|value| (value - mean) * (value - mean))
                .sum::<f64>()
                / (values.len() - 1) as f64;
            let t = student_t_95((values.len() - 1) as f64);
            excess += t * variance.sqrt() * (1.0 + 1.0 / values.len() as f64).sqrt();
        }
        raw[bin] = excess * MODEL_SAFETY;
        field.reference_rms[bin] = reference_variance[bin].max(0.0).sqrt();
    }
    field.variance = smooth(&raw);
    Ok(field)
}

fn tone_slopes(look: &Look) -> [f64; looklab_model::basis::TONE_COEFS] {
    use crate::layout::{softplus_inverse, softplus_slope};
    let mut slopes = [1.0_f64; looklab_model::basis::TONE_COEFS];
    if let looklab_model::Tone::Spline { beta, .. } = &look.tone {
        for (slope, value) in slopes.iter_mut().zip(beta.iter()) {
            *slope = softplus_slope(softplus_inverse(*value));
        }
    }
    slopes
}

/// Add the stored model variance to an interactive probe set and re-mark support.
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
        let extra = field.at(input.s, input.h_in) / 3.0;
        for (channel, row) in probe.covariance.iter_mut().enumerate() {
            row[channel] += extra;
        }
        probe.sigma_de = (probe.covariance[0][0] + probe.covariance[1][1] + probe.covariance[2][2])
            .max(0.0)
            .sqrt();
        let prediction = look.eval(probes_v1.probes[index].xyz);
        let chain = crate::predict::clip_jacobian(prediction, space, clip_k);
        for a in 0..3 {
            for b in 0..3 {
                let mut sum = 0.0;
                for c in 0..3 {
                    for d in 0..3 {
                        sum += chain.0[a][c] * probe.covariance[c][d] * chain.0[b][d];
                    }
                }
                probe.post_covariance[a][b] = sum;
            }
        }
        crate::uncertainty::floor_post_covariance(&probe.covariance, &mut probe.post_covariance);
        probe.post_sigma_de = (probe.post_covariance[0][0]
            + probe.post_covariance[1][1]
            + probe.post_covariance[2][2])
            .max(0.0)
            .sqrt();
        probe.post_ucs =
            crate::predict::post_clip(look, probes_v1.probes[index].xyz, space, clip_k);
        probe.pre_clip_reliable =
            crate::predict::clip_derivative_min(prediction, space, clip_k) >= 0.25;
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
        probe.supported_sigma = flags.sigma;
        probe.supported_density = flags.density;
        probe.supported_clusters = flags.clusters;
        probe.supported_bias = flags.bias;
        probe.supported = flags.all();
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
        out.push((delta_e_ucs(look.eval(xyz), sample.target), sample.target));
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
                supported: false,
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
    fn radial_model_variance_is_divided_across_three_axes() {
        let look = Look::colorimetric_spline().expect("look");
        let mut field = ModelErrorField::empty();
        field.variance.fill(3.0);
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
        let sigma = estimates.probes[0].sigma_de;
        assert!((sigma * sigma - 3.0).abs() < 1e-12);
        assert!(estimates.probes[0].post_sigma_de <= sigma + 1e-12);
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
            max_sweeps: 3,
            ..FitOptions::default()
        };
        let field = estimate_model_error(
            &data.images,
            &[],
            &vec![0.0; MODEL_HUE_BINS * MODEL_S_BINS],
            &options,
            0.03,
        )
        .expect("image folds");
        assert!(field.held_out_rms.iter().any(|value| *value > 0.0));
    }
}
