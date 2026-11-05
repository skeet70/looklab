// SPDX-License-Identifier: AGPL-3.0-or-later
//! Body-held-out model error for the published uncertainty path (docs/02 §5).

use looklab_color::RgbSpace;
use looklab_color::ucs::{Ucs, delta_e_ucs};
use looklab_model::Look;

use crate::hierarchical::BodyGroup;
use crate::problem::Problem;
use crate::types::{FitImage, FitOptions, ProbeUncertainty, Result};

pub const MODEL_HUE_BINS: usize = 12;
pub const MODEL_S_BINS: usize = 6;
pub const MODEL_S_MIN: f64 = -12.0;
pub const MODEL_S_MAX: f64 = 6.0;

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

/// K-fold over bodies: hold one body out, refit the Look on the rest, and take
/// the excess squared error over what the fit variance predicted.
pub fn estimate_model_error(
    images: &[FitImage],
    groups: &[BodyGroup],
    reference_variance: &[f64],
    options: &FitOptions,
) -> Result<ModelErrorField> {
    let mut field = ModelErrorField::empty();
    if groups.len() < 2 {
        return Ok(field);
    }
    let mut squared = vec![0.0_f64; field.variance.len()];
    let mut counts = vec![0.0_f64; field.variance.len()];
    let space = images.first().map_or(RgbSpace::Srgb, |image| {
        match image.samples.header().output_space {
            looklab_samples::OutputSpace::Srgb => RgbSpace::Srgb,
            looklab_samples::OutputSpace::AdobeRgb => RgbSpace::AdobeRgb,
        }
    });
    for held in groups {
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
        for sample in &problem.samples {
            let index = sample.image as usize;
            let image = &held_images[index];
            let exposure = image.exposure_hint;
            let scale = libm::pow(2.0, exposure);
            let xyz = looklab_color::Xyz::from_array(image.transform.mul_vec([
                sample.rgb[0] * scale,
                sample.rgb[1] * scale,
                sample.rgb[2] * scale,
            ]));
            let input = looklab_model::LookInput::new(xyz);
            let prediction = result.look.eval(xyz);
            let error = delta_e_ucs(prediction, sample.target);
            let bin = region(input.s, input.h_in);
            let weight = sample.weight.max(0.0);
            squared[bin] += weight * error * error;
            counts[bin] += weight;
        }
        let _ = space;
    }
    let mut raw = vec![0.0_f64; field.variance.len()];
    for bin in 0..raw.len() {
        if counts[bin] > 0.0 {
            let held_mse = squared[bin] / counts[bin];
            field.held_out_rms[bin] = held_mse.sqrt();
            raw[bin] = (held_mse - reference_variance[bin]).max(0.0);
        }
        field.reference_rms[bin] = reference_variance[bin].max(0.0).sqrt();
    }
    field.variance = smooth(&raw);
    Ok(field)
}

/// Add the stored model variance to an interactive probe set and re-mark support.
pub fn apply_model_error(
    probes: &mut ProbeUncertainty,
    field: &ModelErrorField,
    look: &Look,
    density: &crate::types::Density,
    space: RgbSpace,
    support_sigma: f64,
    min_clusters: usize,
) {
    let probes_v1 = looklab_model::probes::v1();
    let mut supported = 0usize;
    for (index, probe) in probes.probes.iter_mut().enumerate() {
        let input = looklab_model::LookInput::new(probes_v1.probes[index].xyz);
        let extra = field.at(input.s, input.h_in);
        for (channel, row) in probe.covariance.iter_mut().enumerate() {
            row[channel] += extra;
        }
        probe.sigma_de = (probe.covariance[0][0] + probe.covariance[1][1] + probe.covariance[2][2])
            .max(0.0)
            .sqrt();
        let prediction = look.eval(probes_v1.probes[index].xyz);
        probe.supported = probe.sigma_de <= support_sigma
            && probe.density >= density.floor
            && probe.clusters >= min_clusters
            && !crate::predict::in_clip_shoulder(prediction, space, look.clip.k);
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
