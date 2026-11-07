// SPDX-License-Identifier: AGPL-3.0-or-later
//! `looklab export`: bake a Look for a software interface profile.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use looklab_export::{
    Artifact, BakeIlluminant, Baker, CctK, InsertionSpec, ValidationReport, cube_files, validate,
    write_haldclut, write_native_json,
};
use looklab_model::LookDocument;
use looklab_model::probes;
use looklab_schema::{Format, IlluminantHandling, InterfaceProfile, validate_interface_profile};

use crate::body_transform::TransformFile;
use crate::util;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum FormatChoice {
    Cube,
    Haldclut,
    NativeJson,
}

impl FormatChoice {
    fn schema(self) -> Format {
        match self {
            Self::Cube => Format::Cube,
            Self::Haldclut => Format::Haldclut,
            Self::NativeJson => Format::NativeJson,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Cube => "cube",
            Self::Haldclut => "haldclut",
            Self::NativeJson => "native_json",
        }
    }
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    /// `.looklab` file to bake.
    pub look: PathBuf,
    /// Interface profile YAML.
    #[arg(long)]
    pub profile: PathBuf,
    /// Body transform YAML (DNG colour matrices or a fixed matrix).
    #[arg(long)]
    pub body_transform: PathBuf,
    /// Bake CCT for a `single` illuminant profile.
    #[arg(long, default_value_t = 6500)]
    pub cct: u32,
    #[arg(long, value_enum)]
    pub format: FormatChoice,
    #[arg(long)]
    pub out: PathBuf,
    /// Write even when validation flags the export.
    #[arg(long)]
    pub force: bool,
}

pub fn run(args: &ExportArgs) -> Result<()> {
    let cct = match args.cct {
        6500 => CctK::D65,
        5500 => CctK::DAYLIGHT,
        3200 => CctK::TUNGSTEN,
        other => bail!("--cct must be 6500, 5500 or 3200, not {other}"),
    };
    let document = util::load_document(&args.look)?;
    ensure_matchable(&document)?;
    let profile = load_profile(&args.profile)?;
    let format = args.format.schema();
    if !profile.formats.contains(&format) {
        bail!(
            "profile {} does not accept {} (it lists {:?})",
            profile.id.as_str(),
            args.format.label(),
            profile.formats
        );
    }
    let transform_file = TransformFile::load(&args.body_transform)?;
    let transform = transform_file.input_transform()?;

    let illuminant = match profile.insertion.illuminant_handling {
        IlluminantHandling::Single => BakeIlluminant::Single { cct },
        IlluminantHandling::DualInterpolated => BakeIlluminant::DualInterpolated,
    };
    let output = Baker::new(&profile, &document.look, &transform)
        .bake(illuminant)
        .context("baking the Look")?;
    let probes = probes::v1();
    let support: Option<Vec<bool>> = document
        .uncertainty
        .support
        .as_ref()
        .filter(|mask| mask.values.len() == probes.probes.len())
        .map(|mask| mask.values.iter().map(|value| *value > 0.5).collect());
    let report = validate(
        &output,
        &document.look,
        &transform,
        &probes,
        support.as_deref(),
    )
    .context("validating the baked artifact")?;
    print_report(&report);

    if report.flagged() && !args.force {
        bail!("export flagged: p95 exceeds 1.0 ΔE at a bake CCT; rerun with --force to write it");
    }
    write_output(args, &document, &profile, &output, format, cct)
}

fn ensure_matchable(document: &LookDocument) -> Result<()> {
    if let Some(refusal) = document.match_refusal() {
        bail!("{} is not matchable: {refusal}", document.file.look.slug);
    }
    Ok(())
}

fn load_profile(path: &Path) -> Result<InterfaceProfile> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    validate_interface_profile(&text).map_err(|errors| {
        let messages: Vec<String> = errors.iter().map(ToString::to_string).collect();
        anyhow::anyhow!("{}: {}", path.display(), messages.join("; "))
    })
}

fn print_report(report: &ValidationReport) {
    println!("validation:");
    for entry in std::iter::once(&report.primary).chain(&report.others) {
        println!(
            "  {}: n {} median {:.2} p95 {:.2} max {:.2}, unsupported {},
             {} clipped targets",
            entry.cct,
            entry.stats.count,
            entry.stats.median,
            entry.stats.p95,
            entry.stats.max,
            entry.unsupported,
            entry.clipped
        );
    }
    println!(
        "  clipped {:.1}% of probes, unsupported {:.1}%, flagged: {}",
        report.clipped_fraction() * 100.0,
        report.unsupported_fraction() * 100.0,
        if report.flagged() { "yes" } else { "no" }
    );
    if let Some(mismatch) = &report.tone_mismatch {
        println!(
            "  tone mismatch: median {:.2} max {:.2}",
            mismatch.median, mismatch.max
        );
    }
}

fn write_output(
    args: &ExportArgs,
    document: &LookDocument,
    profile: &InterfaceProfile,
    output: &looklab_export::BakeOutput,
    format: Format,
    cct: CctK,
) -> Result<()> {
    let insertion = InsertionSpec::from_profile(profile, None)?;
    match format {
        Format::NativeJson => {
            let text = write_native_json(output)?;
            let written = crate::write(&args.out, text.as_bytes())?;
            println!("wrote {} ({} bytes)", args.out.display(), written);
        }
        Format::Cube | Format::Haldclut => {
            let published = match output.bake {
                BakeIlluminant::Single { .. } => vec![(None, output.primary_artifact())],
                BakeIlluminant::DualInterpolated => output
                    .artifacts
                    .iter()
                    .map(|entry| {
                        let suffix = format!("-{}", entry.cct.kelvin() as u32);
                        (Some(suffix), Some(&entry.artifact))
                    })
                    .collect(),
            };
            for (suffix, artifact) in published {
                let artifact =
                    artifact.with_context(|| format!("the bake has no artifact at {}", cct))?;
                let path = match suffix {
                    Some(suffix) => util::replace_extension(&args.out, &suffix, extension(format)),
                    None => args.out.clone(),
                };
                write_artifact(
                    &path,
                    artifact,
                    format,
                    &format!(
                        "{} {}",
                        document.file.look.slug.as_str(),
                        document.file.look.version.as_str()
                    ),
                    insertion.range,
                )?;
            }
        }
    }
    Ok(())
}

fn write_artifact(
    path: &Path,
    artifact: &Artifact,
    format: Format,
    title: &str,
    range: [f64; 2],
) -> Result<()> {
    let lut = artifact
        .as_lut()
        .context("the requested format needs a 3D LUT artifact")?;
    match format {
        Format::Cube => {
            let (cube, shaper) = cube_files(lut, title, range);
            let written = crate::write(path, cube.as_bytes())?;
            println!("wrote {} ({} bytes)", path.display(), written);
            if let Some(shaper) = shaper {
                let shaper_path = util::replace_extension(path, "-shaper", "cube");
                let written = crate::write(&shaper_path, shaper.as_bytes())?;
                println!("wrote {} ({} bytes)", shaper_path.display(), written);
            }
        }
        Format::Haldclut => {
            let bytes = write_haldclut(lut)?;
            let written = crate::write(path, &bytes)?;
            println!("wrote {} ({} bytes)", path.display(), written);
        }
        Format::NativeJson => unreachable!("native_json is written from the whole output"),
    }
    Ok(())
}

fn extension(format: Format) -> &'static str {
    match format {
        Format::Cube => "cube",
        Format::Haldclut => "png",
        Format::NativeJson => "json",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use looklab_model::file::{
        BaseField, BaseReference, BaseStatus, DeclaredEncoding, LicenseRecord, LutData,
        LutDefinition, MatchRefusal, OutputEncoding,
    };
    use looklab_model::{Look, LookIdentity, LookKind, WbPolicy};

    fn non_matchable() -> LookDocument {
        let lut = LutData::new(2, [0.0; 3], [1.0; 3], vec![0.0; 24]).expect("lut");
        let definition = LutDefinition {
            name: "Pending base".to_string(),
            license: LicenseRecord {
                own_work: true,
                source_license: None,
                source_url: None,
                attribution: "test".to_string(),
            },
            encoding: DeclaredEncoding::DisplayReferred {
                base: BaseReference::neutral_baseline(),
                status: BaseStatus::BaseIncomplete {
                    missing: BaseField::FilmSimulation,
                },
            },
            output: OutputEncoding::Srgb,
            lut: lut.record(),
            emulates: None,
            superseded_by: None,
            notes: Vec::new(),
        };
        LookDocument::from_lut_definition(
            LookIdentity::new("pending", "1.0.0", LookKind::LutDefined, WbPolicy::Adaptive)
                .expect("identity"),
            Look::colorimetric(),
            definition,
            lut,
        )
        .expect("document")
    }

    #[test]
    fn export_rejects_a_non_matchable_look() {
        let document = non_matchable();
        assert_eq!(
            document.match_refusal(),
            Some(MatchRefusal::DisplayBaseUnpinned)
        );
        assert!(ensure_matchable(&document).is_err());
    }
}
