// SPDX-License-Identifier: AGPL-3.0-or-later
//! Parsing helpers shared by the proposal form and the confirmation form.

use std::collections::BTreeMap;

use looklab_schema::{
    Date, FixtureDonation, FixtureLicense, LabeledFile, LabeledTag, MakernoteTag, MakernoteValue,
    MappingInference, Sha256Hex, SimKey, infer_mapping, validate_confirmation,
};
use sha2::{Digest, Sha256};

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
