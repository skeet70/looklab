// SPDX-License-Identifier: AGPL-3.0-or-later
//! Control definitions shared by the schema validator and consumers.

use core::fmt;
use core::str::FromStr;

use serde::Deserialize;
use serde::de::value::SeqAccessDeserializer;
use serde::de::{self, Visitor};

use crate::ids::{ControlKey, EnumValue};
use crate::rational::Rational;

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
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

fn empty_steps() -> core::ops::RangeInclusive<i64> {
    core::ops::RangeInclusive::new(0, -1)
}

impl Control {
    /// Empty for non-numeric controls.
    pub fn steps(&self) -> core::ops::RangeInclusive<i64> {
        let Some(range) = self.range.as_ref() else {
            return empty_steps();
        };
        let Some(step) = self.step.as_ref() else {
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

    pub fn value_at(&self, step_index: i64) -> Option<Rational> {
        self.step.as_ref()?.checked_mul_i64(step_index)
    }
}
