// SPDX-License-Identifier: AGPL-3.0-or-later
//! `looklab match` and `looklab verify` (docs/04 §3-§5).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use looklab_color::Mat3;
use looklab_match::{
    Calibration, ControlCovariance, Emphasis, FitObservedOptions, MatchModel, MatchOptions,
    ObservedRender, Permalink, PermalinkSources, Plan, PreparedTarget, ScaleFitter, TargetSpec,
    measure_target_error, optimize, settings_card,
};
use looklab_model::ProbeSetVersion;
use looklab_model::file::LookDocument;
use looklab_model::realization::{ControlResponseModel, Settings, decode_crm, encode_crm};
use looklab_publish::{
    ArtifactHash, ArtifactName, MANIFEST_NAME, MatchBundle, VersionManifest, body_artifact,
    bundle_artifact, sim_artifacts, transform_artifact, verify_artifact,
};
use looklab_schema::ControlSchema;
use same_file::is_same_file;

use crate::body_transform::TransformFile;
use crate::util;

#[derive(Debug, Args)]
pub struct MatchArgs {
    /// Target Look (`.looklab`).
    #[arg(long)]
    pub look: PathBuf,
    /// Body control schema YAML. Required unless --bundle is given.
    #[arg(long)]
    pub body: Option<PathBuf>,
    /// Control-response model (`crm.json`). Required unless --bundle is given.
    #[arg(long)]
    pub crm: Option<PathBuf>,
    /// Published match bundle directory, holding its version manifest, or a
    /// `-sims.json` descriptor. Ranks every sim it lists.
    #[arg(long)]
    pub bundle: Option<PathBuf>,
    /// Probe emphasis: balanced, skin, landscape or product.
    #[arg(long, default_value = "balanced")]
    pub emphasis: String,
    #[arg(long, default_value_t = 1)]
    pub epoch: u32,
    /// Base realization Look for the sim. Required unless --synthetic.
    #[arg(long)]
    pub base_look: Option<PathBuf>,
    /// Body input transform YAML. Required unless --synthetic.
    #[arg(long)]
    pub transform: Option<PathBuf>,
    /// Use synthetic defaults: the target Look as base and the identity transform.
    #[arg(long)]
    pub synthetic: bool,
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
    /// Published match bundle that supplied the original search; defaults beside the pinned model.
    #[arg(long)]
    pub bundle: Option<PathBuf>,
    /// Allow synthetic defaults: the target Look as base and the identity transform.
    #[arg(long)]
    pub synthetic: bool,
    /// Updated local CRM; defaults beside the input permalink.
    #[arg(long)]
    pub out_crm: Option<PathBuf>,
    /// Updated permalink; defaults beside the input permalink.
    #[arg(long)]
    pub out_permalink: Option<PathBuf>,
    /// Replay information for the next iteration; defaults beside the input permalink.
    #[arg(long)]
    pub out_result: Option<PathBuf>,
}

pub fn run_match(args: &MatchArgs) -> Result<()> {
    let emphasis = Emphasis::parse(&args.emphasis)
        .ok_or_else(|| anyhow::anyhow!("unknown emphasis {:?}", args.emphasis))?;
    validate_match_args(args)?;
    let document = util::load_document(&args.look)?;
    if let Some(refusal) = document.match_refusal() {
        bail!("this Look is not matchable: {refusal}");
    }
    let LoadedMatchInputs {
        schema,
        input_transform_version: InputTransformVersion(input_transform_version),
        body_path,
        transform_path,
        sims,
    } = load_match_inputs(args, &document)?;
    let plan = Plan::new(&schema, args.epoch)?;
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
    let models: Vec<MatchModel> = sims.iter().map(|sim| sim.model.clone()).collect();
    let options = MatchOptions {
        seed: args.seed,
        ..MatchOptions::default()
    };
    let outcome = optimize(&models, &target, &plan, &options)?;
    let Some(best) = outcome.results.first() else {
        bail!("no settings suggestion");
    };
    let winner = sims
        .iter()
        .find(|sim| sim.model.sim() == &best.sim)
        .ok_or_else(|| anyhow::anyhow!("no loaded sim for {}", best.sim))?;
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
    let card = settings_card(&target, &plan, &winner.model, best)?;
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
        base_slug: winner.base_document.file.look.slug.to_string(),
        base_version: winner.base_document.file.look.version.to_string(),
        body: schema.body.to_string(),
        epoch: args.epoch,
        sim: best.sim.to_string(),
        crm_version: winner.model.version(),
        input_transform_version,
        realization_params_version: args.realization_version,
        epoch_map_version,
        probe_set: document.file.probe_set,
        optimizer: looklab_match::OPTIMIZER_VERSION.to_string(),
        emphasis,
        seed: args.seed,
        sources: PermalinkSources {
            target_look: Some(relative_source(&args.look, base_dir)),
            base_look: winner
                .paths
                .base
                .as_ref()
                .map(|path| relative_source(path, base_dir)),
            body: Some(relative_source(&body_path, base_dir)),
            crm: winner
                .paths
                .crm
                .as_ref()
                .map(|path| relative_source(path, base_dir)),
            transform: transform_path
                .as_ref()
                .map(|path| relative_source(path, base_dir)),
            covariance: winner
                .paths
                .covariance
                .as_ref()
                .map(|path| relative_source(path, base_dir)),
        },
        calibration: Calibration::default(),
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

struct SimPaths {
    base: Option<PathBuf>,
    crm: Option<PathBuf>,
    covariance: Option<PathBuf>,
}

struct LoadedSim {
    base_document: LookDocument,
    model: MatchModel,
    paths: SimPaths,
}

struct InputTransformVersion(u32);

struct LoadedMatchInputs {
    schema: ControlSchema,
    input_transform_version: InputTransformVersion,
    body_path: PathBuf,
    transform_path: Option<PathBuf>,
    sims: Vec<LoadedSim>,
}

fn validate_match_args(args: &MatchArgs) -> Result<()> {
    if args.bundle.is_some() {
        if args.crm.is_some() {
            bail!("--bundle supplies the control-response models; drop --crm");
        }
        if args.base_look.is_some() {
            bail!("--bundle supplies each sim's base Look; drop --base-look");
        }
        if args.body.is_some() {
            bail!("--bundle supplies the body schema snapshot; drop --body");
        }
        if args.transform.is_some() {
            bail!("--bundle supplies the input transform; drop --transform");
        }
        if args.covariance.is_some() {
            bail!("--bundle supplies each sim's covariance; drop --covariance");
        }
        if args.synthetic {
            bail!("--bundle cannot be combined with --synthetic");
        }
        return Ok(());
    }
    if !args.synthetic {
        if args.base_look.is_none() {
            bail!("--base-look is required unless --synthetic is given");
        }
        if args.transform.is_none() {
            bail!("--transform is required unless --synthetic is given");
        }
    }
    if args.crm.is_none() {
        bail!("--crm is required unless --bundle is given");
    }
    if args.body.is_none() {
        bail!("--body is required unless --bundle is given");
    }
    Ok(())
}

fn load_match_inputs(args: &MatchArgs, document: &LookDocument) -> Result<LoadedMatchInputs> {
    if let Some(bundle_path) = &args.bundle {
        let (dir, manifest, bundle) = load_bundle(bundle_path, args.epoch)?;
        let body_name = body_artifact(&bundle.body, bundle.epoch)?;
        let body_path = dir.join(body_name.as_str());
        let body_bytes = read_verified(&dir, &manifest, &body_name)?;
        let schema = decode_schema(&body_bytes, &body_path)?;
        if schema.body != bundle.body {
            bail!(
                "bundle body {} does not match the schema body {}",
                bundle.body,
                schema.body
            );
        }
        let transform_name = transform_artifact(&bundle.body, bundle.epoch)?;
        let transform_path = dir.join(transform_name.as_str());
        let transform_bytes = read_verified(&dir, &manifest, &transform_name)?;
        let transform_text = std::str::from_utf8(&transform_bytes)
            .with_context(|| format!("{} is not UTF-8", transform_path.display()))?;
        let transform_file = TransformFile::parse(transform_text)
            .with_context(|| format!("parsing {}", transform_path.display()))?;
        let transform = body_transform(&transform_file)?;
        let input_transform_version = transform_file.version;

        let mut sims = Vec::with_capacity(bundle.sims.len());
        for entry in &bundle.sims {
            let names = sim_artifacts(&bundle.body, bundle.epoch, &entry.sim)?;
            let base_path = dir.join(names.base.as_str());
            let base_bytes = read_verified(&dir, &manifest, &names.base)?;
            let base_document = LookDocument::decode(&base_bytes)
                .with_context(|| format!("decoding {}", base_path.display()))?;
            let crm_path = dir.join(names.crm.as_str());
            let crm_bytes = read_verified(&dir, &manifest, &names.crm)?;
            let crm = decode_crm_bytes(&crm_bytes, &crm_path)?;
            if crm.body != bundle.body || crm.epoch != bundle.epoch || crm.base_sim != entry.sim {
                bail!(
                    "{} does not describe {} on body {} epoch {}",
                    crm_path.display(),
                    entry.sim,
                    bundle.body,
                    bundle.epoch
                );
            }
            let (covariance, covariance_path) = if entry.covariance.is_some() {
                let path = dir.join(names.covariance.as_str());
                let bytes = read_verified(&dir, &manifest, &names.covariance)?;
                let covariance = decode_covariance_bytes(&bytes, &path)?;
                (Some(covariance), Some(path))
            } else {
                (None, None)
            };
            let model = MatchModel::new(
                base_document.look.clone(),
                crm,
                transform,
                covariance,
                input_transform_version,
                args.realization_version,
            )?;
            sims.push(LoadedSim {
                base_document,
                model,
                paths: SimPaths {
                    base: Some(base_path),
                    crm: Some(crm_path),
                    covariance: covariance_path,
                },
            });
        }
        if sims.is_empty() {
            bail!("the bundle lists no sims to rank");
        }
        return Ok(LoadedMatchInputs {
            schema,
            input_transform_version: InputTransformVersion(input_transform_version),
            body_path,
            transform_path: Some(transform_path),
            sims,
        });
    }

    let body_path = args
        .body
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--body is required unless --bundle is given"))?;
    let crm_path = args
        .crm
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--crm is required unless --bundle is given"))?;
    let schema = load_schema(&body_path)?;
    let crm = load_crm(&crm_path)?;
    if crm.epoch != args.epoch {
        bail!(
            "control-response model is for epoch {}, not {}",
            crm.epoch,
            args.epoch
        );
    }
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
    let model = MatchModel::new(
        base_document.look.clone(),
        crm,
        transform,
        covariance,
        input_transform_version,
        args.realization_version,
    )?;
    Ok(LoadedMatchInputs {
        schema,
        input_transform_version: InputTransformVersion(input_transform_version),
        body_path,
        transform_path: args.transform.clone(),
        sims: vec![LoadedSim {
            base_document,
            model,
            paths: SimPaths {
                base: args.base_look.clone(),
                crm: Some(crm_path),
                covariance: args.covariance.clone(),
            },
        }],
    })
}

fn load_bundle(path: &Path, epoch: u32) -> Result<(PathBuf, VersionManifest, MatchBundle)> {
    let (dir, descriptor) = if path.is_dir() {
        let mut found = Vec::new();
        for entry in
            std::fs::read_dir(path).with_context(|| format!("listing {}", path.display()))?
        {
            let candidate = entry?.path();
            if candidate
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("-sims.json"))
            {
                found.push(candidate);
            }
        }
        match found.as_slice() {
            [only] => (path.to_path_buf(), only.clone()),
            [] => bail!("{} holds no match bundle descriptor", path.display()),
            _ => bail!(
                "{} holds several match bundles; pass the -sims.json file",
                path.display()
            ),
        }
    } else {
        let dir = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        (dir, path.to_path_buf())
    };
    let name = descriptor
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("{} is not a UTF-8 artifact name", descriptor.display()))?;
    let descriptor_name = ArtifactName::new(name)
        .with_context(|| format!("{} is not an artifact name", descriptor.display()))?;
    let manifest = load_manifest(&dir)?;
    let bytes =
        std::fs::read(&descriptor).with_context(|| format!("reading {}", descriptor.display()))?;
    verify_artifact(&manifest, &descriptor_name, &bytes)
        .with_context(|| format!("verifying {}", descriptor.display()))?;
    let bundle = MatchBundle::decode(&bytes)
        .with_context(|| format!("decoding {}.", descriptor.display()))?;
    if descriptor_name != bundle_artifact(&bundle.body, bundle.epoch)? {
        bail!(
            "{} is not the descriptor for body {} epoch {}",
            descriptor.display(),
            bundle.body,
            bundle.epoch
        );
    }
    if bundle.epoch != epoch {
        bail!(
            "bundle {} is for epoch {}, not {}",
            descriptor.display(),
            bundle.epoch,
            epoch
        );
    }
    Ok((dir, manifest, bundle))
}

fn load_manifest(dir: &Path) -> Result<VersionManifest> {
    let path = dir.join(MANIFEST_NAME);
    let bytes = std::fs::read(&path).with_context(|| {
        format!(
            "reading {}; a bundle is loaded from its version directory",
            path.display()
        )
    })?;
    serde_json::from_slice(&bytes).with_context(|| format!("decoding {}", path.display()))
}

fn read_verified(dir: &Path, manifest: &VersionManifest, name: &ArtifactName) -> Result<Vec<u8>> {
    let path = dir.join(name.as_str());
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    verify_artifact(manifest, name, &bytes)
        .with_context(|| format!("verifying {}", path.display()))?;
    Ok(bytes)
}

#[derive(serde::Serialize)]
struct Replay<'a> {
    permalink: &'a str,
    seed: u64,
    sim: String,
    settings: &'a Settings,
    predicted_median_delta_e: f64,
    predicted_p95_delta_e: f64,
    first_measured_median: f64,
}

pub fn run_verify(args: &VerifyArgs) -> Result<()> {
    let (permalink, permalink_path) = resolve_permalink(&args.permalink)?;
    if permalink.optimizer != looklab_match::OPTIMIZER_VERSION {
        bail!(
            "permalink optimizer {:?} is not supported by {}",
            permalink.optimizer,
            looklab_match::OPTIMIZER_VERSION
        );
    }
    let emphasis = permalink.emphasis;
    let look_path = resolve_source(
        &permalink_path,
        permalink.sources.target_look.as_deref(),
        args.look.clone(),
    )
    .ok_or_else(|| anyhow::anyhow!("permalink has no target Look source; pass --look"))?;

    let document = util::load_document(&look_path)?;
    check_identity(
        document.file.look.slug.as_str(),
        document.file.look.version.as_str(),
        &permalink.target_slug,
        &permalink.target_version,
        "target Look",
    )?;
    if document.file.probe_set != permalink.probe_set {
        bail!("probe set does not match the permalink");
    }
    let epoch_map_version = document
        .file
        .provenance
        .bodies
        .first()
        .map(|body| body.epoch_map_version)
        .unwrap_or(0);
    if epoch_map_version != permalink.epoch_map_version {
        bail!(
            "epoch map version {epoch_map_version} does not match the permalink {}",
            permalink.epoch_map_version
        );
    }
    let context = verify_context(args, &permalink, &permalink_path, &document, &look_path)?;
    let plan = Plan::new(&context.schema, permalink.epoch)?;
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
    let files = util::samples_in(&args.observed)?;
    let mut observed = Vec::with_capacity(files.len());
    for file in &files {
        let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
        let samples = looklab_samples::decode(&bytes)
            .with_context(|| format!("decoding {}", file.display()))?;
        observed.push(ObservedRender { samples });
    }
    if observed.is_empty() {
        bail!("no observed renders in {}", args.observed.display());
    }
    let options = MatchOptions {
        seed: permalink.seed,
        ..MatchOptions::default()
    };
    let sim_keys: Vec<looklab_schema::SimKey> = context
        .sims
        .iter()
        .map(|sim| sim.model.sim().clone())
        .collect();
    permalink.calibration.validate(&sim_keys, &plan)?;
    let models: Vec<MatchModel> = context
        .sims
        .iter()
        .map(|sim| permalink.calibration.apply(&sim.model))
        .collect::<looklab_match::Result<Vec<_>>>()?;
    let first_outcome = optimize(&models, &target, &plan, &options)?;
    let first = first_outcome
        .results
        .first()
        .ok_or_else(|| anyhow::anyhow!("no settings suggestion"))?;
    if first.sim.as_str() != permalink.sim.as_str() {
        bail!(
            "the pinned sim {} is not the winner of its search context ({})",
            permalink.sim,
            first.sim
        );
    }
    let winner_index = context
        .sims
        .iter()
        .position(|sim| sim.model.sim() == &first.sim)
        .ok_or_else(|| anyhow::anyhow!("no loaded sim for {}", first.sim))?;
    let winner = &context.sims[winner_index];
    let (fitted, scales) = ScaleFitter.fit_with_scales(
        &models[winner_index],
        &plan,
        &first.settings,
        &observed,
        &FitObservedOptions::default(),
    )?;
    let updated = models[winner_index].with_crm(fitted)?;
    let next_calibration = permalink.calibration.merged(&first.sim, &scales)?;
    let mut updated_models = models;
    updated_models[winner_index] = updated.clone();
    let second_outcome = optimize(&updated_models, &target, &plan, &options)?;
    let second = second_outcome
        .results
        .first()
        .ok_or_else(|| anyhow::anyhow!("no settings suggestion"))?;
    let second_index = context
        .sims
        .iter()
        .position(|sim| sim.model.sim() == &second.sim)
        .ok_or_else(|| anyhow::anyhow!("no loaded sim for {}", second.sim))?;
    let second_sim = &context.sims[second_index];
    let card_model = if second.sim == first.sim {
        &updated
    } else {
        &updated_models[second_index]
    };
    let card = settings_card(&target, &plan, card_model, second)?;
    let first_measured = measure_target_error(&target, &winner.model, &first.settings, &observed)?;

    let crm_out = args
        .out_crm
        .clone()
        .unwrap_or_else(|| sibling(&permalink_path, "updated.crm.json"));
    let permalink_out = args
        .out_permalink
        .clone()
        .unwrap_or_else(|| sibling(&permalink_path, "updated.permalink"));
    let result_out = args
        .out_result
        .clone()
        .unwrap_or_else(|| sibling(&permalink_path, "updated.result.json"));
    let base_dir = permalink_out
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure_output_is_not_a_pinned_crm(
        &crm_out,
        context
            .sims
            .iter()
            .filter_map(|sim| sim.paths.crm.as_deref()),
    )?;
    let crm_bytes = encode_crm(&updated.crm)?;
    crate::write(&crm_out, &crm_bytes)?;
    let mut next = permalink.clone();
    next.sources.target_look = Some(relative_source(&look_path, base_dir));
    next.sources.body = Some(relative_source(&context.body_path, base_dir));
    next.sources.transform = context
        .transform_path
        .as_ref()
        .map(|path| relative_source(path, base_dir));
    next.sim = second.sim.to_string();
    next.base_slug = second_sim.base_document.file.look.slug.to_string();
    next.base_version = second_sim.base_document.file.look.version.to_string();
    next.crm_version = second_sim.model.version();
    next.sources.base_look = second_sim
        .paths
        .base
        .as_ref()
        .map(|path| relative_source(path, base_dir));
    next.sources.crm = second_sim
        .paths
        .crm
        .as_ref()
        .map(|path| relative_source(path, base_dir));
    next.sources.covariance = second_sim
        .paths
        .covariance
        .as_ref()
        .map(|path| relative_source(path, base_dir));
    next.calibration = next_calibration;
    let canonical = next.canonical();
    crate::write(&permalink_out, canonical.as_bytes())?;
    let replay = Replay {
        permalink: &canonical,
        seed: next.seed,
        sim: second.sim.to_string(),
        settings: &second.settings,
        predicted_median_delta_e: second.median_delta_e,
        predicted_p95_delta_e: second.p95_delta_e,
        first_measured_median: first_measured.median,
    };
    let replay_bytes = serde_json::to_vec_pretty(&replay)?;
    crate::write(&result_out, &replay_bytes)?;

    println!(
        "verify: first suggestion {}, measured median ΔE {:.2} on {} observed render(s)",
        first.sim,
        first_measured.median,
        observed.len()
    );
    println!(
        "  second suggestion: median ΔE {:.2} (first predicted {:.2})",
        second.median_delta_e, first.median_delta_e
    );
    println!("  re-render at the second settings to measure the reduction");
    println!("second settings:");
    print!("{}", card.render_text());
    println!("  calibrated CRM for {}", first.sim);
    println!("wrote {}", crm_out.display());
    println!("wrote {}", permalink_out.display());
    println!("wrote {}", result_out.display());
    println!(
        "  re-render at the second settings, then run verify --permalink {}",
        permalink_out.display()
    );
    Ok(())
}

struct VerifySim {
    base_document: LookDocument,
    model: MatchModel,
    paths: SimPaths,
    hashes: Option<SimHashes>,
}

struct SimHashes {
    base: ArtifactHash,
    crm: ArtifactHash,
    covariance: Option<ArtifactHash>,
}

struct VerifyContext {
    schema: ControlSchema,
    body_path: PathBuf,
    transform_path: Option<PathBuf>,
    sims: Vec<VerifySim>,
}

fn verify_context(
    args: &VerifyArgs,
    permalink: &Permalink,
    permalink_path: &Path,
    document: &LookDocument,
    look_path: &Path,
) -> Result<VerifyContext> {
    let bundle_path = match &args.bundle {
        Some(path) => Some(path.clone()),
        None => discover_bundle(permalink, permalink_path)?,
    };
    if bundle_path.is_none() && pins_bundle_artifacts(permalink)? {
        bail!(
            "this permalink pins a published bundle's model or base, but its bundle is not beside the pinned files; pass --bundle <version-dir>"
        );
    }
    if let Some(path) = bundle_path {
        for (flag, present) in [
            ("--crm", args.crm.is_some()),
            ("--base-look", args.base_look.is_some()),
            ("--covariance", args.covariance.is_some()),
            ("--body", args.body.is_some()),
            ("--transform", args.transform.is_some()),
            ("--synthetic", args.synthetic),
        ] {
            if present {
                bail!("a match bundle supplies its own verified inputs; drop {flag}");
            }
        }
        return load_bundle_context(&path, permalink, permalink_path);
    }
    load_single_context(args, permalink, permalink_path, document, look_path)
}

fn discover_bundle(permalink: &Permalink, permalink_path: &Path) -> Result<Option<PathBuf>> {
    let body = looklab_schema::BodyId::new(permalink.body.as_str())
        .map_err(|error| anyhow::anyhow!("permalink body {:?}: {error}", permalink.body))?;
    let name = bundle_artifact(&body, permalink.epoch)?;
    let mut dirs = vec![permalink_dir(permalink_path)];
    for source in [
        permalink.sources.crm.as_deref(),
        permalink.sources.base_look.as_deref(),
        permalink.sources.covariance.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let path = resolve_source(permalink_path, Some(source), None);
        if let Some(dir) = path.as_deref().and_then(Path::parent) {
            dirs.push(dir.to_path_buf());
        }
    }
    for dir in dirs {
        let candidate = dir.join(name.as_str());
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn permalink_dir(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

fn pins_bundle_artifacts(permalink: &Permalink) -> Result<bool> {
    let body = looklab_schema::BodyId::new(permalink.body.as_str())
        .map_err(|error| anyhow::anyhow!("permalink body {:?}: {error}", permalink.body))?;
    let sim = looklab_schema::SimKey::new(permalink.sim.as_str())
        .map_err(|error| anyhow::anyhow!("permalink sim {:?}: {error}", permalink.sim))?;
    let names = sim_artifacts(&body, permalink.epoch, &sim)?;
    Ok(names_artifact(permalink.sources.crm.as_deref(), &names.crm)
        || names_artifact(permalink.sources.base_look.as_deref(), &names.base))
}

fn names_artifact(source: Option<&str>, name: &ArtifactName) -> bool {
    source
        .and_then(|path| Path::new(path).file_name())
        .and_then(|file| file.to_str())
        == Some(name.as_str())
}

fn load_bundle_context(
    path: &Path,
    permalink: &Permalink,
    permalink_path: &Path,
) -> Result<VerifyContext> {
    let (dir, manifest, bundle) = load_bundle(path, permalink.epoch)?;
    if bundle.body.as_str() != permalink.body.as_str() {
        bail!(
            "bundle {} is for body {}, not {}",
            path.display(),
            bundle.body,
            permalink.body
        );
    }
    let body_name = body_artifact(&bundle.body, bundle.epoch)?;
    let body_path = dir.join(body_name.as_str());
    let body_bytes = read_verified(&dir, &manifest, &body_name)?;
    let body_hash = ArtifactHash::of(&body_bytes);
    let schema = decode_schema(&body_bytes, &body_path)?;
    if schema.body != bundle.body {
        bail!(
            "bundle body {} does not match the schema body {}",
            bundle.body,
            schema.body
        );
    }
    let transform_name = transform_artifact(&bundle.body, bundle.epoch)?;
    let transform_path = dir.join(transform_name.as_str());
    let transform_bytes = read_verified(&dir, &manifest, &transform_name)?;
    let transform_hash = ArtifactHash::of(&transform_bytes);
    let transform_text = std::str::from_utf8(&transform_bytes)
        .with_context(|| format!("{} is not UTF-8", transform_path.display()))?;
    let transform_file = TransformFile::parse(transform_text)
        .with_context(|| format!("parsing {}", transform_path.display()))?;
    if transform_file.version != permalink.input_transform_version {
        bail!(
            "input transform version {} does not match the permalink {}",
            transform_file.version,
            permalink.input_transform_version
        );
    }
    let transform = body_transform(&transform_file)?;
    let mut sims = Vec::with_capacity(bundle.sims.len());
    for entry in &bundle.sims {
        let names = sim_artifacts(&bundle.body, bundle.epoch, &entry.sim)?;
        let base_path = dir.join(names.base.as_str());
        let base_bytes = read_verified(&dir, &manifest, &names.base)?;
        let base_hash = ArtifactHash::of(&base_bytes);
        let base_document = LookDocument::decode(&base_bytes)
            .with_context(|| format!("decoding {}", base_path.display()))?;
        let crm_path = dir.join(names.crm.as_str());
        let crm_bytes = read_verified(&dir, &manifest, &names.crm)?;
        let crm_hash = ArtifactHash::of(&crm_bytes);
        let crm = decode_crm_bytes(&crm_bytes, &crm_path)?;
        if crm.body != bundle.body || crm.epoch != bundle.epoch || crm.base_sim != entry.sim {
            bail!(
                "{} does not describe {} on body {} epoch {}",
                crm_path.display(),
                entry.sim,
                bundle.body,
                bundle.epoch
            );
        }
        let (covariance, covariance_path, covariance_hash) = if entry.covariance.is_some() {
            let covariance_path = dir.join(names.covariance.as_str());
            let bytes = read_verified(&dir, &manifest, &names.covariance)?;
            let covariance_hash = ArtifactHash::of(&bytes);
            let covariance = decode_covariance_bytes(&bytes, &covariance_path)?;
            (
                Some(covariance),
                Some(covariance_path),
                Some(covariance_hash),
            )
        } else {
            (None, None, None)
        };
        let model = MatchModel::new(
            base_document.look.clone(),
            crm,
            transform,
            covariance,
            transform_file.version,
            permalink.realization_params_version,
        )?;
        sims.push(VerifySim {
            base_document,
            model,
            paths: SimPaths {
                base: Some(base_path),
                crm: Some(crm_path),
                covariance: covariance_path,
            },
            hashes: Some(SimHashes {
                base: base_hash,
                crm: crm_hash,
                covariance: covariance_hash,
            }),
        });
    }
    if sims.is_empty() {
        bail!("the bundle lists no sims");
    }
    let winner = sims
        .iter()
        .position(|sim| sim.model.sim().as_str() == permalink.sim.as_str())
        .ok_or_else(|| anyhow::anyhow!("the pinned sim {} is not in the bundle", permalink.sim))?;
    if sims[winner].model.version() != permalink.crm_version {
        bail!(
            "the pinned control-response model version {} is not the bundle's {}",
            permalink.crm_version,
            sims[winner].model.version()
        );
    }
    let hashes = sims[winner].hashes.as_ref().ok_or_else(|| {
        anyhow::anyhow!("the bundle has no verified content for {}", permalink.sim)
    })?;
    check_pinned_source(
        "body schema",
        permalink.sources.body.as_deref(),
        permalink_path,
        &body_hash,
    )?;
    check_pinned_source(
        "transform",
        permalink.sources.transform.as_deref(),
        permalink_path,
        &transform_hash,
    )?;
    check_pinned_source(
        "base realization",
        permalink.sources.base_look.as_deref(),
        permalink_path,
        &hashes.base,
    )?;
    check_pinned_source(
        "control-response model",
        permalink.sources.crm.as_deref(),
        permalink_path,
        &hashes.crm,
    )?;
    match (&hashes.covariance, permalink.sources.covariance.as_deref()) {
        (Some(expected), Some(source)) => {
            check_pinned_source("covariance", Some(source), permalink_path, expected)?;
        }
        (None, None) => {}
        _ => bail!("the bundle's covariance does not match the permalink"),
    }
    check_identity(
        sims[winner].base_document.file.look.slug.as_str(),
        sims[winner].base_document.file.look.version.as_str(),
        &permalink.base_slug,
        &permalink.base_version,
        "base Look",
    )?;
    Ok(VerifyContext {
        schema,
        body_path,
        transform_path: Some(transform_path),
        sims,
    })
}

fn check_pinned_source(
    what: &str,
    source: Option<&str>,
    permalink_path: &Path,
    expected: &ArtifactHash,
) -> Result<()> {
    let Some(source) = source else {
        return Ok(());
    };
    let path = resolve_source(permalink_path, Some(source), None)
        .ok_or_else(|| anyhow::anyhow!("the permalink has no {what} source"))?;
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    if ArtifactHash::of(&bytes).as_str() != expected.as_str() {
        bail!(
            "the pinned {what} source {} does not match the bundle content",
            path.display()
        );
    }
    Ok(())
}

fn load_single_context(
    args: &VerifyArgs,
    permalink: &Permalink,
    permalink_path: &Path,
    document: &LookDocument,
    look_path: &Path,
) -> Result<VerifyContext> {
    let body_path = resolve_source(
        permalink_path,
        permalink.sources.body.as_deref(),
        args.body.clone(),
    )
    .ok_or_else(|| anyhow::anyhow!("permalink has no body schema source; pass --body"))?;
    let crm_path = resolve_source(
        permalink_path,
        permalink.sources.crm.as_deref(),
        args.crm.clone(),
    )
    .ok_or_else(|| anyhow::anyhow!("permalink has no control-response model; pass --crm"))?;
    let base_path = resolve_source(
        permalink_path,
        permalink.sources.base_look.as_deref(),
        args.base_look.clone(),
    );
    let transform_path = resolve_source(
        permalink_path,
        permalink.sources.transform.as_deref(),
        args.transform.clone(),
    );
    let covariance_path = resolve_source(
        permalink_path,
        permalink.sources.covariance.as_deref(),
        args.covariance.clone(),
    );
    if !args.synthetic {
        if base_path.is_none() {
            bail!("--base-look is required unless --synthetic is given");
        }
        if transform_path.is_none() {
            bail!("--transform is required unless --synthetic is given");
        }
    }
    let schema = load_schema(&body_path)?;
    if schema.body.as_str() != permalink.body.as_str() {
        bail!(
            "control schema is for body {}, not the permalink body {}",
            schema.body,
            permalink.body
        );
    }
    let crm = load_crm(&crm_path)?;
    if crm.body.as_str() != permalink.body.as_str()
        || crm.epoch != permalink.epoch
        || crm.base_sim.as_str() != permalink.sim.as_str()
        || crm.version != permalink.crm_version
    {
        bail!("control-response model does not match the permalink");
    }
    let base_document = match &base_path {
        Some(path) => util::load_document(path)?,
        None => document.clone(),
    };
    check_identity(
        base_document.file.look.slug.as_str(),
        base_document.file.look.version.as_str(),
        &permalink.base_slug,
        &permalink.base_version,
        "base Look",
    )?;
    let (transform, transform_version) = match &transform_path {
        Some(path) => {
            let file = TransformFile::load(path)?;
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
    let covariance = match &covariance_path {
        Some(path) => Some(load_covariance(path)?),
        None => None,
    };
    let model = MatchModel::new(
        base_document.look.clone(),
        crm,
        transform,
        covariance,
        permalink.input_transform_version,
        permalink.realization_params_version,
    )?;
    Ok(VerifyContext {
        schema,
        body_path,
        transform_path,
        sims: vec![VerifySim {
            base_document,
            model,
            paths: SimPaths {
                base: Some(base_path.unwrap_or_else(|| look_path.to_path_buf())),
                crm: Some(crm_path),
                covariance: covariance_path,
            },
            hashes: None,
        }],
    })
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("match");
    path.with_file_name(format!("{name}.{suffix}"))
}

fn load_schema(path: &Path) -> Result<ControlSchema> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    decode_schema(&bytes, path)
}

fn decode_schema(bytes: &[u8], path: &Path) -> Result<ControlSchema> {
    let text =
        std::str::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))?;
    looklab_schema::validate_control_schema(text)
        .map_err(|errors| anyhow::anyhow!("invalid control schema: {errors:?}"))
}

fn load_crm(path: &Path) -> Result<ControlResponseModel> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    decode_crm_bytes(&bytes, path)
}

fn decode_crm_bytes(bytes: &[u8], path: &Path) -> Result<ControlResponseModel> {
    decode_crm(bytes).with_context(|| format!("decoding CRM {}", path.display()))
}

fn load_covariance(path: &Path) -> Result<ControlCovariance> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    decode_covariance_bytes(&bytes, path)
}

fn decode_covariance_bytes(bytes: &[u8], path: &Path) -> Result<ControlCovariance> {
    ControlCovariance::from_json(bytes)
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
    let base = if base.as_os_str().is_empty() {
        Path::new(".")
    } else {
        base
    };
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

fn ensure_output_is_not_a_pinned_crm<'a>(
    output: &Path,
    pinned: impl IntoIterator<Item = &'a Path>,
) -> Result<()> {
    for source in pinned {
        if same_file(output, source)? {
            bail!(
                "the output CRM {} aliases the pinned control-response model {}; choose another path",
                output.display(),
                source.display()
            );
        }
    }
    Ok(())
}

fn same_file(output: &Path, source: &Path) -> Result<bool> {
    if !output
        .try_exists()
        .with_context(|| format!("checking {}", output.display()))?
    {
        return Ok(false);
    }
    is_same_file(output, source)
        .with_context(|| format!("comparing {} and {}", output.display(), source.display()))
}

#[cfg(test)]
mod tests {
    use super::{check_pinned_source, ensure_output_is_not_a_pinned_crm};
    use looklab_publish::ArtifactHash;
    use std::path::Path;

    #[test]
    fn output_crm_cannot_alias_any_pinned_model() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("first.crm.json");
        std::fs::write(&first, b"first").expect("write");
        let second = dir.path().join("second.crm.json");
        std::fs::write(&second, b"second").expect("write");
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).expect("sub");
        let dotted = sub.join("..").join("first.crm.json");
        let pinned: Vec<&Path> = vec![first.as_path(), second.as_path()];
        assert!(ensure_output_is_not_a_pinned_crm(&dotted, pinned.iter().copied()).is_err());
        assert!(ensure_output_is_not_a_pinned_crm(&second, pinned.iter().copied()).is_err());
        assert!(
            ensure_output_is_not_a_pinned_crm(
                &dir.path().join("fresh.crm.json"),
                pinned.iter().copied()
            )
            .is_ok()
        );
        let hard = dir.path().join("hard.crm.json");
        std::fs::hard_link(&first, &hard).expect("hard link");
        assert!(ensure_output_is_not_a_pinned_crm(&hard, pinned.iter().copied()).is_err());
        #[cfg(unix)]
        {
            let symlink = dir.path().join("symlink.crm.json");
            std::os::unix::fs::symlink(&first, &symlink).expect("symlink");
            assert!(ensure_output_is_not_a_pinned_crm(&symlink, pinned.iter().copied()).is_err());
        }
    }

    #[test]
    fn pinned_sources_must_match_bundle_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let permalink = dir.path().join("recipe.permalink");
        let pinned = dir.path().join("transform.yaml");
        std::fs::write(&pinned, b"substituted").expect("write");
        let expected = ArtifactHash::of(b"original");
        assert!(
            check_pinned_source("transform", Some("transform.yaml"), &permalink, &expected)
                .is_err()
        );
        std::fs::write(&pinned, b"original").expect("write");
        check_pinned_source("transform", Some("transform.yaml"), &permalink, &expected)
            .expect("matching content");
        check_pinned_source("transform", None, &permalink, &expected).expect("unpinned");
        assert!(
            check_pinned_source("transform", Some("missing.yaml"), &permalink, &expected).is_err()
        );
    }
}
