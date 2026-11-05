// SPDX-License-Identifier: AGPL-3.0-or-later
//! Parsing helpers shared by the proposal form and the confirmation form.

use std::collections::BTreeMap;

use looklab_schema::{
    Date, FixtureDonation, FixtureLicense, LabeledFile, LabeledTag, MakernoteTag, MakernoteValue,
    MappingInference, Sha256Hex, SimKey, infer_mapping, validate_confirmation,
};

pub fn kind_label(kind: looklab_schema::ProposalKind) -> &'static str {
    use looklab_schema::ProposalKind;
    match kind {
        ProposalKind::Body => "Add a camera",
        ProposalKind::Mapping => "Add or fix a film simulation on a camera",
        ProposalKind::SchemaChange => "Change a camera's controls",
        ProposalKind::LutLook => "Add a custom look",
        ProposalKind::InterfaceProfile => "Add an editing-app profile",
        ProposalKind::EpochSplit => "Separate firmware versions",
        ProposalKind::LookSplit => "Separate looks",
        ProposalKind::GateOverride => "Review a release exception",
    }
}

pub fn menu_summary(
    kind: looklab_schema::ProposalKind,
    summary: &looklab_schema::ProposalSummary,
) -> looklab_schema::ProposalSummary {
    use looklab_schema::ProposalKind;
    let headline = match kind {
        ProposalKind::Body => summary
            .headline
            .replace("camera body", "camera")
            .replace("software body", "editing app"),
        ProposalKind::SchemaChange => summary.headline.replace("control schema", "controls"),
        ProposalKind::LutLook => summary.headline.replace("LUT-defined look", "custom look"),
        ProposalKind::InterfaceProfile => summary
            .headline
            .replace("software interface profile", "editing-app profile"),
        ProposalKind::EpochSplit => summary.headline.replace("new epoch", "new firmware range"),
        _ => summary.headline.clone(),
    };
    let details = if kind == ProposalKind::SchemaChange {
        summary.details.clone()
    } else {
        Vec::new()
    };
    looklab_schema::ProposalSummary::with_details(headline, details)
}
use sha2::{Digest, Sha256};

pub fn file_menu_value(bytes: &[u8]) -> Result<String, String> {
    let metadata = looklab_meta::parse(bytes).map_err(|error| error.to_string())?;
    let look = metadata.fuji.map(|fuji| fuji.look).ok_or_else(|| {
        "This file has no supported camera menu information. Try an original camera JPEG."
            .to_owned()
    })?;
    let values = [
        ("FilmMode", look.film_mode.map(|value| value.raw)),
        ("DynamicRange", look.dynamic_range.map(|value| value.raw)),
        (
            "DynamicRangeSetting",
            look.dynamic_range_setting.map(|value| value.raw),
        ),
        (
            "DevelopmentDynamicRange",
            look.development_dynamic_range.map(|value| value.raw),
        ),
        ("GrainSize", look.grain_size.map(|value| value.raw)),
        (
            "GrainRoughness",
            look.grain_roughness.map(|value| value.raw),
        ),
        (
            "ColorChromeEffect",
            look.color_chrome_effect.map(|value| value.raw),
        ),
        (
            "ColorChromeFxBlue",
            look.color_chrome_fx_blue.map(|value| value.raw),
        ),
        ("WhiteBalance", look.white_balance.map(|value| value.raw)),
        ("HighlightTone", look.highlight_tone.map(|value| value.raw)),
        ("ShadowTone", look.shadow_tone.map(|value| value.raw)),
        (
            "NoiseReduction",
            look.noise_reduction.map(|value| value.raw),
        ),
    ];
    let tags = values
        .into_iter()
        .filter_map(|(tag, value)| {
            value
                .and_then(|value| u64::try_from(value).ok())
                .map(|value| format!("{tag}={value}"))
        })
        .collect::<Vec<_>>();
    if tags.is_empty() {
        return Err(
            "This file has no supported camera menu information. Try an original camera JPEG."
                .to_owned(),
        );
    }
    Ok(tags.join("\n"))
}

#[derive(Clone, Debug, PartialEq)]
pub struct FixtureFile {
    pub name: String,
    pub bytes: Vec<u8>,
    pub sha256: Sha256Hex,
}

impl FixtureFile {
    /// # Errors
    /// Reports a file with no name.
    pub fn new(name: String, bytes: Vec<u8>) -> Result<Self, String> {
        if name.is_empty() {
            return Err("the browser reported a file with no name".to_owned());
        }
        let sha256 = digest(&bytes)?;
        Ok(Self {
            name,
            bytes,
            sha256,
        })
    }
}

fn digest(bytes: &[u8]) -> Result<Sha256Hex, String> {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    Sha256Hex::parse(&hex).map_err(|error| error.to_string())
}

/// # Errors
/// Reports a malformed line, an unknown menu answer or an unknown tag.
pub fn parse_labeled_files(text: &str) -> Result<MappingInference, String> {
    let mut files = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (label, rest) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| format!("line {}: expected `answer Tag=value`", index + 1))?;
        let label =
            SimKey::try_from(label).map_err(|error| format!("line {}: {error}", index + 1))?;
        let mut tags = BTreeMap::new();
        for token in rest.split_whitespace() {
            let (tag, value) = token
                .split_once('=')
                .ok_or_else(|| format!("line {}: `{token}` is not `Tag=value`", index + 1))?;
            let tag = MakernoteTag::try_from(tag)
                .map_err(|error| format!("line {}: {error}", index + 1))?;
            let value: u64 = match value
                .strip_prefix("0x")
                .or_else(|| value.strip_prefix("0X"))
            {
                Some(hex) => u64::from_str_radix(hex, 16),
                None => value.parse(),
            }
            .map_err(|_| format!("line {}: `{value}` is not a makernote value", index + 1))?;
            tags.insert(tag, MakernoteValue(value));
        }
        files.push(LabeledFile { label, tags });
    }
    infer_mapping(&files).map_err(|error| error.to_string())
}

/// # Errors
/// Reports a malformed line, an unknown tag, or a tag outside the confirmation whitelist.
pub fn parse_tag_values(text: &str, label: &str) -> Result<Vec<LabeledTag>, String> {
    let label = SimKey::try_from(label).map_err(|error| error.to_string())?;
    let mut tags = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (tag, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {}: `{line}` is not `Tag=value`", index + 1))?;
        let tag = MakernoteTag::try_from(tag.trim())
            .map_err(|error| format!("line {}: {error}", index + 1))?;
        let value: u64 = match value.trim().strip_prefix("0x") {
            Some(hex) => u64::from_str_radix(hex, 16),
            None => value.trim().parse(),
        }
        .map_err(|_| format!("line {}: not a makernote value", index + 1))?;
        tags.push(LabeledTag {
            tag,
            value: MakernoteValue(value),
            label: label.clone(),
        });
    }
    validate_confirmation(&tags).map_err(|errors| {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    Ok(tags)
}

/// # Errors
/// Reports a donation the fixture manifest would reject.
pub fn donation(
    file: &FixtureFile,
    make: &str,
    model: &str,
    source: String,
) -> Result<FixtureDonation, String> {
    let donated_on = Date::parse(&today()).map_err(|error| error.to_string())?;
    let size = u64::try_from(file.bytes.len()).map_err(|_| "file is too large".to_owned())?;
    let donation = FixtureDonation {
        file: file.name.clone(),
        description: format!("Donated with a proposal: {}", file.name),
        make: make.trim().to_owned(),
        model: model.trim().to_owned(),
        sha256: file.sha256,
        size,
        source_url: None,
        source: Some(source),
        donator: None,
        donated_on,
        license: FixtureLicense::Cc0_1_0,
        committed: false,
    };
    donation.validate().map_err(|errors| {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    Ok(donation)
}

fn today() -> String {
    let iso = js_sys::Date::new_0().to_iso_string();
    iso.as_string()
        .and_then(|text| text.get(0..10).map(str::to_owned))
        .unwrap_or_default()
}
