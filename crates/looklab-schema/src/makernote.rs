// SPDX-License-Identifier: AGPL-3.0-or-later
//! Control-to-makernote mappings and the settings a file's tags determine.
//!
//! The schema YAML names each control's tag and encoding; `looklab-cli
//! verify-payload` and the browser contribution flow derive settings here.

use std::collections::BTreeMap;

use looklab_meta::{FujiTag, LookSettings, ParsedTag};

use crate::control::{Control, ControlType, ScalarValue};
use crate::ids::{ControlKey, SimKey};
use crate::mapping::MappingError;
use crate::rational::Rational;
use crate::schema::{ControlSchema, Epoch};

/// A tag a control mapping reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MakernoteSource {
    Fuji(FujiTag),
    /// The EXIF exposure-bias field, in thirds of a stop.
    ExposureCompensation,
}

impl MakernoteSource {
    /// Accepts an ExifTool FujiFilm tag name or `ExposureCompensation`.
    pub fn from_name(name: &str) -> Option<Self> {
        if name == "ExposureCompensation" {
            return Some(Self::ExposureCompensation);
        }
        FujiTag::from_exiftool_name(name).map(Self::Fuji)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Fuji(tag) => tag.exiftool_name(),
            Self::ExposureCompensation => "ExposureCompensation",
        }
    }

    /// Paired tags return 2; their values are indexed by `component`.
    pub(crate) fn components(self) -> u8 {
        match self {
            Self::Fuji(FujiTag::WhiteBalanceFineTune) => 2,
            _ => 1,
        }
    }
}

/// How a control's value becomes a raw tag value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MakernoteEncoding {
    /// `value = raw / scale`; `raw` must be a whole number for every step.
    Scale(Rational),
    /// The raw values that mean each control value; several raw values may
    /// share one control value, such as the grain sizes above `off`.
    Values(BTreeMap<ScalarValue, Vec<i64>>),
}

/// A control's mapping from its value to one tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlMakernote {
    pub tag: MakernoteSource,
    /// Index into a paired tag's values; required for [`MakernoteSource::Fuji`]
    /// tags that carry two, absent otherwise.
    pub component: Option<u8>,
    pub encoding: MakernoteEncoding,
    /// The mapping tried when [`Self::tag`] is absent from the file.
    pub fallback: Option<Box<ControlMakernote>>,
}

/// The raw tag values one file carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MakernoteValues {
    tags: Vec<(FujiTag, Vec<i64>)>,
    exposure_compensation_thirds: Option<i64>,
}

impl MakernoteValues {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, tag: FujiTag, values: Vec<i64>) {
        match self
            .tags
            .iter_mut()
            .find(|(candidate, _)| *candidate == tag)
        {
            Some(slot) => *slot = (tag, values),
            None => self.tags.push((tag, values)),
        }
    }

    /// Stores the value only when it lands on the camera's thirds-of-a-stop
    /// grid.
    pub fn set_exposure_compensation_ev(&mut self, ev: f64) {
        let thirds = ev * 3.0;
        let rounded = thirds.round();
        if (thirds - rounded).abs() <= 1e-9 && rounded.abs() <= 1e6 {
            self.exposure_compensation_thirds = Some(rounded as i64);
        }
    }

    fn raw(&self, source: MakernoteSource, component: Option<u8>) -> Option<i64> {
        match source {
            MakernoteSource::Fuji(tag) => {
                let (_, values) = self.tags.iter().find(|(candidate, _)| *candidate == tag)?;
                values.get(usize::from(component.unwrap_or(0))).copied()
            }
            MakernoteSource::ExposureCompensation => self.exposure_compensation_thirds,
        }
    }
}

impl From<&LookSettings> for MakernoteValues {
    fn from(look: &LookSettings) -> Self {
        let mut values = Self::new();
        push(&mut values, look.film_mode.as_ref());
        push(&mut values, look.dynamic_range.as_ref());
        push(&mut values, look.dynamic_range_setting.as_ref());
        push(&mut values, look.development_dynamic_range.as_ref());
        push(&mut values, look.saturation.as_ref());
        push(&mut values, look.white_balance.as_ref());
        push(&mut values, look.highlight_tone.as_ref());
        push(&mut values, look.shadow_tone.as_ref());
        push(&mut values, look.sharpness.as_ref());
        push(&mut values, look.noise_reduction.as_ref());
        push(&mut values, look.clarity.as_ref());
        push(&mut values, look.grain_roughness.as_ref());
        push(&mut values, look.grain_size.as_ref());
        push(&mut values, look.color_chrome_effect.as_ref());
        push(&mut values, look.color_chrome_fx_blue.as_ref());
        if let Some(pair) = look.white_balance_fine_tune.as_ref() {
            values.insert(
                pair.tag,
                pair.raw.iter().map(|value| i64::from(*value)).collect(),
            );
        }
        values
    }
}

fn push<T>(values: &mut MakernoteValues, tag: Option<&ParsedTag<T>>) {
    if let Some(tag) = tag {
        values.insert(tag.tag, vec![tag.raw]);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub key: ControlKey,
    pub step: i64,
    pub source: MakernoteSource,
}

/// The controls a file's tags determine, in schema order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    entries: Vec<Setting>,
}

impl Settings {
    pub fn entries(&self) -> &[Setting] {
        &self.entries
    }

    pub fn get(&self, key: &ControlKey) -> Option<&Setting> {
        self.entries.iter().find(|setting| &setting.key == key)
    }
}

/// Derives every mapped control of `epoch`.
///
/// # Errors
/// The schema has no such epoch, a mapped control's tag is absent, or a tag
/// value does not decode to one of the control's steps.
pub fn settings_from_makernote(
    schema: &ControlSchema,
    epoch: u32,
    values: &MakernoteValues,
) -> Result<Settings, MappingError> {
    let (settings, problems) = settings_from_makernote_partial(schema, epoch, values);
    match problems.into_iter().next() {
        Some(error) => Err(error),
        None => Ok(settings),
    }
}

/// Like [`settings_from_makernote`], but reports undecodable controls instead
/// of failing.
pub fn settings_from_makernote_partial(
    schema: &ControlSchema,
    epoch: u32,
    values: &MakernoteValues,
) -> (Settings, Vec<MappingError>) {
    let mut settings = Settings::default();
    let Some(epoch) = schema.epochs.iter().find(|candidate| candidate.id == epoch) else {
        return (settings, vec![MappingError::UnknownEpoch { epoch }]);
    };
    let mut problems = Vec::new();
    for control in &epoch.controls {
        match control.makernote_step(values) {
            Some(Ok((step, source))) => settings.entries.push(Setting {
                key: control.key.clone(),
                step,
                source,
            }),
            Some(Err(error)) => problems.push(error),
            None => {}
        }
    }
    (settings, problems)
}

/// The native simulation the file's `native_sims` tags select.
///
/// # Errors
/// No native simulation matches, or several do.
pub fn sim_from_makernote(epoch: &Epoch, values: &MakernoteValues) -> Result<SimKey, MappingError> {
    let mut matches = Vec::new();
    for sim in &epoch.native_sims {
        let Some(source) = MakernoteSource::from_name(sim.makernote.tag.as_str()) else {
            continue;
        };
        let wanted = i64::try_from(sim.makernote.value.0).ok();
        if wanted.is_some_and(|wanted| values.raw(source, None) == Some(wanted)) {
            matches.push(sim);
        }
    }
    match matches.as_slice() {
        [] => Err(MappingError::UnknownSim),
        [sim] => Ok(sim.key.clone()),
        _ => Err(MappingError::AmbiguousSim),
    }
}

/// The look-affecting controls of `epoch` that a makernote can supply.
pub fn mapped_controls(epoch: &Epoch) -> Vec<ControlKey> {
    epoch
        .controls
        .iter()
        .filter(|control| control.look_affecting && control.makernote.is_some())
        .map(|control| control.key.clone())
        .collect()
}

/// The look-affecting controls of `epoch` with no makernote mapping.
pub fn unmapped_controls(epoch: &Epoch) -> Vec<ControlKey> {
    epoch
        .controls
        .iter()
        .filter(|control| control.look_affecting && control.makernote.is_none())
        .map(|control| control.key.clone())
        .collect()
}

impl Control {
    /// The step this control takes from the file's tags, with the tag it came
    /// from. `None` when the schema gives the control no mapping.
    ///
    /// # Errors
    /// The mapped tag is absent, or its value does not decode to a step.
    pub fn makernote_step(
        &self,
        values: &MakernoteValues,
    ) -> Option<Result<(i64, MakernoteSource), MappingError>> {
        self.makernote
            .as_ref()
            .map(|mapping| decode(self, mapping, values))
    }
}

fn decode(
    control: &Control,
    mapping: &ControlMakernote,
    values: &MakernoteValues,
) -> Result<(i64, MakernoteSource), MappingError> {
    let Some(raw) = values.raw(mapping.tag, mapping.component) else {
        return match mapping.fallback.as_deref() {
            Some(fallback) => decode(control, fallback, values),
            None => Err(MappingError::MissingTag {
                control: control.key.to_string(),
                tag: mapping.tag.name(),
            }),
        };
    };
    let step = match &mapping.encoding {
        MakernoteEncoding::Scale(scale) => scale_step(control, mapping.tag, *scale, raw)?,
        MakernoteEncoding::Values(table) => table_step(control, mapping.tag, table, raw)?,
    };
    Ok((step, mapping.tag))
}

fn scale_step(
    control: &Control,
    source: MakernoteSource,
    scale: Rational,
    raw: i64,
) -> Result<i64, MappingError> {
    let not_on_step = || MappingError::NotOnStep {
        control: control.key.to_string(),
        tag: source.name(),
    };
    let Some(step) = control.step.as_ref() else {
        return Err(MappingError::ScaleNotNumeric {
            control: control.key.to_string(),
            tag: source.name(),
        });
    };
    let index = exact_step(raw, scale, *step).ok_or_else(not_on_step)?;
    if !control.steps().contains(&index) {
        return Err(not_on_step());
    }
    Ok(index)
}

/// The step index of `raw / scale`, when that is a whole multiple of `step`.
fn exact_step(raw: i64, scale: Rational, step: Rational) -> Option<i64> {
    let num = i128::from(raw) * i128::from(scale.den) * i128::from(step.den);
    let den = i128::from(scale.num) * i128::from(step.num);
    if den == 0 || num % den != 0 {
        return None;
    }
    i64::try_from(num / den).ok()
}

fn table_step(
    control: &Control,
    source: MakernoteSource,
    table: &BTreeMap<ScalarValue, Vec<i64>>,
    raw: i64,
) -> Result<i64, MappingError> {
    let unmapped = || MappingError::UnmappedValue {
        control: control.key.to_string(),
        tag: source.name(),
        raw: raw.to_string(),
    };
    let value = table
        .iter()
        .find(|(_, raws)| raws.contains(&raw))
        .map(|(value, _)| value)
        .ok_or_else(unmapped)?;
    match control.kind {
        ControlType::Enum => {
            let ScalarValue::Text(name) = value else {
                return Err(unmapped());
            };
            let values = control.values.as_ref().ok_or_else(unmapped)?;
            values
                .iter()
                .position(|candidate| candidate.as_str() == name)
                .and_then(|index| i64::try_from(index).ok())
                .ok_or_else(unmapped)
        }
        ControlType::Bool => match value {
            ScalarValue::Bool(flag) => Ok(i64::from(*flag)),
            _ => Err(unmapped()),
        },
        ControlType::Int | ControlType::Decimal => {
            let ScalarValue::Number(number) = value else {
                return Err(unmapped());
            };
            let step = control.step.ok_or_else(unmapped)?;
            let index = number.div_exact(&step).ok_or_else(unmapped)?;
            if !control.steps().contains(&index) {
                return Err(unmapped());
            }
            Ok(index)
        }
    }
}
