// SPDX-License-Identifier: AGPL-3.0-or-later
//! Firmware card file (docs/04 §8): a deterministic writer, a strict reader and
//! the normative grid evaluator.

use looklab_color::RgbSpace;
use looklab_color::cam16::{Cam16Model, ViewingConditions};
use looklab_color::ucs::{UcsSpace, ucs_to_jmh};
use looklab_color::{Illuminant, Rgb, Xyz};
use looklab_model::LookDocument;
use looklab_model::file::{LookIdentity, LookVersion, Slug, WbPolicy as LookWbPolicy};
use looklab_model::probes;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::quantize::{f16_bits, f16_value};

pub const MAGIC: [u8; 4] = *b"LLCF";
pub const FORMAT_VERSION: u16 = 1;
pub const HEADER_LENGTH: usize = 64;
pub const SECTION_ENTRY_LENGTH: usize = 16;

pub const KIND_META: u32 = 1;
pub const KIND_SEMANTICS: u32 = 2;
pub const KIND_NEUTRAL: u32 = 3;
pub const KIND_FIT: u32 = 4;
pub const KIND_GRID: u32 = 5;

pub const NEUTRAL_COUNT: usize = 257;
pub const FIT_RECORD_LENGTH: usize = 40;

const LOW_EV: f64 = 12.0;
const HIGH_EV: f64 = 6.0;
const PIVOT: f64 = 0.18;

/// Every variant rejects the whole file.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum CardError {
    #[error("card is shorter than its 64-byte header")]
    TruncatedHeader,
    #[error("card magic is not LLCF")]
    Magic,
    #[error("card format version {0} is not 1")]
    FormatVersion(u16),
    #[error("card header length {0} is not 64")]
    HeaderLength(u16),
    #[error("card file length is {found}, expected {expected}")]
    FileLength { expected: u32, found: usize },
    #[error("card header CRC does not match")]
    HeaderCrc,
    #[error("card section table entry {0} is malformed")]
    Table(usize),
    #[error("card section kind {0} appears twice")]
    DuplicateSection(u32),
    #[error("card section kind {0} has a bad CRC")]
    SectionCrc(u32),
    #[error("card content id does not match its section data")]
    ContentId,
    #[error("card is missing the {0} section")]
    MissingSection(&'static str),
    #[error("card {0} section is malformed: {1}")]
    Section(&'static str, &'static str),
    #[error("card semantics field {field} is {value}, not 1")]
    Semantics { field: &'static str, value: u16 },
    #[error("card neutral count is {0}, not 257")]
    NeutralCount(u32),
    #[error("card grid side {0} is not 33 or 65")]
    GridSide(u32),
    #[error("card contains a value that is not finite")]
    NonFinite,
    #[error("card metadata: {0}")]
    Meta(&'static str),
    #[error("Look is not matchable: its display base is not pinned")]
    NotMatchable,
}

/// Input space id 1: scene-linear Rec.2020 adapted to D65 (docs/04 §8.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputSpace {
    SceneLinearRec2020,
}

/// Output space id 1: reference-viewing pre-clip linear Rec.2020 (docs/04 §8.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputSpace {
    ReferenceLinearRec2020,
}

/// Shaper id 1: the `preview.lut33` log2 shaper (docs/04 §8.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shaper {
    Log2,
}

/// Viewing conditions id 1: the reference conditions (docs/02 §1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Viewing {
    Reference,
}

/// White-balance policy id 1: adaptive (docs/04 §8.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WbPolicy {
    Adaptive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Semantics {
    pub input: InputSpace,
    pub output: OutputSpace,
    pub shaper: Shaper,
    pub viewing: Viewing,
    pub wb_policy: WbPolicy,
}

impl Semantics {
    pub const V1: Self = Self {
        input: InputSpace::SceneLinearRec2020,
        output: OutputSpace::ReferenceLinearRec2020,
        shaper: Shaper::Log2,
        viewing: Viewing::Reference,
        wb_policy: WbPolicy::Adaptive,
    };
}

/// META fields other than slug and version, which come from the Look identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardMeta {
    pub name: String,
    pub short_name: String,
    pub description: Option<String>,
    pub license: Option<String>,
}

impl CardMeta {
    pub fn new(name: impl Into<String>, short_name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            short_name: short_name.into(),
            description: None,
            license: None,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn with_license(mut self, license: impl Into<String>) -> Self {
        self.license = Some(license.into());
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    pub slug: Slug,
    pub version: LookVersion,
    pub name: String,
    pub short_name: String,
    pub description: Option<String>,
    pub license: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GridSide {
    S33,
    S65,
}

impl GridSide {
    pub const fn side(self) -> u32 {
        match self {
            Self::S33 => 33,
            Self::S65 => 65,
        }
    }

    pub const fn nodes(self) -> usize {
        let side = self.side() as usize;
        side * side * side
    }

    pub const fn from_side(side: u32) -> Option<Self> {
        match side {
            33 => Some(Self::S33),
            65 => Some(Self::S65),
            _ => None,
        }
    }
}

/// A shaped-coordinate grid, red fastest.
#[derive(Clone, Debug, PartialEq)]
pub struct Grid {
    side: GridSide,
    values: Vec<[f32; 3]>,
}

impl Grid {
    /// # Errors
    /// Rejects the wrong node count or a non-finite value.
    pub fn new(side: GridSide, values: Vec<[f32; 3]>) -> Result<Self, CardError> {
        if values.len() != side.nodes() {
            return Err(CardError::Section(
                "GRID",
                "node count does not match the side",
            ));
        }
        if values.iter().flatten().any(|value| !value.is_finite()) {
            return Err(CardError::NonFinite);
        }
        Ok(Self { side, values })
    }

    pub fn side(&self) -> GridSide {
        self.side
    }

    pub fn values(&self) -> &[[f32; 3]] {
        &self.values
    }

    fn node(&self, r: usize, g: usize, b: usize) -> [f64; 3] {
        let side = self.side.side() as usize;
        let value = self.values[r + side * (g + side * b)];
        [
            f64::from(value[0]),
            f64::from(value[1]),
            f64::from(value[2]),
        ]
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FitRecord {
    pub input: [f32; 3],
    pub output: [f32; 3],
    pub sigma: f32,
    pub weight: f32,
    pub supported: bool,
    pub probe: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CardSections {
    pub identity: LookIdentity,
    pub meta: CardMeta,
    pub neutral: Vec<[f32; 3]>,
    pub fit: Vec<FitRecord>,
    pub grid: Option<Grid>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CardOptions {
    pub grid: Option<GridSide>,
}

impl CardOptions {
    pub const fn with_grid(grid: Option<GridSide>) -> Self {
        Self { grid }
    }
}

impl Default for CardOptions {
    fn default() -> Self {
        Self {
            grid: Some(GridSide::S33),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectionInfo {
    pub kind: u32,
    pub offset: u32,
    pub length: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    content_id: [u8; 32],
    sections: Vec<SectionInfo>,
    meta: Meta,
    semantics: Semantics,
    neutral: Vec<[f32; 3]>,
    fit: Vec<FitRecord>,
    grid: Option<Grid>,
}

impl Card {
    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    pub fn semantics(&self) -> &Semantics {
        &self.semantics
    }

    pub fn neutral(&self) -> &[[f32; 3]] {
        &self.neutral
    }

    pub fn fit(&self) -> &[FitRecord] {
        &self.fit
    }

    pub fn grid(&self) -> Option<&Grid> {
        self.grid.as_ref()
    }

    /// SHA-256 of every byte after the section table.
    pub fn content_id(&self) -> [u8; 32] {
        self.content_id
    }

    pub fn sections(&self) -> &[SectionInfo] {
        &self.sections
    }

    pub fn cid_prefix(&self) -> [u8; 8] {
        let mut prefix = [0_u8; 8];
        prefix.copy_from_slice(&self.content_id[..8]);
        prefix
    }

    pub fn content_id_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.content_id {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    /// The grid output for a linear Rec.2020 input, or `None` without a GRID
    /// section or for a non-finite input.
    pub fn eval_grid(&self, rgb2020_linear: [f64; 3]) -> Option<[f64; 3]> {
        if rgb2020_linear.iter().any(|value| !value.is_finite()) {
            return None;
        }
        let shaped = [
            shape(rgb2020_linear[0]),
            shape(rgb2020_linear[1]),
            shape(rgb2020_linear[2]),
        ];
        self.eval_grid_shaped(shaped)
    }

    /// The normative interpolation over shaped coordinates (docs/04 §8.3).
    pub fn eval_grid_shaped(&self, shaped: [f64; 3]) -> Option<[f64; 3]> {
        let grid = self.grid.as_ref()?;
        if shaped.iter().any(|value| !value.is_finite()) {
            return None;
        }
        let side = grid.side.side() as usize;
        let scale = (side - 1) as f64;
        let mut base = [0_usize; 3];
        let mut fraction = [0.0_f64; 3];
        for channel in 0..3 {
            let position = shaped[channel].clamp(0.0, 1.0) * scale;
            let mut node = position.floor() as usize;
            let mut frac = position - node as f64;
            if node >= side - 1 {
                node = side - 2;
                frac = 1.0;
            }
            base[channel] = node;
            fraction[channel] = frac;
        }

        // Stable sort keeps red before green before blue on equal fractions.
        let mut order = [(0_usize, fraction[0]), (1, fraction[1]), (2, fraction[2])];
        order.sort_by(|left, right| right.1.total_cmp(&left.1));
        let first = order[0].1;
        let second = order[1].1;
        let third = order[2].1;

        let zero = grid.node(base[0], base[1], base[2]);
        let mut step = base;
        step[order[0].0] += 1;
        let one = grid.node(step[0], step[1], step[2]);
        step[order[1].0] += 1;
        let two = grid.node(step[0], step[1], step[2]);
        step[order[2].0] += 1;
        let three = grid.node(step[0], step[1], step[2]);

        let w0 = 1.0 - first;
        let w1 = first - second;
        let w2 = second - third;
        Some([
            w0 * zero[0] + w1 * one[0] + w2 * two[0] + third * three[0],
            w0 * zero[1] + w1 * one[1] + w2 * two[1] + third * three[1],
            w0 * zero[2] + w1 * one[2] + w2 * two[2] + third * three[2],
        ])
    }
}

/// # Errors
/// Rejects out-of-range metadata, a non-adaptive or non-matchable look, or non-finite values.
pub fn write_card(
    document: &LookDocument,
    meta: &CardMeta,
    options: &CardOptions,
) -> Result<Vec<u8>, CardError> {
    if !document.is_matchable() {
        return Err(CardError::NotMatchable);
    }
    if document.file.look.wb_policy != LookWbPolicy::Adaptive {
        return Err(CardError::Meta("wb_policy is not adaptive"));
    }
    let identity = document.file.look.clone();
    let neutral = neutral_axis(document);
    let fit = fit_records(document);
    let grid = match options.grid {
        Some(side) => Some(build_grid(document, side)?),
        None => None,
    };
    write_card_sections(&CardSections {
        identity,
        meta: meta.clone(),
        neutral,
        fit,
        grid,
    })
}

/// Writes sections without a Look, for tools that build cards from fitted data.
///
/// # Errors
/// Rejects wrong section lengths, out-of-range metadata and non-finite values.
pub fn write_card_sections(sections: &CardSections) -> Result<Vec<u8>, CardError> {
    if sections.neutral.len() != NEUTRAL_COUNT {
        return Err(CardError::NeutralCount(sections.neutral.len() as u32));
    }
    if sections.fit.is_empty() {
        return Err(CardError::Section("FIT", "record count is zero"));
    }
    if sections
        .neutral
        .iter()
        .flatten()
        .any(|value| !value.is_finite())
    {
        return Err(CardError::NonFinite);
    }
    for record in &sections.fit {
        if record
            .input
            .iter()
            .chain(record.output.iter())
            .any(|v| !v.is_finite())
            || !record.weight.is_finite()
            || !valid_sigma(record.sigma)
            || record.weight < 0.0
        {
            return Err(CardError::NonFinite);
        }
    }

    let meta = encode_meta(&sections.identity, &sections.meta)?;
    let semantics = encode_semantics();
    let neutral = encode_neutral(&sections.neutral);
    let fit = encode_fit(&sections.fit);
    let mut entries = vec![
        (KIND_META, meta),
        (KIND_SEMANTICS, semantics),
        (KIND_NEUTRAL, neutral),
        (KIND_FIT, fit),
    ];
    if let Some(grid) = &sections.grid {
        entries.push((KIND_GRID, encode_grid(grid)));
    }
    frame(&entries)
}

fn frame(entries: &[(u32, Vec<u8>)]) -> Result<Vec<u8>, CardError> {
    if entries.len() > u16::MAX as usize {
        return Err(CardError::Section("table", "too many sections"));
    }
    for pair in entries.windows(2) {
        if pair[0].0 >= pair[1].0 {
            return Err(CardError::Section("table", "sections are not sorted"));
        }
    }
    let table_end = HEADER_LENGTH + entries.len() * SECTION_ENTRY_LENGTH;
    let mut body = Vec::new();
    let mut table = Vec::with_capacity(entries.len() * SECTION_ENTRY_LENGTH);
    for (kind, data) in entries {
        while !(table_end + body.len()).is_multiple_of(4) {
            body.push(0);
        }
        let offset = table_end + body.len();
        let offset = u32::try_from(offset).map_err(|_| CardError::Section("table", "too large"))?;
        let length =
            u32::try_from(data.len()).map_err(|_| CardError::Section("table", "too large"))?;
        table.extend_from_slice(&kind.to_le_bytes());
        table.extend_from_slice(&offset.to_le_bytes());
        table.extend_from_slice(&length.to_le_bytes());
        table.extend_from_slice(&crc32(data.iter().copied()).to_le_bytes());
        body.extend_from_slice(data);
    }
    let file_length = HEADER_LENGTH + table.len() + body.len();
    let file_length =
        u32::try_from(file_length).map_err(|_| CardError::Section("header", "too large"))?;

    let mut header = vec![0_u8; HEADER_LENGTH];
    header[0..4].copy_from_slice(&MAGIC);
    header[4..6].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    header[6..8].copy_from_slice(&(HEADER_LENGTH as u16).to_le_bytes());
    header[8..12].copy_from_slice(&file_length.to_le_bytes());
    header[12..14].copy_from_slice(&(entries.len() as u16).to_le_bytes());
    header[20..52].copy_from_slice(&Sha256::digest(&body));
    header[16..20].fill(0);

    let mut crc = Crc32::new();
    crc.update(&header);
    crc.update(&table);
    header[16..20].copy_from_slice(&crc.finish().to_le_bytes());

    let mut out = Vec::with_capacity(HEADER_LENGTH + table.len() + body.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&table);
    out.extend_from_slice(&body);
    Ok(out)
}

fn encode_meta(identity: &LookIdentity, meta: &CardMeta) -> Result<Vec<u8>, CardError> {
    let slug = identity.slug.as_str();
    let version = identity.version.as_str();
    if !valid_slug(slug) {
        return Err(CardError::Meta("slug"));
    }
    if !valid_version(version) {
        return Err(CardError::Meta("version"));
    }
    if meta.name.is_empty() || meta.name.len() > 63 {
        return Err(CardError::Meta("name"));
    }
    if !valid_short_name(&meta.short_name) {
        return Err(CardError::Meta("short name"));
    }
    if let Some(description) = &meta.description
        && (description.len() > 511 || !description.is_ascii())
    {
        return Err(CardError::Meta("description"));
    }
    if let Some(license) = &meta.license
        && license.len() > 255
    {
        return Err(CardError::Meta("license"));
    }

    let mut out = Vec::new();
    push_tlv(&mut out, 1, slug.as_bytes())?;
    push_tlv(&mut out, 2, version.as_bytes())?;
    push_tlv(&mut out, 3, meta.name.as_bytes())?;
    push_tlv(&mut out, 4, meta.short_name.as_bytes())?;
    if let Some(description) = &meta.description {
        push_tlv(&mut out, 5, description.as_bytes())?;
    }
    if let Some(license) = &meta.license {
        push_tlv(&mut out, 6, license.as_bytes())?;
    }
    Ok(out)
}

fn push_tlv(out: &mut Vec<u8>, key: u8, data: &[u8]) -> Result<(), CardError> {
    let length =
        u16::try_from(data.len()).map_err(|_| CardError::Meta("value is longer than 65535"))?;
    out.push(key);
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(data);
    Ok(())
}

fn encode_semantics() -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    for _ in 0..5 {
        out.extend_from_slice(&1_u16.to_le_bytes());
    }
    out.extend_from_slice(&0_u16.to_le_bytes());
    out
}

fn encode_neutral(values: &[[f32; 3]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + values.len() * 12);
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    for value in values {
        for channel in value {
            out.extend_from_slice(&channel.to_le_bytes());
        }
    }
    out
}

fn encode_fit(records: &[FitRecord]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + records.len() * FIT_RECORD_LENGTH);
    out.extend_from_slice(&(records.len() as u32).to_le_bytes());
    for record in records {
        for value in record.input.iter().chain(record.output.iter()) {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&record.sigma.to_le_bytes());
        out.extend_from_slice(&record.weight.to_le_bytes());
        out.extend_from_slice(&u32::from(record.supported).to_le_bytes());
        out.extend_from_slice(&record.probe.to_le_bytes());
    }
    out
}

fn encode_grid(grid: &Grid) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + grid.values.len() * 6);
    out.extend_from_slice(&grid.side.side().to_le_bytes());
    for value in &grid.values {
        for channel in value {
            out.extend_from_slice(&f16_bits(sanitize_grid_value(*channel)).to_le_bytes());
        }
    }
    out
}

/// # Errors
/// [`CardError`] for the first inconsistency found.
pub fn read_card(bytes: &[u8]) -> Result<Card, CardError> {
    if bytes.len() < HEADER_LENGTH {
        return Err(CardError::TruncatedHeader);
    }
    if bytes[0..4] != MAGIC {
        return Err(CardError::Magic);
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != FORMAT_VERSION {
        return Err(CardError::FormatVersion(version));
    }
    let header_length = u16::from_le_bytes([bytes[6], bytes[7]]);
    if header_length as usize != HEADER_LENGTH {
        return Err(CardError::HeaderLength(header_length));
    }
    let file_length = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    if file_length as usize != bytes.len() {
        return Err(CardError::FileLength {
            expected: file_length,
            found: bytes.len(),
        });
    }
    if bytes[14] != 0 || bytes[15] != 0 || bytes[52..64].iter().any(|byte| *byte != 0) {
        return Err(CardError::Section("header", "reserved bytes are not zero"));
    }
    let count = usize::from(u16::from_le_bytes([bytes[12], bytes[13]]));
    let table_end = HEADER_LENGTH + count * SECTION_ENTRY_LENGTH;
    if table_end > bytes.len() {
        return Err(CardError::Table(0));
    }

    let expected_crc = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let mut crc = Crc32::new();
    crc.update(&bytes[0..16]);
    crc.update(&[0_u8; 4]);
    crc.update(&bytes[20..table_end]);
    if crc.finish() != expected_crc {
        return Err(CardError::HeaderCrc);
    }

    let digest = Sha256::digest(&bytes[table_end..]);
    if bytes[20..52] != digest[..] {
        return Err(CardError::ContentId);
    }

    let mut entries = Vec::with_capacity(count);
    let mut sections = Vec::with_capacity(count);
    let mut previous_kind = None;
    for index in 0..count {
        let start = HEADER_LENGTH + index * SECTION_ENTRY_LENGTH;
        let entry = &bytes[start..start + SECTION_ENTRY_LENGTH];
        let kind = u32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]);
        let offset = u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]);
        let length = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]);
        let crc = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]);
        if previous_kind.is_some_and(|previous| previous >= kind) {
            return Err(CardError::DuplicateSection(kind));
        }
        previous_kind = Some(kind);
        let offset = offset as usize;
        let length = length as usize;
        if !offset.is_multiple_of(4)
            || offset < table_end
            || offset
                .checked_add(length)
                .is_none_or(|end| end > bytes.len())
        {
            return Err(CardError::Table(index));
        }
        if crc32(bytes[offset..offset + length].iter().copied()) != crc {
            return Err(CardError::SectionCrc(kind));
        }
        entries.push((kind, offset, length));
        sections.push(SectionInfo {
            kind,
            offset: offset as u32,
            length: length as u32,
        });
    }

    let mut ranges: Vec<(usize, usize)> = entries
        .iter()
        .map(|(_, offset, length)| (*offset, offset + length))
        .filter(|(start, end)| start != end)
        .collect();
    ranges.sort_unstable();
    for pair in ranges.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(CardError::Section("table", "sections overlap"));
        }
    }

    let section = |kind: u32, name: &'static str| -> Result<&[u8], CardError> {
        entries
            .iter()
            .find(|(entry_kind, _, _)| *entry_kind == kind)
            .map(|(_, offset, length)| &bytes[*offset..offset + length])
            .ok_or(CardError::MissingSection(name))
    };

    let meta = parse_meta(section(KIND_META, "META")?)?;
    let semantics = parse_semantics(section(KIND_SEMANTICS, "SEMANTICS")?)?;
    let neutral = parse_neutral(section(KIND_NEUTRAL, "NEUTRAL")?)?;
    let fit = parse_fit(section(KIND_FIT, "FIT")?)?;
    let grid = match entries.iter().find(|(kind, _, _)| *kind == KIND_GRID) {
        Some((_, offset, length)) => Some(parse_grid(&bytes[*offset..offset + length])?),
        None => None,
    };

    let mut content_id = [0_u8; 32];
    content_id.copy_from_slice(&bytes[20..52]);
    Ok(Card {
        content_id,
        sections,
        meta,
        semantics,
        neutral,
        fit,
        grid,
    })
}

fn parse_meta(bytes: &[u8]) -> Result<Meta, CardError> {
    let mut cursor = 0;
    let mut slug = None;
    let mut version = None;
    let mut name = None;
    let mut short_name = None;
    let mut description = None;
    let mut license = None;
    while cursor < bytes.len() {
        if bytes.len() - cursor < 3 {
            return Err(CardError::Section("META", "truncated TLV record"));
        }
        let key = bytes[cursor];
        let length = u16::from_le_bytes([bytes[cursor + 1], bytes[cursor + 2]]) as usize;
        cursor += 3;
        let end = cursor
            .checked_add(length)
            .ok_or(CardError::Section("META", "TLV length overflows"))?;
        if end > bytes.len() {
            return Err(CardError::Section(
                "META",
                "TLV record runs past the section",
            ));
        }
        let data = &bytes[cursor..end];
        cursor = end;
        match key {
            1 => {
                if slug.is_some() {
                    return Err(CardError::Section("META", "duplicate slug"));
                }
                let text = core::str::from_utf8(data)
                    .map_err(|_| CardError::Section("META", "slug is not ASCII"))?;
                if !valid_slug(text) {
                    return Err(CardError::Meta("slug"));
                }
                slug = Some(Slug::new(text).map_err(|_| CardError::Meta("slug"))?);
            }
            2 => {
                if version.is_some() {
                    return Err(CardError::Section("META", "duplicate version"));
                }
                let text = core::str::from_utf8(data)
                    .map_err(|_| CardError::Section("META", "version is not ASCII"))?;
                if !valid_version(text) {
                    return Err(CardError::Meta("version"));
                }
                version = Some(LookVersion::new(text).map_err(|_| CardError::Meta("version"))?);
            }
            3 => {
                if name.is_some() {
                    return Err(CardError::Section("META", "duplicate name"));
                }
                let text = core::str::from_utf8(data)
                    .map_err(|_| CardError::Section("META", "name is not UTF-8"))?;
                if text.is_empty() || data.len() > 63 {
                    return Err(CardError::Meta("name"));
                }
                name = Some(text.to_owned());
            }
            4 => {
                if short_name.is_some() {
                    return Err(CardError::Section("META", "duplicate short name"));
                }
                let text = core::str::from_utf8(data)
                    .map_err(|_| CardError::Section("META", "short name is not ASCII"))?;
                if !valid_short_name(text) {
                    return Err(CardError::Meta("short name"));
                }
                short_name = Some(text.to_owned());
            }
            5 => {
                if description.is_some() {
                    return Err(CardError::Section("META", "duplicate description"));
                }
                let text = core::str::from_utf8(data)
                    .map_err(|_| CardError::Section("META", "description is not UTF-8"))?;
                if data.len() > 511 || !data.is_ascii() {
                    return Err(CardError::Meta("description"));
                }
                description = Some(text.to_owned());
            }
            6 => {
                if license.is_some() {
                    return Err(CardError::Section("META", "duplicate license"));
                }
                let text = core::str::from_utf8(data)
                    .map_err(|_| CardError::Section("META", "license is not UTF-8"))?;
                if data.len() > 255 {
                    return Err(CardError::Meta("license"));
                }
                license = Some(text.to_owned());
            }
            _ => {}
        }
    }
    Ok(Meta {
        slug: slug.ok_or(CardError::Section("META", "missing slug"))?,
        version: version.ok_or(CardError::Section("META", "missing version"))?,
        name: name.ok_or(CardError::Section("META", "missing name"))?,
        short_name: short_name.ok_or(CardError::Section("META", "missing short name"))?,
        description,
        license,
    })
}

fn parse_semantics(bytes: &[u8]) -> Result<Semantics, CardError> {
    if bytes.len() != 12 {
        return Err(CardError::Section("SEMANTICS", "length is not 12 bytes"));
    }
    let field = |index: usize| u16::from_le_bytes([bytes[index * 2], bytes[index * 2 + 1]]);
    let names = ["input", "output", "shaper", "viewing", "wb_policy"];
    for (index, name) in names.iter().enumerate() {
        let value = field(index);
        if value != 1 {
            return Err(CardError::Semantics { field: name, value });
        }
    }
    if field(5) != 0 {
        return Err(CardError::Section("SEMANTICS", "padding is not zero"));
    }
    Ok(Semantics::V1)
}

fn parse_neutral(bytes: &[u8]) -> Result<Vec<[f32; 3]>, CardError> {
    if bytes.len() < 4 {
        return Err(CardError::Section("NEUTRAL", "truncated count"));
    }
    let count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if count as usize != NEUTRAL_COUNT {
        return Err(CardError::NeutralCount(count));
    }
    let expected = 4 + NEUTRAL_COUNT * 12;
    if bytes.len() != expected {
        return Err(CardError::Section(
            "NEUTRAL",
            "length does not match the count",
        ));
    }
    let mut values = Vec::with_capacity(NEUTRAL_COUNT);
    for chunk in bytes[4..].as_chunks::<12>().0 {
        let mut value = [0.0_f32; 3];
        for (channel, slot) in value.iter_mut().enumerate() {
            let start = channel * 4;
            *slot = f32::from_le_bytes([
                chunk[start],
                chunk[start + 1],
                chunk[start + 2],
                chunk[start + 3],
            ]);
            if !slot.is_finite() {
                return Err(CardError::NonFinite);
            }
        }
        values.push(value);
    }
    Ok(values)
}

fn parse_fit(bytes: &[u8]) -> Result<Vec<FitRecord>, CardError> {
    if bytes.len() < 4 {
        return Err(CardError::Section("FIT", "truncated count"));
    }
    let count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if count == 0 {
        return Err(CardError::Section("FIT", "record count is zero"));
    }
    let expected = count
        .checked_mul(FIT_RECORD_LENGTH)
        .and_then(|length| length.checked_add(4))
        .ok_or(CardError::Section("FIT", "length overflows"))?;
    if bytes.len() != expected {
        return Err(CardError::Section("FIT", "length does not match the count"));
    }
    let mut records = Vec::with_capacity(count);
    for record in bytes[4..].as_chunks::<FIT_RECORD_LENGTH>().0 {
        let read = |index: usize| {
            let start = index * 4;
            f32::from_le_bytes([
                record[start],
                record[start + 1],
                record[start + 2],
                record[start + 3],
            ])
        };
        let input = [read(0), read(1), read(2)];
        let output = [read(3), read(4), read(5)];
        let sigma = read(6);
        let weight = read(7);
        let flags = u32::from_le_bytes([record[32], record[33], record[34], record[35]]);
        let probe = u32::from_le_bytes([record[36], record[37], record[38], record[39]]);
        if input.iter().chain(output.iter()).any(|v| !v.is_finite())
            || !weight.is_finite()
            || !valid_sigma(sigma)
            || weight < 0.0
        {
            return Err(CardError::NonFinite);
        }
        records.push(FitRecord {
            input,
            output,
            sigma,
            weight,
            supported: flags & 1 != 0,
            probe,
        });
    }
    Ok(records)
}

fn parse_grid(bytes: &[u8]) -> Result<Grid, CardError> {
    if bytes.len() < 4 {
        return Err(CardError::Section("GRID", "truncated side"));
    }
    let side = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let side = GridSide::from_side(side).ok_or(CardError::GridSide(side))?;
    let expected = 4 + side.nodes() * 6;
    if bytes.len() != expected {
        return Err(CardError::Section("GRID", "length does not match the side"));
    }
    let mut values = Vec::with_capacity(side.nodes());
    for chunk in bytes[4..].as_chunks::<6>().0 {
        let mut value = [0.0_f32; 3];
        for (channel, slot) in value.iter_mut().enumerate() {
            let start = channel * 2;
            *slot = f16_value(u16::from_le_bytes([chunk[start], chunk[start + 1]]));
            if !slot.is_finite() {
                return Err(CardError::NonFinite);
            }
        }
        values.push(value);
    }
    Ok(Grid { side, values })
}

fn neutral_axis(document: &LookDocument) -> Vec<[f32; 3]> {
    let white = Illuminant::D65.xyz();
    let mut values = Vec::with_capacity(NEUTRAL_COUNT);
    for index in 0..NEUTRAL_COUNT {
        let s = -LOW_EV + (HIGH_EV + LOW_EV) * index as f64 / (NEUTRAL_COUNT - 1) as f64;
        let xyz = white.scale(PIVOT * libm::pow(2.0, s));
        values.push(output_rgb(document, xyz));
    }
    values
}

fn fit_records(document: &LookDocument) -> Vec<FitRecord> {
    let probes = probes::v1();
    let support = document.uncertainty.support.as_ref();
    let mut records = Vec::with_capacity(probes.probes.len());
    for (index, probe) in probes.probes.iter().enumerate() {
        let input = RgbSpace::Rec2020.xyz_to_rgb_linear(probe.xyz);
        let input = [input.r as f32, input.g as f32, input.b as f32];
        let output = output_rgb(document, probe.xyz);
        let supported = support
            .and_then(|mask| mask.values.get(index))
            .is_some_and(|value| *value == 1.0);
        let sigma = if supported {
            post_sigma(document, index)
        } else {
            f32::INFINITY
        };
        let weight = if supported { probe.weight } else { 0.0 };
        records.push(FitRecord {
            input,
            output,
            sigma,
            weight: weight as f32,
            supported,
            probe: index as u32,
        });
    }
    let total: f64 = records
        .iter()
        .filter(|record| record.supported)
        .map(|record| f64::from(record.weight))
        .sum();
    if total > 0.0 {
        for record in &mut records {
            if record.supported {
                record.weight = (f64::from(record.weight) / total) as f32;
            }
        }
    }
    records
}

fn post_sigma(document: &LookDocument, index: usize) -> f32 {
    let uncertainty = &document.uncertainty;
    if let Some(scalars) = &uncertainty.probe_sigma_post
        && let Some(value) = scalars.values.get(index)
    {
        return sigma_value(*value);
    }
    let Some(covariance) = &uncertainty.probe_cov_post else {
        return f32::INFINITY;
    };
    let start = index * 9;
    let Some(values) = covariance.values.get(start..start + 9) else {
        return f32::INFINITY;
    };
    sigma_value((values[0] + values[4] + values[8]).max(0.0).sqrt())
}

fn sigma_value(value: f64) -> f32 {
    if value.is_finite() && value >= 0.0 {
        (value as f32).max(0.0)
    } else {
        f32::INFINITY
    }
}

fn valid_sigma(sigma: f32) -> bool {
    (sigma.is_finite() && sigma >= 0.0) || sigma == f32::INFINITY
}

/// A grid value must fit f16: a non-finite output becomes 0 and a finite one
/// clamps to ±65504 (docs/04 §8.3).
fn sanitize_grid_value(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(-65504.0, 65504.0)
    } else {
        0.0
    }
}

fn output_rgb(document: &LookDocument, xyz: Xyz) -> [f32; 3] {
    let ucs = document.look.eval(xyz);
    let model = Cam16Model::new(&ViewingConditions::reference());
    let output = model.jmh_to_xyz(ucs_to_jmh(ucs, UcsSpace::Ucs));
    let rgb = RgbSpace::Rec2020.xyz_to_rgb_linear(output);
    [rgb.r as f32, rgb.g as f32, rgb.b as f32]
}

fn build_grid(document: &LookDocument, side: GridSide) -> Result<Grid, CardError> {
    let count = side.side() as usize;
    let scale = 1.0 / (count - 1) as f64;
    let mut values = Vec::with_capacity(side.nodes());
    for b in 0..count {
        for g in 0..count {
            for r in 0..count {
                let linear = Rgb::new(
                    unshape(r as f64 * scale),
                    unshape(g as f64 * scale),
                    unshape(b as f64 * scale),
                );
                let output = output_rgb(document, RgbSpace::Rec2020.rgb_linear_to_xyz(linear));
                values.push(output.map(sanitize_grid_value));
            }
        }
    }
    Grid::new(side, values)
}

fn valid_slug(slug: &str) -> bool {
    (1..=63).contains(&slug.len())
        && slug.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn valid_version(version: &str) -> bool {
    (1..=31).contains(&version.len())
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-'))
}

fn valid_short_name(short_name: &str) -> bool {
    (1..=27).contains(&short_name.len()) && short_name.is_ascii()
}

/// `0..1` shaped coordinate of a scene-linear value (docs/04 §8.1).
fn shape(value: f64) -> f64 {
    if value >= 0.0 {
        0.5 + 0.5 * libm::log2(1.0 + value / PIVOT) / libm::log2(1.0 + libm::pow(2.0, HIGH_EV))
    } else {
        0.5 - 0.5 * libm::log2(1.0 - value / PIVOT) / libm::log2(1.0 + libm::pow(2.0, LOW_EV))
    }
}

fn unshape(coordinate: f64) -> f64 {
    let coordinate = coordinate.clamp(0.0, 1.0);
    if coordinate >= 0.5 {
        PIVOT
            * (libm::pow(
                2.0,
                (coordinate - 0.5) * 2.0 * libm::log2(1.0 + libm::pow(2.0, HIGH_EV)),
            ) - 1.0)
    } else {
        -PIVOT
            * (libm::pow(
                2.0,
                (0.5 - coordinate) * 2.0 * libm::log2(1.0 + libm::pow(2.0, LOW_EV)),
            ) - 1.0)
    }
}

fn crc32(bytes: impl Iterator<Item = u8>) -> u32 {
    let mut crc = Crc32::new();
    for byte in bytes {
        crc.update(&[byte]);
    }
    crc.finish()
}

struct Crc32(u32);

impl Crc32 {
    fn new() -> Self {
        Self(0xffff_ffff)
    }

    fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u32::from(*byte);
            for _ in 0..8 {
                self.0 = if self.0 & 1 != 0 {
                    (self.0 >> 1) ^ 0xedb8_8320
                } else {
                    self.0 >> 1
                };
            }
        }
    }

    fn finish(&self) -> u32 {
        !self.0
    }
}
