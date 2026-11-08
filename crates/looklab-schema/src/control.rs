// SPDX-License-Identifier: AGPL-3.0-or-later
//! Control definitions shared by the schema validator and consumers.

use core::fmt;
use core::str::FromStr;

use serde::Deserialize;
use serde::Serialize;
use serde::de::value::SeqAccessDeserializer;
use serde::de::{self, Visitor};

use crate::ids::{ControlKey, EnumValue, LookId};
use crate::makernote::ControlMakernote;
use crate::rational::Rational;

/// How a control's value reached a batch's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingSource {
    /// Decoded from the file's makernote through the control's schema mapping.
    Reproduced,
    /// Entered by the contributor because the snapshot maps no tag for the
    /// control, or the file's tag was unreadable.
    Declared,
}

impl SettingSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reproduced => "reproduced",
            Self::Declared => "declared",
        }
    }
}

impl fmt::Display for SettingSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlType {
    Int,
    Decimal,
    Enum,
    Bool,
}

impl ControlType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Int => "int",
            Self::Decimal => "decimal",
            Self::Enum => "enum",
            Self::Bool => "bool",
        }
    }
}

impl fmt::Display for ControlType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What moving a control changes in the color pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Affects {
    Tone,
    Tint,
    Chroma,
    Hue,
    Lightness,
    Wb,
    Exposure,
}

impl Affects {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tone => "tone",
            Self::Tint => "tint",
            Self::Chroma => "chroma",
            Self::Hue => "hue",
            Self::Lightness => "lightness",
            Self::Wb => "wb",
            Self::Exposure => "exposure",
        }
    }
}

impl FromStr for Affects {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "tone" => Ok(Self::Tone),
            "tint" => Ok(Self::Tint),
            "chroma" => Ok(Self::Chroma),
            "hue" => Ok(Self::Hue),
            "lightness" => Ok(Self::Lightness),
            "wb" => Ok(Self::Wb),
            "exposure" => Ok(Self::Exposure),
            _ => Err(()),
        }
    }
}

impl fmt::Display for Affects {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "(Rational, Rational)")]
pub struct Range {
    pub min: Rational,
    pub max: Rational,
}

impl From<(Rational, Rational)> for Range {
    fn from((min, max): (Rational, Rational)) -> Self {
        Self { min, max }
    }
}

/// A YAML scalar classified from its written text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScalarValue {
    Bool(bool),
    Number(Rational),
    Text(String),
}

impl ScalarValue {
    pub(crate) fn from_raw(value: &str) -> Self {
        match value {
            "true" | "True" | "TRUE" => Self::Bool(true),
            "false" | "False" | "FALSE" => Self::Bool(false),
            _ => Rational::parse(value)
                .map(Self::Number)
                .unwrap_or_else(|_| Self::Text(value.to_owned())),
        }
    }
}

impl<'de> Deserialize<'de> for ScalarValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|raw| Self::from_raw(&raw))
    }
}

/// Values of `spatial` controls that an image may carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tolerated {
    Any,
    Values(Vec<ScalarValue>),
}

/// The render families a control applies to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ControlScope {
    #[default]
    All,
    Families(Vec<LookId>),
}

impl ControlScope {
    pub fn includes(&self, family: &LookId) -> bool {
        match self {
            Self::All => true,
            Self::Families(families) => families.iter().any(|candidate| candidate == family),
        }
    }
}

impl<'de> Deserialize<'de> for ControlScope {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ControlScopeVisitor)
    }
}

struct ControlScopeVisitor;

impl<'de> Visitor<'de> for ControlScopeVisitor {
    type Value = ControlScope;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("`all` or a list of look families")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        if value == "all" {
            Ok(ControlScope::All)
        } else {
            Err(E::custom("expected `all` or a list of look families"))
        }
    }

    fn visit_seq<A: de::SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
        Vec::<LookId>::deserialize(SeqAccessDeserializer::new(seq)).map(ControlScope::Families)
    }
}

impl<'de> Deserialize<'de> for Tolerated {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ToleratedVisitor)
    }
}

struct ToleratedVisitor;

impl<'de> Visitor<'de> for ToleratedVisitor {
    type Value = Tolerated;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("`any` or a list of allowed values")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        if value == "any" {
            Ok(Tolerated::Any)
        } else {
            Err(E::custom("expected `any` or a list of allowed values"))
        }
    }

    fn visit_seq<A: de::SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
        Vec::<ScalarValue>::deserialize(SeqAccessDeserializer::new(seq)).map(Tolerated::Values)
    }
}

/// Placeholder ISO constraints for enum values.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Constraints {
    #[serde(default)]
    pub min_iso: std::collections::BTreeMap<EnumValue, u32>,
}

/// The value a control has in the reference setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlDefault {
    Number(Rational),
    Enum(EnumValue),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Control {
    pub key: ControlKey,
    pub kind: ControlType,
    pub range: Option<Range>,
    pub step: Option<Rational>,
    pub values: Option<Vec<EnumValue>>,
    pub default: ControlDefault,
    pub look_affecting: bool,
    pub affects: Vec<Affects>,
    pub spatial: bool,
    pub tolerated: Option<Tolerated>,
    pub constraints: Option<Constraints>,
    pub makernote: Option<ControlMakernote>,
    pub scope: ControlScope,
}

fn empty_steps() -> core::ops::RangeInclusive<i64> {
    core::ops::RangeInclusive::new(0, -1)
}

pub(crate) fn numeric_steps(
    range: Option<&Range>,
    step: Option<&Rational>,
) -> core::ops::RangeInclusive<i64> {
    let Some(range) = range else {
        return empty_steps();
    };
    let Some(step) = step else {
        return empty_steps();
    };
    if step.is_zero() {
        return empty_steps();
    }
    let Some(lo) = range.min.div_exact(step) else {
        return empty_steps();
    };
    let Some(hi) = range.max.div_exact(step) else {
        return empty_steps();
    };
    lo..=hi
}

pub(crate) fn step_value(step: Option<&Rational>, index: i64) -> Option<Rational> {
    step?.checked_mul_i64(index)
}

impl Control {
    /// Empty for non-numeric controls.
    pub fn steps(&self) -> core::ops::RangeInclusive<i64> {
        numeric_steps(self.range.as_ref(), self.step.as_ref())
    }

    pub fn value_at(&self, step_index: i64) -> Option<Rational> {
        step_value(self.step.as_ref(), step_index)
    }
}
