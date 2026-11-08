// SPDX-License-Identifier: AGPL-3.0-or-later
//! The body control-schema file.

use core::cmp::Ordering;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::str::FromStr;
use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;
use serde::de::{self, Visitor};

use crate::control::{
    Affects, Constraints, Control, ControlDefault, ControlScope, ControlType, Range, ScalarValue,
    Tolerated, numeric_steps,
};
use crate::error::{ErrorKind, ValidationError};
use crate::ids::{
    BodyId, ControlKey, EnumValue, ExposureModeKey, LookId, MakernoteTag, SimKey, StageName,
};
use crate::makernote::{ControlMakernote, MakernoteEncoding, MakernoteSource};
use crate::rational::Rational;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlSchema {
    pub body: BodyId,
    pub epochs: Vec<Epoch>,
    source: String,
}

impl ControlSchema {
    /// The YAML document this schema was validated from.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// An unknown firmware version takes the nearest lower epoch's range.
    pub fn epoch_for(&self, firmware: &str) -> Option<&Epoch> {
        let version: FirmwareVersion = firmware.parse().ok()?;
        if let Some(epoch) = self
            .epochs
            .iter()
            .find(|epoch| epoch.firmware.from <= version && version <= epoch.firmware.to)
        {
            return Some(epoch);
        }
        self.epochs
            .iter()
            .filter(|epoch| epoch.firmware.to < version)
            .max_by(|left, right| left.firmware.to.cmp(&right.firmware.to))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Epoch {
    pub id: u32,
    pub firmware: FirmwareRange,
    pub native_sims: Vec<NativeSim>,
    pub monochrome: Option<MonochromeCapability>,
    pub exposure_modes: Vec<ExposureMode>,
    pub controls: Vec<Control>,
    pub solve_plan: Vec<SolvePlanStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonochromeCapability {
    /// Monochrome families deferred intake may accept.
    pub families: Vec<LookId>,
    /// Families whose filter variants the body offers.
    pub filter: Vec<LookId>,
    /// Families whose WC/MG toning axes the body offers.
    #[serde(default)]
    pub toning: Vec<LookId>,
    /// Whether the body retains an independent GrainEffectSize setting.
    #[serde(default)]
    pub grain_size: bool,
}

impl MonochromeCapability {
    pub fn supports(&self, family: &LookId) -> bool {
        self.families.contains(family)
    }
}

impl Epoch {
    pub fn native_sim(&self, key: &SimKey) -> Option<&NativeSim> {
        self.native_sims
            .iter()
            .find(|sim| sim.key.as_str() == key.as_str())
    }

    pub fn monochrome_capability(&self) -> Option<&MonochromeCapability> {
        self.monochrome.as_ref()
    }

    pub fn supports_monochrome_intake(&self, family: &LookId) -> bool {
        let Some(capability) = self.monochrome.as_ref() else {
            return false;
        };
        if !capability.supports(family) {
            return false;
        }
        !capability.grain_size
            || self.controls.iter().any(|control| {
                control.key.as_str() == MonochromeControl::GrainSize.key()
                    && control.spatial
                    && control.makernote.is_some()
                    && control.scope == ControlScope::All
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareRange {
    pub from: FirmwareVersion,
    pub to: FirmwareVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSim {
    pub key: SimKey,
    pub look: LookId,
    pub makernote: Makernote,
    /// The sim renders a monochrome image; the fit has no B&W Look kind.
    #[serde(default)]
    pub monochrome: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
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
    #[serde(default)]
    monochrome: Option<MonochromeCapability>,
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
    makernote: Option<RawControlMakernote>,
    applies_to: Option<ControlScope>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawControlMakernote {
    tag: MakernoteTag,
    #[serde(default)]
    component: Option<u8>,
    #[serde(default)]
    encoding: Option<RawMakernoteScale>,
    #[serde(default)]
    values: Option<BTreeMap<String, OneOrMany>>,
    #[serde(default)]
    fallback: Option<Box<RawControlMakernote>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMakernoteScale {
    scale: Rational,
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(i64),
    Many(Vec<i64>),
}

impl OneOrMany {
    fn into_vec(self) -> Vec<i64> {
        match self {
            Self::One(value) => vec![value],
            Self::Many(values) => values,
        }
    }
}

pub fn validate_control_schema(yaml: &str) -> Result<ControlSchema, Vec<ValidationError>> {
    crate::yaml::check(yaml)?;
    let raw: RawSchema = serde_norway::from_str(yaml).map_err(ValidationError::syntax)?;
    let mut errors = Vec::new();
    validate_schema(&raw, &mut errors);
    if errors.is_empty() {
        let mut schema = build_schema(raw);
        schema.source = yaml.to_owned();
        Ok(schema)
    } else {
        Err(errors)
    }
}

/// Inserts or replaces the native-sim entry a mapping adds, then re-validates
/// the document it produced.
///
/// # Errors
/// The document does not validate, the epoch is absent, or the mapping's
/// firmware range differs from the epoch's.
pub fn apply_sim_mapping(
    yaml: &str,
    epoch: u32,
    firmware_from: &FirmwareVersion,
    firmware_to: Option<&FirmwareVersion>,
    sim: &NativeSim,
) -> Result<String, Vec<ValidationError>> {
    let schema = validate_control_schema(yaml)?;
    let invalid = |path: &str, expected: &'static str| {
        vec![ValidationError::new(
            path,
            ErrorKind::InvalidValue { expected },
        )]
    };
    let index = schema
        .epochs
        .iter()
        .position(|candidate| candidate.id == epoch)
        .ok_or_else(|| invalid("epoch", "an epoch the body's control schema declares"))?;
    if schema.epochs[index].firmware.from != *firmware_from {
        return Err(invalid("firmware_from", "the epoch's firmware range start"));
    }
    if let Some(to) = firmware_to
        && schema.epochs[index].firmware.to != *to
    {
        return Err(invalid("firmware_to", "the epoch's firmware range end"));
    }
    let mut document: serde_norway::Value =
        serde_norway::from_str(yaml).map_err(ValidationError::syntax)?;
    let sims = document
        .get_mut("epochs")
        .and_then(|epochs| epochs.get_mut(index))
        .and_then(|epoch| epoch.get_mut("native_sims"))
        .and_then(serde_norway::Value::as_sequence_mut)
        .ok_or_else(|| invalid("native_sims", "the epoch's list of native sims"))?;
    let entry = serde_norway::to_value(sim).map_err(ValidationError::syntax)?;
    match sims.iter_mut().find(|existing| {
        existing.get("key").and_then(serde_norway::Value::as_str) == Some(sim.key.as_str())
    }) {
        Some(slot) => *slot = entry,
        None => sims.push(entry),
    }
    let updated = serde_norway::to_string(&document).map_err(ValidationError::syntax)?;
    validate_control_schema(&updated)?;
    Ok(updated)
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
    let mut family_classes: BTreeMap<&LookId, bool> = BTreeMap::new();
    for (index, sim) in epoch.native_sims.iter().enumerate() {
        match family_classes.get(&sim.look) {
            Some(monochrome) if *monochrome != sim.monochrome => {
                errors.push(ValidationError::new(
                    format!("{path}.native_sims[{index}].monochrome"),
                    ErrorKind::FamilyRenderClassConflict {
                        family: sim.look.to_string(),
                    },
                ));
            }
            Some(_) => {}
            None => {
                family_classes.insert(&sim.look, sim.monochrome);
            }
        }
    }
    validate_monochrome(epoch, path, errors);

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
    let families: HashSet<&LookId> = epoch.native_sims.iter().map(|sim| &sim.look).collect();
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
        validate_control(control, &families, &control_path, errors);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MonochromeControl {
    Filter,
    WarmCool,
    MagentaGreen,
    GrainSize,
}

impl MonochromeControl {
    const fn key(self) -> &'static str {
        match self {
            Self::Filter => "mono_filter",
            Self::WarmCool => "bw_adjustment",
            Self::MagentaGreen => "bw_magenta_green",
            Self::GrainSize => "grain_size",
        }
    }

    const fn capability_field(self) -> &'static str {
        match self {
            Self::Filter => "filter",
            Self::WarmCool | Self::MagentaGreen => "toning",
            Self::GrainSize => "grain_size",
        }
    }
}

fn validate_monochrome(epoch: &RawEpoch, path: &str, errors: &mut Vec<ValidationError>) {
    let mono_families: HashSet<&LookId> = epoch
        .native_sims
        .iter()
        .filter(|sim| sim.monochrome)
        .map(|sim| &sim.look)
        .collect();
    let Some(capability) = &epoch.monochrome else {
        return;
    };
    let capability_path = format!("{path}.monochrome");
    validate_family_list(
        &capability.families,
        &mono_families,
        &format!("{capability_path}.families"),
        errors,
    );
    let families: HashSet<&LookId> = capability.families.iter().collect();
    validate_family_list(
        &capability.filter,
        &families,
        &format!("{capability_path}.filter"),
        errors,
    );
    validate_family_list(
        &capability.toning,
        &families,
        &format!("{capability_path}.toning"),
        errors,
    );
    validate_monochrome_control(
        epoch,
        MonochromeControl::Filter,
        &capability.filter,
        &capability_path,
        errors,
    );
    validate_monochrome_control(
        epoch,
        MonochromeControl::WarmCool,
        &capability.toning,
        &capability_path,
        errors,
    );
    validate_monochrome_control(
        epoch,
        MonochromeControl::MagentaGreen,
        &capability.toning,
        &capability_path,
        errors,
    );
    validate_grain_size(epoch, capability, &capability_path, errors);
}

fn validate_grain_size(
    epoch: &RawEpoch,
    capability: &MonochromeCapability,
    capability_path: &str,
    errors: &mut Vec<ValidationError>,
) {
    let path = format!("{capability_path}.grain_size");
    let control = epoch
        .controls
        .iter()
        .find(|control| control.key.as_str() == MonochromeControl::GrainSize.key());
    match (control, capability.grain_size) {
        (None, false) => {}
        (None, true) => errors.push(ValidationError::new(
            path,
            ErrorKind::MonochromeGrainSizeMissing,
        )),
        (Some(_), false) => errors.push(ValidationError::new(
            path,
            ErrorKind::MonochromeGrainSizeNotDeclared,
        )),
        (Some(control), true) => {
            let scoped = matches!(&control.applies_to, None | Some(ControlScope::All));
            if !control.spatial || control.makernote.is_none() || !scoped {
                errors.push(ValidationError::new(
                    path,
                    ErrorKind::MonochromeGrainSizeMismatch,
                ));
            }
        }
    }
}

fn validate_family_list(
    families: &[LookId],
    allowed: &HashSet<&LookId>,
    path: &str,
    errors: &mut Vec<ValidationError>,
) {
    let mut seen = HashSet::new();
    for (index, family) in families.iter().enumerate() {
        if !seen.insert(family) {
            errors.push(ValidationError::new(
                format!("{path}[{index}]"),
                ErrorKind::DuplicateKey {
                    key: family.to_string(),
                },
            ));
        }
        if !allowed.contains(family) {
            errors.push(ValidationError::new(
                format!("{path}[{index}]"),
                ErrorKind::MonochromeFamilyNotDeclared {
                    family: family.to_string(),
                },
            ));
        }
    }
}

fn validate_monochrome_control(
    epoch: &RawEpoch,
    role: MonochromeControl,
    families: &[LookId],
    capability_path: &str,
    errors: &mut Vec<ValidationError>,
) {
    let control = epoch
        .controls
        .iter()
        .find(|control| control.key.as_str() == role.key());
    let path = format!("{capability_path}.{}", role.capability_field());
    let expected: HashSet<&LookId> = families.iter().collect();
    match (control, families.is_empty()) {
        (None, true) => {}
        (None, false) => errors.push(ValidationError::new(
            path,
            ErrorKind::MonochromeControlMissing {
                control: role.key(),
            },
        )),
        (Some(_), true) => errors.push(ValidationError::new(
            path,
            ErrorKind::MonochromeControlNotDeclared {
                control: role.key(),
            },
        )),
        (Some(control), false) => {
            let actual: HashSet<&LookId> = match &control.applies_to {
                Some(ControlScope::Families(scope)) => scope.iter().collect(),
                _ => HashSet::new(),
            };
            if actual != expected {
                errors.push(ValidationError::new(
                    path,
                    ErrorKind::MonochromeControlMismatch {
                        control: role.key(),
                    },
                ));
            }
        }
    }
}

fn validate_control(
    control: &RawControl,
    families: &HashSet<&LookId>,
    path: &str,
    errors: &mut Vec<ValidationError>,
) {
    let field = |name: &str| format!("{path}.{name}");
    if let Some(ControlScope::Families(scope)) = &control.applies_to {
        if scope.is_empty() {
            errors.push(ValidationError::new(
                field("applies_to"),
                ErrorKind::InvalidValue {
                    expected: "at least one look family",
                },
            ));
        }
        let mut seen = HashSet::new();
        for (index, family) in scope.iter().enumerate() {
            if !seen.insert(family) {
                errors.push(ValidationError::new(
                    format!("{path}.applies_to[{index}]"),
                    ErrorKind::DuplicateKey {
                        key: family.to_string(),
                    },
                ));
            }
            if !families.contains(family) {
                errors.push(ValidationError::new(
                    format!("{path}.applies_to[{index}]"),
                    ErrorKind::UnknownFamily {
                        family: family.to_string(),
                    },
                ));
            }
        }
    }
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
    if let Some(mapping) = control.makernote.as_ref() {
        validate_makernote(control, mapping, &field("makernote"), errors);
    }
}

fn validate_makernote(
    control: &RawControl,
    mapping: &RawControlMakernote,
    path: &str,
    errors: &mut Vec<ValidationError>,
) {
    let field = |name: &str| format!("{path}.{name}");
    let source = MakernoteSource::from_name(mapping.tag.as_str());
    match source {
        Some(source) => {
            let components = source.components();
            match mapping.component {
                Some(component) if component >= components => {
                    errors.push(ValidationError::new(
                        field("component"),
                        ErrorKind::MakernoteComponentOutOfRange {
                            component,
                            components,
                        },
                    ));
                }
                Some(_) if components == 1 => errors.push(ValidationError::new(
                    field("component"),
                    ErrorKind::MakernoteComponentNotAllowed,
                )),
                None if components > 1 => errors.push(ValidationError::new(
                    field("component"),
                    ErrorKind::MakernoteComponentRequired,
                )),
                _ => {}
            }
        }
        None => errors.push(ValidationError::new(
            field("tag"),
            ErrorKind::UnknownMakernoteTag {
                tag: mapping.tag.to_string(),
            },
        )),
    }
    match (&mapping.encoding, &mapping.values) {
        (None, None) => errors.push(ValidationError::new(
            path,
            ErrorKind::MissingMakernoteEncoding,
        )),
        (Some(_), Some(_)) => errors.push(ValidationError::new(
            path,
            ErrorKind::ConflictingMakernoteEncoding,
        )),
        (Some(scale), None) => validate_makernote_scale(control, scale, path, errors),
        (None, Some(values)) => validate_makernote_values(control, values, path, errors),
    }
    if let Some(fallback) = mapping.fallback.as_deref() {
        validate_makernote(control, fallback, &field("fallback"), errors);
    }
}

fn validate_makernote_scale(
    control: &RawControl,
    scale: &RawMakernoteScale,
    path: &str,
    errors: &mut Vec<ValidationError>,
) {
    if !matches!(control.kind, ControlType::Int | ControlType::Decimal) {
        errors.push(ValidationError::new(
            format!("{path}.encoding"),
            ErrorKind::MakernoteScaleNotNumeric,
        ));
        return;
    }
    if scale.scale.is_zero() {
        errors.push(ValidationError::new(
            format!("{path}.encoding.scale"),
            ErrorKind::MakernoteScaleZero,
        ));
        return;
    }
    let Some(step) = raw_step(control) else {
        return;
    };
    for index in raw_control_steps(control) {
        let Some(value) = step.checked_mul_i64(index) else {
            continue;
        };
        if scaled_raw(value, scale.scale).is_none() {
            errors.push(ValidationError::new(
                format!("{path}.encoding.scale"),
                ErrorKind::MakernoteRawNotInteger {
                    value: value.to_string(),
                },
            ));
            return;
        }
    }
}

fn validate_makernote_values(
    control: &RawControl,
    values: &BTreeMap<String, OneOrMany>,
    path: &str,
    errors: &mut Vec<ValidationError>,
) {
    let key_path = |key: &str| format!("{path}.values.{key}");
    let mut seen_raws: BTreeMap<i64, String> = BTreeMap::new();
    let mut covered: Vec<ScalarValue> = Vec::new();
    for (key, raws) in values {
        let raws = raws.clone().into_vec();
        if raws.is_empty() {
            errors.push(ValidationError::new(
                key_path(key),
                ErrorKind::MakernoteValueHasNoRaw,
            ));
        }
        for raw in &raws {
            if seen_raws.insert(*raw, key.clone()).is_some() {
                errors.push(ValidationError::new(
                    key_path(key),
                    ErrorKind::MakernoteDuplicateRaw { raw: *raw },
                ));
            }
        }
        let value = ScalarValue::from_raw(key);
        match control.kind {
            ControlType::Enum => match &value {
                ScalarValue::Text(name)
                    if control.values.as_ref().is_some_and(|declared| {
                        declared.iter().any(|value| value.as_str() == name)
                    }) =>
                {
                    covered.push(value);
                }
                _ => errors.push(ValidationError::new(
                    key_path(key),
                    ErrorKind::MakernoteValueUnknown { value: key.clone() },
                )),
            },
            ControlType::Bool => match value {
                ScalarValue::Bool(_) => covered.push(value),
                _ => errors.push(ValidationError::new(
                    key_path(key),
                    ErrorKind::InvalidValue {
                        expected: "boolean",
                    },
                )),
            },
            ControlType::Int | ControlType::Decimal => match value {
                ScalarValue::Number(number) => {
                    let Some(step) = raw_step(control) else {
                        continue;
                    };
                    match number.div_exact(&step) {
                        Some(index) if raw_control_steps(control).contains(&index) => {
                            covered.push(ScalarValue::Number(number));
                        }
                        Some(_) => errors.push(ValidationError::new(
                            key_path(key),
                            ErrorKind::MakernoteValueOutOfRange { value: key.clone() },
                        )),
                        None => {
                            errors.push(ValidationError::new(key_path(key), ErrorKind::NotOnStep))
                        }
                    }
                }
                _ => errors.push(ValidationError::new(
                    key_path(key),
                    ErrorKind::InvalidValue { expected: "number" },
                )),
            },
        }
    }
    match control.kind {
        ControlType::Enum => {
            for value in control.values.iter().flatten() {
                if !covered.contains(&ScalarValue::Text(value.to_string())) {
                    errors.push(ValidationError::new(
                        path,
                        ErrorKind::MakernoteValueMissing {
                            value: value.to_string(),
                        },
                    ));
                }
            }
        }
        ControlType::Bool => {
            for flag in [true, false] {
                if !covered.contains(&ScalarValue::Bool(flag)) {
                    errors.push(ValidationError::new(
                        path,
                        ErrorKind::MakernoteValueMissing {
                            value: flag.to_string(),
                        },
                    ));
                }
            }
        }
        ControlType::Int | ControlType::Decimal => {
            if let Some(ScalarValue::Number(default)) = control.default.as_ref()
                && !covered.contains(&ScalarValue::Number(*default))
            {
                errors.push(ValidationError::new(
                    path,
                    ErrorKind::MakernoteValueMissing {
                        value: default.to_string(),
                    },
                ));
            }
        }
    }
}

fn raw_step(control: &RawControl) -> Option<Rational> {
    match control.kind {
        ControlType::Int => Some(control.step.unwrap_or_else(|| Rational::from_integer(1))),
        ControlType::Decimal => control.step,
        ControlType::Enum | ControlType::Bool => None,
    }
}

fn raw_control_steps(control: &RawControl) -> core::ops::RangeInclusive<i64> {
    let range = control
        .range
        .as_ref()
        .and_then(|values| match values.as_slice() {
            [min, max] => Some(Range {
                min: *min,
                max: *max,
            }),
            _ => None,
        });
    numeric_steps(range.as_ref(), raw_step(control).as_ref())
}

fn scaled_raw(value: Rational, scale: Rational) -> Option<i64> {
    let num = i128::from(value.num) * i128::from(scale.num);
    let den = i128::from(value.den) * i128::from(scale.den);
    if den == 0 || num % den != 0 {
        return None;
    }
    i64::try_from(num / den).ok()
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
        source: String::new(),
    }
}

fn build_epoch(raw: RawEpoch) -> Epoch {
    Epoch {
        id: raw.id,
        firmware: raw.firmware,
        native_sims: raw.native_sims,
        monochrome: raw.monochrome,
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
        makernote: raw.makernote.and_then(build_makernote),
        scope: raw.applies_to.unwrap_or_default(),
    }
}

/// `None` only for a document [`validate_control_schema`] rejected.
fn build_makernote(raw: RawControlMakernote) -> Option<ControlMakernote> {
    let tag = MakernoteSource::from_name(raw.tag.as_str())?;
    let encoding = match (raw.encoding, raw.values) {
        (Some(scale), None) => MakernoteEncoding::Scale(scale.scale),
        (None, Some(values)) => MakernoteEncoding::Values(
            values
                .into_iter()
                .map(|(key, raws)| (ScalarValue::from_raw(&key), raws.into_vec()))
                .collect(),
        ),
        _ => return None,
    };
    Some(ControlMakernote {
        tag,
        component: raw.component,
        encoding,
        fallback: raw
            .fallback
            .and_then(|fallback| build_makernote(*fallback))
            .map(Box::new),
    })
}
