// SPDX-License-Identifier: AGPL-3.0-or-later
//! `looklab match` and `looklab verify` (docs/04 §3-§5).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use looklab_color::Mat3;
use looklab_match::{
    Calibration, ControlCovariance, Emphasis, FitObservedOptions, MatchModel, MatchOptions,
    ObservedRender, OutputPin, Permalink, PermalinkSources, Plan, PreparedTarget, QualificationRef,
    ReportPin, ReportSubject, ScaleFitter, ScoringMetric, SelectionContext, SelectionMode,
    TargetAppearance, TargetSpec, measure_target_error, optimize, optimize_scoped,
    settings_card_with_support,
};
use looklab_model::ProbeSetVersion;
use looklab_model::file::LookDocument;
use looklab_model::realization::{ControlResponseModel, Settings, decode_crm, encode_crm};
use looklab_publish::{
    ArtifactHash, ArtifactName, MANIFEST_NAME, MatchBundle, TargetEligibilitySnapshot,
    VersionManifest, body_artifact, bundle_artifact, sim_artifacts, transform_artifact,
    verify_artifact,
};
use looklab_schema::{
    ControlSchema, OutputEncoding, RealizationRevisionId, Sha256Hex, TargetEligibilityStatus,
    TargetVersion,
};
use same_file::is_same_file;

use crate::body_transform::TransformFile;
use crate::util;

mod qualification;

/// Inert output encoding for pre-clip replay, which never renders the pin.
const LEGACY_REPLAY_OUTPUT: OutputEncoding = OutputEncoding::Srgb;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum SelectionArg {
    Qualified,
    Experimental,
}

impl From<SelectionArg> for looklab_match::SelectionMode {
    fn from(value: SelectionArg) -> Self {
        match value {
            SelectionArg::Qualified => Self::Qualified,
            SelectionArg::Experimental => Self::Experimental,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputEncodingArg {
    Srgb,
    AdobeRgb,
}

impl From<OutputEncodingArg> for OutputEncoding {
    fn from(value: OutputEncodingArg) -> Self {
        match value {
            OutputEncodingArg::Srgb => Self::Srgb,
            OutputEncodingArg::AdobeRgb => Self::AdobeRgb,
        }
    }
}

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
    /// Requested recipe output encoding.
    #[arg(long, default_value = "srgb")]
    pub output_encoding: OutputEncodingArg,
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
    /// Positive realization revision for unbundled or historical bundle inputs.
    #[arg(long)]
    pub realization_version: Option<u32>,
    /// Published controls-report artifact to pin for a qualified search.
    #[arg(long)]
    pub controls_report: Option<PathBuf>,
    /// Published `looklab-match-params/1` shared body parameters.
    #[arg(long)]
    pub params: Option<PathBuf>,
    /// Published `looklab-match-params/1` per-sim realization/firmware parameters.
    #[arg(long)]
    pub realization_params: Option<PathBuf>,
    /// Publication root authenticating a camera bundle's inventory and manifest.
    #[arg(long)]
    pub publication: Option<PathBuf>,
    /// Published target eligibility snapshot that authorizes the selected target.
    #[arg(long)]
    pub target_eligibility: Option<PathBuf>,
    /// Selection mode recorded in the permalink.
    #[arg(long)]
    pub selection: Option<SelectionArg>,
    /// Match bundle revision id pinned by a selected permalink.
    #[arg(long)]
    pub bundle_revision: Option<String>,
    /// Pinned firmware identity, `body@epoch:version`.
    #[arg(long)]
    pub firmware: Option<String>,
    /// Pinned logical white-balance mode.
    #[arg(long)]
    pub wb_mode: Option<String>,
    /// Assessment pin, `subject=<report:hex|eligibility:hex>`; repeatable.
    #[arg(long = "report")]
    pub reports: Vec<String>,
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
    /// Published `looklab-match-params/1` shared body parameters.
    #[arg(long)]
    pub params: Option<PathBuf>,
    /// Published `looklab-match-params/1` per-sim realization/firmware parameters.
    #[arg(long)]
    pub realization_params: Option<PathBuf>,
    /// Publication root holding `data/index.json`, reports and eligibility snapshots.
    #[arg(long)]
    pub publication: Option<PathBuf>,
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
    let selection = selection_context(args, &document)?;
    let output_encoding = OutputEncoding::from(args.output_encoding);
    let LoadedMatchInputs {
        schema,
        input_transform_version: InputTransformVersion(input_transform_version),
        body_path,
        transform_path,
        mut sims,
        inventory_verified,
        ..
    } = load_match_inputs(args, &document, selection.as_ref(), output_encoding)?;
    if selection.is_some() && !inventory_verified {
        bail!(
            "selected matching needs a publication root authenticating the bundle inventory; pass --publication"
        );
    }
    let (wb_mode, firmware) = selection.as_ref().map_or((None, None), |context| {
        (context.wb_mode.clone(), context.firmware.clone())
    });
    for sim in &mut sims {
        sim.model.wb_mode = wb_mode.clone();
        sim.model.firmware = firmware.clone();
    }
    if selection.is_some()
        && sims
            .iter()
            .any(|sim| sim.paths.params.is_none() || sim.model.realization.is_none())
    {
        bail!(
            "a selected permalink needs pinned body and realization parameters for every ranked sim; pass --params/--realization-params or a camera bundle"
        );
    }
    let plan = Plan::new(&schema, args.epoch)?;
    let models: Vec<MatchModel> = sims.iter().map(|sim| sim.model.clone()).collect();
    let assessments = qualification::load(args.publication.as_deref(), selection.as_ref())?;
    if let (Some(assessments), Some(selection)) = (&assessments, &selection) {
        qualification::validate_target(
            assessments,
            selection,
            &document,
            &args.look,
            qualification::TargetUse::Match,
        )?;
    }
    let domains = qualification::domains(
        &plan,
        &models,
        selection.as_ref(),
        assessments.as_ref(),
        Some(output_encoding),
    )?;
    let (probes, probe_indices) = qualification::probes(&domains)?;
    if document.file.probe_set != ProbeSetVersion::V1 {
        bail!("unsupported probe set");
    }
    let requested_points = looklab_model::probes::v1().probes.len();
    let uncertainty = document.uncertainty.restrict_to(&probe_indices)?;
    let target = PreparedTarget::new(TargetSpec {
        probes: &probes,
        function: &document.look,
        uncertainty: Some(&uncertainty),
        appearance: TargetAppearance::Rendered(OutputPin::from_document(&document)?),
        emphasis,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
    })?
    .with_declared_points(requested_points)?;
    let options = MatchOptions {
        seed: args.seed,
        ..MatchOptions::default()
    };
    let outcome = if domains.is_empty() {
        optimize(&models, &target, &plan, &options)?
    } else {
        optimize_scoped(&models, &target, &plan, &options, &domains)?
    };
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
        let band = match suggestion.sigma_band {
            Some(band) => format!("band ±{band:.2}"),
            None => "band unavailable".to_owned(),
        };
        println!(
            "  #{}. {}: median ΔE {:.2}, p95 {:.2}, {band}",
            index + 1,
            suggestion.sim,
            suggestion.median_delta_e,
            suggestion.p95_delta_e
        );
    }
    let card =
        settings_card_with_support(&target, &plan, &winner.model, best, domains.get(&best.sim))?;
    print!("{}", card.render_text());
    match &selection {
        Some(context) => println!(
            "selection: {} (bundle {}, {} report pins)",
            context.mode.as_str(),
            context.bundle.to_hex(),
            context.reports.len()
        ),
        None => println!("selection: legacy unassessed"),
    }
    println!(
        "recipe output encoding: {} (requested setting)",
        output_encoding.as_str()
    );

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
        realization_params_version: winner.model.realization_params_version,
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
        selection,
        metric: ScoringMetric::RenderedPostClipV1,
        output: Some(target.output_context(&winner.model, &best.settings)?),
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
    params: Option<PathBuf>,
}

struct LoadedSim {
    base_document: LookDocument,
    model: MatchModel,
    paths: SimPaths,
    hashes: Option<SimHashes>,
}

struct InputTransformVersion(u32);

struct LoadedMatchInputs {
    schema: ControlSchema,
    input_transform_version: InputTransformVersion,
    body_path: PathBuf,
    transform_path: Option<PathBuf>,
    body_hash: Option<ArtifactHash>,
    transform_hash: Option<ArtifactHash>,
    sims: Vec<LoadedSim>,
    inventory_verified: bool,
}

fn pinned_realization_version(args: &MatchArgs) -> Result<u32> {
    let version = args
        .realization_version
        .ok_or_else(|| anyhow::anyhow!("--realization-version is required to pin a realization"))?;
    if version == 0 {
        bail!("--realization-version must be positive");
    }
    Ok(version)
}

fn selection_context(
    args: &MatchArgs,
    document: &LookDocument,
) -> Result<Option<SelectionContext>> {
    let eligibility = match &args.target_eligibility {
        Some(path) => {
            let bytes =
                std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            let eligibility: TargetEligibilitySnapshot = serde_json::from_slice(&bytes)
                .with_context(|| format!("decoding {}", path.display()))?;
            if eligibility.status != TargetEligibilityStatus::Eligible {
                bail!(
                    "the target eligibility snapshot is {:?}",
                    eligibility.status
                );
            }
            let version = document
                .file
                .look
                .version
                .as_str()
                .split('.')
                .next()
                .and_then(|part| part.parse::<u32>().ok())
                .and_then(TargetVersion::new);
            if eligibility.target.look.as_str() != document.file.look.slug.as_str()
                || Some(eligibility.target.version) != version
            {
                bail!(
                    "the target eligibility snapshot is for {}@{}",
                    eligibility.target.look,
                    eligibility.target.version.get()
                );
            }
            Some(eligibility)
        }
        None => None,
    };
    let Some(mode) = args.selection else {
        if args.bundle_revision.is_some()
            || !args.reports.is_empty()
            || eligibility.is_some()
            || args.controls_report.is_some()
            || args.firmware.is_some()
            || args.wb_mode.is_some()
        {
            bail!(
                "--bundle-revision, --report, --controls-report, --target-eligibility, --firmware and --wb-mode require --selection"
            );
        }
        return Ok(None);
    };
    let bundle = args
        .bundle_revision
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--selection requires --bundle-revision"))?;
    let bundle = Sha256Hex::parse(bundle)
        .map_err(|_| anyhow::anyhow!("--bundle-revision must be 64 lowercase hex characters"))?;
    let mut reports = Vec::new();
    for pin in &args.reports {
        let (subject, reference) = pin
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--report expects subject=<ref>"))?;
        reports.push(ReportPin {
            subject: ReportSubject::parse(subject).map_err(|error| anyhow::anyhow!("{error}"))?,
            reference: QualificationRef::parse(reference)
                .map_err(|error| anyhow::anyhow!("{error}"))?,
        });
    }
    if let Some(path) = &args.controls_report {
        let pin = qualification::controls_pin(path)?;
        if !reports.contains(&pin) {
            reports.push(pin);
        }
    }
    if let Some(eligibility) = eligibility {
        let pin = ReportPin {
            subject: ReportSubject::Target(eligibility.target),
            reference: QualificationRef::Eligibility(eligibility.id),
        };
        if !reports.contains(&pin) {
            reports.push(pin);
        }
    }
    let firmware = args
        .firmware
        .as_deref()
        .map(looklab_schema::FirmwareId::parse)
        .transpose()
        .map_err(|error| anyhow::anyhow!(error))?;
    let wb_mode = args
        .wb_mode
        .as_deref()
        .map(looklab_schema::WbModeKey::new)
        .transpose()
        .map_err(|error| anyhow::anyhow!(error))?;
    let mode = SelectionMode::from(mode);
    if mode == SelectionMode::Qualified
        && !reports
            .iter()
            .any(|pin| matches!(pin.subject, ReportSubject::Target(_)))
    {
        bail!(
            "a qualified selection needs a target assessment pin; pass --target-eligibility or --report <look@version>=report:<hex>"
        );
    }
    let context = SelectionContext {
        bundle,
        mode,
        reports,
        firmware,
        wb_mode,
    };
    context
        .validate()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(Some(context))
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

fn load_match_inputs(
    args: &MatchArgs,
    document: &LookDocument,
    selection: Option<&SelectionContext>,
    output_encoding: OutputEncoding,
) -> Result<LoadedMatchInputs> {
    if let Some(bundle_path) = &args.bundle {
        if let Some(loaded) = load_camera_bundle(
            bundle_path,
            args.epoch,
            args.wb_mode.as_deref(),
            args.firmware.as_deref(),
            args.publication.as_deref(),
            selection.map(|selection| {
                looklab_schema::MatchBundleRevisionId::from_digest(*selection.bundle.as_bytes())
            }),
            output_encoding,
        )? {
            return Ok(loaded);
        }
        let realization_version = pinned_realization_version(args)?;
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
                realization_version,
                output_encoding,
            )?;
            sims.push(LoadedSim {
                base_document,
                model,
                paths: SimPaths {
                    base: Some(base_path),
                    crm: Some(crm_path),
                    covariance: covariance_path,
                    params: None,
                },
                hashes: None,
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
            body_hash: None,
            transform_hash: None,
            sims,
            inventory_verified: false,
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
    let params = match &args.params {
        Some(path) => {
            let bytes =
                std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            let params: looklab_schema::MatchParams = serde_json::from_slice(&bytes)
                .with_context(|| format!("decoding {}", path.display()))?;
            params
                .validate(&schema.body)
                .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
            Some(params)
        }
        None => None,
    };
    let realization_version = pinned_realization_version(args)?;
    let mut model = MatchModel::new(
        base_document.look.clone(),
        crm,
        transform,
        covariance,
        input_transform_version,
        realization_version,
        output_encoding,
    )?;
    if let Some(params) = params {
        model = model.with_params(params)?;
    }
    if let Some(path) = &args.realization_params {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let params: looklab_schema::MatchRealizationParams = serde_json::from_slice(&bytes)
            .with_context(|| format!("decoding {}", path.display()))?;
        params
            .validate()
            .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
        model = model.with_realization(params);
    }
    Ok(LoadedMatchInputs {
        schema,
        input_transform_version: InputTransformVersion(input_transform_version),
        body_path,
        transform_path: args.transform.clone(),
        body_hash: None,
        transform_hash: None,
        inventory_verified: false,
        sims: vec![LoadedSim {
            base_document,
            model,
            paths: SimPaths {
                base: args.base_look.clone(),
                crm: Some(crm_path),
                covariance: args.covariance.clone(),
                params: args.params.clone(),
            },
            hashes: None,
        }],
    })
}

fn load_camera_bundle(
    path: &Path,
    epoch: u32,
    wb_mode: Option<&str>,
    firmware: Option<&str>,
    publication: Option<&Path>,
    expected_bundle: Option<looklab_schema::MatchBundleRevisionId>,
    output_encoding: OutputEncoding,
) -> Result<Option<LoadedMatchInputs>> {
    let dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    };
    let manifest_path = dir.join(MANIFEST_NAME);
    if !manifest_path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    if !text.contains("\"schema_snapshot_version\"") {
        return Ok(None);
    }
    let manifest = looklab_schema::MatchBundleRevision::parse_manifest(&text, 0)
        .with_context(|| format!("decoding {}", manifest_path.display()))?;
    if expected_bundle.is_some_and(|expected| expected != manifest.id) {
        bail!(
            "{} does not match the pinned bundle revision",
            manifest_path.display()
        );
    }
    if manifest.epoch.get() != epoch {
        bail!(
            "bundle {} is for epoch {}, not {}",
            manifest_path.display(),
            manifest.epoch.get(),
            epoch
        );
    }
    let (inventory, inventory_verified) =
        authenticated_inventory(&dir, publication, &manifest.body, epoch, &manifest.id)?;
    if inventory_verified {
        for entry in &inventory {
            let path = dir.join(entry.name.as_str());
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            if ArtifactHash::of(&bytes) != entry.sha256 || bytes.len() as u64 != entry.bytes {
                bail!("{} does not match the bundle inventory", path.display());
            }
        }
    }
    let body_path = dir.join("body.yaml");
    let body_bytes =
        std::fs::read(&body_path).with_context(|| format!("reading {}", body_path.display()))?;
    check_inventory(&inventory, inventory_verified, "body.yaml", &body_bytes)?;
    let body_hash = ArtifactHash::of(&body_bytes);
    let schema = decode_schema(&body_bytes, &body_path)?;
    if schema.body != manifest.body {
        bail!(
            "bundle body {} does not match the schema body {}",
            manifest.body,
            schema.body
        );
    }
    let transform_path = dir.join("transform.yaml");
    let transform_bytes = std::fs::read(&transform_path)
        .with_context(|| format!("reading {}", transform_path.display()))?;
    check_inventory(
        &inventory,
        inventory_verified,
        "transform.yaml",
        &transform_bytes,
    )?;
    let transform_hash = ArtifactHash::of(&transform_bytes);
    let transform_file = TransformFile::parse(
        std::str::from_utf8(&transform_bytes).context("the transform is not UTF-8")?,
    )?;
    if transform_file.version != manifest.input_transform_version {
        bail!(
            "the bundle transform version {} does not match its manifest {}",
            transform_file.version,
            manifest.input_transform_version
        );
    }
    let transform = body_transform(&transform_file)?;
    let input_transform_version = transform_file.version;
    let params_path = dir.join("body_params.json");
    let params_bytes = std::fs::read(&params_path)
        .with_context(|| format!("reading {}", params_path.display()))?;
    check_inventory(
        &inventory,
        inventory_verified,
        "body_params.json",
        &params_bytes,
    )?;
    let params: looklab_schema::MatchParams = serde_json::from_slice(&params_bytes)
        .with_context(|| format!("decoding {}", params_path.display()))?;
    params
        .validate(&manifest.body)
        .map_err(|error| anyhow::anyhow!("{}: {error}", params_path.display()))?;
    for entry in &manifest.sims {
        let Some(hash) = entry.body_params else {
            bail!(
                "{} pins no body parameters for {}",
                manifest_path.display(),
                entry.native_sim
            );
        };
        if ArtifactHash::of(&params_bytes).as_str() != hash.to_hex() {
            bail!(
                "{} does not match the body-parameters pin for {}",
                params_path.display(),
                entry.native_sim
            );
        }
    }
    let wb_mode = wb_mode
        .map(looklab_schema::WbModeKey::new)
        .transpose()
        .map_err(|error| anyhow::anyhow!(error))?;
    let firmware = firmware
        .map(looklab_schema::FirmwareId::parse)
        .transpose()
        .map_err(|error| anyhow::anyhow!(error))?;
    let mut sims = Vec::with_capacity(manifest.sims.len());
    for entry in &manifest.sims {
        let base_path = dir.join(entry.base_artifact.to_hex());
        let base_bytes = read_hashed(&base_path, entry.base_artifact)?;
        let base_hash = ArtifactHash::of(&base_bytes);
        check_inventory(
            &inventory,
            inventory_verified,
            &entry.base_artifact.to_hex(),
            &base_bytes,
        )?;
        let base_document = LookDocument::decode(&base_bytes)
            .with_context(|| format!("decoding {}", base_path.display()))?;
        let crm_path = dir.join(entry.control_response.to_hex());
        let crm_bytes = read_hashed(&crm_path, entry.control_response)?;
        let crm_hash = ArtifactHash::of(&crm_bytes);
        check_inventory(
            &inventory,
            inventory_verified,
            &entry.control_response.to_hex(),
            &crm_bytes,
        )?;
        let crm = decode_crm_bytes(&crm_bytes, &crm_path)?;
        if crm.body != manifest.body
            || crm.epoch != epoch
            || crm.base_sim != entry.native_sim
            || crm.version != entry.control_response_version
        {
            bail!("{} does not match the pinned sim", crm_path.display());
        }
        let (covariance, covariance_path, covariance_hash) = match entry.covariance {
            Some(hash) => {
                let path = dir.join(hash.to_hex());
                let bytes = read_hashed(&path, hash)?;
                check_inventory(&inventory, inventory_verified, &hash.to_hex(), &bytes)?;
                (
                    Some(decode_covariance_bytes(&bytes, &path)?),
                    Some(path),
                    Some(ArtifactHash::of(&bytes)),
                )
            }
            None => (None, None, None),
        };
        let realization_params = match entry.realization_params {
            Some(hash) => {
                let path = dir.join(hash.to_hex());
                let bytes = read_hashed(&path, hash)?;
                check_inventory(&inventory, inventory_verified, &hash.to_hex(), &bytes)?;
                let params: looklab_schema::MatchRealizationParams = serde_json::from_slice(&bytes)
                    .with_context(|| format!("decoding {}", path.display()))?;
                params
                    .validate()
                    .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
                if params.realization.realization.native_sim != entry.native_sim
                    || params.realization.realization.body != manifest.body
                    || params.realization.realization.epoch.get() != epoch
                    || RealizationRevisionId::for_version(&params.realization) != entry.realization
                    || params.realization.target != entry.family_version
                    || params.family != entry.family_version
                    || params.version != entry.realization_params_version
                {
                    bail!("{} names another realization", path.display());
                }
                Some(params)
            }
            None => bail!(
                "{} pins no realization parameters for {}",
                manifest_path.display(),
                entry.native_sim
            ),
        };
        let mut model = MatchModel::new(
            base_document.look.clone(),
            crm,
            transform,
            covariance,
            input_transform_version,
            entry.realization_params_version,
            output_encoding,
        )?
        .with_params(params.clone())?
        .with_modes(wb_mode.clone(), firmware.clone());
        if let Some(realization) = realization_params {
            if realization.control_response_version != model.version() {
                bail!(
                    "{} pins control-response version {}, not {}",
                    crm_path.display(),
                    realization.control_response_version,
                    model.version()
                );
            }
            model = model.with_realization(realization);
        }
        sims.push(LoadedSim {
            base_document,
            model,
            paths: SimPaths {
                base: Some(base_path),
                crm: Some(crm_path),
                covariance: covariance_path,
                params: Some(dir.join("body_params.json")),
            },
            hashes: Some(SimHashes {
                base: base_hash,
                crm: crm_hash,
                covariance: covariance_hash,
            }),
        });
    }
    if sims.is_empty() {
        bail!("the bundle lists no sims to rank");
    }
    Ok(Some(LoadedMatchInputs {
        schema,
        input_transform_version: InputTransformVersion(input_transform_version),
        body_path,
        transform_path: Some(transform_path),
        body_hash: Some(body_hash),
        transform_hash: Some(transform_hash),
        sims,
        inventory_verified,
    }))
}

fn authenticated_inventory(
    dir: &Path,
    publication: Option<&Path>,
    body: &looklab_schema::BodyId,
    epoch: u32,
    id: &looklab_schema::MatchBundleRevisionId,
) -> Result<(Vec<looklab_publish::ArtifactEntry>, bool)> {
    let inventory_path = dir.join("artifacts.json");
    let inventory_bytes = std::fs::read(&inventory_path).ok();
    let inventory: Vec<looklab_publish::ArtifactEntry> = match &inventory_bytes {
        Some(bytes) => serde_json::from_slice(bytes)
            .with_context(|| format!("decoding {}", inventory_path.display()))?,
        None => Vec::new(),
    };
    let Some(root) = publication else {
        return Ok((inventory, false));
    };
    let index: looklab_publish::SiteIndex = serde_json::from_slice(
        &std::fs::read(root.join("data/index.json"))
            .with_context(|| "reading the publication index")?,
    )
    .context("decoding the publication index")?;
    let entry = index
        .bundles
        .iter()
        .find(|entry| &entry.body == body && entry.epoch == epoch && &entry.bundle == id)
        .ok_or_else(|| anyhow::anyhow!("bundle {id} is not listed in the publication index"))?;
    let manifest_bytes =
        std::fs::read(dir.join(MANIFEST_NAME)).with_context(|| "reading the bundle manifest")?;
    if ArtifactHash::of(&manifest_bytes) != entry.sha256
        || manifest_bytes.len() as u64 != entry.bytes
    {
        bail!("the bundle manifest does not match the publication index");
    }
    let inventory_bytes =
        inventory_bytes.ok_or_else(|| anyhow::anyhow!("the bundle has no artifacts.json"))?;
    if ArtifactHash::of(&inventory_bytes) != entry.artifacts_sha256
        || inventory_bytes.len() as u64 != entry.artifacts_bytes
    {
        bail!("the bundle inventory does not match the publication index");
    }
    Ok((inventory, true))
}

fn check_inventory(
    inventory: &[looklab_publish::ArtifactEntry],
    verified: bool,
    name: &str,
    bytes: &[u8],
) -> Result<()> {
    let Some(entry) = inventory.iter().find(|entry| entry.name.as_str() == name) else {
        if verified {
            bail!("{name} is missing from the authenticated bundle inventory");
        }
        return Ok(());
    };
    if entry.sha256 != ArtifactHash::of(bytes) || entry.bytes != bytes.len() as u64 {
        bail!("{name} does not match the bundle inventory");
    }
    Ok(())
}

fn read_hashed(path: &Path, hash: looklab_schema::Sha256Hex) -> Result<Vec<u8>> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if ArtifactHash::of(&bytes).as_str() != hash.to_hex() {
        bail!("{} does not match its pinned digest", path.display());
    }
    Ok(bytes)
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
    let assessments =
        qualification::load(args.publication.as_deref(), permalink.selection.as_ref())?;
    if let Some(selection) = &permalink.selection {
        selection.validate()?;
        println!(
            "recorded selection: {} (bundle {}, {} report pins)",
            selection.mode.as_str(),
            selection.bundle.to_hex(),
            selection.reports.len()
        );
    } else {
        println!("selection: legacy unassessed");
    }
    let expected_optimizer = if permalink.metric.is_legacy() {
        looklab_match::PRE_CLIP_OPTIMIZER_VERSION
    } else {
        looklab_match::OPTIMIZER_VERSION
    };
    if permalink.optimizer != expected_optimizer {
        bail!(
            "permalink optimizer {:?} is not supported by {expected_optimizer}",
            permalink.optimizer
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
    if let (Some(assessments), Some(selection)) = (&assessments, &permalink.selection) {
        qualification::validate_target(
            assessments,
            selection,
            &document,
            &look_path,
            qualification::TargetUse::Replay,
        )?;
        for entry in &assessments.index.reports {
            if selection
                .reports
                .iter()
                .any(|pin| pin.reference == QualificationRef::Report(entry.report))
                && !entry.applicability.is_current()
            {
                println!("assessment {}: {:?}", entry.report, entry.applicability);
            }
        }
    }
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
    if permalink.selection.is_some() && !context.inventory_verified {
        bail!(
            "selected replay needs a publication root authenticating the bundle inventory; pass --publication"
        );
    }
    let plan = Plan::new(&context.schema, permalink.epoch)?;
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
    if let Some(context) = &permalink.output {
        looklab_match::validate_observed_encoding(&observed, context.camera.encoding)?;
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
    let camera_output = permalink
        .output
        .as_ref()
        .map(|context| context.camera.encoding);
    let mut domains = qualification::domains(
        &plan,
        &models,
        permalink.selection.as_ref(),
        assessments.as_ref(),
        camera_output,
    )?;
    let (first_probes, first_indices) = qualification::probes(&domains)?;
    let requested_points = looklab_model::probes::v1().probes.len();
    let first_uncertainty = document.uncertainty.restrict_to(&first_indices)?;
    let first_target = PreparedTarget::new(TargetSpec {
        probes: &first_probes,
        function: &document.look,
        uncertainty: Some(&first_uncertainty),
        appearance: target_appearance(&permalink, &document)?,
        emphasis,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
    })?
    .with_declared_points(requested_points)?;
    let first_outcome = if domains.is_empty() {
        optimize(&models, &first_target, &plan, &options)?
    } else {
        optimize_scoped(&models, &first_target, &plan, &options, &domains)?
    };
    let first = first_outcome
        .results
        .first()
        .ok_or_else(|| anyhow::anyhow!("no settings suggestion"))?;
    if permalink.metric.is_rendered() {
        let pinned = permalink
            .output
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("a rendered permalink needs an output context"))?;
        let produced = first
            .output
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("the search produced no output context"))?;
        if produced.canonical() != pinned.canonical() {
            bail!("the pinned output context does not match the search context");
        }
    }
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
    let first_measured =
        measure_target_error(&first_target, &winner.model, &first.settings, &observed)?;
    let (fitted, scales) = ScaleFitter.fit_with_scales(
        &models[winner_index],
        &plan,
        &first.settings,
        permalink.metric,
        &observed,
        &FitObservedOptions::default(),
    )?;
    let updated = models[winner_index].with_crm(fitted)?;
    let next_calibration = permalink.calibration.merged(&first.sim, &scales)?;
    let next_mode = permalink.selection.as_ref().map(|selection| {
        if next_calibration.is_empty() {
            selection.mode
        } else {
            SelectionMode::Experimental
        }
    });
    if next_mode == Some(SelectionMode::Experimental) {
        domains.clear();
    }
    let (second_probes, second_indices) = qualification::probes(&domains)?;
    let second_uncertainty = document.uncertainty.restrict_to(&second_indices)?;
    let second_target = PreparedTarget::new(TargetSpec {
        probes: &second_probes,
        function: &document.look,
        uncertainty: Some(&second_uncertainty),
        appearance: target_appearance(&permalink, &document)?,
        emphasis,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
    })?
    .with_declared_points(requested_points)?;
    let mut updated_models = models;
    updated_models[winner_index] = updated.clone();
    let second_outcome = if domains.is_empty() {
        optimize(&updated_models, &second_target, &plan, &options)?
    } else {
        optimize_scoped(&updated_models, &second_target, &plan, &options, &domains)?
    };
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
    let card = settings_card_with_support(
        &second_target,
        &plan,
        card_model,
        second,
        domains.get(&second.sim),
    )?;

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
    next.realization_params_version = second_sim.model.realization_params_version;
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
    if let (Some(selection), Some(mode)) = (next.selection.as_mut(), next_mode) {
        selection.mode = mode;
    }
    if next.metric.is_rendered() {
        next.output = Some(second_target.output_context(card_model, &second.settings)?);
    }
    let downgraded = permalink
        .selection
        .as_ref()
        .is_some_and(|selection| selection.mode == SelectionMode::Qualified)
        && next_mode == Some(SelectionMode::Experimental);
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
    if downgraded {
        println!("  the local calibration makes the next selection experimental");
    }
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
    inventory_verified: bool,
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
    if let Some(path) = &bundle_path
        && camera_bundle_dir(path).is_some()
    {
        let wb = permalink
            .selection
            .as_ref()
            .and_then(|selection| selection.wb_mode.as_ref())
            .map(|mode| mode.as_str().to_owned());
        let firmware = permalink
            .selection
            .as_ref()
            .and_then(|selection| selection.firmware.as_ref())
            .map(|firmware| firmware.as_text());
        let camera_output = permalink
            .output
            .as_ref()
            .map_or(LEGACY_REPLAY_OUTPUT, |context| context.camera.encoding);
        let loaded = load_camera_bundle(
            path,
            permalink.epoch,
            wb.as_deref(),
            firmware.as_deref(),
            args.publication.as_deref(),
            permalink.selection.as_ref().map(|selection| {
                looklab_schema::MatchBundleRevisionId::from_digest(*selection.bundle.as_bytes())
            }),
            camera_output,
        )?
        .ok_or_else(|| anyhow::anyhow!("{} is not a camera bundle", path.display()))?;
        if loaded.schema.body.as_str() != permalink.body.as_str() {
            bail!(
                "the camera bundle is for body {}, not the permalink body {}",
                loaded.schema.body,
                permalink.body
            );
        }
        if loaded.input_transform_version.0 != permalink.input_transform_version {
            bail!(
                "input transform version {} does not match the permalink {}",
                loaded.input_transform_version.0,
                permalink.input_transform_version
            );
        }
        let winner = loaded
            .sims
            .iter()
            .position(|sim| sim.model.sim().as_str() == permalink.sim.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!("the pinned sim {} is not in the bundle", permalink.sim)
            })?;
        let pinned = &loaded.sims[winner];
        if pinned.model.version() != permalink.crm_version {
            bail!(
                "the pinned control-response model version {} is not the bundle's {}",
                permalink.crm_version,
                pinned.model.version()
            );
        }
        if pinned.model.realization_params_version != permalink.realization_params_version {
            bail!(
                "the pinned realization version {} is not the bundle's {}",
                permalink.realization_params_version,
                pinned.model.realization_params_version
            );
        }
        check_identity(
            pinned.base_document.file.look.slug.as_str(),
            pinned.base_document.file.look.version.as_str(),
            &permalink.base_slug,
            &permalink.base_version,
            "base Look",
        )?;
        let hashes = pinned.hashes.as_ref().ok_or_else(|| {
            anyhow::anyhow!("the bundle has no verified content for {}", permalink.sim)
        })?;
        let body_hash = loaded
            .body_hash
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("the bundle has no verified body schema"))?;
        let transform_hash = loaded
            .transform_hash
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("the bundle has no verified transform"))?;
        check_pinned_source(
            "body schema",
            permalink.sources.body.as_deref(),
            permalink_path,
            body_hash,
        )?;
        check_pinned_source(
            "transform",
            permalink.sources.transform.as_deref(),
            permalink_path,
            transform_hash,
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
        let sims = loaded
            .sims
            .into_iter()
            .map(|sim| VerifySim {
                base_document: sim.base_document,
                model: sim.model,
                paths: sim.paths,
                hashes: sim.hashes,
            })
            .collect();
        return Ok(VerifyContext {
            schema: loaded.schema,
            body_path: loaded.body_path,
            transform_path: loaded.transform_path,
            sims,
            inventory_verified: loaded.inventory_verified,
        });
    }
    if bundle_path.is_none() && pins_bundle_artifacts(permalink)? {
        bail!(
            "this permalink pins a published bundle's model or base, but its bundle is not beside the pinned files; pass --bundle <version-dir>"
        );
    }
    if bundle_path.is_none() && permalink.selection.is_some() {
        bail!(
            "this permalink pins a match bundle revision; pass --bundle <revision-dir> so its full ordered sim list can be replayed"
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

fn camera_bundle_dir(path: &Path) -> Option<PathBuf> {
    let dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };
    let manifest = dir.join(MANIFEST_NAME);
    let text = std::fs::read_to_string(&manifest).ok()?;
    text.contains("\"schema_snapshot_version\"").then_some(dir)
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
        permalink.sources.body.as_deref(),
        permalink.sources.transform.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let path = resolve_source(permalink_path, Some(source), None);
        if let Some(dir) = path.as_deref().and_then(Path::parent) {
            dirs.push(dir.to_path_buf());
        }
    }
    let mut seen: Vec<PathBuf> = Vec::with_capacity(dirs.len());
    dirs.retain(|dir| {
        if seen.contains(dir) {
            false
        } else {
            seen.push(dir.clone());
            true
        }
    });
    for dir in dirs {
        if camera_bundle_dir(&dir).is_some() {
            if camera_bundle_matches(permalink, &dir)? {
                return Ok(Some(dir));
            }
            continue;
        }
        let candidate = dir.join(name.as_str());
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn camera_bundle_matches(permalink: &Permalink, dir: &Path) -> Result<bool> {
    let Some(selection) = &permalink.selection else {
        return Ok(true);
    };
    let path = dir.join(MANIFEST_NAME);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let manifest = looklab_schema::MatchBundleRevision::parse_manifest(&text, 0)
        .with_context(|| format!("decoding {}", path.display()))?;
    let pinned = looklab_schema::MatchBundleRevisionId::from_digest(*selection.bundle.as_bytes());
    Ok(manifest.id == pinned)
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
    if let Some(selection) = &permalink.selection {
        let loaded = dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if loaded != selection.bundle.to_hex() {
            bail!(
                "the loaded bundle directory {loaded} is not the pinned revision {}",
                selection.bundle.to_hex()
            );
        }
    }
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
        let output_encoding = match &permalink.output {
            Some(context) => context.camera.encoding,
            None => LEGACY_REPLAY_OUTPUT,
        };
        let model = MatchModel::new(
            base_document.look.clone(),
            crm,
            transform,
            covariance,
            transform_file.version,
            permalink.realization_params_version,
            output_encoding,
        )?
        .with_modes(
            permalink
                .selection
                .as_ref()
                .and_then(|context| context.wb_mode.clone()),
            permalink
                .selection
                .as_ref()
                .and_then(|context| context.firmware.clone()),
        );
        sims.push(VerifySim {
            base_document,
            model,
            paths: SimPaths {
                base: Some(base_path),
                crm: Some(crm_path),
                covariance: covariance_path,
                params: None,
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
        inventory_verified: false,
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
    let params = match &args.params {
        Some(path) => {
            let bytes =
                std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            let params: looklab_schema::MatchParams = serde_json::from_slice(&bytes)
                .with_context(|| format!("decoding {}", path.display()))?;
            params
                .validate(&schema.body)
                .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
            Some(params)
        }
        None => None,
    };
    let output_encoding = match &permalink.output {
        Some(context) => context.camera.encoding,
        None => LEGACY_REPLAY_OUTPUT,
    };
    let mut model = MatchModel::new(
        base_document.look.clone(),
        crm,
        transform,
        covariance,
        permalink.input_transform_version,
        permalink.realization_params_version,
        output_encoding,
    )?;
    if let Some(params) = params {
        model = model.with_params(params)?;
    }
    if let Some(path) = &args.realization_params {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let params: looklab_schema::MatchRealizationParams = serde_json::from_slice(&bytes)
            .with_context(|| format!("decoding {}", path.display()))?;
        params
            .validate()
            .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
        model = model.with_realization(params);
    }
    model = model.with_modes(
        permalink
            .selection
            .as_ref()
            .and_then(|context| context.wb_mode.clone()),
        permalink
            .selection
            .as_ref()
            .and_then(|context| context.firmware.clone()),
    );
    Ok(VerifyContext {
        schema,
        body_path,
        transform_path,
        inventory_verified: false,
        sims: vec![VerifySim {
            base_document,
            model,
            paths: SimPaths {
                base: Some(base_path.unwrap_or_else(|| look_path.to_path_buf())),
                crm: Some(crm_path),
                covariance: covariance_path,
                params: args.params.clone(),
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

fn target_appearance(permalink: &Permalink, document: &LookDocument) -> Result<TargetAppearance> {
    if permalink.metric.is_legacy() {
        return Ok(TargetAppearance::PreClip);
    }
    let output = permalink
        .output
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("a rendered permalink needs an output context"))?;
    let pinned = OutputPin::from_document(document)?;
    if output.target.encoding != pinned.encoding || output.target.clip_k != pinned.clip_k {
        bail!("the pinned target output does not match the target Look");
    }
    Ok(TargetAppearance::Rendered(output.target))
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
