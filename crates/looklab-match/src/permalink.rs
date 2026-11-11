// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeMap;

use looklab_model::ProbeSetVersion;
use looklab_schema::{LookId, PublishedTargetId, RealizationRevisionId, Sha256Hex, TargetVersion};
use serde::{Deserialize, Serialize};

use crate::calibration::Calibration;
use crate::emphasis::Emphasis;
use crate::error::{MatchError, Result};
use crate::metric::{OutputContext, ScoringMetric};
use crate::optimizer::{OPTIMIZER_VERSION, PRE_CLIP_OPTIMIZER_VERSION};

pub const PERMALINK_FORMAT: &str = "looklab-match/1";

pub const CALIBRATED_PERMALINK_FORMAT: &str = "looklab-match/2";

/// A link that records the bundle revision and the assessment context it used.
pub const SELECTION_PERMALINK_FORMAT: &str = "looklab-match/3";

/// A link that records the post-clip scoring metric and its output context.
pub const RENDERED_PERMALINK_FORMAT: &str = "looklab-match/4";

const KNOWN_FIELDS: &[&str] = &[
    "look",
    "base",
    "crm",
    "input",
    "realization",
    "epoch_map",
    "probes",
    "optimizer",
    "emphasis",
    "seed",
    "metric",
    "output",
    "target_file",
    "base_file",
    "body_file",
    "crm_file",
    "transform_file",
    "covariance_file",
    "calibration",
    "bundle",
    "selection",
    "reports",
    "firmware",
    "wb_mode",
];

const MAX_REPORT_PINS: usize = 256;

const MAX_PERMALINK_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PermalinkSources {
    pub target_look: Option<String>,
    pub base_look: Option<String>,
    pub body: Option<String>,
    pub crm: Option<String>,
    pub transform: Option<String>,
    pub covariance: Option<String>,
}

/// Whether a result used only assessed scopes or the explicit experimental path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionMode {
    Qualified,
    Experimental,
}

impl SelectionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Qualified => "qualified",
            Self::Experimental => "experimental",
        }
    }

    /// # Errors
    /// Text other than `qualified` or `experimental`.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "qualified" => Ok(Self::Qualified),
            "experimental" => Ok(Self::Experimental),
            other => Err(MatchError::Permalink(format!(
                "unknown selection {other:?}"
            ))),
        }
    }
}

/// The immutable report or target-eligibility snapshot a selection depended on.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum QualificationRef {
    Report(looklab_schema::ModelReportId),
    Eligibility(looklab_schema::EligibilitySnapshotId),
}

impl QualificationRef {
    pub fn token(&self) -> String {
        match self {
            Self::Report(id) => format!("report:{}", id.to_hex()),
            Self::Eligibility(id) => format!("eligibility:{}", id.to_hex()),
        }
    }

    /// # Errors
    /// A token that is not `report:<hex>` or `eligibility:<hex>`.
    pub fn parse(token: &str) -> Result<Self> {
        if let Some(hex) = token.strip_prefix("report:") {
            return looklab_schema::ModelReportId::parse(hex)
                .map(Self::Report)
                .map_err(|_| MatchError::Permalink(format!("report id {hex:?}")));
        }
        if let Some(hex) = token.strip_prefix("eligibility:") {
            return looklab_schema::EligibilitySnapshotId::parse(hex)
                .map(Self::Eligibility)
                .map_err(|_| MatchError::Permalink(format!("eligibility id {hex:?}")));
        }
        Err(MatchError::Permalink(format!(
            "unknown qualification reference {token:?}"
        )))
    }
}

/// The subject whose assessment a pinned reference belongs to.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ReportSubject {
    Target(PublishedTargetId),
    Realization(RealizationRevisionId),
}

impl ReportSubject {
    pub fn token(&self) -> String {
        match self {
            Self::Target(target) => format!("{}@{}", target.look.as_str(), target.version.get()),
            Self::Realization(revision) => format!("realization:{}", revision.as_str()),
        }
    }

    /// # Errors
    /// Text that is neither a target `look@version` nor a `realization:<id>`.
    pub fn parse(value: &str) -> Result<Self> {
        if let Some(revision) = value.strip_prefix("realization:") {
            return RealizationRevisionId::new(revision)
                .map(Self::Realization)
                .map_err(|error| MatchError::Permalink(error.to_string()));
        }
        parse_published_target(value).map(Self::Target)
    }
}

/// One `subject=<ref>` entry of a link's recorded assessment context.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReportPin {
    pub subject: ReportSubject,
    pub reference: QualificationRef,
}

/// The bundle revision and report set a result was produced from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionContext {
    pub bundle: Sha256Hex,
    pub mode: SelectionMode,
    pub reports: Vec<ReportPin>,
    #[serde(default)]
    pub firmware: Option<looklab_schema::FirmwareId>,
    #[serde(default)]
    pub wb_mode: Option<looklab_schema::WbModeKey>,
}

impl SelectionContext {
    /// # Errors
    /// A qualified selection with no pinned report, or too many reports.
    pub fn validate(&self) -> Result<()> {
        if self.mode == SelectionMode::Qualified && self.reports.is_empty() {
            return Err(MatchError::Permalink(
                "a qualified selection pins at least one report".to_owned(),
            ));
        }
        if self.reports.len() > MAX_REPORT_PINS {
            return Err(MatchError::Permalink(format!(
                "{} report pins, more than {MAX_REPORT_PINS}",
                self.reports.len()
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Permalink {
    pub target_slug: String,
    pub target_version: String,
    pub base_slug: String,
    pub base_version: String,
    pub body: String,
    pub epoch: u32,
    pub sim: String,
    pub crm_version: u32,
    pub input_transform_version: u32,
    pub realization_params_version: u32,
    pub epoch_map_version: u32,
    pub probe_set: ProbeSetVersion,
    pub optimizer: String,
    pub metric: ScoringMetric,
    pub output: Option<OutputContext>,
    pub emphasis: Emphasis,
    pub seed: u64,
    pub sources: PermalinkSources,
    pub calibration: Calibration,
    /// `None` for a legacy link that recorded no bundle revision or assessment context.
    pub selection: Option<SelectionContext>,
}

impl Permalink {
    pub fn canonical(&self) -> String {
        let calibrated = !self.calibration.is_empty();
        let rendered = self.metric.is_rendered();
        let mut out = String::from(if rendered {
            RENDERED_PERMALINK_FORMAT
        } else if self.selection.is_some() {
            SELECTION_PERMALINK_FORMAT
        } else if calibrated {
            CALIBRATED_PERMALINK_FORMAT
        } else {
            PERMALINK_FORMAT
        });
        let mut push = |key: &str, value: &str| {
            out.push('|');
            out.push_str(key);
            out.push('=');
            out.push_str(&encode(value));
        };
        push(
            "look",
            &format!("{}@{}", self.target_slug, self.target_version),
        );
        push("base", &format!("{}@{}", self.base_slug, self.base_version));
        push(
            "crm",
            &format!(
                "{}@{}@{}@{}",
                self.body, self.epoch, self.sim, self.crm_version
            ),
        );
        push("input", &self.input_transform_version.to_string());
        push("realization", &self.realization_params_version.to_string());
        push("epoch_map", &self.epoch_map_version.to_string());
        push("probes", probe_set_name(self.probe_set));
        push("optimizer", &self.optimizer);
        if rendered {
            push("metric", self.metric.identity());
            if let Some(output) = &self.output {
                push("output", &output.canonical());
            }
        }
        push("emphasis", self.emphasis.as_str());
        push("seed", &self.seed.to_string());
        if let Some(value) = &self.sources.target_look {
            push("target_file", value);
        }
        if let Some(value) = &self.sources.base_look {
            push("base_file", value);
        }
        if let Some(value) = &self.sources.body {
            push("body_file", value);
        }
        if let Some(value) = &self.sources.crm {
            push("crm_file", value);
        }
        if let Some(value) = &self.sources.transform {
            push("transform_file", value);
        }
        if let Some(value) = &self.sources.covariance {
            push("covariance_file", value);
        }
        if calibrated {
            push("calibration", &self.calibration.to_field());
        }
        if let Some(selection) = &self.selection {
            push("bundle", &selection.bundle.to_hex());
            push("selection", selection.mode.as_str());
            push("reports", &reports_field(&selection.reports));
            if let Some(firmware) = &selection.firmware {
                push("firmware", &firmware.as_text());
            }
            if let Some(wb_mode) = &selection.wb_mode {
                push("wb_mode", wb_mode.as_str());
            }
        }
        out
    }

    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        if text.len() > MAX_PERMALINK_BYTES {
            return Err(MatchError::Permalink(format!(
                "permalink is {} bytes, more than {MAX_PERMALINK_BYTES}",
                text.len()
            )));
        }
        let mut parts = text.split('|');
        let format = parts
            .next()
            .ok_or_else(|| MatchError::Permalink("empty permalink".to_string()))?;
        match format {
            PERMALINK_FORMAT
            | CALIBRATED_PERMALINK_FORMAT
            | SELECTION_PERMALINK_FORMAT
            | RENDERED_PERMALINK_FORMAT => {}
            other => {
                return Err(MatchError::Permalink(format!(
                    "expected {PERMALINK_FORMAT}, {CALIBRATED_PERMALINK_FORMAT}, \
                     {SELECTION_PERMALINK_FORMAT} or {RENDERED_PERMALINK_FORMAT}, got {other:?}"
                )));
            }
        }
        let mut values: BTreeMap<String, String> = BTreeMap::new();
        for part in parts {
            let (key, value) = part
                .split_once('=')
                .ok_or_else(|| MatchError::Permalink(format!("malformed field {part:?}")))?;
            if !KNOWN_FIELDS.contains(&key) {
                return Err(MatchError::Permalink(format!("unknown field {key}")));
            }
            if values.insert(key.to_string(), decode(value)?).is_some() {
                return Err(MatchError::Permalink(format!("duplicate field {key}")));
            }
        }
        let required = |key: &str| -> Result<&str> {
            values
                .get(key)
                .map(String::as_str)
                .ok_or_else(|| MatchError::Permalink(format!("missing field {key}")))
        };
        let (target_slug, target_version) = split_pair(required("look")?, "look")?;
        let (base_slug, base_version) = split_pair(required("base")?, "base")?;
        let crm = required("crm")?;
        let crm_parts: Vec<&str> = crm.split('@').collect();
        if crm_parts.len() != 4 {
            return Err(MatchError::Permalink("crm field".to_string()));
        }
        let epoch = crm_parts[1]
            .parse()
            .map_err(|_| MatchError::Permalink("epoch".to_string()))?;
        let crm_version = crm_parts[3]
            .parse()
            .map_err(|_| MatchError::Permalink("crm version".to_string()))?;
        let parse_u32 = |key: &str| -> Result<u32> {
            required(key)?
                .parse()
                .map_err(|_| MatchError::Permalink(key.to_string()))
        };
        let emphasis = Emphasis::parse(required("emphasis")?)
            .ok_or_else(|| MatchError::Permalink("emphasis".to_string()))?;
        let sources = PermalinkSources {
            target_look: values.get("target_file").cloned(),
            base_look: values.get("base_file").cloned(),
            body: values.get("body_file").cloned(),
            crm: values.get("crm_file").cloned(),
            transform: values.get("transform_file").cloned(),
            covariance: values.get("covariance_file").cloned(),
        };
        let calibration = match format {
            PERMALINK_FORMAT => {
                reject_selection_fields(&values)?;
                if values.contains_key("calibration") {
                    return Err(MatchError::Permalink(
                        "calibration requires looklab-match/2".to_string(),
                    ));
                }
                Calibration::default()
            }
            CALIBRATED_PERMALINK_FORMAT => {
                reject_selection_fields(&values)?;
                Calibration::parse(required("calibration")?)?
            }
            _ => match values.get("calibration") {
                Some(text) => Calibration::parse(text)?,
                None => Calibration::default(),
            },
        };
        let metric = if format == RENDERED_PERMALINK_FORMAT {
            let metric = ScoringMetric::parse(required("metric")?)
                .map_err(|error| MatchError::Permalink(error.to_string()))?;
            if !metric.is_rendered() {
                return Err(MatchError::Permalink(
                    "looklab-match/4 records the rendered post-clip metric".to_string(),
                ));
            }
            metric
        } else {
            if values.contains_key("metric") || values.contains_key("output") {
                return Err(MatchError::Permalink(format!(
                    "metric and output require {RENDERED_PERMALINK_FORMAT}"
                )));
            }
            ScoringMetric::LegacyPreClipV1
        };
        let output = if format == RENDERED_PERMALINK_FORMAT {
            Some(
                OutputContext::parse(required("output")?)
                    .map_err(|error| MatchError::Permalink(error.to_string()))?,
            )
        } else {
            None
        };
        let carries_selection = ["bundle", "selection", "reports", "firmware", "wb_mode"]
            .iter()
            .any(|key| values.contains_key(*key));
        let selection = if format == SELECTION_PERMALINK_FORMAT
            || (format == RENDERED_PERMALINK_FORMAT && carries_selection)
        {
            let bundle = Sha256Hex::parse(required("bundle")?)
                .map_err(|_| MatchError::Permalink("bundle revision id".to_string()))?;
            let mode = SelectionMode::parse(required("selection")?)?;
            let reports = parse_reports(required("reports")?)?;
            let firmware = values
                .get("firmware")
                .map(|text| looklab_schema::FirmwareId::parse(text))
                .transpose()
                .map_err(|error| MatchError::Permalink(error.to_owned()))?;
            let wb_mode = values
                .get("wb_mode")
                .map(|text| looklab_schema::WbModeKey::new(text))
                .transpose()
                .map_err(|error| MatchError::Permalink(error.to_owned()))?;
            let context = SelectionContext {
                bundle,
                mode,
                reports,
                firmware,
                wb_mode,
            };
            context.validate()?;
            Some(context)
        } else {
            None
        };
        if selection
            .as_ref()
            .is_some_and(|context| context.mode == SelectionMode::Qualified)
            && !calibration.is_empty()
        {
            return Err(MatchError::Permalink(
                "a qualified selection requires an identity calibration".to_owned(),
            ));
        }
        if let Some(context) = &selection
            && let Some(firmware) = &context.firmware
            && (firmware.body.as_str() != crm_parts[0] || firmware.epoch.get() != epoch)
        {
            return Err(MatchError::Permalink(
                "the pinned firmware belongs to another body or epoch".to_owned(),
            ));
        }
        let permalink = Self {
            target_slug: target_slug.to_string(),
            target_version: target_version.to_string(),
            base_slug: base_slug.to_string(),
            base_version: base_version.to_string(),
            body: crm_parts[0].to_string(),
            epoch,
            sim: crm_parts[2].to_string(),
            crm_version,
            input_transform_version: parse_u32("input")?,
            realization_params_version: parse_u32("realization")?,
            epoch_map_version: parse_u32("epoch_map")?,
            probe_set: parse_probe_set(required("probes")?)?,
            optimizer: required("optimizer")?.to_string(),
            metric,
            output,
            emphasis,
            seed: required("seed")?
                .parse()
                .map_err(|_| MatchError::Permalink("seed".to_string()))?,
            sources,
            calibration,
            selection,
        };
        permalink.validate()?;
        Ok(permalink)
    }

    /// # Errors
    /// A metric, optimizer identity or output context that do not agree.
    pub fn validate(&self) -> Result<()> {
        if let Some(selection) = &self.selection {
            selection.validate()?;
        }
        match self.metric {
            ScoringMetric::RenderedPostClipV1 => {
                if self.optimizer != OPTIMIZER_VERSION {
                    return Err(MatchError::Permalink(format!(
                        "the rendered metric requires optimizer {OPTIMIZER_VERSION}, got {:?}",
                        self.optimizer
                    )));
                }
                if self.output.is_none() {
                    return Err(MatchError::Permalink(
                        "the rendered metric requires an output context".to_string(),
                    ));
                }
            }
            ScoringMetric::LegacyPreClipV1 => {
                if self.optimizer != PRE_CLIP_OPTIMIZER_VERSION {
                    return Err(MatchError::Permalink(format!(
                        "the legacy pre-clip metric requires optimizer {PRE_CLIP_OPTIMIZER_VERSION}, got {:?}",
                        self.optimizer
                    )));
                }
                if self.output.is_some() {
                    return Err(MatchError::Permalink(
                        "the legacy pre-clip metric records no output context".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }
}

fn reject_selection_fields(values: &BTreeMap<String, String>) -> Result<()> {
    for key in ["bundle", "selection", "reports", "firmware", "wb_mode"] {
        if values.contains_key(key) {
            return Err(MatchError::Permalink(format!(
                "{key} requires {SELECTION_PERMALINK_FORMAT}"
            )));
        }
    }
    Ok(())
}

fn reports_field(reports: &[ReportPin]) -> String {
    let mut pins = reports.to_vec();
    pins.sort();
    pins.dedup();
    pins.iter()
        .map(|pin| {
            format!(
                "{}={}",
                encode_subject(&pin.subject),
                encode_component(&pin.reference.token())
            )
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn encode_subject(subject: &ReportSubject) -> String {
    match subject {
        ReportSubject::Target(target) => format!(
            "{}@{}",
            encode_component(target.look.as_str()),
            encode_component(&target.version.get().to_string())
        ),
        ReportSubject::Realization(revision) => {
            encode_component(&format!("realization:{}", revision.as_str()))
        }
    }
}

fn parse_reports(field: &str) -> Result<Vec<ReportPin>> {
    if field.is_empty() {
        return Ok(Vec::new());
    }
    let mut pins = Vec::new();
    for entry in field.split(';') {
        let (subject, reference) = entry
            .split_once('=')
            .ok_or_else(|| MatchError::Permalink(format!("report entry {entry:?}")))?;
        pins.push(ReportPin {
            subject: parse_subject(subject)?,
            reference: QualificationRef::parse(&decode_component(reference)?)?,
        });
        if pins.len() > MAX_REPORT_PINS {
            return Err(MatchError::Permalink(format!(
                "more than {MAX_REPORT_PINS} report pins"
            )));
        }
    }
    Ok(pins)
}

fn parse_subject(value: &str) -> Result<ReportSubject> {
    if value.contains('@') {
        let (look, version) = split_pair(value, "report subject")?;
        return ReportSubject::parse(&format!(
            "{}@{}",
            decode_component(look)?,
            decode_component(version)?
        ));
    }
    ReportSubject::parse(&decode_component(value)?)
}

fn parse_published_target(value: &str) -> Result<PublishedTargetId> {
    let (look, version) = split_pair(value, "target")?;
    let look = LookId::new(look)
        .map_err(|error| MatchError::Permalink(format!("target look: {error}")))?;
    let version: u32 = version
        .parse()
        .map_err(|_| MatchError::Permalink(format!("target version {version:?}")))?;
    let version = TargetVersion::new(version)
        .ok_or_else(|| MatchError::Permalink("target version is zero".to_string()))?;
    Ok(PublishedTargetId { look, version })
}

fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

fn decode_component(value: &str) -> Result<String> {
    decode(value)
}

fn split_pair<'a>(value: &'a str, field: &str) -> Result<(&'a str, &'a str)> {
    value
        .split_once('@')
        .ok_or_else(|| MatchError::Permalink(format!("{field} field")))
}

fn probe_set_name(version: ProbeSetVersion) -> &'static str {
    match version {
        ProbeSetVersion::V1 => "v1",
    }
}

fn parse_probe_set(value: &str) -> Result<ProbeSetVersion> {
    match value {
        "v1" => Ok(ProbeSetVersion::V1),
        other => Err(MatchError::Permalink(format!("probe set {other}"))),
    }
}

fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/' | b':') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

fn decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = value
                .get(index + 1..index + 3)
                .ok_or_else(|| MatchError::Permalink("truncated escape".to_string()))?;
            let byte = u8::from_str_radix(hex, 16)
                .map_err(|_| MatchError::Permalink("escape".to_string()))?;
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| MatchError::Permalink("utf-8".to_string()))
}
