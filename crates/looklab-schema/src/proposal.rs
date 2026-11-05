// SPDX-License-Identifier: AGPL-3.0-or-later
//! Proposal payloads, evidence and the validator the browser form and CI share.

use core::fmt;
use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;

use crate::body::{Body, MappingStatus, validate_body};
use crate::control::{Control, ControlType};
use crate::error::{ErrorKind, ValidationError};
use crate::ids::{BodyId, LookId, MakernoteTag, ProposalRef, SimKey};
use crate::interface::{InterfaceProfile, validate_interface_profile};
use crate::rational::Rational;
use crate::schema::{
    ControlSchema, FirmwareVersion, Makernote, MakernoteValue, NativeSim, validate_control_schema,
};
use crate::summary::{ProposalSummary, summarize};

const MAX_NAME_CHARS: usize = 200;
const MAX_ATTRIBUTION_CHARS: usize = 500;
const MAX_JUSTIFICATION_CHARS: usize = 2000;
const MAX_TAGS_PER_CONFIRMATION: usize = 32;

pub const MAX_FIXTURE_BYTES: u64 = 64 * 1024 * 1024;
pub const FIXTURE_UPLOAD_MAGIC: &[u8] = b"looklab-fixture-1";
pub const FIXTURE_UPLOAD_HEADER_MAX: usize = 16 * 1024;

/// Makernote tags a confirmation may carry.
pub const CONFIRMATION_TAGS: [&str; 13] = [
    "FilmMode",
    "DynamicRange",
    "DynamicRangeSetting",
    "DevelopmentDynamicRange",
    "GrainSize",
    "GrainRoughness",
    "ColorChromeEffect",
    "ColorChromeFxBlue",
    "WhiteBalance",
    "WhiteBalanceFineTune",
    "HighlightTone",
    "ShadowTone",
    "NoiseReduction",
];

/// The kinds in docs/03 §1, in moderation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    Body,
    SchemaChange,
    Mapping,
    InterfaceProfile,
    LutLook,
    EpochSplit,
    LookSplit,
    GateOverride,
}

impl ProposalKind {
    pub const ALL: [Self; 8] = [
        Self::Body,
        Self::SchemaChange,
        Self::Mapping,
        Self::InterfaceProfile,
        Self::LutLook,
        Self::EpochSplit,
        Self::LookSplit,
        Self::GateOverride,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Body => "body",
            Self::SchemaChange => "schema_change",
            Self::Mapping => "mapping",
            Self::InterfaceProfile => "interface_profile",
            Self::LutLook => "lut_look",
            Self::EpochSplit => "epoch_split",
            Self::LookSplit => "look_split",
            Self::GateOverride => "gate_override",
        }
    }

    /// The menu wording the proposal form uses.
    pub const fn menu_label(self) -> &'static str {
        match self {
            Self::Body => "New camera body",
            Self::SchemaChange => "Control schema change",
            Self::Mapping => "Film simulation mapping",
            Self::InterfaceProfile => "Firmware interface profile",
            Self::LutLook => "LUT-defined look",
            Self::EpochSplit => "Firmware epoch split",
            Self::LookSplit => "Look split",
            Self::GateOverride => "Release gate override",
        }
    }

    /// Whether an approval adds a document to the community data snapshot.
    pub const fn contributes_document(self) -> bool {
        matches!(
            self,
            Self::Body | Self::SchemaChange | Self::Mapping | Self::InterfaceProfile
        )
    }
}

impl fmt::Display for ProposalKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProposalKind {
    type Err = ValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == value)
            .ok_or_else(|| {
                ValidationError::new(
                    "kind",
                    ErrorKind::InvalidValue {
                        expected: "a known proposal kind",
                    },
                )
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatedPayload {
    Body(Body),
    SchemaChange(ControlSchema),
    Mapping(MappingProposal),
    InterfaceProfile(InterfaceProfile),
    LutLook(LutLookProposal),
    EpochSplit(EpochSplitProposal),
    LookSplit(LookSplitProposal),
    GateOverride(GateOverrideProposal),
}

impl ValidatedPayload {
    pub const fn kind(&self) -> ProposalKind {
        match self {
            Self::Body(_) => ProposalKind::Body,
            Self::SchemaChange(_) => ProposalKind::SchemaChange,
            Self::Mapping(_) => ProposalKind::Mapping,
            Self::InterfaceProfile(_) => ProposalKind::InterfaceProfile,
            Self::LutLook(_) => ProposalKind::LutLook,
            Self::EpochSplit(_) => ProposalKind::EpochSplit,
            Self::LookSplit(_) => ProposalKind::LookSplit,
            Self::GateOverride(_) => ProposalKind::GateOverride,
        }
    }

    pub fn summary(&self) -> ProposalSummary {
        summarize(self)
    }
}

/// Validates a payload; the browser form and CI both call this.
///
/// # Errors
/// Every rule violation in the payload, as paths into the payload YAML.
pub fn validate_proposal(
    kind: ProposalKind,
    payload: &str,
) -> Result<ValidatedPayload, Vec<ValidationError>> {
    crate::yaml::check(payload)?;
    match kind {
        ProposalKind::Body => validate_body(payload).map(ValidatedPayload::Body),
        ProposalKind::SchemaChange => {
            validate_control_schema(payload).map(ValidatedPayload::SchemaChange)
        }
        ProposalKind::InterfaceProfile => {
            validate_interface_profile(payload).map(ValidatedPayload::InterfaceProfile)
        }
        ProposalKind::Mapping => structured(payload).and_then(|proposal: MappingProposal| {
            proposal.validate()?;
            Ok(ValidatedPayload::Mapping(proposal))
        }),
        ProposalKind::LutLook => structured(payload).and_then(|proposal: LutLookProposal| {
            proposal.validate()?;
            Ok(ValidatedPayload::LutLook(proposal))
        }),
        ProposalKind::EpochSplit => structured(payload).and_then(|proposal: EpochSplitProposal| {
            proposal.validate()?;
            Ok(ValidatedPayload::EpochSplit(proposal))
        }),
        ProposalKind::LookSplit => structured(payload).and_then(|proposal: LookSplitProposal| {
            proposal.validate()?;
            Ok(ValidatedPayload::LookSplit(proposal))
        }),
        ProposalKind::GateOverride => {
            structured(payload).and_then(|proposal: GateOverrideProposal| {
                proposal.validate()?;
                Ok(ValidatedPayload::GateOverride(proposal))
            })
        }
    }
}

fn structured<T: serde::de::DeserializeOwned>(payload: &str) -> Result<T, Vec<ValidationError>> {
    serde_norway::from_str(payload).map_err(ValidationError::syntax)
}

fn invalid(path: &str, expected: &'static str) -> Vec<ValidationError> {
    vec![ValidationError::new(
        path,
        ErrorKind::InvalidValue { expected },
    )]
}

/// A proposed native-sim makernote mapping for one epoch.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingProposal {
    pub body: BodyId,
    pub epoch: u32,
    pub sim_key: SimKey,
    pub look: LookId,
    pub firmware_from: FirmwareVersion,
    #[serde(default)]
    pub firmware_to: Option<FirmwareVersion>,
    pub makernote: Makernote,
}

impl MappingProposal {
    /// # Errors
    /// An epoch below 1, or a firmware range whose end precedes its start.
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        let mut errors = Vec::new();
        if self.epoch == 0 {
            errors.extend(invalid("epoch", "an epoch ordinal of at least 1"));
        }
        if let Some(to) = &self.firmware_to
            && *to < self.firmware_from
        {
            errors.extend(invalid(
                "firmware_to",
                "a version at or above firmware_from",
            ));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    pub fn native_sim(&self) -> NativeSim {
        NativeSim {
            key: self.sim_key.clone(),
            look: self.look.clone(),
            makernote: self.makernote.clone(),
        }
    }
}

pub fn mapping_status_supported(body: &Body, fixtures: &[FixtureDonation]) -> bool {
    body.mapping_status == MappingStatus::Provisional
        || fixtures
            .iter()
            .any(|fixture| body.matches_fixture(&fixture.make, &fixture.model))
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LutLookProposal {
    pub look: LookId,
    pub name: String,
    pub license: LicenseDeclaration,
    pub attribution: String,
}

impl LutLookProposal {
    /// # Errors
    /// Empty or over-long name and attribution, or a license declaration that
    /// neither claims the work nor names a source license.
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        let mut errors = Vec::new();
        check_text(&self.name, "name", MAX_NAME_CHARS, &mut errors);
        check_text(
            &self.attribution,
            "attribution",
            MAX_ATTRIBUTION_CHARS,
            &mut errors,
        );
        errors.extend(self.license.validate());
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Exactly one of "this is my work" and a named source license.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LicenseDeclaration {
    #[serde(default)]
    pub own_work: bool,
    #[serde(default)]
    pub source_license: Option<String>,
    #[serde(default)]
    pub source_url: Option<String>,
}

pub const DATA_LICENSE: &str = "CC0-1.0";

impl LicenseDeclaration {
    /// # Errors
    /// No declaration, both declarations, an empty source license, or a source
    /// URL without a source license.
    pub fn validate(&self) -> Vec<ValidationError> {
        let mut errors = Vec::new();
        match (&self.source_license, self.own_work) {
            (Some(_), true) => errors.extend(invalid(
                "license",
                "either own_work or source_license, not both",
            )),
            (Some(source), false) => check_text(source, "license.source_license", 200, &mut errors),
            (None, true) => {}
            (None, false) => errors.extend(invalid(
                "license",
                "own_work: true or a named source_license",
            )),
        }
        if self.source_url.is_some() && self.source_license.is_none() {
            errors.extend(invalid(
                "license.source_url",
                "a source_license alongside the source URL",
            ));
        }
        errors
    }

    pub fn describe(&self) -> String {
        match (&self.source_license, self.own_work) {
            (Some(source), _) => format!("licensed for redistribution under {source}"),
            (None, _) => "the submitter's own work".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpochSplitProposal {
    pub body: BodyId,
    pub from_epoch: u32,
    pub new_epoch: u32,
    pub split_at: FirmwareVersion,
}

impl EpochSplitProposal {
    /// # Errors
    /// An epoch below 1, or a new epoch equal to the one it splits.
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        let mut errors = Vec::new();
        if self.from_epoch == 0 {
            errors.extend(invalid("from_epoch", "an epoch ordinal of at least 1"));
        }
        if self.new_epoch == 0 {
            errors.extend(invalid("new_epoch", "an epoch ordinal of at least 1"));
        }
        if self.new_epoch == self.from_epoch {
            errors.extend(invalid("new_epoch", "an epoch other than from_epoch"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LookSplitProposal {
    pub look: LookId,
    pub new_look: LookId,
    pub name: String,
}

impl LookSplitProposal {
    /// # Errors
    /// An empty name, or a new look id equal to the source.
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        let mut errors = Vec::new();
        check_text(&self.name, "name", MAX_NAME_CHARS, &mut errors);
        if self.new_look == self.look {
            errors.extend(invalid("new_look", "a look other than look"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateOverrideProposal {
    pub look: LookId,
    pub version: u32,
    pub justification: String,
}

impl GateOverrideProposal {
    /// # Errors
    /// Version 0, or an empty justification.
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        let mut errors = Vec::new();
        if self.version == 0 {
            errors.extend(invalid("version", "a version of at least 1"));
        }
        check_text(
            &self.justification,
            "justification",
            MAX_JUSTIFICATION_CHARS,
            &mut errors,
        );
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

fn check_text(value: &str, path: &str, max: usize, errors: &mut Vec<ValidationError>) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        errors.extend(invalid(path, "non-empty text"));
    } else if trimmed.chars().count() > max {
        errors.extend(invalid(path, "shorter text"));
    }
}

/// Parsed, whitelisted makernote values from one confirmer's file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabeledTag {
    pub tag: MakernoteTag,
    pub value: MakernoteValue,
    /// The menu answer the confirmer gave for this file.
    pub label: SimKey,
}

/// # Errors
/// No tags, more than 32 tags, a repeated tag, or a tag outside
/// [`CONFIRMATION_TAGS`].
pub fn validate_confirmation(tags: &[LabeledTag]) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    if tags.is_empty() {
        errors.extend(invalid("tags", "at least one parsed tag value"));
    }
    if tags.len() > MAX_TAGS_PER_CONFIRMATION {
        errors.extend(invalid("tags", "at most 32 tag values"));
    }
    let mut seen = std::collections::HashSet::new();
    for (index, labeled) in tags.iter().enumerate() {
        if !CONFIRMATION_TAGS.contains(&labeled.tag.as_str()) {
            errors.extend(invalid(
                &format!("tags[{index}].tag"),
                "a whitelisted makernote tag",
            ));
        }
        if !seen.insert(labeled.tag.as_str()) {
            errors.extend(vec![ValidationError::new(
                format!("tags[{index}].tag"),
                ErrorKind::DuplicateKey {
                    key: labeled.tag.to_string(),
                },
            )]);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Evidence {
    Confirmation {
        #[serde(default)]
        author: Option<String>,
        tags: Vec<LabeledTag>,
        recorded_at: String,
    },
    Fixture {
        donation: FixtureDonation,
    },
    /// A worker's split or gate report.
    Worker {
        summary: String,
        #[serde(default)]
        reference: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureDonation {
    pub file: String,
    pub description: String,
    pub make: String,
    pub model: String,
    pub sha256: Sha256Hex,
    pub size: u64,
    #[serde(default)]
    pub source_url: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub donator: Option<String>,
    pub donated_on: Date,
    pub license: FixtureLicense,
    #[serde(default)]
    pub committed: bool,
}

impl FixtureDonation {
    /// # Errors
    /// A file name that could escape the fixtures directory, empty or over-long
    /// descriptions, a size over the per-file upload cap, an unbound `source`,
    /// or a non-HTTPS source URL.
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        let mut errors = Vec::new();
        if !valid_file_name(&self.file) {
            errors.extend(invalid("file", "a plain file name"));
        }
        check_text(&self.description, "description", 200, &mut errors);
        check_text(&self.make, "make", MAX_NAME_CHARS, &mut errors);
        check_text(&self.model, "model", MAX_NAME_CHARS, &mut errors);
        if self.size == 0 || self.size > MAX_FIXTURE_BYTES {
            errors.extend(invalid("size", "a size between 1 byte and 64 MiB"));
        }
        match (&self.source_url, &self.source) {
            (Some(_), Some(_)) => {
                errors.extend(invalid("source", "either source_url or source, not both"))
            }
            (None, None) if !self.committed => errors.extend(invalid(
                "source",
                "an uploaded source or an https URL for a fixture that is not committed",
            )),
            _ => {}
        }
        if self.committed && self.source.is_some() {
            errors.extend(invalid(
                "source",
                "a blob source only before the donation is published",
            ));
        }
        if let Some(source) = &self.source
            && source != &fixture_source(self.sha256)
        {
            errors.extend(invalid(
                "source",
                "`sha256:` followed by this file's digest",
            ));
        }
        match &self.source_url {
            Some(url) if !url.starts_with("https://") => {
                errors.extend(invalid("source_url", "an https URL"));
            }
            _ => {}
        }
        if let Some(donator) = &self.donator {
            check_text(donator, "donator", MAX_NAME_CHARS, &mut errors);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// The consent sentence recorded in `fixtures/manifest.toml`.
    pub fn consent(&self) -> String {
        match &self.donator {
            Some(donator) => format!(
                "Donated by {donator} on {} under {}",
                self.donated_on.as_str(),
                self.license.as_str()
            ),
            None => format!(
                "Donated on {} under {}",
                self.donated_on.as_str(),
                self.license.as_str()
            ),
        }
    }
}

/// The `source` string a donation carries before its file is published.
pub fn fixture_source(sha256: Sha256Hex) -> String {
    format!("sha256:{}", sha256.to_hex())
}

fn valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && name.bytes().all(|byte| byte.is_ascii_graphic())
        && name.contains('.')
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureUpload {
    pub file: String,
    pub sha256: Sha256Hex,
    pub size: u64,
}

impl FixtureUpload {
    /// # Errors
    /// Rejects a plain file name, or a size outside the 64 MiB cap.
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        let mut errors = Vec::new();
        if !valid_file_name(&self.file) {
            errors.extend(invalid("file", "a plain file name"));
        }
        if self.size == 0 || self.size > MAX_FIXTURE_BYTES {
            errors.extend(invalid("size", "a size between 1 byte and 64 MiB"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FixtureFrameError {
    #[error("the fixture upload frame is malformed")]
    Malformed,
    #[error("the fixture upload header is {0} bytes, over the limit")]
    HeaderTooLarge(usize),
}

/// `magic ‖ u32 header length ‖ header json ‖ file bytes`.
///
/// # Errors
/// An empty or over-long header.
pub fn frame_fixture_upload(header: &[u8], bytes: &[u8]) -> Result<Vec<u8>, FixtureFrameError> {
    if header.is_empty() {
        return Err(FixtureFrameError::Malformed);
    }
    if header.len() > FIXTURE_UPLOAD_HEADER_MAX {
        return Err(FixtureFrameError::HeaderTooLarge(header.len()));
    }
    let length =
        u32::try_from(header.len()).map_err(|_| FixtureFrameError::HeaderTooLarge(header.len()))?;
    let mut out = Vec::with_capacity(FIXTURE_UPLOAD_MAGIC.len() + 4 + header.len() + bytes.len());
    out.extend_from_slice(FIXTURE_UPLOAD_MAGIC);
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(header);
    out.extend_from_slice(bytes);
    Ok(out)
}

/// # Errors
/// A missing magic, an empty or over-long header, or a truncated frame.
pub fn split_fixture_upload(body: &[u8]) -> Result<(&[u8], &[u8]), FixtureFrameError> {
    let magic = FIXTURE_UPLOAD_MAGIC.len();
    if body.len() < magic + 4 || &body[..magic] != FIXTURE_UPLOAD_MAGIC {
        return Err(FixtureFrameError::Malformed);
    }
    let length = u32::from_le_bytes(
        body[magic..magic + 4]
            .try_into()
            .map_err(|_| FixtureFrameError::Malformed)?,
    ) as usize;
    if length == 0 {
        return Err(FixtureFrameError::Malformed);
    }
    if length > FIXTURE_UPLOAD_HEADER_MAX {
        return Err(FixtureFrameError::HeaderTooLarge(length));
    }
    let start = magic + 4;
    let end = start
        .checked_add(length)
        .ok_or(FixtureFrameError::Malformed)?;
    if body.len() < end {
        return Err(FixtureFrameError::Malformed);
    }
    Ok((&body[start..end], &body[end..]))
}

/// A lowercase SHA-256 digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Sha256Hex([u8; 32]);

impl Sha256Hex {
    /// # Errors
    /// Anything but exactly 64 lowercase hex characters.
    pub fn parse(value: &str) -> Result<Self, Sha256Error> {
        if value.len() != 64 {
            return Err(Sha256Error);
        }
        let mut bytes = [0u8; 32];
        for (index, slot) in bytes.iter_mut().enumerate() {
            let high = hex_nibble(value.as_bytes()[index * 2])?;
            let low = hex_nibble(value.as_bytes()[index * 2 + 1])?;
            *slot = (high << 4) | low;
        }
        Ok(Self(bytes))
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("expected 64 lowercase hex characters")]
pub struct Sha256Error;

fn hex_nibble(byte: u8) -> Result<u8, Sha256Error> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(Sha256Error),
    }
}

impl fmt::Display for Sha256Hex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl TryFrom<String> for Sha256Hex {
    type Error = Sha256Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<Sha256Hex> for String {
    fn from(value: Sha256Hex) -> Self {
        value.to_hex()
    }
}

/// A calendar date, `YYYY-MM-DD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Date {
    year: u16,
    month: u8,
    day: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("expected a calendar date as YYYY-MM-DD")]
pub struct DateError;

impl Date {
    /// # Errors
    /// A malformed date or one that does not exist in the calendar.
    pub fn parse(value: &str) -> Result<Self, DateError> {
        let bytes = value.as_bytes();
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return Err(DateError);
        }
        let digits = |range: core::ops::Range<usize>| -> Result<u32, DateError> {
            let slice = value.get(range).ok_or(DateError)?;
            if !slice.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(DateError);
            }
            slice.parse().map_err(|_| DateError)
        };
        let year = u16::try_from(digits(0..4)?).map_err(|_| DateError)?;
        let month = u8::try_from(digits(5..7)?).map_err(|_| DateError)?;
        let day = u8::try_from(digits(8..10)?).map_err(|_| DateError)?;
        if !(1..=12).contains(&month) {
            return Err(DateError);
        }
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let days = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            2 => 28,
            _ => return Err(DateError),
        };
        if day == 0 || day > days {
            return Err(DateError);
        }
        Ok(Self { year, month, day })
    }

    pub fn as_str(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

impl TryFrom<String> for Date {
    type Error = DateError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<Date> for String {
    fn from(value: Date) -> Self {
        value.as_str()
    }
}

/// Fixture files are published under CC0-1.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FixtureLicense {
    #[serde(rename = "CC0-1.0")]
    Cc0_1_0,
}

impl FixtureLicense {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cc0_1_0 => "CC0-1.0",
        }
    }
}

/// Renders donations as `fixtures/manifest.toml` tables.
pub fn fixture_manifest_toml(donations: &[FixtureDonation]) -> String {
    let mut out = String::from(
        "# Donated by contributors; the consent record is the donation's opt-in.\n\
         # `committed = true` files live in this directory, the rest are download-only\n\
         # into fixtures/cache/ (gitignored).\n",
    );
    for donation in donations {
        out.push_str("\n[[fixture]]\n");
        out.push_str(&format!("file = {}\n", toml_string(&donation.file)));
        out.push_str(&format!(
            "description = {}\n",
            toml_string(&donation.description)
        ));
        out.push_str(&format!("make = {}\n", toml_string(&donation.make)));
        out.push_str(&format!("model = {}\n", toml_string(&donation.model)));
        out.push_str(&format!(
            "sha256 = {}\n",
            toml_string(&donation.sha256.to_hex())
        ));
        out.push_str(&format!("size = {}\n", donation.size));
        if let Some(url) = &donation.source_url {
            out.push_str(&format!("source_url = {}\n", toml_string(url)));
        }
        if let Some(source) = &donation.source {
            out.push_str(&format!("source = {}\n", toml_string(source)));
        }
        out.push_str(&format!(
            "license = {}\n",
            toml_string(donation.license.as_str())
        ));
        out.push_str(&format!("consent = {}\n", toml_string(&donation.consent())));
        out.push_str(&format!("committed = {}\n", donation.committed));
    }
    out
}

fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if (character as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04X}", character as u32));
            }
            character => out.push(character),
        }
    }
    out.push('"');
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Pending,
    NeedsInfo,
    Approved,
    Rejected,
}

impl ProposalStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::NeedsInfo => "needs_info",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "needs_info" => Some(Self::NeedsInfo),
            "approved" => Some(Self::Approved),
            "rejected" => Some(Self::Rejected),
            _ => None,
        }
    }

    pub const fn is_open(self) -> bool {
        matches!(self, Self::Pending | Self::NeedsInfo)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalView {
    pub id: ProposalRef,
    pub kind: ProposalKind,
    pub summary: ProposalSummary,
    pub status: ProposalStatus,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    pub created_at: i64,
}

/// Exact: `0.5` stays `0.5`, never `1/2`.
pub fn menu_value(value: Rational) -> String {
    if value.den == 1 {
        return value.num.to_string();
    }
    decimal_exact(value).unwrap_or_else(|| format!("{}/{}", value.num, value.den))
}

/// A range endpoint, with an explicit `+` on positive values.
pub fn menu_signed(value: Rational) -> String {
    let text = menu_value(value);
    if value.num > 0 {
        format!("+{text}")
    } else {
        text
    }
}

fn decimal_exact(value: Rational) -> Option<String> {
    let den = value.den;
    let places = (1..=9u32).find(|places| 10u64.pow(*places) % den == 0)?;
    let scale = 10i128.pow(places) / den as i128;
    let scaled = (value.num as i128) * scale;
    let sign = if scaled < 0 { "-" } else { "" };
    let magnitude = scaled.unsigned_abs();
    let divisor = 10u128.pow(places);
    let whole = magnitude / divisor;
    let fraction = magnitude % divisor;
    if fraction == 0 {
        return Some(format!("{sign}{whole}"));
    }
    let mut fraction_text = format!("{fraction:0width$}", width = places as usize);
    while fraction_text.ends_with('0') {
        fraction_text.pop();
    }
    Some(format!("{sign}{whole}.{fraction_text}"))
}

/// The step phrase a menu would use, e.g. "half steps".
pub fn step_phrase(step: Rational) -> String {
    let named = [
        (1, 1, "whole steps"),
        (1, 2, "half steps"),
        (1, 3, "third steps"),
        (1, 4, "quarter steps"),
        (1, 6, "sixth steps"),
    ];
    for (num, den, phrase) in named {
        if let Ok(named) = Rational::new(num, den)
            && step == named
        {
            return phrase.to_owned();
        }
    }
    format!("steps of {step}")
}

/// Renders a control's menu-level change, e.g. "Highlight tone goes -2 to +4 in half steps".
pub fn describe_control(control: &Control) -> String {
    let label = crate::summary::menu_label(&control.key);
    match control.kind {
        ControlType::Int | ControlType::Decimal => match (&control.range, &control.step) {
            (Some(range), Some(step)) => format!(
                "{label} goes {} to {} in {}",
                menu_signed(range.min),
                menu_signed(range.max),
                step_phrase(*step)
            ),
            _ => format!("{label} becomes a {}-valued control", control.kind),
        },
        ControlType::Enum => match &control.values {
            Some(values) if !values.is_empty() => {
                let words: Vec<String> = values
                    .iter()
                    .map(|value| value.as_str().replace('_', " "))
                    .collect();
                format!("{label} offers {}", words.join(", "))
            }
            _ => format!("{label} becomes an enum control"),
        },
        ControlType::Bool => format!("{label} toggles on or off"),
    }
}
