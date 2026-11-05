// SPDX-License-Identifier: AGPL-3.0-or-later
//! `looklab lut-look import` (docs/03 §3, docs/02 §3.8).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use looklab_export::{
    LutLookOptions, definition_delta, import_lut_look, lut_data_from_cube, read_cube,
};
use looklab_model::file::{
    BaseReference, BaseStatus, DeclaredEncoding, DynamicRange, Gamut, LicenseRecord, LogTransfer,
    LookVersion, LutDefinition, LutEmulation, LutRecord, NEUTRAL_BASELINE_SLUG, OutputEncoding,
    Renderer, Slug,
};
use looklab_model::{LookDocument, LookIdentity, LookKind, WbPolicy};
use looklab_schema::{BodyId, SimKey, validate_control_schema};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum OutputArg {
    Srgb,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum RendererArg {
    XRawStudio,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum DynamicRangeArg {
    Dr100,
    Dr200,
    Dr400,
}

impl From<DynamicRangeArg> for DynamicRange {
    fn from(value: DynamicRangeArg) -> Self {
        match value {
            DynamicRangeArg::Dr100 => Self::Dr100,
            DynamicRangeArg::Dr200 => Self::Dr200,
            DynamicRangeArg::Dr400 => Self::Dr400,
        }
    }
}

#[derive(Debug, Args)]
pub struct LutLookArgs {
    #[command(subcommand)]
    pub command: LutLookCommand,
}

#[derive(Debug, Subcommand)]
pub enum LutLookCommand {
    /// Fit a layered approximation from a `.cube` and write a `.looklab`.
    Import(ImportArgs),
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    /// Input `.cube` file.
    #[arg(long)]
    pub cube: PathBuf,
    /// `flog2-fgamut` or `display:<look slug>`.
    #[arg(long)]
    pub input: String,
    /// Output encoding; v1 accepts `srgb`.
    #[arg(long, value_enum, default_value_t = OutputArg::Srgb)]
    pub output: OutputArg,
    #[arg(long)]
    pub slug: String,
    #[arg(long)]
    pub name: String,
    #[arg(long, default_value = "1.0.0")]
    pub version: String,
    /// SPDX license the submitter declares for redistribution.
    #[arg(long)]
    pub license: Option<String>,
    /// The submitter's own work.
    #[arg(long)]
    pub own_work: bool,
    #[arg(long)]
    pub source_url: Option<String>,
    #[arg(long)]
    pub attribution: String,
    /// Body realization body id, for example `fujifilm/x-e3`.
    #[arg(long)]
    pub base_body: Option<String>,
    /// Intended family base, for example `fujifilm:"Classic Negative"`.
    #[arg(long)]
    pub base_family: Option<String>,
    /// Renderer for a body-realization base.
    #[arg(long, value_enum)]
    pub base_renderer: Option<RendererArg>,
    /// Film simulation key for a body-realization base.
    #[arg(long)]
    pub base_sim: Option<String>,
    /// Dynamic range for a body-realization base.
    #[arg(long, value_enum)]
    pub base_dr: Option<DynamicRangeArg>,
    #[arg(long, default_value = "")]
    pub base_notes: String,
    /// Directory of body YAML files used to resolve the base.
    #[arg(long, default_value = "data/bodies")]
    pub bodies_dir: PathBuf,
    /// Base Look version for `display:<slug>`.
    #[arg(long, default_value = "1.0.0")]
    pub base_version: String,
    /// Base Look document for `display:<slug>` when it is not the neutral baseline.
    #[arg(long)]
    pub base_look: Option<PathBuf>,
    /// Base Look document for the `--compare` Look, when its base is a Look.
    #[arg(long)]
    pub compare_base_look: Option<PathBuf>,
    /// What the LUT emulates: camera maker (with --emulates-camera-rendering).
    #[arg(long)]
    pub emulates_camera_maker: Option<String>,
    /// What the LUT emulates: camera rendering name.
    #[arg(long)]
    pub emulates_camera_rendering: Option<String>,
    /// What the LUT emulates: film stock name.
    #[arg(long)]
    pub emulates_film_stock: Option<String>,
    #[arg(long)]
    pub note: Vec<String>,
    /// Another `.looklab` to measure the definition ΔE against.
    #[arg(long)]
    pub compare: Option<PathBuf>,
    #[arg(long)]
    pub out: PathBuf,
    #[arg(long, default_value_t = 0x6c75_742d_6c6f_6f6b)]
    pub seed: u64,
}

pub fn run(args: &LutLookArgs) -> Result<()> {
    match &args.command {
        LutLookCommand::Import(args) => import(args),
    }
}

fn import(args: &ImportArgs) -> Result<()> {
    let encoding = parse_encoding(args)?;
    let license = license(args)?;
    let text = std::fs::read_to_string(&args.cube)
        .with_context(|| format!("reading {}", args.cube.display()))?;
    let cube =
        read_cube(&text).map_err(|error| anyhow::anyhow!("{}: {error}", args.cube.display()))?;
    let lut = lut_data_from_cube(&cube)?;
    let definition = LutDefinition {
        name: args.name.clone(),
        license,
        encoding,
        output: match args.output {
            OutputArg::Srgb => OutputEncoding::Srgb,
        },
        lut: LutRecord {
            size: lut.size,
            domain_min: lut.domain_min,
            domain_max: lut.domain_max,
            values_ref: looklab_model::BinRef::new("bin:lut")?,
        },
        emulates: emulation(args)?,
        superseded_by: None,
        notes: args.note.clone(),
    };
    let identity = LookIdentity::new(
        &args.slug,
        &args.version,
        LookKind::LutDefined,
        WbPolicy::Adaptive,
    )?;
    let base_document = match &args.base_look {
        Some(path) => Some(crate::util::load_document(path)?),
        None => None,
    };
    if let DeclaredEncoding::DisplayReferred { base, .. } = &definition.encoding {
        match base {
            BaseReference::Look { .. } => {
                if let Some(document) = base_document.as_ref() {
                    validate_base_document(document, base)?;
                }
            }
            _ => {
                if base_document.is_some() {
                    bail!("--base-look only applies to a display:<slug> base");
                }
            }
        }
    }
    let imported = import_lut_look(
        identity,
        definition,
        &cube,
        base_document.as_ref(),
        LutLookOptions {
            seed: args.seed,
            ..LutLookOptions::default()
        },
    )?;
    let bytes = imported.document.encode()?;
    crate::write(&args.out, &bytes)?;
    let report = imported.approximation;
    println!(
        "{} {}: approximation median {:.3}, p95 {:.3}, max {:.3} ΔE on {} probes",
        args.slug, args.version, report.median, report.p95, report.max, report.probes
    );
    for (group, stats) in &imported.by_group {
        println!(
            "  {:?}: median {:.3}, p95 {:.3}, max {:.3} on {} probes",
            group, stats.median, stats.p95, stats.max, stats.probes
        );
    }
    println!(
        "matchable: {}; base: {}",
        if imported.document.is_matchable() {
            "yes"
        } else {
            "no"
        },
        args.input
    );
    if let Some(path) = &args.compare {
        let other = crate::util::load_document(path)?;
        let compare_base = match &args.compare_base_look {
            Some(path) => Some(crate::util::load_document(path)?),
            None => None,
        };
        let new_base = resolve_base_document(&imported.document, base_document.as_ref())?;
        let other_base = resolve_base_document(&other, compare_base.as_ref())?;
        let delta = definition_delta(&imported.document, new_base, &other, other_base)?;
        println!(
            "definition ΔE vs {}: median {:.3}, p95 {:.3}, max {:.3} on {} probes",
            path.display(),
            delta.median,
            delta.p95,
            delta.max,
            delta.probes
        );
    }
    Ok(())
}

fn emulation(args: &ImportArgs) -> Result<Option<LutEmulation>> {
    let camera = args
        .emulates_camera_maker
        .as_ref()
        .zip(args.emulates_camera_rendering.as_ref());
    let stock = args.emulates_film_stock.as_ref();
    match (camera, stock) {
        (Some(_), Some(_)) => bail!("use either camera emulation or film stock, not both"),
        (Some((maker, rendering)), None) => Ok(Some(LutEmulation::CameraRendering {
            maker: maker.clone(),
            rendering: rendering.clone(),
        })),
        (None, Some(name)) => Ok(Some(LutEmulation::FilmStock { name: name.clone() })),
        (None, None) => Ok(None),
    }
}

fn validate_base_document(document: &LookDocument, base: &BaseReference) -> Result<()> {
    if !document.is_matchable() {
        bail!(
            "base Look {}@{} is not matchable",
            document.file.look.slug,
            document.file.look.version
        );
    }
    let BaseReference::Look { slug, version } = base else {
        return Ok(());
    };
    if document.file.look.slug != *slug || document.file.look.version != *version {
        bail!(
            "--base-look is {}@{}, expected {}@{}",
            document.file.look.slug,
            document.file.look.version,
            slug,
            version
        );
    }
    Ok(())
}

fn resolve_base_document<'a>(
    document: &LookDocument,
    provided: Option<&'a LookDocument>,
) -> Result<Option<&'a LookDocument>> {
    let Some(base) = document.base() else {
        return Ok(None);
    };
    if looklab_model::baseline::is_neutral_baseline(base) {
        return Ok(None);
    }
    match base {
        BaseReference::Look { .. } => {
            let provided = provided.ok_or_else(|| {
                anyhow::anyhow!(
                    "{}@{} needs its base Look document",
                    document.file.look.slug,
                    document.file.look.version
                )
            })?;
            validate_base_document(provided, base)?;
            Ok(Some(provided))
        }
        BaseReference::FamilyRendering { .. } => {
            bail!("a family base has no published Look version yet")
        }
        BaseReference::BodyRealization { .. } => {
            bail!("a body-realization base has no published Look version yet")
        }
    }
}

fn license(args: &ImportArgs) -> Result<LicenseRecord> {
    if args.license.is_some() && args.own_work {
        bail!("use either --license or --own-work, not both");
    }
    let record = LicenseRecord {
        own_work: args.own_work,
        source_license: args.license.clone(),
        source_url: args.source_url.clone(),
        attribution: args.attribution.clone(),
    };
    record.validate()?;
    Ok(record)
}

fn parse_encoding(args: &ImportArgs) -> Result<DeclaredEncoding> {
    if args.input == "flog2-fgamut" {
        return Ok(DeclaredEncoding::SceneReferred {
            transfer: LogTransfer::Flog2,
            gamut: Gamut::Fgamut,
        });
    }
    let Some(base) = args.input.strip_prefix("display:") else {
        bail!("--input must be flog2-fgamut or display:<look slug>");
    };
    if let Some(family) = &args.base_family {
        if args.base_body.is_some() {
            bail!("use either --base-body or --base-family, not both");
        }
        let Some((maker, rendering)) = family.split_once(':') else {
            bail!("--base-family must be maker:\"rendering\"");
        };
        let maker = maker.trim();
        let rendering = rendering.trim().trim_matches('"');
        if maker.is_empty() || rendering.is_empty() {
            bail!("--base-family must be maker:\"rendering\"");
        }
        let reference = BaseReference::FamilyRendering {
            maker: maker.to_owned(),
            rendering: rendering.to_owned(),
            dynamic_range: args.base_dr.map(DynamicRange::from),
            notes: args.base_notes.clone(),
        };
        let status = reference.resolve_status(None);
        return Ok(DeclaredEncoding::DisplayReferred {
            base: reference,
            status,
        });
    }
    if let Some(body) = &args.base_body {
        let body = BodyId::new(body)?;
        let renderer = match args.base_renderer.unwrap_or(RendererArg::XRawStudio) {
            RendererArg::XRawStudio => Renderer::XRawStudio,
        };
        let film_simulation = args
            .base_sim
            .as_ref()
            .map(|sim| SimKey::new(sim))
            .transpose()?;
        let dynamic_range = args.base_dr.map(DynamicRange::from);
        let reference = BaseReference::BodyRealization {
            body: body.clone(),
            renderer,
            film_simulation,
            dynamic_range,
            notes: args.base_notes.clone(),
        };
        let schema_path = args
            .bodies_dir
            .join(format!("{}.yaml", body.as_str().replace('/', "-")));
        let schema = match std::fs::read_to_string(&schema_path) {
            Ok(text) => Some(
                validate_control_schema(&text)
                    .map_err(|errors| anyhow::anyhow!("{}: {errors:?}", schema_path.display()))?,
            ),
            Err(_) => None,
        };
        let status = reference.resolve_status(schema.as_ref());
        return Ok(DeclaredEncoding::DisplayReferred {
            base: reference,
            status,
        });
    }
    if base.is_empty() {
        bail!("display: needs a base Look slug");
    }
    let base = if base == NEUTRAL_BASELINE_SLUG {
        BaseReference::neutral_baseline()
    } else {
        BaseReference::Look {
            slug: Slug::new(base)?,
            version: LookVersion::new(&args.base_version)?,
        }
    };
    Ok(DeclaredEncoding::DisplayReferred {
        base,
        status: BaseStatus::Pinned,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use looklab_model::file::{
        BaseField, BaseStatus, LicenseRecord, LutData, LutDefinition, LutRecord, OutputEncoding,
    };
    use looklab_model::{Look, LookDocument, LookIdentity, LookKind, WbPolicy};

    fn identity(slug: &str, version: &str) -> LookIdentity {
        LookIdentity::new(slug, version, LookKind::NativeFamily, WbPolicy::Adaptive)
            .expect("identity")
    }

    fn base_look(slug: &str, version: &str) -> LookDocument {
        LookDocument::from_look(identity(slug, version), Look::colorimetric()).expect("document")
    }

    fn lut() -> LutData {
        LutData::new(2, [0.0; 3], [1.0; 3], vec![0.0; 24]).expect("lut")
    }

    fn display_document(base: BaseReference, status: BaseStatus) -> LookDocument {
        let lut = lut();
        let definition = LutDefinition {
            name: "Display".to_string(),
            license: LicenseRecord {
                own_work: true,
                source_license: None,
                source_url: None,
                attribution: "test".to_string(),
            },
            encoding: DeclaredEncoding::DisplayReferred { base, status },
            output: OutputEncoding::Srgb,
            lut: LutRecord {
                size: 2,
                domain_min: [0.0; 3],
                domain_max: [1.0; 3],
                values_ref: looklab_model::BinRef::new("bin:lut").expect("ref"),
            },
            emulates: None,
            superseded_by: None,
            notes: Vec::new(),
        };
        LookDocument::from_lut_definition(
            LookIdentity::new("display", "1.0.0", LookKind::LutDefined, WbPolicy::Adaptive)
                .expect("identity"),
            Look::colorimetric(),
            definition,
            lut,
        )
        .expect("document")
    }

    #[test]
    fn base_document_must_match_the_declared_identity() {
        let base = BaseReference::Look {
            slug: Slug::new("base").expect("slug"),
            version: LookVersion::new("2.0.0").expect("version"),
        };
        assert!(validate_base_document(&base_look("base", "2.0.0"), &base).is_ok());
        assert!(validate_base_document(&base_look("other", "2.0.0"), &base).is_err());
        assert!(validate_base_document(&base_look("base", "1.0.0"), &base).is_err());
    }

    #[test]
    fn base_document_must_be_matchable() {
        let base = BaseReference::neutral_baseline();
        let document = display_document(
            base.clone(),
            BaseStatus::BaseIncomplete {
                missing: BaseField::FilmSimulation,
            },
        );
        assert!(validate_base_document(&document, &base).is_err());
    }

    #[test]
    fn compare_resolves_a_non_neutral_base_document() {
        let base = BaseReference::Look {
            slug: Slug::new("base").expect("slug"),
            version: LookVersion::new("2.0.0").expect("version"),
        };
        let document = display_document(base.clone(), BaseStatus::Pinned);
        let provided = base_look("base", "2.0.0");
        assert!(resolve_base_document(&document, Some(&provided)).is_ok());
        assert!(resolve_base_document(&document, None).is_err());
        let wrong = base_look("other", "2.0.0");
        assert!(resolve_base_document(&document, Some(&wrong)).is_err());
    }
}
