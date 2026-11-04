// SPDX-License-Identifier: AGPL-3.0-or-later
//! `look.json` + `look.bin` in a `.looklab` zip (docs/03 §3). `look.bin` is
//! `b"LKBN"`, a u32 array count, then name/len/little-endian-f64 arrays by name.

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};

use looklab_color::cam16::ViewingConditions;
use serde::{Deserialize, Serialize};
use zip::write::SimpleFileOptions;

use crate::basis::{
    C_COEFS, C_MAX, HUE_COEFS, RESIDUAL_COEFS, RESIDUAL_GRID, S_COEFS, S_KNOTS, TINT_COEFS,
    TINT_KNOTS, TONE_COEFS, TONE_KNOTS,
};
use crate::clip::ClipSettings;
use crate::error::{ModelError, Result};
use crate::look::{ColorLayer, GRL_COEFS, GrlComponent, Look, RESIDUAL_DOMAIN, Residual, Tint};
use crate::tone::Tone;

pub const FORMAT_ID: &str = "looklab-look/1";

const BIN_MAGIC: &[u8; 4] = b"LKBN";
const JSON_NAME: &str = "look.json";
const BIN_NAME: &str = "look.bin";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FormatId {
    #[serde(rename = "looklab-look/1")]
    LookV1,
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
    pub l_a: f64,
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
    #[serde(rename = "colorimetric_at_output_lightness")]
    ColorimetricAtOutputLightness,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChromaRecord {
    pub hue_knots: usize,
    pub s_knots: Vec<f64>,
    pub c_knots: usize,
    pub c_max: f64,
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
            format: FormatId::LookV1,
            look: identity,
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
        if self.format != FormatId::LookV1 {
            return Err(ModelError::Format(format!("{:?}", self.format)));
        }
        self.viewing_conditions.validate()?;
        if self.layers.tone.knots != TONE_KNOTS || self.layers.tint.knots != TINT_KNOTS {
            return Err(ModelError::Knots);
        }
        if self.layers.chroma.hue_knots != HUE_COEFS
            || self.layers.chroma.s_knots != S_KNOTS[4..S_COEFS]
            || self.layers.chroma.c_knots != C_COEFS
            || self.layers.chroma.c_max != C_MAX
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
                coef_ref: BinRef::new("bin:G").expect("static bin reference"),
                reference: ChromaReference::ColorimetricAtOutputLightness,
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
pub struct LookDocument {
    pub file: LookFile,
    pub look: Look,
}

impl LookDocument {
    pub fn new(file: LookFile, look: Look) -> Result<Self> {
        file.validate()?;
        Ok(Self { file, look })
    }

    pub fn from_look(identity: LookIdentity, look: Look) -> Result<Self> {
        let file = LookFile::from_look(identity, &look)?;
        Self::new(file, look)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.file.validate()?;
        let json = serde_json::to_vec_pretty(&self.file)?;
        let bin = encode_bin(&self.look)?;

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
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
        let mut json = Vec::new();
        archive.by_name(JSON_NAME)?.read_to_end(&mut json)?;
        let mut bin = Vec::new();
        archive.by_name(BIN_NAME)?.read_to_end(&mut bin)?;
        let file: LookFile = serde_json::from_slice(&json)?;
        file.validate()?;
        let arrays = decode_bin(&bin)?;
        let look = look_from_arrays(&file, arrays)?;
        Ok(Self { file, look })
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
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "T" => Self::Tone,
            "N" => Self::Tint,
            "G" => Self::Chroma,
            "R" => Self::Hue,
            "Lambda" => Self::Lightness,
            "res" => Self::Residual,
            _ => return None,
        })
    }
}

fn encode_bin(look: &Look) -> Result<Vec<u8>> {
    let arrays = look_into_arrays(look);
    let mut out = Vec::new();
    out.extend_from_slice(BIN_MAGIC);
    out.extend_from_slice(&(arrays.len() as u32).to_le_bytes());
    for (name, values) in arrays {
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

fn look_into_arrays(look: &Look) -> Vec<(&'static str, Vec<f64>)> {
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
        .into_iter()
        .map(|(array, values)| (array.name(), values))
        .collect()
}

fn grl_coefficients(component: &GrlComponent) -> Vec<f64> {
    let mut out = Vec::with_capacity(GRL_COEFS);
    out.extend_from_slice(&component.plane);
    out.extend_from_slice(&component.volume);
    out
}

fn decode_bin(bytes: &[u8]) -> Result<BTreeMap<BinArray, Vec<f64>>> {
    if bytes.len() < 8 || &bytes[..4] != BIN_MAGIC {
        return Err(ModelError::Invalid("look.bin magic".to_string()));
    }
    let count = u32::from_le_bytes(
        bytes[4..8]
            .try_into()
            .map_err(|_| ModelError::Invalid("look.bin count".to_string()))?,
    );
    let mut cursor = 8;
    let mut arrays = BTreeMap::new();
    for _ in 0..count {
        let name_length = *bytes
            .get(cursor)
            .ok_or_else(|| ModelError::Invalid("look.bin name".to_string()))?
            as usize;
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
            values.push(f64::from_le_bytes(*chunk));
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
        component.volume.copy_from_slice(&values[S_COEFS..]);
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
