// SPDX-License-Identifier: AGPL-3.0-or-later
//! `looklab prep`: pair, prepare and write sample blobs; or report the pairing
//! of a folder.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use looklab_meta::{RemovedField, SanitizedMetadata, parse, parse_jpeg};
use looklab_prep::{
    JpegMember, JpegRole, LocalImageKey, PairingFile, PairingOutcome, PrepParams, PreparedImage,
    PreselectOptions, PriorTransform, RawPair, UnpairedFile, UnpairedReason, decode_jpeg, pair,
    prepare_group, prepare_group_with_decoded_raw, preselect,
};
use looklab_raw::{DecodedRaw, RawDecoder, RawlerDecoder};
use looklab_samples::{PrepVersion, PriorTransformVersion};
use serde::Serialize;

use crate::body_transform::TransformFile;
use crate::util;

#[derive(Debug, Args)]
pub struct PrepArgs {
    /// RAW file to prepare; the JPEGs in `--jpeg` are its renders.
    #[arg(long, conflicts_with = "dir")]
    pub raw: Option<PathBuf>,
    /// JPEG rendered from the RAW; repeat to prepare one re-render group.
    #[arg(long, num_args = 1.., conflicts_with = "dir")]
    pub jpeg: Vec<PathBuf>,
    /// Body prior: a transform YAML, or `from-raw` to take the RAW's DNG colour matrices.
    #[arg(long, default_value = "from-raw")]
    pub body_prior: String,
    /// Folder of RAWs and JPEGs to pair, rank and report; writes nothing.
    #[arg(long, conflicts_with_all = ["raw", "jpeg", "out"])]
    pub dir: Option<PathBuf>,
    /// Output directory for `.llsamp` blobs, `prep.json` and `transform.yaml`.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// SENSITIVE: the seed file reveals sampling order. Reusing one reproduces a previous run byte for byte; never upload or paste it. Defaults to `<out>/seeds.json`.
    #[arg(long)]
    pub seed_file: Option<PathBuf>,
}

pub fn run(args: &PrepArgs) -> Result<()> {
    if let Some(dir) = &args.dir {
        return report_folder(dir);
    }
    prepare_one(args)
}

fn prepare_one(args: &PrepArgs) -> Result<()> {
    let raw_path = args
        .raw
        .as_deref()
        .context("`--raw` is required unless `--dir` is given")?;
    if args.jpeg.is_empty() {
        bail!("`--jpeg` is required unless `--dir` is given");
    }
    let out = args
        .out
        .as_deref()
        .context("`--out` is required unless `--dir` is given")?;

    let raw_bytes =
        std::fs::read(raw_path).with_context(|| format!("reading {}", raw_path.display()))?;
    let jpeg_bytes = args
        .jpeg
        .iter()
        .map(|path| std::fs::read(path).with_context(|| format!("reading {}", path.display())))
        .collect::<Result<Vec<_>>>()?;
    let jpeg_refs: Vec<&[u8]> = jpeg_bytes.iter().map(Vec::as_slice).collect();

    let prior_version = if args.body_prior == "from-raw" {
        PriorTransformVersion::new(1)
    } else {
        PriorTransformVersion::new(TransformFile::load(Path::new(&args.body_prior))?.version)
    };
    let seed_key = LocalImageKey::new(&file_name(raw_path))
        .with_context(|| format!("{} is not a usable image key", raw_path.display()))?;

    let seed_path = args
        .seed_file
        .clone()
        .unwrap_or_else(|| out.join("seeds.json"));
    let mut seeds = if seed_path.exists() {
        let text = std::fs::read_to_string(&seed_path)
            .with_context(|| format!("reading {}", seed_path.display()))?;
        looklab_prep::SeedFile::from_json(&text)
            .with_context(|| format!("parsing {}", seed_path.display()))?
    } else {
        looklab_prep::SeedFile::new(prior_version)
    };
    let seed = match seeds.seed_for(&seed_key) {
        Some(seed) => seed,
        None => looklab_prep::SamplingSeed::fresh().context("generating a sampling seed")?,
    };
    seeds.insert(seed_key.clone(), seed);

    let params = PrepParams::default();
    let prepared = match util::catch_file(raw_path, || {
        let (prior, transform, decoded) = load_prior(args, &raw_bytes)?;
        let prepared = match decoded {
            Some(decoded) => prepare_group_with_decoded_raw(
                &raw_bytes, decoded, &jpeg_refs, &prior, seed, &params,
            ),
            None => prepare_group(&raw_bytes, &jpeg_refs, &prior, seed, &params),
        }
        .context("preparing the RAW/JPEG group")?;
        Ok((prior, transform, prepared))
    }) {
        util::FileResult::Done(value) => value,
        util::FileResult::Failed => bail!("no samples were written"),
    };
    let (prior, transform, prepared) = prepared;

    let raw_stem = file_stem(raw_path);
    let mut summary_images = Vec::with_capacity(prepared.len());
    for (index, image) in prepared.iter().enumerate() {
        let blob_name = blob_name(&raw_stem, args, index);
        let bytes = looklab_samples::encode(&image.samples)
            .with_context(|| format!("encoding {}", blob_name))?;
        let blob_path = out.join(&blob_name);
        let written = crate::write(&blob_path, &bytes)?;
        summary_images.push(SummaryImage::new(
            raw_path,
            args.jpeg.get(index),
            &blob_name,
            written,
            image,
        ));
        println!(
            "{}: {} samples, {} bytes from {}",
            blob_path.display(),
            image.samples.n(),
            written,
            args.jpeg
                .get(index)
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "-".to_owned())
        );
    }

    let summary = PrepSummary {
        format: "looklab-prep/1",
        prep_version: PrepVersion::new(looklab_prep::PREP_VERSION).get(),
        prior_transform_version: prior.version().get(),
        prep_params: params,
        seeds: SeedSummary {
            file: seed_path.display().to_string(),
            sensitive: true,
        },
        images: summary_images,
    };
    let json = serde_json::to_string_pretty(&summary)?;
    let summary_path = out.join("prep.json");
    crate::write(&summary_path, json.as_bytes())?;

    if let Some(transform) = transform {
        let yaml = transform.to_yaml()?;
        crate::write(&out.join("transform.yaml"), yaml.as_bytes())?;
    }

    let seed_json = seeds.to_json().context("serialising the seed file")?;
    crate::write(&seed_path, seed_json.as_bytes())?;
    eprintln!(
        "warning: {} is SENSITIVE: it reveals sampling order. Never upload it or paste it into a bug report.",
        seed_path.display()
    );
    println!("summary: {}", summary_path.display());
    Ok(())
}

fn load_prior(
    args: &PrepArgs,
    raw_bytes: &[u8],
) -> Result<(PriorTransform, Option<TransformFile>, Option<DecodedRaw>)> {
    if args.body_prior == "from-raw" {
        let decoded = RawlerDecoder
            .decode(raw_bytes)
            .context("decoding the RAW for its colour matrices and preparation")?;
        let matrices = decoded
            .color_matrices
            .as_ref()
            .context("the RAW carries no colour matrices; pass `--body-prior <yaml>`")?;
        let prior = PriorTransform::from_camera_matrices(
            matrices,
            decoded.as_shot_wb,
            PriorTransformVersion::new(1),
        )
        .context("building the body prior from the RAW's colour matrices")?;
        let body = body_id(&decoded.make, &decoded.model);
        let transform = TransformFile::from_color_matrices(matrices, body, 1).ok();
        return Ok((prior, transform, Some(decoded)));
    }
    let file = TransformFile::load(Path::new(&args.body_prior))?;
    let prior = file.prior_transform()?;
    Ok((prior, None, None))
}

fn blob_name(raw_stem: &str, args: &PrepArgs, index: usize) -> String {
    if args.jpeg.len() == 1 {
        return format!("{raw_stem}.llsamp");
    }
    let member = args
        .jpeg
        .get(index)
        .map(|path| file_stem(path))
        .unwrap_or_else(|| index.to_string());
    format!("{raw_stem}-{member}.llsamp")
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".to_owned())
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".to_owned())
}

/// Lowercase `<make>/<model>` with runs of non-alphanumerics collapsed to `-`.
fn body_id(make: &str, model: &str) -> Option<String> {
    let slug = |text: &str| -> String {
        let mut out = String::new();
        let mut dash = false;
        for character in text.chars() {
            if character.is_ascii_alphanumeric() {
                out.push(character.to_ascii_lowercase());
                dash = false;
            } else if !dash && !out.is_empty() {
                out.push('-');
                dash = true;
            }
        }
        out.trim_end_matches('-').to_owned()
    };
    let identity = looklab_prep::CameraIdentity::new(make, model)?;
    let make = slug(identity.make());
    let model = slug(identity.model());
    (!make.is_empty() && !model.is_empty()).then(|| format!("{make}/{model}"))
}

#[derive(Serialize)]
struct PrepSummary {
    format: &'static str,
    prep_version: u32,
    prior_transform_version: u32,
    prep_params: PrepParams,
    seeds: SeedSummary,
    images: Vec<SummaryImage>,
}

#[derive(Serialize)]
struct SeedSummary {
    file: String,
    sensitive: bool,
}

#[derive(Serialize)]
struct SummaryImage {
    raw: String,
    jpeg: Option<String>,
    blob: String,
    bytes: u64,
    samples: u32,
    cct_hint_k: f32,
    output_space: String,
    sanitized: SanitizedMetadata,
    removed_fields: Vec<RemovedField>,
}

impl SummaryImage {
    fn new(
        raw: &Path,
        jpeg: Option<&PathBuf>,
        blob: &str,
        bytes: u64,
        prepared: &PreparedImage,
    ) -> Self {
        let header = prepared.samples.header();
        Self {
            raw: file_name(raw),
            jpeg: jpeg.map(|path| file_name(path)),
            blob: blob.to_owned(),
            bytes,
            samples: header.n,
            cct_hint_k: header.cct_hint_k,
            output_space: format!("{:?}", header.output_space),
            sanitized: prepared.sanitized.clone(),
            removed_fields: prepared.removed.clone(),
        }
    }
}

fn report_folder(dir: &Path) -> Result<()> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))?;
    let mut names = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.is_file() {
            names.push(path);
        }
    }
    names.sort();

    let mut files = Vec::new();
    let mut bare_jpegs = Vec::new();
    let mut unreadable = Vec::new();
    let mut unsupported = Vec::new();
    for path in &names {
        let name = file_name(path);
        let Some(key) = LocalImageKey::new(&name) else {
            unsupported.push(format!("{name} (not a usable file name)"));
            continue;
        };
        if !matches!(
            key.kind(),
            looklab_prep::FileKind::Raw(_) | looklab_prep::FileKind::Jpeg
        ) {
            unsupported.push(name);
            continue;
        }
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                unreadable.push(format!("{name}: {error}"));
                continue;
            }
        };
        let parsed = util::catch_panic(|| match key.kind() {
            looklab_prep::FileKind::Jpeg => parse_jpeg(&bytes),
            _ => parse(&bytes),
        });
        let metadata = match parsed {
            Ok(metadata) => metadata,
            Err(payload) => {
                let message = payload
                    .downcast_ref::<&str>()
                    .map(|message| (*message).to_owned())
                    .or_else(|| payload.downcast_ref::<String>().cloned());
                match message {
                    Some(message) => eprintln!("{}: {message}", path.display()),
                    None => eprintln!("{}: metadata parsing panicked", path.display()),
                }
                unreadable.push(name);
                continue;
            }
        };
        match metadata {
            Ok(metadata) => {
                files.push(PairingFile::new(key, &metadata));
            }
            Err(_) if matches!(key.kind(), looklab_prep::FileKind::Jpeg) => bare_jpegs.push(key),
            Err(error) => unreadable.push(format!("{name}: {error}")),
        }
    }

    let mut outcome = pair(&files);
    if !bare_jpegs.is_empty() {
        attach_bare_jpegs(&mut outcome, bare_jpegs, &files);
    }
    println!(
        "{}: {} paired RAW(s), {} unpaired file(s)",
        dir.display(),
        outcome.pairs.len(),
        outcome.unpaired.len()
    );
    for raw in &outcome.pairs {
        println!("pair {}", raw.raw.as_str());
        for member in &raw.jpegs {
            println!("  {} ({})", member.key.as_str(), role_label(&member.role));
        }
    }
    for file in &outcome.unpaired {
        println!(
            "unpaired {} ({})",
            file.key.as_str(),
            unpaired_label(file.reason)
        );
    }
    for name in &unsupported {
        println!(
            "unpaired {name} ({})",
            unpaired_label(UnpairedReason::UnsupportedFileKind)
        );
    }
    for name in &unreadable {
        println!("unreadable {name}");
    }

    print_preselection(dir, &outcome)
}

/// JPEGs without EXIF pair by file stem, since no capture time or body is available.
fn attach_bare_jpegs(
    outcome: &mut PairingOutcome,
    jpegs: Vec<LocalImageKey>,
    named: &[PairingFile],
) {
    for key in jpegs {
        let member = JpegMember {
            key: key.clone(),
            role: JpegRole::Rerender,
        };
        if let Some(pair) = outcome
            .pairs
            .iter_mut()
            .find(|pair| pair.raw.stem().eq_ignore_ascii_case(key.stem()))
        {
            pair.jpegs.push(member);
            pair.jpegs.sort_by(|a, b| a.key.cmp(&b.key));
            continue;
        }
        let raw = named
            .iter()
            .find(|file| file.kind().is_raw() && file.key().stem().eq_ignore_ascii_case(key.stem()))
            .map(|file| file.key().clone());
        match raw {
            Some(raw) => {
                outcome.pairs.push(RawPair {
                    raw: raw.clone(),
                    jpegs: vec![member],
                });
                outcome.unpaired.retain(|file| file.key != raw);
            }
            None => outcome.unpaired.push(UnpairedFile {
                key,
                reason: UnpairedReason::NoCounterpart,
            }),
        }
    }
    outcome.pairs.sort_by(|a, b| a.raw.cmp(&b.raw));
    outcome.unpaired.sort_by(|a, b| a.key.cmp(&b.key));
}

fn print_preselection(dir: &Path, outcome: &PairingOutcome) -> Result<()> {
    let mut thumbnails = Vec::new();
    let mut pairs = Vec::new();
    for raw in &outcome.pairs {
        let Some(member) = raw.jpegs.first() else {
            continue;
        };
        let path = dir.join(member.key.as_str());
        let decoded = util::catch_file(&path, || {
            let bytes = std::fs::read(&path)?;
            Ok(decode_jpeg(&bytes)?)
        });
        match decoded {
            util::FileResult::Done(jpeg) => {
                thumbnails.push(jpeg.image.downsample_area(40_000));
                pairs.push(raw.raw.as_str().to_owned());
            }
            util::FileResult::Failed => {}
        }
    }
    if thumbnails.is_empty() {
        return Ok(());
    }
    let order =
        preselect(&thumbnails, PreselectOptions::default()).context("ranking the paired JPEGs")?;
    println!("preselection order ({} pair(s)):", order.len());
    for (rank, index) in order.iter().enumerate() {
        println!("  {}. {}", rank + 1, pairs[*index]);
    }
    Ok(())
}

fn role_label(role: &JpegRole) -> String {
    match role {
        JpegRole::Rerender => "in-camera render".to_owned(),
        JpegRole::Duplicate { of } => format!("duplicate of {}", of.as_str()),
        JpegRole::SeparateShot => "separate shot".to_owned(),
    }
}

fn unpaired_label(reason: UnpairedReason) -> &'static str {
    match reason {
        UnpairedReason::UnsupportedFileKind => "unsupported file kind",
        UnpairedReason::NoCounterpart => "no RAW/JPEG counterpart",
        UnpairedReason::BodyMismatch => "counterpart is from another body",
        UnpairedReason::AmbiguousCounterpart => "ambiguous counterpart",
    }
}
