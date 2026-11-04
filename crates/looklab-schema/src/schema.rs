// SPDX-License-Identifier: AGPL-3.0-or-later
//! The body control-schema file.

use core::cmp::Ordering;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::str::FromStr;
use std::collections::HashSet;

use serde::Deserialize;
use serde::de::{self, Visitor};

use crate::control::{
    Affects, Constraints, Control, ControlDefault, ControlType, Range, ScalarValue, Tolerated,
};
use crate::error::{ErrorKind, ValidationError};
use crate::ids::{
    BodyId, ControlKey, EnumValue, ExposureModeKey, LookId, MakernoteTag, SimKey, StageName,
};
use crate::rational::Rational;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlSchema {
    pub body: BodyId,
    pub epochs: Vec<Epoch>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Epoch {
    pub id: u32,
    pub firmware: FirmwareRange,
    pub native_sims: Vec<NativeSim>,
    pub exposure_modes: Vec<ExposureMode>,
    pub controls: Vec<Control>,
    pub solve_plan: Vec<SolvePlanStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareRange {
    pub from: FirmwareVersion,
    pub to: FirmwareVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSim {
    pub key: SimKey,
    pub look: LookId,
    pub makernote: Makernote,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Makernote {
    pub tag: MakernoteTag,
    pub value: MakernoteValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExposureMode {
    pub key: ExposureModeKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SolvePlanStep {
    pub stage: StageName,
    pub controls: Vec<ControlKey>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "String")]
pub struct FirmwareVersion {
    raw: String,
    parts: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid firmware version `{0}`")]
pub struct FirmwareVersionError(String);

impl FirmwareVersion {
    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl FromStr for FirmwareVersion {
    type Err = FirmwareVersionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(FirmwareVersionError(s.to_owned()));
        }
        let mut parts = Vec::new();
        for part in s.split('.') {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(FirmwareVersionError(s.to_owned()));
            }
            parts.push(
                part.parse::<u64>()
                    .map_err(|_| FirmwareVersionError(s.to_owned()))?,
            );
        }
        while parts.len() > 1 && parts.last() == Some(&0) {
            parts.pop();
        }
        Ok(Self {
            raw: s.to_owned(),
            parts,
        })
    }
}

impl TryFrom<String> for FirmwareVersion {
    type Error = FirmwareVersionError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl PartialEq for FirmwareVersion {
    fn eq(&self, other: &Self) -> bool {
        self.parts == other.parts
    }
}

impl Eq for FirmwareVersion {}

impl PartialOrd for FirmwareVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FirmwareVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.parts.cmp(&other.parts)
    }
}

impl Hash for FirmwareVersion {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.parts.hash(state);
    }
}

impl fmt::Display for FirmwareVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub struct MakernoteValue(pub u64);

impl<'de> Deserialize<'de> for MakernoteValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(MakernoteValueVisitor)
    }
}

struct MakernoteValueVisitor;

impl Visitor<'_> for MakernoteValueVisitor {
    type Value = MakernoteValue;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an integer or a 0x-prefixed hex string")
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
        u64::try_from(value)
            .map(MakernoteValue)
            .map_err(|_| E::custom("makernote value must be non-negative"))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
        Ok(MakernoteValue(value))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        let parsed = match value
            .strip_prefix("0x")
            .or_else(|| value.strip_prefix("0X"))
        {
            Some(hex) => u64::from_str_radix(hex, 16),
            None => value.parse(),
        };
        parsed.map(MakernoteValue).map_err(E::custom)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSchema {
    body: BodyId,
    epochs: Vec<RawEpoch>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEpoch {
    id: u32,
    firmware: FirmwareRange,
    native_sims: Vec<NativeSim>,
    exposure_modes: Vec<ExposureMode>,
    controls: Vec<RawControl>,
    solve_plan: Vec<SolvePlanStep>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawControl {
    key: ControlKey,
    #[serde(rename = "type")]
    kind: ControlType,
    range: Option<Vec<Rational>>,
    step: Option<Rational>,
    default: Option<ScalarValue>,
    values: Option<Vec<EnumValue>>,
    #[serde(default)]
    look_affecting: bool,
    #[serde(default)]
    affects: Vec<String>,
    #[serde(default)]
    spatial: bool,
    tolerated: Option<Tolerated>,
    constraints: Option<Constraints>,
}

pub fn validate_control_schema(yaml: &str) -> Result<ControlSchema, Vec<ValidationError>> {
    let raw: RawSchema = serde_norway::from_str(yaml).map_err(ValidationError::syntax)?;
    let mut errors = Vec::new();
    validate_schema(&raw, &mut errors);
    if errors.is_empty() {
        Ok(build_schema(raw))
    } else {
        Err(errors)
    }
}

fn validate_schema(raw: &RawSchema, errors: &mut Vec<ValidationError>) {
    for (index, epoch) in raw.epochs.iter().enumerate() {
        validate_epoch(epoch, &format!("epochs[{index}]"), errors);
    }
    validate_firmware(&raw.epochs, errors);
}

fn validate_epoch(epoch: &RawEpoch, path: &str, errors: &mut Vec<ValidationError>) {
    let mut sim_keys = HashSet::new();
    for (index, sim) in epoch.native_sims.iter().enumerate() {
        if !sim_keys.insert(sim.key.as_str()) {
            errors.push(ValidationError::new(
                format!("{path}.native_sims[{index}].key"),
                ErrorKind::DuplicateKey {
                    key: sim.key.to_string(),
                },
            ));
        }
    }

    let mut mode_keys = HashSet::new();
    for (index, mode) in epoch.exposure_modes.iter().enumerate() {
        if !mode_keys.insert(mode.key.as_str()) {
            errors.push(ValidationError::new(
                format!("{path}.exposure_modes[{index}].key"),
                ErrorKind::DuplicateKey {
                    key: mode.key.to_string(),
                },
            ));
        }
    }

    let mut control_keys = HashSet::new();
    for (index, control) in epoch.controls.iter().enumerate() {
        let control_path = format!("{path}.controls[{index}]");
        if !control_keys.insert(control.key.as_str()) {
            errors.push(ValidationError::new(
                format!("{control_path}.key"),
                ErrorKind::DuplicateKey {
                    key: control.key.to_string(),
                },
            ));
        }
        validate_control(control, &control_path, errors);
    }

    let mut planned = HashSet::new();
    for (stage_index, stage) in epoch.solve_plan.iter().enumerate() {
        for (control_index, key) in stage.controls.iter().enumerate() {
            let control_path =
                format!("{path}.solve_plan[{stage_index}].controls[{control_index}]");
            if !control_keys.contains(key.as_str()) {
                errors.push(ValidationError::new(
                    control_path,
                    ErrorKind::SolvePlanUnknownControl {
                        key: key.to_string(),
                    },
                ));
            } else if !planned.insert(key.as_str()) {
                errors.push(ValidationError::new(
                    control_path,
                    ErrorKind::SolvePlanDuplicateControl {
                        key: key.to_string(),
                    },
                ));
            }
        }
    }
}

fn validate_control(control: &RawControl, path: &str, errors: &mut Vec<ValidationError>) {
    let field = |name: &str| format!("{path}.{name}");
    match control.kind {
        ControlType::Int => {
            if control.values.is_some() {
                errors.push(ValidationError::new(
                    field("values"),
                    ErrorKind::FieldNotAllowed {
                        field: "values",
                        control_type: control.kind,
                    },
                ));
            }
            let step = control.step.unwrap_or_else(|| Rational::from_integer(1));
            if step.num <= 0 {
                errors.push(ValidationError::new(
                    field("step"),
                    ErrorKind::StepNotPositive,
                ));
            } else if step.den != 1 {
                errors.push(ValidationError::new(
                    field("step"),
                    ErrorKind::InvalidValue {
                        expected: "positive integer step",
                    },
                ));
            }
            validate_numeric(control, step, path, errors);
        }
        ControlType::Decimal => {
            if control.values.is_some() {
                errors.push(ValidationError::new(
                    field("values"),
                    ErrorKind::FieldNotAllowed {
                        field: "values",
                        control_type: control.kind,
                    },
                ));
            }
            match control.step {
                Some(step) if step.num > 0 => validate_numeric(control, step, path, errors),
                Some(_) => errors.push(ValidationError::new(
                    field("step"),
                    ErrorKind::StepNotPositive,
                )),
                None => errors.push(ValidationError::new(
                    field("step"),
                    ErrorKind::MissingField { field: "step" },
                )),
            }
        }
        ControlType::Enum => {
            for name in ["range", "step"] {
                let present = match name {
                    "range" => control.range.is_some(),
                    _ => control.step.is_some(),
                };
                if present {
                    errors.push(ValidationError::new(
                        field(name),
                        ErrorKind::FieldNotAllowed {
                            field: name,
                            control_type: control.kind,
                        },
                    ));
                }
            }
            match &control.values {
                Some(values) if !values.is_empty() => match &control.default {
                    Some(ScalarValue::Text(default)) => {
                        if !values.iter().any(|value| value.as_str() == default) {
                            errors.push(ValidationError::new(
                                field("default"),
                                ErrorKind::EnumDefaultNotInValues,
                            ));
                        }
                    }
                    Some(_) => errors.push(ValidationError::new(
                        field("default"),
                        ErrorKind::InvalidValue {
                            expected: "enum value",
                        },
                    )),
                    None => errors.push(ValidationError::new(
                        field("default"),
                        ErrorKind::MissingField { field: "default" },
                    )),
                },
                Some(_) => errors.push(ValidationError::new(
                    field("values"),
                    ErrorKind::InvalidValue {
                        expected: "at least one enum value",
                    },
                )),
                None => errors.push(ValidationError::new(
                    field("values"),
                    ErrorKind::MissingField { field: "values" },
                )),
            }
        }
        ControlType::Bool => {
            for name in ["range", "step", "values"] {
                let present = match name {
                    "range" => control.range.is_some(),
                    "step" => control.step.is_some(),
                    _ => control.values.is_some(),
                };
                if present {
                    errors.push(ValidationError::new(
                        field(name),
                        ErrorKind::FieldNotAllowed {
                            field: name,
                            control_type: control.kind,
                        },
                    ));
                }
            }
            match &control.default {
                Some(ScalarValue::Bool(_)) => {}
                Some(_) => errors.push(ValidationError::new(
                    field("default"),
                    ErrorKind::InvalidValue {
                        expected: "boolean",
                    },
                )),
                None => errors.push(ValidationError::new(
                    field("default"),
                    ErrorKind::MissingField { field: "default" },
                )),
            }
        }
    }

    for (index, value) in control.affects.iter().enumerate() {
        if Affects::from_str(value).is_err() {
            errors.push(ValidationError::new(
                format!("{path}.affects[{index}]"),
                ErrorKind::UnknownAffect {
                    value: value.clone(),
                },
            ));
        }
    }
    if control.look_affecting && control.affects.is_empty() {
        errors.push(ValidationError::new(
            field("affects"),
            ErrorKind::AffectsRequired,
        ));
    }
    if control.spatial && control.tolerated.is_none() {
        errors.push(ValidationError::new(
            field("tolerated"),
            ErrorKind::SpatialToleratedRequired,
        ));
    }
}

fn validate_numeric(
    control: &RawControl,
    step: Rational,
    path: &str,
    errors: &mut Vec<ValidationError>,
) {
    let Some(range) = &control.range else {
        errors.push(ValidationError::new(
            format!("{path}.range"),
            ErrorKind::MissingField { field: "range" },
        ));
        return;
    };
    if range.len() != 2 {
        errors.push(ValidationError::new(
            format!("{path}.range"),
            ErrorKind::RangeArity { found: range.len() },
        ));
        return;
    }
    let (min, max) = (range[0], range[1]);
    if min > max {
        errors.push(ValidationError::new(
            format!("{path}.range"),
            ErrorKind::RangeInverted,
        ));
    }
    for (name, value) in [("range[0]", min), ("range[1]", max)] {
        if value.div_exact(&step).is_none() {
            errors.push(ValidationError::new(
                format!("{path}.{name}"),
                ErrorKind::NotOnStep,
            ));
        }
    }
    match &control.default {
        Some(ScalarValue::Number(default)) => {
            if *default < min || *default > max {
                errors.push(ValidationError::new(
                    format!("{path}.default"),
                    ErrorKind::DefaultOutOfRange,
                ));
            }
            if default.div_exact(&step).is_none() {
                errors.push(ValidationError::new(
                    format!("{path}.default"),
                    ErrorKind::NotOnStep,
                ));
            }
        }
        Some(_) => errors.push(ValidationError::new(
            format!("{path}.default"),
            ErrorKind::InvalidValue { expected: "number" },
        )),
        None => errors.push(ValidationError::new(
            format!("{path}.default"),
            ErrorKind::MissingField { field: "default" },
        )),
    }
}

fn validate_firmware(epochs: &[RawEpoch], errors: &mut Vec<ValidationError>) {
    let mut ranges = Vec::new();
    for (index, epoch) in epochs.iter().enumerate() {
        if epoch.firmware.from > epoch.firmware.to {
            errors.push(ValidationError::new(
                format!("epochs[{index}].firmware"),
                ErrorKind::FirmwareRangeInverted,
            ));
        } else {
            ranges.push((index, &epoch.firmware.from, &epoch.firmware.to));
        }
    }
    ranges.sort_by(|a, b| a.1.cmp(b.1).then_with(|| a.2.cmp(b.2)));
    for pair in ranges.windows(2) {
        if pair[1].1 <= pair[0].2 {
            errors.push(ValidationError::new(
                format!("epochs[{}].firmware", pair[1].0),
                ErrorKind::FirmwareRangeOverlap,
            ));
        }
    }
}

fn build_schema(raw: RawSchema) -> ControlSchema {
    ControlSchema {
        body: raw.body,
        epochs: raw.epochs.into_iter().map(build_epoch).collect(),
    }
}

fn build_epoch(raw: RawEpoch) -> Epoch {
    Epoch {
        id: raw.id,
        firmware: raw.firmware,
        native_sims: raw.native_sims,
        exposure_modes: raw.exposure_modes,
        controls: raw.controls.into_iter().map(build_control).collect(),
        solve_plan: raw.solve_plan,
    }
}

fn build_control(raw: RawControl) -> Control {
    let step = match raw.kind {
        ControlType::Int => Some(raw.step.unwrap_or_else(|| Rational::from_integer(1))),
        ControlType::Decimal => raw.step,
        ControlType::Enum | ControlType::Bool => None,
    };
    let range = raw.range.and_then(|values| match values.as_slice() {
        [min, max] => Some(Range {
            min: *min,
            max: *max,
        }),
        _ => None,
    });
    let default = match raw.default {
        Some(ScalarValue::Number(value)) => ControlDefault::Number(value),
        Some(ScalarValue::Text(value)) => ControlDefault::Enum(EnumValue::from_unchecked(value)),
        Some(ScalarValue::Bool(value)) => ControlDefault::Bool(value),
        None => ControlDefault::Bool(false),
    };
    Control {
        key: raw.key,
        kind: raw.kind,
        range,
        step,
        values: raw.values,
        default,
        look_affecting: raw.look_affecting,
        affects: raw
            .affects
            .iter()
            .filter_map(|value| Affects::from_str(value).ok())
            .collect(),
        spatial: raw.spatial,
        tolerated: raw.tolerated,
        constraints: raw.constraints,
    }
}
