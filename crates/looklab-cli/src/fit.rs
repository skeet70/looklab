// SPDX-License-Identifier: AGPL-3.0-or-later
//! `looklab fit`: fit one Look from sample blobs, write the `.looklab` file and
//! report per-image verdicts plus held-out error.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use looklab_color::{Rgb, RgbSpace, Xyz};
use looklab_export::DeltaStats;
use looklab_fit::{ExposureModeId, FitImage, FitOptions, FitResult, OutlierReason, Verdict, fit};
use looklab_model::file::{BodyProvenance, Provenance, RootProvenance};
use looklab_model::solve::ucs_of_xyz;
use looklab_model::{LookDocument, ProbeCovariance, UncertaintyData, output_encode_clip};
use looklab_samples::OutputSpace;

use crate::body_transform::TransformFile;
use crate::util;

#[derive(Debug, Args)]
pub struct FitArgs {
    /// Directory of `.llsamp` blobs.
    #[arg(long)]
    pub samples: PathBuf,
    /// Transform YAML, or `from-metadata` for `<samples>/transform.yaml`.
    #[arg(long)]
    pub transform: String,
    #[arg(long)]
    pub look_slug: String,
    #[arg(long, default_value = "0.1.0")]
    pub look_version: String,
    /// Body id recorded in provenance when the transform file has none.
    #[arg(long, default_value = "unknown/unknown")]
    pub body: String,
    /// Images kept out of the fit and scored afterwards.
    #[arg(long, default_value_t = 0)]
    pub holdout: usize,
    #[arg(long, default_value_t = 0x5eed)]
    pub seed: u64,
    #[arg(long)]
    pub out: PathBuf,
}

pub fn run(args: &FitArgs) -> Result<()> {
    let transform_path = util::transform_path(&args.transform, &args.samples)
        .unwrap_or_else(|| PathBuf::from(&args.transform));
    let transform = TransformFile::load(&transform_path)?;

    let files = util::samples_in(&args.samples)?;
    if args.holdout >= files.len() {
        bail!("cannot hold out {} of {} images", args.holdout, files.len());
    }
    let mut images = Vec::with_capacity(files.len());
    for file in &files {
        let name = file_name(file);
        let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
        let samples = looklab_samples::decode(&bytes)
            .with_context(|| format!("decoding {}", file.display()))?;
        let matrix = transform.image_matrix(&name, samples.header().wb_multipliers)?;
        let exposure_hint = f64::from(samples.header().exposure_hint_ev);
        let mut image = FitImage::new(samples, matrix);
        image.exposure_hint = exposure_hint;
        image.exposure_mode = ExposureModeId(0);
        images.push((name, image));
    }

    let split = images.len() - args.holdout;
    let (train, held_out) = images.split_at(split);
    let train_images: Vec<FitImage> = train.iter().map(|(_, image)| image.clone()).collect();
    let options = FitOptions {
        seed: args.seed,
        ..FitOptions::default()
    };
    let result = fit(&train_images, &options).context("fitting the Look")?;

    let document = build_document(args, &transform, &result, train)?;
    let bytes = document.encode().context("encoding the look file")?;
    let written = crate::write(&args.out, &bytes)?;

    println!(
        "fit: {} image(s), {} held out, {} samples, sweeps {}, cost {:.3}",
        train.len(),
        held_out.len(),
        train
            .iter()
            .map(|(_, image)| image.samples.n())
            .sum::<usize>(),
        result.diagnostics.sweeps,
        result.diagnostics.final_cost
    );
    for ((name, _), stats) in train.iter().zip(&result.image_stats) {
        println!(
            "  {name}: {} (rms ΔE {:.2}, z {:.2}{})",
            verdict_label(stats.verdict),
            stats.rms_de,
            stats.z,
            stats
                .reason
                .map(|reason| format!(", {}", reason_label(reason)))
                .unwrap_or_default()
        );
    }
    println!(
        "  quarantined {}, down-weighted {}",
        result.diagnostics.quarantined, result.diagnostics.down_weighted
    );
    println!("wrote {} ({written} bytes)", args.out.display());

    if !held_out.is_empty() {
        print_held_out(&document, &result, held_out);
    }
    Ok(())
}

fn build_document(
    args: &FitArgs,
    transform: &TransformFile,
    result: &FitResult,
    train: &[(String, FitImage)],
) -> Result<LookDocument> {
    let identity = looklab_model::file::identity(&args.look_slug, &args.look_version)?;
    let mut document = LookDocument::from_look(identity, result.look.clone())?;

    let mut covariance = Vec::with_capacity(result.probes.probes.len() * 9);
    let mut density = Vec::with_capacity(result.probes.probes.len());
    for probe in &result.probes.probes {
        for row in &probe.covariance {
            covariance.extend_from_slice(row);
        }
        density.push(probe.density);
    }
    let uncertainty = UncertaintyData {
        probe_cov: Some(ProbeCovariance::new(
            result.probes.probes.len(),
            covariance,
        )?),
        density: Some(looklab_model::Density::new(density)?),
    };
    document = document.with_uncertainty(uncertainty)?;

    let mut prep_versions: Vec<u32> = train
        .iter()
        .map(|(_, image)| image.samples.header().prep_version.get())
        .collect();
    prep_versions.sort_unstable();
    prep_versions.dedup();
    document.file.provenance = Provenance {
        bodies: vec![BodyProvenance {
            body: transform.body.clone().unwrap_or_else(|| args.body.clone()),
            epoch_map_version: 0,
            epochs: Vec::new(),
            input_transform_version: transform.version,
            realization_params_version: 0,
            control_response_version: 0,
            clip_k: result.realization.clip_k,
            images: train.len() as u32,
            credentials: 1,
        }],
        root: RootProvenance {
            admissions: Vec::new(),
            anchor_version: String::new(),
        },
        community_snapshot_version: 0,
        prep_versions,
        created: util::now_rfc3339(),
        software: format!("looklab-cli {}", env!("CARGO_PKG_VERSION")),
    };
    Ok(document)
}

fn print_held_out(document: &LookDocument, result: &FitResult, held_out: &[(String, FitImage)]) {
    let clip_k = result.realization.clip_k;
    for (name, image) in held_out {
        let set = &image.samples;
        let space = rgb_space(set.header().output_space);
        let mut errors = Vec::with_capacity(set.n());
        for (in_rgb, out_rgb) in set.in_rgb().iter().zip(set.out_rgb()) {
            let xyz = Xyz::from_array(image.transform.mul_vec(in_rgb.map(f64::from)));
            let predicted = output_encode_clip(document.look.eval(xyz), space, clip_k);
            let target_xyz = space.rgb_linear_to_xyz(Rgb::from_array(out_rgb.map(f64::from)));
            let target = ucs_of_xyz(target_xyz);
            let delta = looklab_color::ucs::delta_e_ucs(predicted, target);
            if delta.is_finite() {
                errors.push(delta);
            }
        }
        let stats = DeltaStats::from_values(errors);
        println!(
            "  held-out {name}: n {} median {:.2} p95 {:.2} max {:.2}",
            stats.count, stats.median, stats.p95, stats.max
        );
    }
}

fn rgb_space(space: OutputSpace) -> RgbSpace {
    match space {
        OutputSpace::Srgb => RgbSpace::Srgb,
        OutputSpace::AdobeRgb => RgbSpace::AdobeRgb,
    }
}

fn verdict_label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Accept => "accept",
        Verdict::DownWeight => "down-weight",
        Verdict::Quarantine => "quarantine",
    }
}

fn reason_label(reason: OutlierReason) -> &'static str {
    match reason {
        OutlierReason::ExposureLike => "exposure-like residual",
        OutlierReason::HueUniformCast => "uniform hue cast",
        OutlierReason::ShoulderOnly => "shoulder-only residual",
        OutlierReason::ContentCorrelated => "content-correlated residual",
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}
