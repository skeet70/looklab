// SPDX-License-Identifier: AGPL-3.0-or-later
//! `look.json` + `look.bin` in a `.looklab` zip (docs/03 §3). `look.bin` is
//! `b"LKBN"`, a u32 array count, then name/len/little-endian-f64 arrays by name.

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};

use looklab_color::cam16::ViewingConditions;
use looklab_schema::{BodyId, SimKey};
use serde::{Deserialize, Serialize};
use zip::write::SimpleFileOptions;

use crate::basis::{
    C_COEFS, C_MAX, CORE_TERMS, HUE_COEFS, NEUTRAL_JOIN_C, RESIDUAL_COEFS, RESIDUAL_GRID, S_COEFS,
    S_KNOTS, TINT_COEFS, TINT_KNOTS, TONE_COEFS, TONE_KNOTS, VOLUME_COEFS,
};
use crate::clip::ClipSettings;
use crate::error::{ModelError, Result};
use crate::look::{ColorLayer, GRL_COEFS, GrlComponent, Look, RESIDUAL_DOMAIN, Residual, Tint};
use crate::tone::Tone;

pub const FORMAT_ID: &str = "looklab-look/2";

const BIN_MAGIC: &[u8; 4] = b"LKBN";
const JSON_NAME: &str = "look.json";
const BIN_NAME: &str = "look.bin";

const MAX_ARCHIVE_BYTES: usize = 32 * 1024 * 1024;
const MAX_JSON_BYTES: usize = 1024 * 1024;
const MAX_BIN_BYTES: usize = 8 * 1024 * 1024;
const MAX_BIN_ARRAYS: usize = 16;
const MAX_NAME_BYTES: usize = 32;
const MAX_ARRAY_VALUES: usize = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FormatId {
    #[serde(rename = "looklab-look/1")]
    LookV1,
    #[serde(rename = "looklab-look/2")]
    LookV2,
}

macro_rules! string_newtype {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self> {
                let value = value.into();
                if value.is_empty() {
                    return Err(ModelError::Invalid(stringify!($name).to_string()));
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = ModelError;
            fn try_from(value: String) -> Result<Self> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl core::fmt::Display for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_newtype!(Slug);
string_newtype!(LookVersion);

/// A `bin:` reference to a named `look.bin` array.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BinRef(String);

impl BinRef {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        match value.strip_prefix("bin:") {
            Some(name) if !name.is_empty() => Ok(Self(value)),
            _ => Err(ModelError::Invalid(format!("bin reference {value:?}"))),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for BinRef {
    type Error = ModelError;
    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<BinRef> for String {
    fn from(value: BinRef) -> Self {
        value.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LookKind {
    NativeFamily,
    LutDefined,
}

pub const NEUTRAL_BASELINE_SRGB_V1: &str = "looklab-neutral-srgb-v1";

/// Largest stored LUT side.
pub const MAX_LUT_SIDE: u32 = 64;

fn is_false(value: &bool) -> bool {
    !*value
}

/// The submitter's license declaration and the credit shown on the Look page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LicenseRecord {
    #[serde(default, skip_serializing_if = "is_false")]
    pub own_work: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    pub attribution: String,
}

impl LicenseRecord {
    pub fn validate(&self) -> Result<()> {
        match (&self.source_license, self.own_work) {
            (Some(source), false) if !source.trim().is_empty() && source.chars().count() <= 200 => {
            }
            (None, true) => {}
            _ => {
                return Err(ModelError::Invalid(
                    "license needs exactly one of own_work or source_license".to_string(),
                ));
            }
        }
        if self.source_url.is_some() && self.source_license.is_none() {
            return Err(ModelError::Invalid(
                "license source_url without a source_license".to_string(),
            ));
        }
        if self.attribution.trim().is_empty() || self.attribution.chars().count() > 500 {
            return Err(ModelError::Invalid("license attribution".to_string()));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogTransfer {
    #[serde(rename = "flog2")]
    Flog2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gamut {
    Fgamut,
}

pub const NEUTRAL_BASELINE_SLUG: &str = "looklab-neutral-srgb-v1";
pub const NEUTRAL_BASELINE_VERSION: &str = "1.0.0";

/// The Look version or body realization a display-referred LUT expects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BaseReference {
    Look {
        slug: Slug,
        version: LookVersion,
    },
    BodyRealization {
        body: BodyId,
        renderer: Renderer,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        film_simulation: Option<SimKey>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dynamic_range: Option<DynamicRange>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        notes: String,
    },
    /// An intended family rendering with no published Look version yet.
    FamilyRendering {
        maker: String,
        rendering: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dynamic_range: Option<DynamicRange>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        notes: String,
    },
}

impl BaseReference {
    pub fn neutral_baseline() -> Self {
        Self::Look {
            slug: Slug::new(NEUTRAL_BASELINE_SLUG).expect("static neutral baseline slug"),
            version: LookVersion::new(NEUTRAL_BASELINE_VERSION)
                .expect("static neutral baseline version"),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if let Self::FamilyRendering {
            maker, rendering, ..
        } = self
            && (maker.trim().is_empty() || rendering.trim().is_empty())
        {
            return Err(ModelError::Invalid("family rendering base".to_string()));
        }
        Ok(())
    }

    pub fn resolve_status(&self, schema: Option<&looklab_schema::ControlSchema>) -> BaseStatus {
        let Self::BodyRealization {
            body,
            film_simulation,
            dynamic_range,
            ..
        } = self
        else {
            return match self {
                Self::Look { .. } => BaseStatus::Pinned,
                Self::FamilyRendering { .. } => BaseStatus::BaseIdentifiedUnmeasured,
                Self::BodyRealization { .. } => unreachable!(),
            };
        };
        let Some(schema) = schema else {
            return BaseStatus::BaseIncomplete {
                missing: BaseField::Body,
            };
        };
        if schema.body != *body {
            return BaseStatus::BaseInconsistent {
                field: BaseField::Body,
            };
        }
        let Some(sim) = film_simulation else {
            return BaseStatus::BaseIncomplete {
                missing: BaseField::FilmSimulation,
            };
        };
        let Some(dynamic_range) = dynamic_range else {
            return BaseStatus::BaseIncomplete {
                missing: BaseField::DynamicRange,
            };
        };
        let has_sim = schema.epochs.iter().any(|epoch| {
            epoch
                .native_sims
                .iter()
                .any(|declared| declared.key == *sim)
        });
        if !has_sim {
            return BaseStatus::BaseInconsistent {
                field: BaseField::FilmSimulation,
            };
        }
        let dr = dynamic_range.as_str();
        let has_dr = schema.epochs.iter().any(|epoch| {
            epoch.controls.iter().any(|control| {
                control.key.as_str() == "dynamic_range"
                    && control
                        .values
                        .as_ref()
                        .is_some_and(|values| values.iter().any(|value| value.as_str() == dr))
            })
        });
        if !has_dr {
            return BaseStatus::BaseInconsistent {
                field: BaseField::DynamicRange,
            };
        }
        BaseStatus::BaseIdentifiedUnmeasured
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Renderer {
    #[serde(rename = "x_raw_studio")]
    XRawStudio,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DynamicRange {
    Dr100,
    Dr200,
    Dr400,
}

impl DynamicRange {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dr100 => "dr100",
            Self::Dr200 => "dr200",
            Self::Dr400 => "dr400",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseField {
    Body,
    Renderer,
    FilmSimulation,
    DynamicRange,
}

/// Whether a display-referred base is known and fitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BaseStatus {
    Pinned,
    BaseIdentifiedUnmeasured,
    BaseIncomplete { missing: BaseField },
    BaseInconsistent { field: BaseField },
}

/// Why a Look document cannot be matched or baked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchRefusal {
    DisplayBaseUnpinned,
    /// The stored support mask admits no probe.
    Unsupported,
    NotNativeFamily,
}

impl core::fmt::Display for MatchRefusal {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(match self {
            Self::DisplayBaseUnpinned => "the display base is not pinned",
            Self::Unsupported => "no probe is supported",
            Self::NotNativeFamily => "the Look is not native-family",
        })
    }
}

impl BaseStatus {
    fn validate(&self) -> Result<()> {
        Ok(())
    }

    pub fn is_pinned(&self) -> bool {
        matches!(self, Self::Pinned)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeclaredEncoding {
    SceneReferred {
        transfer: LogTransfer,
        gamut: Gamut,
    },
    DisplayReferred {
        base: BaseReference,
        status: BaseStatus,
    },
}

impl DeclaredEncoding {
    pub fn is_scene_referred(&self) -> bool {
        matches!(self, Self::SceneReferred { .. })
    }

    pub fn base(&self) -> Option<&BaseReference> {
        match self {
            Self::SceneReferred { .. } => None,
            Self::DisplayReferred { base, .. } => Some(base),
        }
    }

    pub fn is_matchable(&self) -> bool {
        self.match_refusal().is_none()
    }

    pub fn match_refusal(&self) -> Option<MatchRefusal> {
        match self {
            Self::SceneReferred { .. } => None,
            Self::DisplayReferred { status, .. } => {
                (!status.is_pinned()).then_some(MatchRefusal::DisplayBaseUnpinned)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputEncoding {
    Srgb,
}

/// What a LUT reproduces, when it is not an original grade.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LutEmulation {
    CameraRendering { maker: String, rendering: String },
    FilmStock { name: String },
}

impl LutEmulation {
    fn validate(&self) -> Result<()> {
        let fields: Vec<&str> = match self {
            Self::CameraRendering { maker, rendering } => vec![maker.as_str(), rendering.as_str()],
            Self::FilmStock { name } => vec![name.as_str()],
        };
        if fields
            .iter()
            .any(|value| value.trim().is_empty() || value.chars().count() > 120)
        {
            return Err(ModelError::Invalid("lut emulation".to_string()));
        }
        Ok(())
    }
}

/// The camera rendering a native-family Look reproduces.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraRendering {
    pub maker: String,
    pub name: String,
}

impl CameraRendering {
    fn validate(&self) -> Result<()> {
        for value in [&self.maker, &self.name] {
            if value.trim().is_empty() || value.chars().count() > 120 {
                return Err(ModelError::Invalid("camera rendering".to_string()));
            }
        }
        Ok(())
    }
}

/// A `look slug@version` reference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookPin {
    pub slug: Slug,
    pub version: LookVersion,
}

impl LookPin {
    pub fn new(slug: &str, version: &str) -> Result<Self> {
        Ok(Self {
            slug: Slug::new(slug)?,
            version: LookVersion::new(version)?,
        })
    }
}

impl core::fmt::Display for LookPin {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}@{}", self.slug, self.version)
    }
}

fn normalized_label(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether a native Look's declared camera rendering supersedes this emulation.
pub fn emulation_matches_camera_rendering(
    emulation: &LutEmulation,
    maker: &str,
    rendering: &str,
) -> bool {
    match emulation {
        LutEmulation::CameraRendering {
            maker: declared_maker,
            rendering: declared_rendering,
        } => {
            normalized_label(declared_maker) == normalized_label(maker)
                && normalized_label(declared_rendering) == normalized_label(rendering)
        }
        LutEmulation::FilmStock { .. } => false,
    }
}

/// The stored LUT's grid declaration; values live in `bin:lut`, red fastest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LutRecord {
    pub size: u32,
    pub domain_min: [f64; 3],
    pub domain_max: [f64; 3],
    pub values_ref: BinRef,
}

impl LutRecord {
    pub fn validate(&self) -> Result<()> {
        if !(2..=MAX_LUT_SIDE).contains(&self.size) {
            return Err(ModelError::Invalid("lut size".to_string()));
        }
        for channel in 0..3 {
            let (min, max) = (self.domain_min[channel], self.domain_max[channel]);
            if !(min.is_finite() && max.is_finite() && min < max) {
                return Err(ModelError::Invalid("lut domain".to_string()));
            }
        }
        if self.values_ref.as_str() != "bin:lut" {
            return Err(ModelError::Invalid("lut values_ref".to_string()));
        }
        Ok(())
    }

    pub fn expected_values(&self) -> usize {
        lut_value_count(self.size)
    }
}

/// Stored LUT nodes, red fastest.
#[derive(Clone, Debug, PartialEq)]
pub struct LutData {
    pub size: u32,
    pub domain_min: [f64; 3],
    pub domain_max: [f64; 3],
    pub values: Vec<f64>,
}

pub fn lut_value_count(size: u32) -> usize {
    (size as usize).pow(3) * 3
}

impl LutData {
    pub fn new(
        size: u32,
        domain_min: [f64; 3],
        domain_max: [f64; 3],
        values: Vec<f64>,
    ) -> Result<Self> {
        let data = Self {
            size,
            domain_min,
            domain_max,
            values,
        };
        data.validate()?;
        Ok(data)
    }

    pub fn validate(&self) -> Result<()> {
        LutRecord {
            size: self.size,
            domain_min: self.domain_min,
            domain_max: self.domain_max,
            values_ref: BinRef::new("bin:lut").expect("static bin reference"),
        }
        .validate()?;
        if self.values.len() != lut_value_count(self.size) {
            return Err(ModelError::CoefficientLength {
                name: "lut".to_string(),
                found: self.values.len(),
                expected: lut_value_count(self.size),
            });
        }
        if self.values.iter().any(|value| !value.is_finite()) {
            return Err(ModelError::Invalid("non-finite lut value".to_string()));
        }
        Ok(())
    }

    pub fn record(&self) -> LutRecord {
        LutRecord {
            size: self.size,
            domain_min: self.domain_min,
            domain_max: self.domain_max,
            values_ref: BinRef::new("bin:lut").expect("static bin reference"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LutDefinition {
    pub name: String,
    pub license: LicenseRecord,
    pub encoding: DeclaredEncoding,
    pub output: OutputEncoding,
    pub lut: LutRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emulates: Option<LutEmulation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<LookPin>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl LutDefinition {
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() || self.name.chars().count() > 120 {
            return Err(ModelError::Invalid("lut name".to_string()));
        }
        self.license.validate()?;
        self.lut.validate()?;
        if let Some(emulation) = &self.emulates {
            emulation.validate()?;
        }
        if self.notes.len() > 16
            || self
                .notes
                .iter()
                .any(|note| note.trim().is_empty() || note.chars().count() > 1000)
        {
            return Err(ModelError::Invalid("lut notes".to_string()));
        }
        if let DeclaredEncoding::DisplayReferred { status, base } = &self.encoding {
            base.validate()?;
            status.validate()?;
        }
        Ok(())
    }

    pub fn is_matchable(&self) -> bool {
        self.match_refusal().is_none()
    }

    pub fn match_refusal(&self) -> Option<MatchRefusal> {
        self.encoding.match_refusal()
    }

    pub fn base(&self) -> Option<&BaseReference> {
        self.encoding.base()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WbPolicy {
    Adaptive,
    FixedIlluminant,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookIdentity {
    pub slug: Slug,
    pub version: LookVersion,
    pub kind: LookKind,
    pub wb_policy: WbPolicy,
}

impl LookIdentity {
    pub fn new(slug: &str, version: &str, kind: LookKind, wb_policy: WbPolicy) -> Result<Self> {
        Ok(Self {
            slug: Slug::new(slug)?,
            version: LookVersion::new(version)?,
            kind,
            wb_policy,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WhitePoint {
    #[serde(rename = "D65")]
    D65,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurroundRecord {
    Average,
    Dim,
    Dark,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ViewingConditionsRecord {
    pub white: WhitePoint,
    #[serde(rename = "L_A")]
    pub l_a: f64,
    #[serde(rename = "Y_b")]
    pub y_b: f64,
    pub surround: SurroundRecord,
    pub discount_illuminant: bool,
}

impl ViewingConditionsRecord {
    pub fn reference() -> Self {
        let conditions = ViewingConditions::reference();
        Self {
            white: WhitePoint::D65,
            l_a: conditions.l_a,
            y_b: conditions.y_b,
            surround: SurroundRecord::Average,
            discount_illuminant: true,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.white != WhitePoint::D65
            || self.surround != SurroundRecord::Average
            || !self.discount_illuminant
        {
            return Err(ModelError::ViewingConditions);
        }
        let reference = ViewingConditions::reference();
        if (self.l_a - reference.l_a).abs() > 1e-9 || (self.y_b - reference.y_b).abs() > 1e-9 {
            return Err(ModelError::ViewingConditions);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToneKindRecord {
    Colorimetric,
    Spline,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToneRecord {
    pub kind: ToneKindRecord,
    pub knots: Vec<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coef_ref: Option<BinRef>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TintRecord {
    pub knots: Vec<f64>,
    pub coef_ref: BinRef,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChromaReference {
    #[serde(rename = "colorimetric_at_neutral_matched_luminance")]
    ColorimetricAtNeutralMatchedLuminance,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChromaRecord {
    pub hue_knots: usize,
    pub s_knots: Vec<f64>,
    pub c_knots: usize,
    pub c_max: f64,
    pub core_terms: usize,
    pub core_radius: f64,
    pub coef_ref: BinRef,
    pub reference: ChromaReference,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HueRecord {
    pub coef_ref: BinRef,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LightnessRecord {
    pub coef_ref: BinRef,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResidualDomainRecord {
    pub s: [f64; 2],
    pub a: [f64; 2],
    pub b: [f64; 2],
}

impl ResidualDomainRecord {
    pub fn v1() -> Self {
        Self {
            s: RESIDUAL_DOMAIN[0],
            a: RESIDUAL_DOMAIN[1],
            b: RESIDUAL_DOMAIN[2],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResidualRecord {
    pub grid: [usize; 3],
    pub domain: ResidualDomainRecord,
    pub coef_ref: BinRef,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayersRecord {
    pub tone: ToneRecord,
    pub tint: TintRecord,
    pub chroma: ChromaRecord,
    pub hue: HueRecord,
    pub lightness: LightnessRecord,
    pub residual: ResidualRecord,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeSetVersion {
    #[serde(rename = "v1")]
    V1,
}

impl ProbeSetVersion {
    pub fn probe_count(self) -> usize {
        match self {
            Self::V1 => crate::probes::V1_PROBE_COUNT,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Regularization {
    #[serde(rename = "lambda_T")]
    pub lambda_t: f64,
    #[serde(rename = "lambda_N")]
    pub lambda_n: f64,
    #[serde(rename = "lambda_GRL")]
    pub lambda_grl: f64,
    #[serde(rename = "lambda_res")]
    pub lambda_res: f64,
}

impl Regularization {
    pub fn zeros() -> Self {
        Self {
            lambda_t: 0.0,
            lambda_n: 0.0,
            lambda_grl: 0.0,
            lambda_res: 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UncertaintyMethod {
    Bootstrap,
    Sandwich,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UncertaintyRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_cov_ref: Option<BinRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_cov_post_ref: Option<BinRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_bias_ref: Option<BinRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_sigma_ref: Option<BinRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_sigma_post_ref: Option<BinRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_pre_reliable_ref: Option<BinRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub support_ref: Option<BinRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub density_ref: Option<BinRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<UncertaintyMethod>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BodyProvenance {
    pub body: String,
    pub epoch_map_version: u32,
    pub epochs: Vec<u32>,
    pub input_transform_version: u32,
    pub realization_params_version: u32,
    pub control_response_version: u32,
    pub clip_k: f64,
    pub images: u32,
    pub credentials: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RootProvenance {
    pub admissions: Vec<String>,
    pub anchor_version: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    pub bodies: Vec<BodyProvenance>,
    pub root: RootProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<BaseReference>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub needs_rebase: bool,
    pub community_snapshot_version: u32,
    pub prep_versions: Vec<u32>,
    pub created: String,
    pub software: String,
}

impl Provenance {
    pub fn empty() -> Self {
        Self {
            bodies: Vec::new(),
            root: RootProvenance {
                admissions: Vec::new(),
                anchor_version: String::new(),
            },
            base: None,
            needs_rebase: false,
            community_snapshot_version: 0,
            prep_versions: Vec::new(),
            created: "1970-01-01T00:00:00Z".to_string(),
            software: format!("looklab-model {}", env!("CARGO_PKG_VERSION")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LookFile {
    pub format: FormatId,
    pub look: LookIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<LutDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendering: Option<CameraRendering>,
    pub viewing_conditions: ViewingConditionsRecord,
    pub probe_set: ProbeSetVersion,
    pub layers: LayersRecord,
    pub output_clip: ClipSettings,
    pub regularization: Regularization,
    pub uncertainty: UncertaintyRecord,
    pub provenance: Provenance,
}

impl LookFile {
    pub fn from_look(identity: LookIdentity, look: &Look) -> Result<Self> {
        Ok(Self {
            format: FormatId::LookV2,
            look: identity,
            definition: None,
            rendering: None,
            viewing_conditions: ViewingConditionsRecord::reference(),
            probe_set: ProbeSetVersion::V1,
            layers: LayersRecord::from_look(look),
            output_clip: look.clip,
            regularization: Regularization::zeros(),
            uncertainty: UncertaintyRecord::default(),
            provenance: Provenance::empty(),
        })
    }

    fn validate(&self) -> Result<()> {
        match self.format {
            FormatId::LookV2 => {}
            FormatId::LookV1 => {
                return Err(ModelError::Format(
                    "looklab-look/1 is superseded by looklab-look/2; refit and republish"
                        .to_string(),
                ));
            }
        }
        match (self.look.kind, &self.definition) {
            (LookKind::LutDefined, Some(definition)) => definition.validate()?,
            (LookKind::LutDefined, None) => {
                return Err(ModelError::Invalid(
                    "lut_defined look has no definition".to_string(),
                ));
            }
            (LookKind::NativeFamily, Some(_)) => {
                return Err(ModelError::Invalid(
                    "native_family look has a LUT definition".to_string(),
                ));
            }
            (LookKind::NativeFamily, None) => {}
        }
        match (self.look.kind, &self.rendering) {
            (LookKind::NativeFamily, Some(rendering)) => rendering.validate()?,
            (LookKind::NativeFamily, None) => {}
            (LookKind::LutDefined, Some(_)) => {
                return Err(ModelError::Invalid(
                    "lut_defined look has a camera rendering".to_string(),
                ));
            }
            (LookKind::LutDefined, None) => {}
        }
        self.viewing_conditions.validate()?;
        if self.layers.tone.knots != TONE_KNOTS || self.layers.tint.knots != TINT_KNOTS {
            return Err(ModelError::Knots);
        }
        if self.layers.chroma.hue_knots != HUE_COEFS
            || self.layers.chroma.s_knots != S_KNOTS[4..S_COEFS]
            || self.layers.chroma.c_knots != C_COEFS
            || self.layers.chroma.c_max != C_MAX
            || self.layers.chroma.core_terms != CORE_TERMS
            || self.layers.chroma.core_radius != NEUTRAL_JOIN_C
        {
            return Err(ModelError::Knots);
        }
        if self.layers.residual.grid != [RESIDUAL_GRID; 3]
            || self.layers.residual.domain != ResidualDomainRecord::v1()
        {
            return Err(ModelError::Invalid("residual grid".to_string()));
        }
        if !self.output_clip.validate() {
            return Err(ModelError::Invalid("clip k out of range".to_string()));
        }
        let reference = |value: &BinRef, name: &str| value.as_str() == name;
        match (&self.layers.tone.kind, &self.layers.tone.coef_ref) {
            (ToneKindRecord::Spline, Some(value)) if reference(value, "bin:T") => {}
            (ToneKindRecord::Colorimetric, None) => {}
            _ => return Err(ModelError::Invalid("tone coef_ref".to_string())),
        }
        if !reference(&self.layers.tint.coef_ref, "bin:N")
            || !reference(&self.layers.chroma.coef_ref, "bin:G")
            || !reference(&self.layers.hue.coef_ref, "bin:R")
            || !reference(&self.layers.lightness.coef_ref, "bin:Lambda")
            || !reference(&self.layers.residual.coef_ref, "bin:res")
        {
            return Err(ModelError::Invalid("layer coef_ref".to_string()));
        }
        for (value, name, field) in [
            (
                &self.uncertainty.probe_cov_ref,
                "bin:probe_cov",
                "probe_cov",
            ),
            (
                &self.uncertainty.probe_cov_post_ref,
                "bin:probe_cov_post",
                "probe_cov_post",
            ),
            (
                &self.uncertainty.probe_bias_ref,
                "bin:probe_bias",
                "probe_bias",
            ),
            (
                &self.uncertainty.probe_sigma_ref,
                "bin:probe_sigma",
                "probe_sigma",
            ),
            (
                &self.uncertainty.probe_sigma_post_ref,
                "bin:probe_sigma_post",
                "probe_sigma_post",
            ),
            (
                &self.uncertainty.probe_pre_reliable_ref,
                "bin:probe_pre_reliable",
                "probe_pre_reliable",
            ),
            (&self.uncertainty.support_ref, "bin:support", "support"),
            (&self.uncertainty.density_ref, "bin:kde", "kde"),
        ] {
            if let Some(value) = value
                && !reference(value, name)
            {
                return Err(ModelError::Invalid(format!("{field} ref")));
            }
        }
        Ok(())
    }
}

impl LayersRecord {
    pub fn from_look(look: &Look) -> Self {
        let tone = match &look.tone {
            Tone::Colorimetric => ToneRecord {
                kind: ToneKindRecord::Colorimetric,
                knots: TONE_KNOTS.to_vec(),
                coef_ref: None,
            },
            Tone::Spline { .. } => ToneRecord {
                kind: ToneKindRecord::Spline,
                knots: TONE_KNOTS.to_vec(),
                coef_ref: Some(BinRef::new("bin:T").expect("static bin reference")),
            },
        };
        Self {
            tone,
            tint: TintRecord {
                knots: TINT_KNOTS.to_vec(),
                coef_ref: BinRef::new("bin:N").expect("static bin reference"),
            },
            chroma: ChromaRecord {
                hue_knots: HUE_COEFS,
                s_knots: S_KNOTS[4..S_COEFS].to_vec(),
                c_knots: C_COEFS,
                c_max: C_MAX,
                core_terms: CORE_TERMS,
                core_radius: NEUTRAL_JOIN_C,
                coef_ref: BinRef::new("bin:G").expect("static bin reference"),
                reference: ChromaReference::ColorimetricAtNeutralMatchedLuminance,
            },
            hue: HueRecord {
                coef_ref: BinRef::new("bin:R").expect("static bin reference"),
            },
            lightness: LightnessRecord {
                coef_ref: BinRef::new("bin:Lambda").expect("static bin reference"),
            },
            residual: ResidualRecord {
                grid: [RESIDUAL_GRID; 3],
                domain: ResidualDomainRecord::v1(),
                coef_ref: BinRef::new("bin:res").expect("static bin reference"),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProbeCovariance {
    pub probes: usize,
    pub values: Vec<f64>,
}

impl ProbeCovariance {
    pub fn new(probes: usize, values: Vec<f64>) -> Result<Self> {
        if values.len() != probes.saturating_mul(9) {
            return Err(ModelError::CoefficientLength {
                name: "probe_cov".to_string(),
                found: values.len(),
                expected: probes.saturating_mul(9),
            });
        }
        if values.iter().any(|value| !value.is_finite()) {
            return Err(ModelError::Invalid("non-finite probe_cov".to_string()));
        }
        Ok(Self { probes, values })
    }
}

/// One scalar per probe (bias, support mask).
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeScalars {
    pub values: Vec<f64>,
}

impl ProbeScalars {
    pub fn new(values: Vec<f64>) -> Result<Self> {
        if values.is_empty() || values.iter().any(|value| !value.is_finite()) {
            return Err(ModelError::Invalid("invalid probe scalars".to_string()));
        }
        Ok(Self { values })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Density {
    pub values: Vec<f64>,
}

impl Density {
    pub fn new(values: Vec<f64>) -> Result<Self> {
        if values.is_empty() || values.iter().any(|value| !value.is_finite()) {
            return Err(ModelError::Invalid("invalid kde".to_string()));
        }
        Ok(Self { values })
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UncertaintyData {
    pub probe_cov: Option<ProbeCovariance>,
    pub probe_cov_post: Option<ProbeCovariance>,
    pub probe_bias: Option<ProbeScalars>,
    pub probe_sigma: Option<ProbeScalars>,
    pub probe_sigma_post: Option<ProbeScalars>,
    pub probe_pre_reliable: Option<ProbeScalars>,
    pub support: Option<ProbeScalars>,
    pub density: Option<Density>,
}

impl UncertaintyData {
    fn validate(&self, probes: usize) -> Result<()> {
        for (name, covariance) in [
            ("probe_cov", &self.probe_cov),
            ("probe_cov_post", &self.probe_cov_post),
        ] {
            if let Some(covariance) = covariance
                && covariance.probes != probes
            {
                return Err(ModelError::Invalid(format!("{name} dimensions")));
            }
        }
        for (name, scalars) in [
            ("probe_bias", &self.probe_bias),
            ("probe_sigma", &self.probe_sigma),
            ("probe_sigma_post", &self.probe_sigma_post),
            ("probe_pre_reliable", &self.probe_pre_reliable),
            ("support", &self.support),
        ] {
            if let Some(scalars) = scalars
                && scalars.values.len() != probes
            {
                return Err(ModelError::Invalid(format!("{name} dimensions")));
            }
        }
        if let Some(density) = &self.density
            && density.values.len() != probes
        {
            return Err(ModelError::Invalid("kde dimensions".to_string()));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LookDocument {
    pub file: LookFile,
    pub look: Look,
    pub uncertainty: UncertaintyData,
    pub lut: Option<LutData>,
}

impl LookDocument {
    pub fn new(file: LookFile, look: Look) -> Result<Self> {
        Self {
            file,
            look,
            uncertainty: UncertaintyData::default(),
            lut: None,
        }
        .validated()
    }

    fn validated(self) -> Result<Self> {
        self.validate()?;
        Ok(self)
    }

    pub fn validate(&self) -> Result<()> {
        self.file.validate()?;
        self.look.validate()?;
        self.uncertainty
            .validate(self.file.probe_set.probe_count())?;
        match (&self.file.definition, &self.lut) {
            (Some(definition), Some(lut)) => {
                if lut.record() != definition.lut {
                    return Err(ModelError::Invalid(
                        "stored LUT does not match definition".to_string(),
                    ));
                }
            }
            (Some(_), None) => {
                return Err(ModelError::Invalid(
                    "lut_defined look has no stored LUT".to_string(),
                ));
            }
            (None, Some(_)) => {
                return Err(ModelError::Invalid(
                    "stored LUT without a definition".to_string(),
                ));
            }
            (None, None) => {}
        }
        Ok(())
    }

    pub fn from_look(identity: LookIdentity, look: Look) -> Result<Self> {
        let file = LookFile::from_look(identity, &look)?;
        Self::new(file, look)
    }

    pub fn from_lut_definition(
        identity: LookIdentity,
        look: Look,
        definition: LutDefinition,
        lut: LutData,
    ) -> Result<Self> {
        let mut file = LookFile::from_look(identity, &look)?;
        file.provenance.base = definition.base().cloned();
        file.definition = Some(definition);
        Self {
            file,
            look,
            uncertainty: UncertaintyData::default(),
            lut: Some(lut),
        }
        .validated()
    }

    pub fn with_lut(mut self, lut: LutData) -> Result<Self> {
        self.lut = Some(lut);
        self.validated()
    }

    /// The reason this document cannot be matched or baked.
    pub fn match_refusal(&self) -> Option<MatchRefusal> {
        let encoding = match &self.file.definition {
            Some(definition) => definition.match_refusal(),
            None if self.file.look.kind == LookKind::NativeFamily => None,
            None => Some(MatchRefusal::NotNativeFamily),
        };
        encoding.or_else(|| {
            self.uncertainty
                .support
                .as_ref()
                .filter(|mask| !mask.values.iter().any(|value| *value > 0.5))
                .map(|_| MatchRefusal::Unsupported)
        })
    }

    pub fn is_matchable(&self) -> bool {
        self.match_refusal().is_none()
    }

    pub fn base(&self) -> Option<&BaseReference> {
        self.file.definition.as_ref().and_then(LutDefinition::base)
    }

    pub fn with_uncertainty(mut self, uncertainty: UncertaintyData) -> Result<Self> {
        uncertainty.validate(self.file.probe_set.probe_count())?;
        self.file.uncertainty.probe_cov_ref = uncertainty
            .probe_cov
            .as_ref()
            .map(|_| BinRef::new("bin:probe_cov").expect("static bin reference"));
        self.file.uncertainty.probe_cov_post_ref = uncertainty
            .probe_cov_post
            .as_ref()
            .map(|_| BinRef::new("bin:probe_cov_post").expect("static bin reference"));
        self.file.uncertainty.probe_bias_ref = uncertainty
            .probe_bias
            .as_ref()
            .map(|_| BinRef::new("bin:probe_bias").expect("static bin reference"));
        self.file.uncertainty.probe_sigma_ref = uncertainty
            .probe_sigma
            .as_ref()
            .map(|_| BinRef::new("bin:probe_sigma").expect("static bin reference"));
        self.file.uncertainty.probe_sigma_post_ref = uncertainty
            .probe_sigma_post
            .as_ref()
            .map(|_| BinRef::new("bin:probe_sigma_post").expect("static bin reference"));
        self.file.uncertainty.probe_pre_reliable_ref = uncertainty
            .probe_pre_reliable
            .as_ref()
            .map(|_| BinRef::new("bin:probe_pre_reliable").expect("static bin reference"));
        self.file.uncertainty.support_ref = uncertainty
            .support
            .as_ref()
            .map(|_| BinRef::new("bin:support").expect("static bin reference"));
        self.file.uncertainty.density_ref = uncertainty
            .density
            .as_ref()
            .map(|_| BinRef::new("bin:kde").expect("static bin reference"));
        self.uncertainty = uncertainty;
        Ok(self)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        if self.file.uncertainty.probe_cov_ref.is_some() != self.uncertainty.probe_cov.is_some()
            || self.file.uncertainty.probe_cov_post_ref.is_some()
                != self.uncertainty.probe_cov_post.is_some()
            || self.file.uncertainty.probe_bias_ref.is_some()
                != self.uncertainty.probe_bias.is_some()
            || self.file.uncertainty.probe_sigma_ref.is_some()
                != self.uncertainty.probe_sigma.is_some()
            || self.file.uncertainty.probe_sigma_post_ref.is_some()
                != self.uncertainty.probe_sigma_post.is_some()
            || self.file.uncertainty.probe_pre_reliable_ref.is_some()
                != self.uncertainty.probe_pre_reliable.is_some()
            || self.file.uncertainty.support_ref.is_some() != self.uncertainty.support.is_some()
            || self.file.uncertainty.density_ref.is_some() != self.uncertainty.density.is_some()
        {
            return Err(ModelError::Invalid("uncertainty refs".to_string()));
        }
        let json = serde_json::to_vec_pretty(&self.file)?;
        let bin = encode_bin(&self.look, &self.uncertainty, self.lut.as_ref())?;

        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .last_modified_time(zip::DateTime::default())
            .unix_permissions(0o644);
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer.start_file(JSON_NAME, options)?;
        writer.write_all(&json)?;
        writer.start_file(BIN_NAME, options)?;
        writer.write_all(&bin)?;
        let cursor = writer.finish()?;
        Ok(cursor.into_inner())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_ARCHIVE_BYTES {
            return Err(ModelError::Invalid("looklab archive too large".to_string()));
        }
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
        let (json_size, bin_size) = {
            let json_entry = archive.by_name(JSON_NAME)?;
            let json_size = json_entry.size();
            drop(json_entry);
            let bin_entry = archive.by_name(BIN_NAME)?;
            let bin_size = bin_entry.size();
            (json_size, bin_size)
        };
        if json_size > MAX_JSON_BYTES as u64 || bin_size > MAX_BIN_BYTES as u64 {
            return Err(ModelError::Invalid("looklab entry too large".to_string()));
        }
        let mut json = Vec::new();
        archive.by_name(JSON_NAME)?.read_to_end(&mut json)?;
        let mut bin = Vec::new();
        archive.by_name(BIN_NAME)?.read_to_end(&mut bin)?;
        let file: LookFile = serde_json::from_slice(&json)?;
        file.validate()?;
        let mut arrays = decode_bin(&bin)?;
        let lut = match &file.definition {
            Some(definition) => {
                let values = arrays
                    .remove(&BinArray::Lut)
                    .ok_or_else(|| ModelError::MissingCoefficient("lut".to_string()))?;
                Some(LutData::new(
                    definition.lut.size,
                    definition.lut.domain_min,
                    definition.lut.domain_max,
                    values,
                )?)
            }
            None => {
                if arrays.contains_key(&BinArray::Lut) {
                    return Err(ModelError::UnknownCoefficient("lut".to_string()));
                }
                None
            }
        };
        let uncertainty = uncertainty_from_arrays(&file, &mut arrays)?;
        let look = look_from_arrays(&file, arrays)?;
        Self {
            file,
            look,
            uncertainty,
            lut,
        }
        .validated()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum BinArray {
    Tone,
    Tint,
    Chroma,
    Hue,
    Lightness,
    Residual,
    ProbeCovariance,
    ProbeCovariancePost,
    ProbeBias,
    ProbeSigma,
    ProbeSigmaPost,
    ProbePreReliable,
    Support,
    Density,
    Lut,
}

impl BinArray {
    fn name(self) -> &'static str {
        match self {
            Self::Tone => "T",
            Self::Tint => "N",
            Self::Chroma => "G",
            Self::Hue => "R",
            Self::Lightness => "Lambda",
            Self::Residual => "res",
            Self::ProbeCovariance => "probe_cov",
            Self::ProbeCovariancePost => "probe_cov_post",
            Self::ProbeBias => "probe_bias",
            Self::ProbeSigma => "probe_sigma",
            Self::ProbeSigmaPost => "probe_sigma_post",
            Self::ProbePreReliable => "probe_pre_reliable",
            Self::Support => "support",
            Self::Density => "kde",
            Self::Lut => "lut",
        }
    }

    fn expected_len(self) -> Option<usize> {
        Some(match self {
            Self::Tone => 1 + TONE_COEFS,
            Self::Tint => 2 * TINT_COEFS,
            Self::Chroma | Self::Hue | Self::Lightness => GRL_COEFS,
            Self::Residual => RESIDUAL_COEFS,
            Self::ProbeCovariance
            | Self::ProbeCovariancePost
            | Self::ProbeBias
            | Self::ProbeSigma
            | Self::ProbeSigmaPost
            | Self::ProbePreReliable
            | Self::Support
            | Self::Density
            | Self::Lut => return None,
        })
    }

    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "T" => Self::Tone,
            "N" => Self::Tint,
            "G" => Self::Chroma,
            "R" => Self::Hue,
            "Lambda" => Self::Lightness,
            "res" => Self::Residual,
            "probe_cov" => Self::ProbeCovariance,
            "probe_cov_post" => Self::ProbeCovariancePost,
            "probe_bias" => Self::ProbeBias,
            "probe_sigma" => Self::ProbeSigma,
            "probe_sigma_post" => Self::ProbeSigmaPost,
            "probe_pre_reliable" => Self::ProbePreReliable,
            "support" => Self::Support,
            "kde" => Self::Density,
            "lut" => Self::Lut,
            _ => return None,
        })
    }
}

fn encode_bin(
    look: &Look,
    uncertainty: &UncertaintyData,
    lut: Option<&LutData>,
) -> Result<Vec<u8>> {
    let mut arrays = look_into_arrays(look);
    if let Some(data) = lut {
        arrays.insert(BinArray::Lut, data.values.clone());
    }
    if let Some(covariance) = &uncertainty.probe_cov {
        arrays.insert(BinArray::ProbeCovariance, covariance.values.clone());
    }
    if let Some(covariance) = &uncertainty.probe_cov_post {
        arrays.insert(BinArray::ProbeCovariancePost, covariance.values.clone());
    }
    if let Some(scalars) = &uncertainty.probe_bias {
        arrays.insert(BinArray::ProbeBias, scalars.values.clone());
    }
    if let Some(scalars) = &uncertainty.probe_sigma {
        arrays.insert(BinArray::ProbeSigma, scalars.values.clone());
    }
    if let Some(scalars) = &uncertainty.probe_sigma_post {
        arrays.insert(BinArray::ProbeSigmaPost, scalars.values.clone());
    }
    if let Some(scalars) = &uncertainty.probe_pre_reliable {
        arrays.insert(BinArray::ProbePreReliable, scalars.values.clone());
    }
    if let Some(scalars) = &uncertainty.support {
        arrays.insert(BinArray::Support, scalars.values.clone());
    }
    if let Some(density) = &uncertainty.density {
        arrays.insert(BinArray::Density, density.values.clone());
    }
    if arrays.len() > MAX_BIN_ARRAYS {
        return Err(ModelError::Invalid("look.bin array count".to_string()));
    }
    let mut out = Vec::new();
    out.extend_from_slice(BIN_MAGIC);
    out.extend_from_slice(&(arrays.len() as u32).to_le_bytes());
    for (array, values) in arrays {
        let name = array.name();
        let length = u8::try_from(name.len())
            .map_err(|_| ModelError::Invalid("coefficient name too long".to_string()))?;
        out.push(length);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(values.len() as u32).to_le_bytes());
        for value in values {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    Ok(out)
}

fn look_into_arrays(look: &Look) -> BTreeMap<BinArray, Vec<f64>> {
    let mut arrays = BTreeMap::new();
    if let Tone::Spline { c0, beta } = &look.tone {
        let mut tone = Vec::with_capacity(1 + TONE_COEFS);
        tone.push(*c0);
        tone.extend_from_slice(beta);
        arrays.insert(BinArray::Tone, tone);
    }
    let mut tint = Vec::with_capacity(2 * TINT_COEFS);
    tint.extend_from_slice(&look.tint.a);
    tint.extend_from_slice(&look.tint.b);
    arrays.insert(BinArray::Tint, tint);
    arrays.insert(BinArray::Chroma, grl_coefficients(&look.color.g));
    arrays.insert(BinArray::Hue, grl_coefficients(&look.color.r));
    arrays.insert(BinArray::Lightness, grl_coefficients(&look.color.lambda));
    arrays.insert(BinArray::Residual, look.residual.values.clone());
    arrays
}

fn grl_coefficients(component: &GrlComponent) -> Vec<f64> {
    let mut out = Vec::with_capacity(GRL_COEFS);
    out.extend_from_slice(&component.plane);
    out.extend_from_slice(&component.volume);
    out.extend_from_slice(&component.core);
    out
}

fn decode_bin(bytes: &[u8]) -> Result<BTreeMap<BinArray, Vec<f64>>> {
    if bytes.len() < 8 || &bytes[..4] != BIN_MAGIC {
        return Err(ModelError::Invalid("look.bin magic".to_string()));
    }
    if bytes.len() > MAX_BIN_BYTES {
        return Err(ModelError::Invalid("look.bin too large".to_string()));
    }
    let count = u32::from_le_bytes(
        bytes[4..8]
            .try_into()
            .map_err(|_| ModelError::Invalid("look.bin count".to_string()))?,
    ) as usize;
    if count > MAX_BIN_ARRAYS {
        return Err(ModelError::Invalid("look.bin array count".to_string()));
    }
    let mut cursor = 8;
    let mut arrays = BTreeMap::new();
    for _ in 0..count {
        let name_length = *bytes
            .get(cursor)
            .ok_or_else(|| ModelError::Invalid("look.bin name".to_string()))?
            as usize;
        if name_length == 0 || name_length > MAX_NAME_BYTES {
            return Err(ModelError::Invalid("look.bin name length".to_string()));
        }
        cursor += 1;
        let name_bytes = bytes
            .get(cursor..cursor + name_length)
            .ok_or_else(|| ModelError::Invalid("look.bin name".to_string()))?;
        let name = core::str::from_utf8(name_bytes)
            .map_err(|_| ModelError::Invalid("look.bin name encoding".to_string()))?;
        let array = BinArray::from_name(name)
            .ok_or_else(|| ModelError::UnknownCoefficient(name.to_string()))?;
        cursor += name_length;
        let length_bytes = bytes
            .get(cursor..cursor + 4)
            .ok_or_else(|| ModelError::Invalid("look.bin length".to_string()))?;
        let length = u32::from_le_bytes(
            length_bytes
                .try_into()
                .map_err(|_| ModelError::Invalid("look.bin length".to_string()))?,
        ) as usize;
        cursor += 4;
        if length > MAX_ARRAY_VALUES {
            return Err(ModelError::Invalid("look.bin array length".to_string()));
        }
        if let Some(expected) = array.expected_len()
            && length != expected
        {
            return Err(ModelError::CoefficientLength {
                name: name.to_string(),
                found: length,
                expected,
            });
        }
        let data_length = length
            .checked_mul(8)
            .ok_or_else(|| ModelError::Invalid("look.bin length".to_string()))?;
        let data = bytes
            .get(cursor..cursor + data_length)
            .ok_or_else(|| ModelError::Invalid("look.bin data".to_string()))?;
        cursor += data_length;
        let (chunks, remainder) = data.as_chunks::<8>();
        if !remainder.is_empty() {
            return Err(ModelError::Invalid("look.bin float".to_string()));
        }
        let mut values = Vec::with_capacity(length);
        for chunk in chunks {
            let value = f64::from_le_bytes(*chunk);
            if !value.is_finite() {
                return Err(ModelError::Invalid("non-finite coefficient".to_string()));
            }
            values.push(value);
        }
        if arrays.insert(array, values).is_some() {
            return Err(ModelError::UnknownCoefficient(name.to_string()));
        }
    }
    if cursor != bytes.len() {
        return Err(ModelError::Invalid("look.bin trailing data".to_string()));
    }
    Ok(arrays)
}

fn covariance_from_arrays(
    name: &str,
    present: bool,
    values: Option<Vec<f64>>,
) -> Result<Option<ProbeCovariance>> {
    match (present, values) {
        (false, None) => Ok(None),
        (false, Some(_)) => Err(ModelError::UnknownCoefficient(name.to_string())),
        (true, None) => Err(ModelError::MissingCoefficient(name.to_string())),
        (true, Some(values)) => {
            if values.len() % 9 != 0 {
                return Err(ModelError::Invalid(format!("{name} dimensions")));
            }
            Ok(Some(ProbeCovariance::new(values.len() / 9, values)?))
        }
    }
}

fn scalars_from_arrays(
    name: &str,
    present: bool,
    values: Option<Vec<f64>>,
) -> Result<Option<ProbeScalars>> {
    match (present, values) {
        (false, None) => Ok(None),
        (false, Some(_)) => Err(ModelError::UnknownCoefficient(name.to_string())),
        (true, None) => Err(ModelError::MissingCoefficient(name.to_string())),
        (true, Some(values)) => Ok(Some(ProbeScalars::new(values)?)),
    }
}

fn uncertainty_from_arrays(
    file: &LookFile,
    arrays: &mut BTreeMap<BinArray, Vec<f64>>,
) -> Result<UncertaintyData> {
    let probe_cov = match (
        file.uncertainty.probe_cov_ref.is_some(),
        arrays.remove(&BinArray::ProbeCovariance),
    ) {
        (false, None) => None,
        (false, Some(_)) => {
            return Err(ModelError::UnknownCoefficient("probe_cov".to_string()));
        }
        (true, None) => {
            return Err(ModelError::MissingCoefficient("probe_cov".to_string()));
        }
        (true, Some(values)) => {
            if values.len() % 9 != 0 {
                return Err(ModelError::Invalid("probe_cov dimensions".to_string()));
            }
            Some(ProbeCovariance::new(values.len() / 9, values)?)
        }
    };
    let probe_cov_post = covariance_from_arrays(
        "probe_cov_post",
        file.uncertainty.probe_cov_post_ref.is_some(),
        arrays.remove(&BinArray::ProbeCovariancePost),
    )?;
    let probe_bias = scalars_from_arrays(
        "probe_bias",
        file.uncertainty.probe_bias_ref.is_some(),
        arrays.remove(&BinArray::ProbeBias),
    )?;
    let probe_sigma = scalars_from_arrays(
        "probe_sigma",
        file.uncertainty.probe_sigma_ref.is_some(),
        arrays.remove(&BinArray::ProbeSigma),
    )?;
    let probe_sigma_post = scalars_from_arrays(
        "probe_sigma_post",
        file.uncertainty.probe_sigma_post_ref.is_some(),
        arrays.remove(&BinArray::ProbeSigmaPost),
    )?;
    let probe_pre_reliable = scalars_from_arrays(
        "probe_pre_reliable",
        file.uncertainty.probe_pre_reliable_ref.is_some(),
        arrays.remove(&BinArray::ProbePreReliable),
    )?;
    let support = scalars_from_arrays(
        "support",
        file.uncertainty.support_ref.is_some(),
        arrays.remove(&BinArray::Support),
    )?;
    let density = match (
        file.uncertainty.density_ref.is_some(),
        arrays.remove(&BinArray::Density),
    ) {
        (false, None) => None,
        (false, Some(_)) => return Err(ModelError::UnknownCoefficient("kde".to_string())),
        (true, None) => return Err(ModelError::MissingCoefficient("kde".to_string())),
        (true, Some(values)) => Some(Density::new(values)?),
    };
    let uncertainty = UncertaintyData {
        probe_cov,
        probe_cov_post,
        probe_bias,
        probe_sigma,
        probe_sigma_post,
        probe_pre_reliable,
        support,
        density,
    };
    uncertainty.validate(file.probe_set.probe_count())?;
    Ok(uncertainty)
}

fn look_from_arrays(file: &LookFile, mut arrays: BTreeMap<BinArray, Vec<f64>>) -> Result<Look> {
    let take = |arrays: &mut BTreeMap<BinArray, Vec<f64>>, array: BinArray, expected: usize| {
        let values = arrays
            .remove(&array)
            .ok_or_else(|| ModelError::MissingCoefficient(array.name().to_string()))?;
        if values.len() != expected {
            return Err(ModelError::CoefficientLength {
                name: array.name().to_string(),
                found: values.len(),
                expected,
            });
        }
        Ok(values)
    };

    let tone = match file.layers.tone.kind {
        ToneKindRecord::Colorimetric => {
            if arrays.contains_key(&BinArray::Tone) {
                return Err(ModelError::UnknownCoefficient("T".to_string()));
            }
            Tone::Colorimetric
        }
        ToneKindRecord::Spline => {
            let values = take(&mut arrays, BinArray::Tone, 1 + TONE_COEFS)?;
            let mut beta = [0.0_f64; TONE_COEFS];
            beta.copy_from_slice(&values[1..]);
            Tone::spline(values[0], beta)?
        }
    };

    let tint_values = take(&mut arrays, BinArray::Tint, 2 * TINT_COEFS)?;
    let tint = Tint {
        a: tint_values[..TINT_COEFS].to_vec(),
        b: tint_values[TINT_COEFS..].to_vec(),
    };
    let mut color = ColorLayer::zeros();
    for (component, array) in [
        (&mut color.g, BinArray::Chroma),
        (&mut color.r, BinArray::Hue),
        (&mut color.lambda, BinArray::Lightness),
    ] {
        let values = take(&mut arrays, array, GRL_COEFS)?;
        component.plane.copy_from_slice(&values[..S_COEFS]);
        component
            .volume
            .copy_from_slice(&values[S_COEFS..S_COEFS + VOLUME_COEFS]);
        component
            .core
            .copy_from_slice(&values[S_COEFS + VOLUME_COEFS..]);
    }
    let residual = Residual {
        values: take(&mut arrays, BinArray::Residual, RESIDUAL_COEFS)?,
    };
    if let Some(array) = arrays.keys().next() {
        return Err(ModelError::UnknownCoefficient(array.name().to_string()));
    }

    Look::new(tone, tint, color, residual, file.output_clip)
}

pub fn identity(slug: &str, version: &str) -> Result<LookIdentity> {
    LookIdentity::new(slug, version, LookKind::NativeFamily, WbPolicy::Adaptive)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bin_bytes(arrays: &[(&str, &[f64])]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(BIN_MAGIC);
        out.extend_from_slice(&(arrays.len() as u32).to_le_bytes());
        for (name, values) in arrays {
            out.push(name.len() as u8);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&(values.len() as u32).to_le_bytes());
            for value in *values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        out
    }

    #[test]
    fn rejects_non_finite_coefficients() {
        let bytes = bin_bytes(&[("N", &[f64::NAN; 2 * TINT_COEFS])]);
        assert!(decode_bin(&bytes).is_err());
    }

    #[test]
    fn rejects_wrong_array_length_before_allocating() {
        let bytes = bin_bytes(&[("N", &[0.0; 2 * TINT_COEFS - 1])]);
        assert!(decode_bin(&bytes).is_err());
    }

    #[test]
    fn rejects_oversized_headers() {
        let mut too_many = Vec::from(*BIN_MAGIC);
        too_many.extend_from_slice(&((MAX_BIN_ARRAYS + 1) as u32).to_le_bytes());
        assert!(decode_bin(&too_many).is_err());

        let mut long_name = Vec::from(*BIN_MAGIC);
        long_name.extend_from_slice(&1_u32.to_le_bytes());
        long_name.push((MAX_NAME_BYTES + 1) as u8);
        assert!(decode_bin(&long_name).is_err());

        let mut huge = vec![0_u8; MAX_BIN_BYTES + 1];
        huge[..4].copy_from_slice(BIN_MAGIC);
        assert!(decode_bin(&huge).is_err());
    }
}
