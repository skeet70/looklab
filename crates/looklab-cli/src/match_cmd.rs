// SPDX-License-Identifier: AGPL-3.0-or-later
//! `looklab match` and `looklab verify` (docs/04 §3-§5).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use looklab_color::Mat3;
use looklab_match::{
    ControlCovariance, Emphasis, FitObservedOptions, MatchModel, MatchOptions, ObservedRender,
    Permalink, PermalinkSources, Plan, PreparedTarget, ScaleFitter, TargetSpec, VerifyRequest,
    optimize, settings_card, verify_once,
};
use looklab_model::ProbeSetVersion;
use looklab_model::realization::{ControlResponseModel, decode_crm};
use looklab_schema::ControlSchema;

use crate::body_transform::TransformFile;
use crate::util;

#[derive(Debug, Args)]
pub struct MatchArgs {
    /// Target Look (`.looklab`).
    #[arg(long)]
    pub look: PathBuf,
    /// Body control schema YAML.
    #[arg(long)]
    pub body: PathBuf,
    /// Control-response model (`crm.json`).
    #[arg(long)]
    pub crm: PathBuf,
    /// Probe emphasis: balanced, skin, landscape or product.
    #[arg(long, default_value = "balanced")]
    pub emphasis: String,
    #[arg(long, default_value_t = 1)]
    pub epoch: u32,
    /// Base realization Look for the sim; defaults to the target Look.
    #[arg(long)]
    pub base_look: Option<PathBuf>,
    /// Body input transform YAML; identity when absent.
    #[arg(long)]
    pub transform: Option<PathBuf>,
    /// Control-response covariance JSON for the predicted-variance term.
    #[arg(long)]
    pub covariance: Option<PathBuf>,
    #[arg(long, default_value_t = 1)]
    pub input_transform_version: u32,
    #[arg(long, default_value_t = 0)]
    pub realization_version: u32,
    #[arg(long, default_value_t = 0x5eed_1234)]
    pub seed: u64,
    /// Write the settings card text.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Write the canonical permalink.
    #[arg(long)]
    pub permalink: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct VerifyArgs {
    /// Permalink file or canonical string.
    #[arg(long)]
    pub permalink: String,
    /// Directory of `.llsamp` re-renders at the recommended settings.
    #[arg(long)]
    pub observed: PathBuf,
    #[arg(long)]
    pub look: Option<PathBuf>,
    #[arg(long)]
    pub body: Option<PathBuf>,
    #[arg(long)]
    pub crm: Option<PathBuf>,
    #[arg(long)]
    pub base_look: Option<PathBuf>,
    #[arg(long)]
    pub transform: Option<PathBuf>,
    #[arg(long)]
    pub covariance: Option<PathBuf>,
    #[arg(long, default_value_t = 0x5eed_1234)]
    pub seed: u64,
}

pub fn run_match(args: &MatchArgs) -> Result<()> {
    let emphasis = Emphasis::parse(&args.emphasis)
        .ok_or_else(|| anyhow::anyhow!("unknown emphasis {:?}", args.emphasis))?;
    let document = util::load_document(&args.look)?;
    let schema = load_schema(&args.body)?;
    let crm = load_crm(&args.crm)?;
    let base_document = match &args.base_look {
        Some(path) => util::load_document(path)?,
        None => document.clone(),
    };
    let (transform, input_transform_version) = match &args.transform {
        Some(path) => {
            let file = TransformFile::load(path)?;
            (body_transform(&file)?, file.version)
        }
        None => (Mat3::IDENTITY, args.input_transform_version),
    };
    let covariance = match &args.covariance {
        Some(path) => Some(load_covariance(path)?),
        None => None,
    };
    let plan = Plan::new(&schema, args.epoch)?;
    if crm.epoch != args.epoch {
        bail!(
            "control-response model is for epoch {}, not {}",
            crm.epoch,
            args.epoch
        );
    }
    let probes = looklab_model::probes::v1();
    if document.file.probe_set != ProbeSetVersion::V1 {
        bail!("unsupported probe set");
    }
    let target = PreparedTarget::new(TargetSpec {
        probes: &probes,
        function: &document.look,
        uncertainty: Some(&document.uncertainty),
        emphasis,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
    })?;
    let model = MatchModel::new(
        base_document.look.clone(),
        crm,
        transform,
        covariance,
        input_transform_version,
        args.realization_version,
    )?;
    let options = MatchOptions {
        seed: args.seed,
        ..MatchOptions::default()
    };
    let outcome = optimize(std::slice::from_ref(&model), &target, &plan, &options)?;
    let Some(best) = outcome.results.first() else {
        bail!("no settings suggestion");
    };
    println!(
        "match: {}@{} on {} epoch {} ({} sims, {} probes, {} subset)",
        document.file.look.slug,
        document.file.look.version,
        schema.body,
        args.epoch,
        outcome.stats.sims_considered,
        outcome.stats.supported_probes,
        outcome.stats.probe_subset
    );
    for (index, suggestion) in outcome.results.iter().enumerate() {
        println!(
            "  #{}. {}: median ΔE {:.2}, p95 {:.2}, band ±{:.2}",
            index + 1,
            suggestion.sim,
            suggestion.median_delta_e,
            suggestion.p95_delta_e,
            suggestion.sigma_band
        );
    }
    let card = settings_card(&target, &plan, &model, best)?;
    print!("{}", card.render_text());

    let epoch_map_version = document
        .file
        .provenance
        .bodies
        .first()
        .map(|body| body.epoch_map_version)
        .unwrap_or(0);
    let base_dir = args
        .permalink
        .as_deref()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."));
    let permalink = Permalink {
        target_slug: document.file.look.slug.to_string(),
        target_version: document.file.look.version.to_string(),
        base_slug: base_document.file.look.slug.to_string(),
        base_version: base_document.file.look.version.to_string(),
        body: schema.body.to_string(),
        epoch: args.epoch,
        sim: best.sim.to_string(),
        crm_version: model.version(),
        input_transform_version,
        realization_params_version: args.realization_version,
        epoch_map_version,
        probe_set: document.file.probe_set,
        optimizer: looklab_match::OPTIMIZER_VERSION.to_string(),
        emphasis,
        sources: PermalinkSources {
            target_look: Some(relative_source(&args.look, base_dir)),
            base_look: args
                .base_look
                .as_ref()
                .map(|path| relative_source(path, base_dir)),
            body: Some(relative_source(&args.body, base_dir)),
            crm: Some(relative_source(&args.crm, base_dir)),
            transform: args
                .transform
                .as_ref()
                .map(|path| relative_source(path, base_dir)),
            covariance: args
                .covariance
                .as_ref()
                .map(|path| relative_source(path, base_dir)),
        },
    };
    let canonical = permalink.canonical();
    if let Some(path) = &args.permalink {
        crate::write(path, canonical.as_bytes())?;
        println!("wrote {}", path.display());
    } else {
        println!("permalink: {canonical}");
    }
    if let Some(path) = &args.out {
        crate::write(path, card.render_text().as_bytes())?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

pub fn run_verify(args: &VerifyArgs) -> Result<()> {
    let (permalink, permalink_path) = resolve_permalink(&args.permalink)?;
    let emphasis = permalink.emphasis;
    let look_path = resolve_source(
        &permalink_path,
        permalink.sources.target_look.as_deref(),
        args.look.clone(),
    )
    .ok_or_else(|| anyhow::anyhow!("permalink has no target Look source; pass --look"))?;
    let body_path = resolve_source(
        &permalink_path,
        permalink.sources.body.as_deref(),
        args.body.clone(),
    )
    .ok_or_else(|| anyhow::anyhow!("permalink has no body schema source; pass --body"))?;
    let crm_path = resolve_source(
        &permalink_path,
        permalink.sources.crm.as_deref(),
        args.crm.clone(),
    )
    .ok_or_else(|| anyhow::anyhow!("permalink has no control-response model; pass --crm"))?;
    let base_path = resolve_source(
        &permalink_path,
        permalink.sources.base_look.as_deref(),
        args.base_look.clone(),
    );
    let transform_path = resolve_source(
        &permalink_path,
        permalink.sources.transform.as_deref(),
        args.transform.clone(),
    );
    let covariance_path = resolve_source(
        &permalink_path,
        permalink.sources.covariance.as_deref(),
        args.covariance.clone(),
    );

    let document = util::load_document(&look_path)?;
    let schema = load_schema(&body_path)?;
    let crm = load_crm(&crm_path)?;
    let base_document = match base_path {
        Some(path) => util::load_document(&path)?,
        None => document.clone(),
    };
    let (transform, transform_version) = match transform_path {
        Some(path) => {
            let file = TransformFile::load(&path)?;
            (body_transform(&file)?, file.version)
        }
        None => (Mat3::IDENTITY, permalink.input_transform_version),
    };
    if transform_version != permalink.input_transform_version {
        bail!(
            "input transform version {transform_version} does not match the permalink {}",
            permalink.input_transform_version
        );
    }
    let covariance = match covariance_path {
        Some(path) => Some(load_covariance(&path)?),
        None => None,
    };
    check_identity(
        document.file.look.slug.as_str(),
        document.file.look.version.as_str(),
        &permalink.target_slug,
        &permalink.target_version,
        "target Look",
    )?;
    check_identity(
        base_document.file.look.slug.as_str(),
        base_document.file.look.version.as_str(),
        &permalink.base_slug,
        &permalink.base_version,
        "base Look",
    )?;
    if crm.body.to_string() != permalink.body
        || crm.epoch != permalink.epoch
        || crm.base_sim.to_string() != permalink.sim
        || crm.version != permalink.crm_version
    {
        bail!("control-response model does not match the permalink");
    }
    let plan = Plan::new(&schema, permalink.epoch)?;
    let probes = looklab_model::probes::v1();
    let target = PreparedTarget::new(TargetSpec {
        probes: &probes,
        function: &document.look,
        uncertainty: Some(&document.uncertainty),
        emphasis,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
    })?;
    let model = MatchModel::new(
        base_document.look.clone(),
        crm,
        transform,
        covariance,
        permalink.input_transform_version,
        permalink.realization_params_version,
    )?;
    let files = util::samples_in(&args.observed)?;
    let mut observed = Vec::with_capacity(files.len());
    for file in &files {
        let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
        let samples = looklab_samples::decode(&bytes)
            .with_context(|| format!("decoding {}", file.display()))?;
        observed.push(ObservedRender { samples });
    }
    let options = MatchOptions {
        seed: args.seed,
        ..MatchOptions::default()
    };
    let request = VerifyRequest {
        target: &target,
        plan: &plan,
        model: &model,
        observed: &observed,
        options: &options,
        fit_options: FitObservedOptions::default(),
        truth: None,
    };
    let outcome = verify_once(&request, &ScaleFitter)?;
    println!(
        "verify: first suggestion {}, measured median ΔE {:.2} on {} observed render(s)",
        outcome.first.sim,
        outcome.first_measured.median,
        observed.len()
    );
    println!(
        "  second suggestion: median ΔE {:.2} (first predicted {:.2})",
        outcome.second.median_delta_e, outcome.first.median_delta_e
    );
    if let Some(reduction) = outcome.reduction {
        println!("  measured reduction {:.0}%", reduction * 100.0);
    } else {
        println!("  re-render at the second suggestion to measure the reduction");
    }
    println!(
        "  updated CRM version {}; permalink {}",
        outcome.updated.version(),
        permalink.optimizer
    );
    Ok(())
}

fn load_schema(path: &Path) -> Result<ControlSchema> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    looklab_schema::validate_control_schema(&text)
        .map_err(|errors| anyhow::anyhow!("invalid control schema: {errors:?}"))
}

fn load_crm(path: &Path) -> Result<ControlResponseModel> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    decode_crm(&bytes).with_context(|| format!("decoding CRM {}", path.display()))
}

fn load_covariance(path: &Path) -> Result<ControlCovariance> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    ControlCovariance::from_json(&bytes)
        .with_context(|| format!("decoding covariance {}", path.display()))
}

fn body_transform(file: &TransformFile) -> Result<Mat3> {
    file.camera_to_xyz
        .or_else(|| {
            file.forward_matrices
                .first()
                .map(|entry| entry.camera_to_xyz)
        })
        .or_else(|| file.per_image.first().map(|entry| entry.camera_to_xyz))
        .ok_or_else(|| anyhow::anyhow!("transform file has no camera-to-XYZ matrix"))
}

fn resolve_permalink(argument: &str) -> Result<(Permalink, PathBuf)> {
    let path = PathBuf::from(argument);
    if path.is_file() {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok((Permalink::parse(&text)?, path))
    } else {
        Ok((Permalink::parse(argument)?, PathBuf::from(".")))
    }
}

fn resolve_source(
    permalink: &Path,
    value: Option<&str>,
    override_path: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(path) = override_path {
        return Some(path);
    }
    let value = value?;
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Some(path)
    } else {
        Some(
            permalink
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .join(path),
        )
    }
}

fn relative_source(path: &Path, base: &Path) -> String {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let base = std::path::absolute(base).unwrap_or_else(|_| base.to_path_buf());
    absolute
        .strip_prefix(&base)
        .unwrap_or(&absolute)
        .display()
        .to_string()
}

fn check_identity(
    slug: &str,
    version: &str,
    pinned_slug: &str,
    pinned_version: &str,
    what: &str,
) -> Result<()> {
    if slug != pinned_slug || version != pinned_version {
        bail!(
            "{what} {slug}@{version} does not match the permalink {pinned_slug}@{pinned_version}"
        );
    }
    Ok(())
}
