// SPDX-License-Identifier: AGPL-3.0-or-later
//! The documentation-derived camera catalog.
//!
//! A catalog record states what an official manual or firmware note documents.
//! It is not a body entity, a control schema, a decoder mapping, a fit input or
//! a confirmation.

use core::fmt;
use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{ErrorKind, ValidationError};
use crate::ids::{BodyId, SourceId};
use crate::proposal::Sha256Hex;
use crate::rational::Rational;

pub const MANUAL_CATALOG_VERSION: u32 = 1;
pub const MAX_MANUAL_CAMERAS: usize = 256;
pub const MAX_MANUAL_URL_BYTES: usize = 2048;
pub const MAX_FIRMWARE_SCOPE_BYTES: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualCatalog {
    pub catalog_version: u32,
    pub cameras: Vec<ManualCamera>,
}

impl ManualCatalog {
    pub fn camera(&self, id: &BodyId) -> Option<&ManualCamera> {
        self.cameras.iter().find(|camera| &camera.id == id)
    }

    pub fn camera_by_make_model(&self, make: &str, model: &str) -> Option<&ManualCamera> {
        self.cameras.iter().find(|camera| {
            camera.make.trim().eq_ignore_ascii_case(make.trim())
                && camera.model.trim().eq_ignore_ascii_case(model.trim())
        })
    }

    /// # Errors
    /// Every cross-record rule violation in the catalog.
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        let mut errors = Vec::new();
        if self.catalog_version != MANUAL_CATALOG_VERSION {
            errors.push(ValidationError::new(
                "catalog_version",
                ErrorKind::UnsupportedCatalogVersion {
                    found: self.catalog_version,
                },
            ));
        }
        if self.cameras.is_empty() {
            errors.push(ValidationError::new(
                "cameras",
                ErrorKind::EmptyText { field: "cameras" },
            ));
        }
        if self.cameras.len() > MAX_MANUAL_CAMERAS {
            errors.push(ValidationError::new(
                "cameras",
                ErrorKind::InvalidValue {
                    expected: "at most 256 camera records",
                },
            ));
        }
        let mut ids = HashSet::new();
        for (index, camera) in self.cameras.iter().enumerate() {
            let path = format!("cameras[{index}]");
            if !ids.insert(camera.id.clone()) {
                errors.push(ValidationError::new(
                    format!("{path}.id"),
                    ErrorKind::DuplicateKey {
                        key: camera.id.to_string(),
                    },
                ));
            }
            camera.validate(&path, &mut errors);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualCamera {
    pub id: BodyId,
    pub make: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub introduced_year: Option<u16>,
    pub sources: Vec<ManualSource>,
    pub capabilities: Vec<DocumentedCapability>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl ManualCamera {
    pub fn definition_digest(&self) -> Sha256Hex {
        let bytes = serde_json::to_vec(self).expect("a manual camera has no unserializable value");
        Sha256Hex::from_bytes(Sha256::digest(bytes).into())
    }

    fn validate(&self, path: &str, errors: &mut Vec<ValidationError>) {
        check_text(&self.make, &format!("{path}.make"), "make", errors);
        check_text(&self.model, &format!("{path}.model"), "model", errors);
        if let Some(year) = self.introduced_year
            && !(2011..=2100).contains(&year)
        {
            errors.push(ValidationError::new(
                format!("{path}.introduced_year"),
                ErrorKind::YearOutOfScope { year },
            ));
        }
        for (index, note) in self.notes.iter().enumerate() {
            check_text(note, &format!("{path}.notes[{index}]"), "note", errors);
        }
        if self.sources.is_empty() {
            errors.push(ValidationError::new(
                format!("{path}.sources"),
                ErrorKind::EmptyText { field: "sources" },
            ));
        }
        let mut source_ids = HashSet::new();
        for (index, source) in self.sources.iter().enumerate() {
            let source_path = format!("{path}.sources[{index}]");
            if !source_ids.insert(source.id.clone()) {
                errors.push(ValidationError::new(
                    format!("{source_path}.id"),
                    ErrorKind::DuplicateKey {
                        key: source.id.to_string(),
                    },
                ));
            }
            source.validate(&source_path, errors);
        }
        if self.capabilities.is_empty() {
            errors.push(ValidationError::new(
                format!("{path}.capabilities"),
                ErrorKind::EmptyText {
                    field: "capabilities",
                },
            ));
        }
        let mut keys = HashSet::new();
        for (index, capability) in self.capabilities.iter().enumerate() {
            let capability_path = format!("{path}.capabilities[{index}]");
            if !keys.insert(capability.key) {
                errors.push(ValidationError::new(
                    format!("{capability_path}.key"),
                    ErrorKind::DuplicateKey {
                        key: capability.key.as_str().to_owned(),
                    },
                ));
            }
            capability.validate(&capability_path, &source_ids, errors);
        }
        if !keys.contains(&CapabilityKey::NativeSimulations) {
            errors.push(ValidationError::new(
                format!("{path}.capabilities"),
                ErrorKind::MissingField {
                    field: "native_simulations",
                },
            ));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualSource {
    pub id: SourceId,
    pub kind: SourceKind,
    pub url: ManualUrl,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edition: Option<String>,
    pub firmware_scope: FirmwareScope,
}

impl ManualSource {
    fn validate(&self, path: &str, errors: &mut Vec<ValidationError>) {
        check_text(&self.title, &format!("{path}.title"), "title", errors);
        if let Some(edition) = &self.edition {
            check_text(edition, &format!("{path}.edition"), "edition", errors);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    OwnerManual,
    FirmwareNotes,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentedCapability {
    pub key: CapabilityKey,
    pub label: String,
    pub category: CapabilityCategory,
    pub availability: Availability,
    #[serde(default)]
    pub domain: CapabilityDomain,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub citations: Vec<Citation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl DocumentedCapability {
    fn validate(
        &self,
        path: &str,
        source_ids: &HashSet<SourceId>,
        errors: &mut Vec<ValidationError>,
    ) {
        check_text(&self.label, &format!("{path}.label"), "label", errors);
        if let Some(notes) = &self.notes {
            check_text(notes, &format!("{path}.notes"), "note", errors);
        }
        self.domain.validate(&format!("{path}.domain"), errors);
        if self.availability == Availability::Unavailable
            && self.domain != CapabilityDomain::Unspecified
        {
            errors.push(ValidationError::new(
                format!("{path}.domain"),
                ErrorKind::ManualDomainNotUnspecified {
                    key: self.key.as_str().to_owned(),
                },
            ));
        }
        if self.availability == Availability::NotDocumented
            && self.domain != CapabilityDomain::Unspecified
        {
            errors.push(ValidationError::new(
                format!("{path}.domain"),
                ErrorKind::ManualDomainNotUnspecified {
                    key: self.key.as_str().to_owned(),
                },
            ));
        }
        if self.key == CapabilityKey::NativeSimulations
            && self.availability == Availability::Available
            && !matches!(self.domain, CapabilityDomain::Enum { .. })
        {
            errors.push(ValidationError::new(
                format!("{path}.domain"),
                ErrorKind::ManualKeyNotEnum {
                    key: self.key.as_str().to_owned(),
                },
            ));
        }
        if self.availability != Availability::NotDocumented && self.citations.is_empty() {
            errors.push(ValidationError::new(
                format!("{path}.citations"),
                ErrorKind::CitationRequired {
                    key: self.key.as_str().to_owned(),
                },
            ));
        }
        for (index, citation) in self.citations.iter().enumerate() {
            let citation_path = format!("{path}.citations[{index}]");
            if !source_ids.contains(&citation.source) {
                errors.push(ValidationError::new(
                    format!("{citation_path}.source"),
                    ErrorKind::UnknownCitationSource {
                        source_id: citation.source.clone(),
                    },
                ));
            }
            check_text(
                &citation.locator,
                &format!("{citation_path}.locator"),
                "locator",
                errors,
            );
        }
    }

    pub fn definition_digest(&self, camera: &ManualCamera) -> Sha256Hex {
        #[derive(Serialize)]
        struct Definition<'a> {
            capability: &'a DocumentedCapability,
            sources: Vec<&'a ManualSource>,
        }
        let sources = self
            .citations
            .iter()
            .filter_map(|citation| {
                camera
                    .sources
                    .iter()
                    .find(|source| source.id == citation.source)
            })
            .collect();
        let bytes = serde_json::to_vec(&Definition {
            capability: self,
            sources,
        })
        .expect("a documented capability has no unserializable value");
        Sha256Hex::from_bytes(Sha256::digest(bytes).into())
    }

    pub fn source_scopes<'a>(&'a self, camera: &'a ManualCamera) -> Vec<&'a FirmwareScope> {
        self.citations
            .iter()
            .filter_map(|citation| {
                camera
                    .sources
                    .iter()
                    .find(|source| source.id == citation.source)
                    .map(|source| &source.firmware_scope)
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Citation {
    pub source: SourceId,
    pub locator: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityCategory {
    Rendering,
    Spatial,
    Capture,
    Workflow,
    Mixed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Available,
    Unavailable,
    NotDocumented,
}

impl Availability {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Unavailable => "unavailable",
            Self::NotDocumented => "not_documented",
        }
    }
}

impl fmt::Display for Availability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKey {
    RawCapture,
    JpegCapture,
    HeifCapture,
    RawJpegSimultaneous,
    RawBitDepth,
    RawCompression,
    JpegColorSpaces,
    JpegQuality,
    InCameraRawConversion,
    NativeSimulations,
    WhiteBalancePresets,
    WhiteBalanceShift,
    ColorTemperature,
    ExposureCompensation,
    DynamicRange,
    DynamicRangePriority,
    ToneCurve,
    HighlightTone,
    ShadowTone,
    Contrast,
    Saturation,
    ColorChrome,
    MonochromeToning,
    MonochromeToningShift,
    Sharpening,
    NoiseReduction,
    Clarity,
    Grain,
    GrainSize,
    ColorChromeEffect,
    ColorChromeFxBlue,
    SmoothSkinEffect,
    CreativeFilters,
    ColorProfileControl,
    MonochromeProfileControl,
    FilmSimulationBracket,
}

impl CapabilityKey {
    pub const ALL: [Self; 36] = [
        Self::RawCapture,
        Self::JpegCapture,
        Self::HeifCapture,
        Self::RawJpegSimultaneous,
        Self::RawBitDepth,
        Self::RawCompression,
        Self::JpegColorSpaces,
        Self::JpegQuality,
        Self::InCameraRawConversion,
        Self::NativeSimulations,
        Self::WhiteBalancePresets,
        Self::WhiteBalanceShift,
        Self::ColorTemperature,
        Self::ExposureCompensation,
        Self::DynamicRange,
        Self::DynamicRangePriority,
        Self::ToneCurve,
        Self::HighlightTone,
        Self::ShadowTone,
        Self::Contrast,
        Self::Saturation,
        Self::ColorChrome,
        Self::MonochromeToning,
        Self::MonochromeToningShift,
        Self::Sharpening,
        Self::NoiseReduction,
        Self::Clarity,
        Self::Grain,
        Self::GrainSize,
        Self::ColorChromeEffect,
        Self::ColorChromeFxBlue,
        Self::SmoothSkinEffect,
        Self::CreativeFilters,
        Self::ColorProfileControl,
        Self::MonochromeProfileControl,
        Self::FilmSimulationBracket,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RawCapture => "raw_capture",
            Self::JpegCapture => "jpeg_capture",
            Self::HeifCapture => "heif_capture",
            Self::RawJpegSimultaneous => "raw_jpeg_simultaneous",
            Self::RawBitDepth => "raw_bit_depth",
            Self::RawCompression => "raw_compression",
            Self::JpegColorSpaces => "jpeg_color_spaces",
            Self::JpegQuality => "jpeg_quality",
            Self::InCameraRawConversion => "in_camera_raw_conversion",
            Self::NativeSimulations => "native_simulations",
            Self::WhiteBalancePresets => "white_balance_presets",
            Self::WhiteBalanceShift => "white_balance_shift",
            Self::ColorTemperature => "color_temperature",
            Self::ExposureCompensation => "exposure_compensation",
            Self::DynamicRange => "dynamic_range",
            Self::DynamicRangePriority => "dynamic_range_priority",
            Self::ToneCurve => "tone_curve",
            Self::HighlightTone => "highlight_tone",
            Self::ShadowTone => "shadow_tone",
            Self::Contrast => "contrast",
            Self::Saturation => "saturation",
            Self::ColorChrome => "color_chrome",
            Self::MonochromeToning => "monochrome_toning",
            Self::MonochromeToningShift => "monochrome_toning_shift",
            Self::Sharpening => "sharpening",
            Self::NoiseReduction => "noise_reduction",
            Self::Clarity => "clarity",
            Self::Grain => "grain",
            Self::GrainSize => "grain_size",
            Self::ColorChromeEffect => "color_chrome_effect",
            Self::ColorChromeFxBlue => "color_chrome_fx_blue",
            Self::SmoothSkinEffect => "smooth_skin_effect",
            Self::CreativeFilters => "creative_filters",
            Self::ColorProfileControl => "color_profile_control",
            Self::MonochromeProfileControl => "monochrome_profile_control",
            Self::FilmSimulationBracket => "film_simulation_bracket",
        }
    }
}

impl fmt::Display for CapabilityKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl core::str::FromStr for CapabilityKey {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|key| key.as_str() == value)
            .ok_or(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CapabilityDomain {
    Boolean {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<bool>,
    },
    Enum {
        values: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
    },
    Integer {
        min: i64,
        max: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<i64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        steps: Vec<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<i64>,
    },
    Rational {
        min: String,
        max: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        steps: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
    },
    Axes {
        axes: Vec<Axis>,
    },
    #[default]
    Unspecified,
}

impl<'de> Deserialize<'de> for CapabilityDomain {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
        enum Raw {
            Boolean {
                #[serde(default)]
                default: Option<bool>,
            },
            Enum {
                values: Vec<String>,
                #[serde(default)]
                default: Option<String>,
            },
            Integer {
                min: i64,
                max: i64,
                #[serde(default)]
                step: Option<i64>,
                #[serde(default)]
                steps: Vec<i64>,
                #[serde(default)]
                default: Option<i64>,
            },
            Rational {
                min: String,
                max: String,
                #[serde(default)]
                step: Option<String>,
                #[serde(default)]
                steps: Vec<String>,
                #[serde(default)]
                default: Option<String>,
            },
            Axes {
                axes: Vec<Axis>,
            },
            Unspecified {},
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Boolean { default } => Self::Boolean { default },
            Raw::Enum { values, default } => Self::Enum { values, default },
            Raw::Integer {
                min,
                max,
                step,
                steps,
                default,
            } => Self::Integer {
                min,
                max,
                step,
                steps,
                default,
            },
            Raw::Rational {
                min,
                max,
                step,
                steps,
                default,
            } => Self::Rational {
                min,
                max,
                step,
                steps,
                default,
            },
            Raw::Axes { axes } => Self::Axes { axes },
            Raw::Unspecified {} => Self::Unspecified,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Axis {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainKind {
    Boolean,
    Enum,
    Integer,
    Rational,
    Axes,
    Unspecified,
}

impl CapabilityDomain {
    pub const fn kind(&self) -> DomainKind {
        match self {
            Self::Boolean { .. } => DomainKind::Boolean,
            Self::Enum { .. } => DomainKind::Enum,
            Self::Integer { .. } => DomainKind::Integer,
            Self::Rational { .. } => DomainKind::Rational,
            Self::Axes { .. } => DomainKind::Axes,
            Self::Unspecified => DomainKind::Unspecified,
        }
    }

    pub(crate) fn validate(&self, path: &str, errors: &mut Vec<ValidationError>) {
        match self {
            Self::Boolean { .. } | Self::Unspecified => {}
            Self::Enum { values, default } => {
                if values.is_empty() {
                    errors.push(ValidationError::new(
                        format!("{path}.values"),
                        ErrorKind::EmptyText { field: "values" },
                    ));
                }
                let mut seen = HashSet::new();
                for (index, value) in values.iter().enumerate() {
                    check_text(
                        value,
                        &format!("{path}.values[{index}]"),
                        "enum value",
                        errors,
                    );
                    if !seen.insert(value.as_str()) {
                        errors.push(ValidationError::new(
                            format!("{path}.values[{index}]"),
                            ErrorKind::DuplicateKey { key: value.clone() },
                        ));
                    }
                }
                if let Some(default) = default
                    && !values.iter().any(|value| value == default)
                {
                    errors.push(ValidationError::new(
                        format!("{path}.default"),
                        ErrorKind::EnumDefaultNotInValues,
                    ));
                }
            }
            Self::Integer {
                min,
                max,
                step,
                steps,
                default,
            } => {
                if min > max {
                    errors.push(ValidationError::new(path, ErrorKind::RangeInverted));
                }
                if step.is_some() && !steps.is_empty() {
                    errors.push(ValidationError::new(
                        path,
                        ErrorKind::InvalidValue {
                            expected: "step or steps, not both",
                        },
                    ));
                }
                for (index, step) in steps.iter().enumerate() {
                    if *step <= 0 {
                        errors.push(ValidationError::new(
                            format!("{path}.steps[{index}]"),
                            ErrorKind::StepNotPositive,
                        ));
                    }
                }
                if let Some(step) = step {
                    if *step <= 0 {
                        errors.push(ValidationError::new(
                            format!("{path}.step"),
                            ErrorKind::StepNotPositive,
                        ));
                    } else if let Some(default) = default {
                        if default < min || default > max {
                            errors.push(ValidationError::new(
                                format!("{path}.default"),
                                ErrorKind::DefaultOutOfRange,
                            ));
                        } else if default
                            .checked_sub(*min)
                            .is_none_or(|delta| delta % *step != 0)
                        {
                            errors.push(ValidationError::new(
                                format!("{path}.default"),
                                ErrorKind::NotOnStep,
                            ));
                        }
                    }
                } else if !steps.is_empty() {
                    if let Some(default) = default {
                        if default < min || default > max {
                            errors.push(ValidationError::new(
                                format!("{path}.default"),
                                ErrorKind::DefaultOutOfRange,
                            ));
                        } else if default
                            .checked_sub(*min)
                            .is_none_or(|delta| !steps.iter().any(|step| delta % *step == 0))
                        {
                            errors.push(ValidationError::new(
                                format!("{path}.default"),
                                ErrorKind::NotOnStep,
                            ));
                        }
                    }
                } else if let Some(default) = default
                    && (default < min || default > max)
                {
                    errors.push(ValidationError::new(
                        format!("{path}.default"),
                        ErrorKind::DefaultOutOfRange,
                    ));
                }
            }
            Self::Rational {
                min,
                max,
                step,
                steps,
                default,
            } => {
                let parsed_min = parse_rational(min, &format!("{path}.min"), errors);
                let parsed_max = parse_rational(max, &format!("{path}.max"), errors);
                if step.is_some() && !steps.is_empty() {
                    errors.push(ValidationError::new(
                        path,
                        ErrorKind::InvalidValue {
                            expected: "step or steps, not both",
                        },
                    ));
                }
                let parsed_steps = steps
                    .iter()
                    .enumerate()
                    .filter_map(|(index, value)| {
                        parse_step(value, &format!("{path}.steps[{index}]"), errors)
                    })
                    .collect::<Vec<_>>();
                let parsed_step = step
                    .as_deref()
                    .and_then(|value| parse_step(value, &format!("{path}.step"), errors));
                if let (Some(min), Some(max)) = (parsed_min, parsed_max)
                    && min > max
                {
                    errors.push(ValidationError::new(path, ErrorKind::RangeInverted));
                }
                let parsed_default = default
                    .as_deref()
                    .and_then(|value| parse_rational(value, &format!("{path}.default"), errors));
                if let (Some(min), Some(max), Some(default)) =
                    (parsed_min, parsed_max, parsed_default)
                {
                    if default < min || default > max {
                        errors.push(ValidationError::new(
                            format!("{path}.default"),
                            ErrorKind::DefaultOutOfRange,
                        ));
                    } else {
                        let delta = default.checked_sub(&min);
                        let on_step = match parsed_step {
                            Some(step) => delta.and_then(|delta| delta.div_exact(&step)).is_some(),
                            None if parsed_steps.is_empty() => true,
                            None => delta.is_some_and(|delta| {
                                parsed_steps
                                    .iter()
                                    .any(|step| delta.div_exact(step).is_some())
                            }),
                        };
                        if !on_step {
                            errors.push(ValidationError::new(
                                format!("{path}.default"),
                                ErrorKind::NotOnStep,
                            ));
                        }
                    }
                }
            }
            Self::Axes { axes } => {
                if axes.is_empty() {
                    errors.push(ValidationError::new(
                        format!("{path}.axes"),
                        ErrorKind::EmptyText { field: "axes" },
                    ));
                }
                let mut labels = HashSet::new();
                for (index, axis) in axes.iter().enumerate() {
                    let axis_path = format!("{path}.axes[{index}]");
                    check_text(
                        &axis.label,
                        &format!("{axis_path}.label"),
                        "axis label",
                        errors,
                    );
                    if !labels.insert(axis.label.as_str()) {
                        errors.push(ValidationError::new(
                            format!("{axis_path}.label"),
                            ErrorKind::DuplicateKey {
                                key: axis.label.clone(),
                            },
                        ));
                    }
                    match (axis.min, axis.max, axis.step) {
                        (None, None, None) => {}
                        (Some(min), Some(max), Some(step)) => {
                            if min > max {
                                errors.push(ValidationError::new(
                                    &axis_path,
                                    ErrorKind::RangeInverted,
                                ));
                            }
                            if step <= 0 {
                                errors.push(ValidationError::new(
                                    format!("{axis_path}.step"),
                                    ErrorKind::StepNotPositive,
                                ));
                            }
                        }
                        _ => errors.push(ValidationError::new(
                            axis_path,
                            ErrorKind::InvalidValue {
                                expected: "all three of min, max and step, or none",
                            },
                        )),
                    }
                }
            }
        }
    }
}

fn parse_rational(value: &str, path: &str, errors: &mut Vec<ValidationError>) -> Option<Rational> {
    match Rational::parse(value) {
        Ok(parsed) => Some(parsed),
        Err(_) => {
            errors.push(ValidationError::new(
                path,
                ErrorKind::InvalidRational {
                    value: value.to_owned(),
                },
            ));
            None
        }
    }
}

fn parse_step(value: &str, path: &str, errors: &mut Vec<ValidationError>) -> Option<Rational> {
    match Rational::parse_step(value) {
        Ok(parsed) => Some(parsed),
        Err(_) => {
            errors.push(ValidationError::new(
                path,
                ErrorKind::InvalidRational {
                    value: value.to_owned(),
                },
            ));
            None
        }
    }
}

fn check_text(value: &str, path: &str, field: &'static str, errors: &mut Vec<ValidationError>) {
    if value.trim().is_empty() {
        errors.push(ValidationError::new(path, ErrorKind::EmptyText { field }));
    }
}

/// An official document URL. Only HTTPS is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ManualUrl(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("expected an HTTPS URL")]
pub struct ManualUrlError;

impl ManualUrl {
    /// # Errors
    /// A non-HTTPS scheme, whitespace, an over-long value or a missing host.
    pub fn new(value: &str) -> Result<Self, ManualUrlError> {
        if value.len() > MAX_MANUAL_URL_BYTES || value.chars().any(char::is_whitespace) {
            return Err(ManualUrlError);
        }
        let Some(rest) = value.strip_prefix("https://") else {
            return Err(ManualUrlError);
        };
        if rest.is_empty() || !rest.contains('.') {
            return Err(ManualUrlError);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ManualUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ManualUrl {
    type Error = ManualUrlError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<ManualUrl> for String {
    fn from(value: ManualUrl) -> Self {
        value.0
    }
}

/// The firmware a document covers, or the literal `unspecified`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct FirmwareScope(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("expected a non-empty firmware scope")]
pub struct FirmwareScopeError;

impl FirmwareScope {
    /// # Errors
    /// Empty, over-long or control-character text.
    pub fn new(value: &str) -> Result<Self, FirmwareScopeError> {
        if value.trim().is_empty()
            || value.len() > MAX_FIRMWARE_SCOPE_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(FirmwareScopeError);
        }
        Ok(Self(value.trim().to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_unspecified(&self) -> bool {
        self.0.eq_ignore_ascii_case("unspecified")
    }
}

impl fmt::Display for FirmwareScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for FirmwareScope {
    type Error = FirmwareScopeError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<FirmwareScope> for String {
    fn from(value: FirmwareScope) -> Self {
        value.0
    }
}

/// # Errors
/// A catalog that fails a cross-record rule.
pub fn validate_manual_catalog(yaml: &str) -> Result<ManualCatalog, Vec<ValidationError>> {
    crate::yaml::check(yaml)?;
    let catalog: ManualCatalog = serde_norway::from_str(yaml).map_err(ValidationError::syntax)?;
    catalog.validate()?;
    Ok(catalog)
}

pub fn manual_catalog_digest(source: &str) -> Sha256Hex {
    Sha256Hex::from_bytes(Sha256::digest(source.as_bytes()).into())
}

pub fn manual_catalog_files_digest(files: &[(String, String)]) -> Sha256Hex {
    let mut hasher = Sha256::new();
    for (name, source) in files {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(source.as_bytes());
        hasher.update([0]);
    }
    Sha256Hex::from_bytes(hasher.finalize().into())
}

/// # Errors
/// A version mismatch or a camera id repeated across catalogs.
pub fn merge_manual_catalogs(
    catalogs: Vec<ManualCatalog>,
) -> Result<ManualCatalog, Vec<ValidationError>> {
    let mut merged = ManualCatalog {
        catalog_version: MANUAL_CATALOG_VERSION,
        cameras: Vec::new(),
    };
    let mut errors = Vec::new();
    for catalog in catalogs {
        if catalog.catalog_version != MANUAL_CATALOG_VERSION {
            errors.push(ValidationError::new(
                "catalog_version",
                ErrorKind::UnsupportedCatalogVersion {
                    found: catalog.catalog_version,
                },
            ));
        }
        for camera in catalog.cameras {
            if merged
                .cameras
                .iter()
                .any(|existing| existing.id == camera.id)
            {
                errors.push(ValidationError::new(
                    "cameras",
                    ErrorKind::DuplicateKey {
                        key: camera.id.to_string(),
                    },
                ));
            } else {
                merged.cameras.push(camera);
            }
        }
    }
    if errors.is_empty() {
        merged.validate()?;
        Ok(merged)
    } else {
        Err(errors)
    }
}
