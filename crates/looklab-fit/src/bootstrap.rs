// SPDX-License-Identifier: AGPL-3.0-or-later
//! Published-version uncertainty: image bootstrap, leave-one-body-out disagreement, and support (docs/02 §5).

use looklab_color::RgbSpace;
use looklab_model::realization::ControlResponseModel;
use looklab_model::{Look, LookInput};
use looklab_samples::OutputSpace;

use crate::hierarchical::BodyGroup;
use crate::model_error::{ModelErrorField, estimate_model_error};
use crate::problem::Xorshift;
use crate::types::{
    Density, FitImage, FitOptions, FitResult, ImageIntent, PreviousSupport, ProbeEstimate,
    ProbeUncertainty, Result,
};

pub const BOOTSTRAP_RESAMPLES: usize = 32;

pub type Cov3 = [[f64; 3]; 3];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UncertaintyMethod {
    Bootstrap,
    Sandwich,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BootstrapOptions {
    pub resamples: usize,
    pub seed: u64,
    pub leave_one_body_out: bool,
}

impl Default for BootstrapOptions {
    fn default() -> Self {
        Self {
            resamples: BOOTSTRAP_RESAMPLES,
            seed: 0xb007_57a9_0f1e_2d3c,
            leave_one_body_out: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PublishedUncertainty {
    pub probes: ProbeUncertainty,
    pub fit_method: UncertaintyMethod,
    pub fit_covariance: Vec<Cov3>,
    pub disagreement: Vec<Cov3>,
    pub model: ModelErrorField,
}

fn space_of(output: OutputSpace) -> RgbSpace {
    match output {
        OutputSpace::Srgb => RgbSpace::Srgb,
        OutputSpace::AdobeRgb => RgbSpace::AdobeRgb,
    }
}

fn predictions(look: &Look) -> Vec<looklab_color::ucs::Ucs> {
    looklab_model::probes::v1()
        .probes
        .iter()
        .map(|probe| look.eval(probe.xyz))
        .collect()
}

fn covariance(rows: &[Vec<looklab_color::ucs::Ucs>]) -> Vec<Cov3> {
    if rows.len() < 2 {
        return Vec::new();
    }
    let probes = rows[0].len();
    let mut means = vec![[0.0_f64; 3]; probes];
    for row in rows {
        for (index, value) in row.iter().enumerate() {
            means[index][0] += value.j;
            means[index][1] += value.a;
            means[index][2] += value.b;
        }
    }
    for mean in &mut means {
        for value in mean {
            *value /= rows.len() as f64;
        }
    }
    let scale = 1.0 / (rows.len() - 1) as f64;
    let mut out = vec![[[0.0_f64; 3]; 3]; probes];
    for row in rows {
        for (index, value) in row.iter().enumerate() {
            let delta = [
                value.j - means[index][0],
                value.a - means[index][1],
                value.b - means[index][2],
            ];
            for a in 0..3 {
                for b in 0..3 {
                    out[index][a][b] += scale * delta[a] * delta[b];
                }
            }
        }
    }
    out
}

fn refit(
    _look: &Look,
    images: &[FitImage],
    options: &FitOptions,
    model: Option<&ControlResponseModel>,
) -> Result<FitResult> {
    crate::fit::run_quick(images, options, None, model)
}

/// `Σ_fit` is the bootstrap covariance when available, else the sandwich; `Σ_total` adds the leave-one-body-out disagreement.
#[allow(clippy::too_many_arguments)]
pub fn published_uncertainty(
    look: &Look,
    images: &[FitImage],
    groups: &[BodyGroup],
    sandwich: &ProbeUncertainty,
    density: &Density,
    options: &FitOptions,
    bootstrap: &BootstrapOptions,
    clip_k: f64,
    previous: Option<&PreviousSupport>,
    model: Option<&ControlResponseModel>,
) -> Result<PublishedUncertainty> {
    let space = images.first().map_or(RgbSpace::Srgb, |image| {
        space_of(image.samples.header().output_space)
    });
    let mut rng = Xorshift::new(bootstrap.seed ^ 0x6d5a_56b1_1c37_9f03);
    let mut bootstrap_rows = Vec::new();
    for _ in 0..bootstrap.resamples {
        let mut sample = Vec::with_capacity(images.len());
        if groups.len() >= 2 {
            for _ in 0..groups.len() {
                let group = &groups[rng.below(groups.len())];
                for index in &group.images {
                    sample.push(images[*index].clone());
                }
            }
        } else {
            for _ in 0..images.len() {
                sample.push(images[rng.below(images.len())].clone());
            }
        }
        if sample.is_empty() {
            continue;
        }
        let result = refit(look, &sample, options, model)?;
        bootstrap_rows.push(predictions(&result.look));
    }
    let boot = covariance(&bootstrap_rows);

    let mut leave_one_out_rows = Vec::new();
    if bootstrap.leave_one_body_out && groups.len() > 1 {
        for group in groups {
            let kept: Vec<FitImage> = images
                .iter()
                .enumerate()
                .filter(|(index, _)| !group.images.contains(index))
                .map(|(_, image)| image.clone())
                .filter(|image| image.intent == ImageIntent::Native)
                .collect();
            if kept.is_empty() {
                continue;
            }
            let result = refit(look, &kept, options, model)?;
            leave_one_out_rows.push(predictions(&result.look));
        }
    }
    let disagreement = covariance(&leave_one_out_rows);

    let trace = |covariance: &Cov3| covariance[0][0] + covariance[1][1] + covariance[2][2];
    let boot_trace: f64 = boot.iter().map(trace).sum();
    let sandwich_trace: f64 = sandwich
        .probes
        .iter()
        .map(|probe| trace(&probe.covariance))
        .sum();
    // A prior-dominated pool makes every resample land on the same Look, so the
    // bootstrap collapses to zero while the prior floor still carries variance.
    let use_bootstrap =
        boot.len() == predictions(look).len() && boot_trace >= 0.05 * sandwich_trace.max(1e-12);
    let (fit_method, fit_covariance) = if use_bootstrap {
        (UncertaintyMethod::Bootstrap, boot)
    } else {
        (
            UncertaintyMethod::Sandwich,
            sandwich
                .probes
                .iter()
                .map(|probe| probe.covariance)
                .collect(),
        )
    };

    let probes_v1 = looklab_model::probes::v1();
    let model = estimate_model_error(images, groups, model, options, clip_k)?;

    let mut probes = Vec::with_capacity(probes_v1.probes.len());
    for (index, probe) in probes_v1.probes.iter().enumerate() {
        let fit = fit_covariance.get(index).copied().unwrap_or([[0.0; 3]; 3]);
        let extra = disagreement.get(index).copied().unwrap_or([[0.0; 3]; 3]);
        let mut covariance = [[0.0_f64; 3]; 3];
        for a in 0..3 {
            for b in 0..3 {
                covariance[a][b] = fit[a][b] + extra[a][b];
            }
        }
        let sigma_de = (covariance[0][0] + covariance[1][1] + covariance[2][2])
            .max(0.0)
            .sqrt();
        let input = LookInput::new(probe.xyz);
        let local_density = density.at(input.s, input.h_in, input.c_in);
        let prediction = look.eval(probe.xyz);
        let chain = crate::predict::clip_jacobian(prediction, space, clip_k);
        let mut post_covariance = [[0.0_f64; 3]; 3];
        for a in 0..3 {
            for b in 0..3 {
                let mut sum = 0.0;
                for c in 0..3 {
                    for d in 0..3 {
                        sum += chain.0[a][c] * covariance[c][d] * chain.0[b][d];
                    }
                }
                post_covariance[a][b] = sum;
            }
        }
        let post_sigma_de = (post_covariance[0][0] + post_covariance[1][1] + post_covariance[2][2])
            .max(0.0)
            .sqrt();
        let post_ucs = crate::predict::post_clip(look, probe.xyz, space, clip_k);
        let clusters = sandwich
            .probes
            .get(index)
            .map_or(0, |estimate| estimate.clusters);
        let bias_de = sandwich
            .probes
            .get(index)
            .map_or(0.0, |estimate| estimate.bias_de);
        probes.push(ProbeEstimate {
            id_hint: index,
            ucs: prediction,
            covariance,
            sigma_de,
            post_ucs,
            post_covariance,
            post_sigma_de,
            bias_de,
            pre_clip_reliable: false,
            gamut_edge: probe.id.as_str().split('-').nth(2) == Some("80"),
            density: local_density,
            supported_sigma: false,
            supported_density: false,
            supported_clusters: false,
            supported_bias: false,
            supported_evidence: false,
            supported: false,
            clusters,
        });
    }
    let mut published = ProbeUncertainty {
        probes,
        ids: probes_v1
            .probes
            .iter()
            .map(|probe| probe.id.clone())
            .collect(),
        supported_fraction: 0.0,
    };
    crate::model_error::apply_model_error(
        &mut published,
        &model,
        look,
        density,
        space,
        options,
        clip_k,
        previous,
    );
    Ok(PublishedUncertainty {
        probes: published,
        fit_method,
        fit_covariance,
        disagreement,
        model,
    })
}
