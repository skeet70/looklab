// SPDX-License-Identifier: AGPL-3.0-or-later
//! Local RAW/JPEG pairing for the contribution walkthrough ([docs/07 §2](../../../docs/07-contribution.md)).

use looklab_meta::ParsedMetadata;
use looklab_prep::pair::{self, FileKind, JpegRole, LocalImageKey, PairingFile, UnpairedReason};
use serde::Serialize;

use crate::error::FacadeError;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PairGroup {
    pub raw: String,
    pub jpegs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UnpairedEntry {
    pub name: String,
    pub reason: UnpairedReasonView,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnpairedReasonView {
    UnsupportedFileKind,
    NoCounterpart,
    BodyMismatch,
    AmbiguousCounterpart,
    Unreadable(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKindView {
    Raw,
    Jpeg,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FileFacts {
    pub name: String,
    pub kind: FileKindView,
    pub make: Option<String>,
    pub model: Option<String>,
    pub firmware: Option<String>,
    pub has_serial: bool,
    pub render_tag: Option<looklab_meta::RenderTag>,
    pub render_tag_malformed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PairingPlan {
    pub groups: Vec<PairGroup>,
    pub unpaired: Vec<UnpairedEntry>,
    pub facts: Vec<FileFacts>,
}

/// Pairs files by name, then by capture time.
///
/// # Errors
/// Only a file name that cannot be a seed key fails the whole plan; unreadable
/// metadata is reported per file.
pub fn inspect_files(files: &[(String, Vec<u8>)]) -> Result<PairingPlan, FacadeError> {
    let mut pairing_files = Vec::with_capacity(files.len());
    let mut facts = Vec::with_capacity(files.len());
    let mut unpaired = Vec::new();
    for (name, bytes) in files {
        let key = LocalImageKey::new(name)
            .ok_or_else(|| FacadeError::Input(format!("{name} is not a usable file name")))?;
        let kind = kind_view(key.kind());
        match looklab_meta::parse(bytes) {
            Ok(metadata) => {
                facts.push(facts_of(name, kind, &metadata));
                pairing_files.push(PairingFile::new(key, &metadata));
            }
            Err(error) => {
                facts.push(FileFacts {
                    name: name.clone(),
                    kind,
                    make: None,
                    model: None,
                    firmware: None,
                    has_serial: false,
                    render_tag: None,
                    render_tag_malformed: false,
                });
                unpaired.push(UnpairedEntry {
                    name: name.clone(),
                    reason: UnpairedReasonView::Unreadable(error.to_string()),
                });
            }
        }
    }

    let outcome = pair::pair(&pairing_files);
    let groups = outcome
        .pairs
        .iter()
        .map(|pair| PairGroup {
            raw: pair.raw.as_str().to_owned(),
            jpegs: pair
                .jpegs
                .iter()
                .filter(|member| !matches!(member.role, JpegRole::Duplicate { .. }))
                .map(|member| member.key.as_str().to_owned())
                .collect(),
        })
        .collect();
    unpaired.extend(outcome.unpaired.iter().map(|entry| UnpairedEntry {
        name: entry.key.as_str().to_owned(),
        reason: reason_view(entry.reason),
    }));
    unpaired.sort_by(|left, right| left.name.cmp(&right.name));

    Ok(PairingPlan {
        groups,
        unpaired,
        facts,
    })
}

fn facts_of(name: &str, kind: FileKindView, metadata: &ParsedMetadata) -> FileFacts {
    FileFacts {
        name: name.to_owned(),
        kind,
        make: metadata.exif.make.clone(),
        model: metadata.exif.model.clone(),
        firmware: metadata.firmware.clone(),
        has_serial: metadata.body_serial().is_some(),
        render_tag: metadata.render_tag.valid().cloned(),
        render_tag_malformed: metadata.render_tag.is_malformed(),
    }
}

fn kind_view(kind: FileKind) -> FileKindView {
    match kind {
        FileKind::Raw(_) => FileKindView::Raw,
        FileKind::Jpeg => FileKindView::Jpeg,
        FileKind::Other => FileKindView::Other,
    }
}

fn reason_view(reason: UnpairedReason) -> UnpairedReasonView {
    match reason {
        UnpairedReason::UnsupportedFileKind => UnpairedReasonView::UnsupportedFileKind,
        UnpairedReason::NoCounterpart => UnpairedReasonView::NoCounterpart,
        UnpairedReason::BodyMismatch => UnpairedReasonView::BodyMismatch,
        UnpairedReason::AmbiguousCounterpart => UnpairedReasonView::AmbiguousCounterpart,
    }
}
