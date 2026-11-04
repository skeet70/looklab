// SPDX-License-Identifier: AGPL-3.0-or-later
//! Filesystem-free RAW/JPEG pairing and in-camera re-render detection, [docs/07 §2](../../../docs/07-contribution.md).

use looklab_meta::{CaptureTime, ImageNumber, LookSettings, ParsedMetadata, pairing_data};

/// RAW container formats recognised by file extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RawFormat {
    Arw,
    Cr2,
    Cr3,
    Dng,
    Nef,
    Orf,
    Pef,
    Raf,
    Rw2,
    Srw,
}

impl RawFormat {
    pub fn from_extension(extension: &str) -> Option<Self> {
        Some(match extension.to_ascii_lowercase().as_str() {
            "arw" => Self::Arw,
            "cr2" => Self::Cr2,
            "cr3" => Self::Cr3,
            "dng" => Self::Dng,
            "nef" => Self::Nef,
            "orf" => Self::Orf,
            "pef" => Self::Pef,
            "raf" => Self::Raf,
            "rw2" => Self::Rw2,
            "srw" => Self::Srw,
            _ => return None,
        })
    }

    pub const fn extension(self) -> &'static str {
        match self {
            Self::Arw => "arw",
            Self::Cr2 => "cr2",
            Self::Cr3 => "cr3",
            Self::Dng => "dng",
            Self::Nef => "nef",
            Self::Orf => "orf",
            Self::Pef => "pef",
            Self::Raf => "raf",
            Self::Rw2 => "rw2",
            Self::Srw => "srw",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FileKind {
    Raw(RawFormat),
    Jpeg,
    /// Not a RAW or JPEG name.
    Other,
}

impl FileKind {
    pub const fn is_raw(self) -> bool {
        matches!(self, Self::Raw(_))
    }
}

/// A bare file name. Never uploaded; only a local seed file may hold one.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalImageKey(String);

impl LocalImageKey {
    pub fn new(name: &str) -> Option<Self> {
        let name = name.trim();
        if name.is_empty() || name.len() > 1024 {
            return None;
        }
        if name.contains(['/', '\\', '\0']) || name == "." || name == ".." {
            return None;
        }
        Some(Self(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The name with its final extension removed; `"a.RAF"` and `"a.JPG"` share
    /// the stem `"a"`.
    pub fn stem(&self) -> &str {
        match self.0.rfind('.') {
            Some(index) if index > 0 => &self.0[..index],
            _ => &self.0,
        }
    }

    pub fn extension(&self) -> Option<&str> {
        self.0.rfind('.').map(|index| &self.0[index + 1..])
    }

    pub fn kind(&self) -> FileKind {
        match self.extension() {
            Some(extension) => match RawFormat::from_extension(extension) {
                Some(format) => FileKind::Raw(format),
                None if extension.eq_ignore_ascii_case("jpg")
                    || extension.eq_ignore_ascii_case("jpeg") =>
                {
                    FileKind::Jpeg
                }
                None => FileKind::Other,
            },
            None => FileKind::Other,
        }
    }
}

/// Make and model normalised for comparison.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BodyKey(String);

impl BodyKey {
    /// Normalised `<make> <model>`; `None` when either tag is missing.
    pub fn from_metadata(metadata: &ParsedMetadata) -> Option<Self> {
        let make = metadata.exif.make.as_deref()?;
        let model = metadata.exif.model.as_deref()?;
        let make = normalise(make);
        let model = normalise(model);
        if make.is_empty() || model.is_empty() {
            return None;
        }
        Some(Self(format!("{make} {model}")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn normalise(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.extend(word.chars().flat_map(char::to_lowercase));
    }
    out
}

/// The pairing fields of one file, extracted from parsed metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct PairingFile {
    key: LocalImageKey,
    kind: FileKind,
    capture_time: Option<CaptureTime>,
    image_number: Option<ImageNumber>,
    body: Option<BodyKey>,
    look: Option<LookSettings>,
}

impl PairingFile {
    pub fn new(key: LocalImageKey, metadata: &ParsedMetadata) -> Self {
        let pairing = pairing_data(metadata);
        Self {
            kind: key.kind(),
            capture_time: pairing.capture_time,
            image_number: pairing.image_number,
            body: BodyKey::from_metadata(metadata),
            look: metadata.fuji.as_ref().map(|fuji| fuji.look.clone()),
            key,
        }
    }

    pub fn key(&self) -> &LocalImageKey {
        &self.key
    }

    pub fn kind(&self) -> FileKind {
        self.kind
    }

    pub fn capture_time(&self) -> Option<&CaptureTime> {
        self.capture_time.as_ref()
    }

    pub fn image_number(&self) -> Option<ImageNumber> {
        self.image_number
    }

    pub fn body(&self) -> Option<&BodyKey> {
        self.body.as_ref()
    }

    pub fn look(&self) -> Option<&LookSettings> {
        self.look.as_ref()
    }
}

/// Why a file did not pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnpairedReason {
    /// Not a RAW or JPEG file name.
    UnsupportedFileKind,
    /// No file of the other kind shares a base name and body, or a capture time
    /// and body.
    NoCounterpart,
    /// The only candidates shared a name or capture time but not the body.
    BodyMismatch,
    /// Files shared a name or capture time, but which RAW a JPEG belongs to
    /// could not be decided.
    AmbiguousCounterpart,
}

/// How a JPEG relates to the RAW it paired with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JpegRole {
    /// Rendered in camera from the paired RAW: same capture time or image
    /// number, with settings distinct from every earlier render.
    Rerender,
    /// Same settings as an earlier render of the same RAW.
    Duplicate { of: LocalImageKey },
    /// Paired with the RAW by file name, but not a render of it.
    SeparateShot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JpegMember {
    pub key: LocalImageKey,
    pub role: JpegRole,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawPair {
    pub raw: LocalImageKey,
    pub jpegs: Vec<JpegMember>,
}

impl RawPair {
    /// The JPEGs that form this RAW's in-camera re-render group, in file-name
    /// order.
    pub fn rerender_members(&self) -> impl Iterator<Item = &LocalImageKey> {
        self.jpegs
            .iter()
            .filter(|member| member.role == JpegRole::Rerender)
            .map(|member| &member.key)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnpairedFile {
    pub key: LocalImageKey,
    pub reason: UnpairedReason,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PairingOutcome {
    /// One entry per paired RAW, ordered by RAW file name.
    pub pairs: Vec<RawPair>,
    /// Every file that did not pair, ordered by file name.
    pub unpaired: Vec<UnpairedFile>,
}

impl PairingOutcome {
    /// One entry per re-render group: the RAW and its distinct in-camera
    /// renders. Groups of a single render are included.
    pub fn rerender_groups(&self) -> Vec<RawPair> {
        self.pairs
            .iter()
            .filter_map(|pair| {
                let jpegs: Vec<JpegMember> = pair
                    .jpegs
                    .iter()
                    .filter(|member| member.role == JpegRole::Rerender)
                    .cloned()
                    .collect();
                (!jpegs.is_empty()).then(|| RawPair {
                    raw: pair.raw.clone(),
                    jpegs,
                })
            })
            .collect()
    }
}

fn bodies_compatible(a: Option<&BodyKey>, b: Option<&BodyKey>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

/// Same calendar second; subseconds must agree when both files have them.
fn times_match(a: Option<&CaptureTime>, b: Option<&CaptureTime>) -> bool {
    let (Some(a), Some(b)) = (a, b) else {
        return false;
    };
    a.year == b.year
        && a.month == b.month
        && a.day == b.day
        && a.hour == b.hour
        && a.minute == b.minute
        && a.second == b.second
        && match (&a.subsec, &b.subsec) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
}

fn stems_match(a: &LocalImageKey, b: &LocalImageKey) -> bool {
    a.stem().eq_ignore_ascii_case(b.stem())
}

fn looks_differ(a: Option<&LookSettings>, b: Option<&LookSettings>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a != b,
        _ => true,
    }
}

/// Pairs files by name, then by capture time, and classifies the JPEGs of every
/// pair. The input order does not affect the result.
pub fn pair(files: &[PairingFile]) -> PairingOutcome {
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by(|a, b| {
        files[*a]
            .key
            .cmp(&files[*b].key)
            .then_with(|| files[*a].kind.cmp(&files[*b].kind))
    });
    let raws: Vec<usize> = order
        .iter()
        .copied()
        .filter(|index| files[*index].kind.is_raw())
        .collect();
    let jpegs: Vec<usize> = order
        .iter()
        .copied()
        .filter(|index| files[*index].kind == FileKind::Jpeg)
        .collect();

    let mut unpaired = Vec::new();
    let mut rejected: Vec<Option<UnpairedReason>> = vec![None; files.len()];
    for &index in &order {
        if files[index].kind == FileKind::Other {
            unpaired.push(UnpairedFile {
                key: files[index].key.clone(),
                reason: UnpairedReason::UnsupportedFileKind,
            });
        }
    }

    let mut claimed = vec![false; files.len()];
    let mut members_of: Vec<(usize, Vec<usize>)> = Vec::new();

    for &raw in &raws {
        let mut members = Vec::new();
        for &jpeg in &jpegs {
            if claimed[jpeg] {
                continue;
            }
            if !stems_match(&files[raw].key, &files[jpeg].key) {
                continue;
            }
            if bodies_compatible(files[raw].body.as_ref(), files[jpeg].body.as_ref()) {
                claimed[jpeg] = true;
                members.push(jpeg);
            } else {
                rejected[jpeg] = Some(UnpairedReason::BodyMismatch);
                rejected[raw] = Some(UnpairedReason::BodyMismatch);
            }
        }
        members_of.push((raw, members));
    }

    for &jpeg in &jpegs {
        if claimed[jpeg] {
            continue;
        }
        let time_matches: Vec<usize> = raws
            .iter()
            .copied()
            .filter(|raw| {
                bodies_compatible(files[*raw].body.as_ref(), files[jpeg].body.as_ref())
                    && times_match(
                        files[*raw].capture_time.as_ref(),
                        files[jpeg].capture_time.as_ref(),
                    )
            })
            .collect();
        match time_matches.as_slice() {
            [] => {
                let body_seen = raws.iter().any(|raw| {
                    !bodies_compatible(files[*raw].body.as_ref(), files[jpeg].body.as_ref())
                        && times_match(
                            files[*raw].capture_time.as_ref(),
                            files[jpeg].capture_time.as_ref(),
                        )
                });
                if body_seen {
                    rejected[jpeg] = Some(UnpairedReason::BodyMismatch);
                }
            }
            [raw] => {
                claimed[jpeg] = true;
                if let Some(entry) = members_of.iter_mut().find(|(index, _)| index == raw) {
                    entry.1.push(jpeg);
                }
            }
            _ => {
                for raw in time_matches {
                    rejected[raw] = Some(UnpairedReason::AmbiguousCounterpart);
                }
                rejected[jpeg] = Some(UnpairedReason::AmbiguousCounterpart);
                unpaired.push(UnpairedFile {
                    key: files[jpeg].key.clone(),
                    reason: UnpairedReason::AmbiguousCounterpart,
                });
            }
        }
    }

    let mut pairs = Vec::new();
    for (raw, mut jpeg_indices) in members_of {
        jpeg_indices.sort_by(|a, b| files[*a].key.cmp(&files[*b].key));
        jpeg_indices.dedup();
        if jpeg_indices.is_empty() {
            unpaired.push(UnpairedFile {
                key: files[raw].key.clone(),
                reason: rejected[raw].unwrap_or(UnpairedReason::NoCounterpart),
            });
            continue;
        }
        let mut seen: Vec<(LocalImageKey, LookSettings)> = Vec::new();
        let mut members = Vec::new();
        for jpeg in jpeg_indices {
            let is_render = times_match(
                files[raw].capture_time.as_ref(),
                files[jpeg].capture_time.as_ref(),
            ) || match (files[raw].image_number, files[jpeg].image_number) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            };
            let role = if !is_render {
                JpegRole::SeparateShot
            } else {
                let look = files[jpeg].look.as_ref();
                match seen
                    .iter()
                    .find(|(_, earlier)| !looks_differ(Some(earlier), look))
                {
                    Some((of, _)) => JpegRole::Duplicate { of: of.clone() },
                    None => {
                        if let Some(look) = look {
                            seen.push((files[jpeg].key.clone(), look.clone()));
                        }
                        JpegRole::Rerender
                    }
                }
            };
            members.push(JpegMember {
                key: files[jpeg].key.clone(),
                role,
            });
        }
        pairs.push(RawPair {
            raw: files[raw].key.clone(),
            jpegs: members,
        });
    }
    pairs.sort_by(|a, b| a.raw.cmp(&b.raw));

    for &jpeg in &jpegs {
        if claimed[jpeg] {
            continue;
        }
        if unpaired.iter().any(|entry| entry.key == files[jpeg].key) {
            continue;
        }
        unpaired.push(UnpairedFile {
            key: files[jpeg].key.clone(),
            reason: rejected[jpeg].unwrap_or(UnpairedReason::NoCounterpart),
        });
    }

    unpaired.sort_by(|a, b| a.key.cmp(&b.key));
    PairingOutcome { pairs, unpaired }
}
