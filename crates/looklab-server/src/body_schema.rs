// SPDX-License-Identifier: AGPL-3.0-or-later
//! The `data/bodies` snapshot loaded at startup, and settings validation.

use std::collections::BTreeMap;
use std::path::Path;

use looklab_schema::{
    Control, ControlSchema, ControlType, Epoch, FirmwareVersion, ScalarValue, Tolerated,
    validate_body, validate_control_schema,
};
use looklab_upload::{BodyId, ControlSettings, SimKey};

#[derive(Debug)]
pub struct BodyRecord {
    pub make: String,
    pub model: String,
    pub exif_aliases: Vec<String>,
    pub schema: ControlSchema,
}

#[derive(Debug, Default)]
pub struct Bodies {
    records: BTreeMap<String, BodyRecord>,
}

#[derive(Debug, thiserror::Error)]
pub enum BodiesError {
    #[error("bodies directory: {0}")]
    Io(#[from] std::io::Error),
    #[error("{file} has no valid body entity: {errors}")]
    InvalidBody { file: String, errors: String },
    #[error("{file} has no valid control schema: {errors}")]
    InvalidSchema { file: String, errors: String },
    #[error("{body} has a control schema but no body entity")]
    MissingBodyEntity { body: String },
    #[error("{body} has more than one control schema")]
    DuplicateSchema { body: String },
}

impl Bodies {
    pub fn load(dir: &Path) -> Result<Self, BodiesError> {
        let mut entities: BTreeMap<String, (String, String, Vec<String>)> = BTreeMap::new();
        let mut schemas: BTreeMap<String, ControlSchema> = BTreeMap::new();

        let mut entries = std::fs::read_dir(dir)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()?;
        entries.sort();

        for path in entries {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !name.ends_with(".yaml") {
                continue;
            }
            let text = std::fs::read_to_string(&path)?;
            if name.ends_with(".body.yaml") {
                match validate_body(&text) {
                    Ok(body) => {
                        entities.insert(
                            body.id.as_str().to_owned(),
                            (
                                body.make.clone(),
                                body.model.clone(),
                                body.exif_aliases.clone(),
                            ),
                        );
                    }
                    Err(errors) => {
                        return Err(BodiesError::InvalidBody {
                            file: name.to_owned(),
                            errors: join_errors(&errors),
                        });
                    }
                }
            } else {
                match validate_control_schema(&text) {
                    Ok(schema) => {
                        let id = schema.body.as_str().to_owned();
                        if schemas.insert(id.clone(), schema).is_some() {
                            return Err(BodiesError::DuplicateSchema { body: id });
                        }
                    }
                    Err(errors) => {
                        return Err(BodiesError::InvalidSchema {
                            file: name.to_owned(),
                            errors: join_errors(&errors),
                        });
                    }
                }
            }
        }

        let mut records = BTreeMap::new();
        for (id, schema) in schemas {
            let Some((make, model, exif_aliases)) = entities.get(&id) else {
                return Err(BodiesError::MissingBodyEntity { body: id });
            };
            records.insert(
                id,
                BodyRecord {
                    make: make.clone(),
                    model: model.clone(),
                    exif_aliases: exif_aliases.clone(),
                    schema,
                },
            );
        }
        Ok(Self { records })
    }

    pub fn get(&self, body: &BodyId) -> Option<&BodyRecord> {
        self.records.get(body.as_str())
    }

    pub fn records(&self) -> impl Iterator<Item = (&String, &BodyRecord)> {
        self.records.iter()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

fn join_errors(errors: &[looklab_schema::ValidationError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsProblem {
    NoEpoch,
    UnknownSim,
    UnknownControl,
    StepOutOfRange,
    MissingLookAffectingControl,
    SpatialNotTolerated,
}

pub fn validate_settings(
    record: &BodyRecord,
    firmware: &str,
    sim_key: &SimKey,
    settings: &ControlSettings,
) -> Result<(), SettingsProblem> {
    let epoch = resolve_epoch(&record.schema, firmware).ok_or(SettingsProblem::NoEpoch)?;
    if !epoch
        .native_sims
        .iter()
        .any(|sim| sim.key.as_str() == sim_key.as_str())
    {
        return Err(SettingsProblem::UnknownSim);
    }

    let mut provided = BTreeMap::new();
    for entry in settings.entries() {
        let control = epoch
            .controls
            .iter()
            .find(|control| control.key.as_str() == entry.key.as_str())
            .ok_or(SettingsProblem::UnknownControl)?;
        if !step_in_range(control, entry.step.get()) {
            return Err(SettingsProblem::StepOutOfRange);
        }
        if control.spatial
            && let Some(Tolerated::Values(allowed)) = control.tolerated.as_ref()
        {
            let value =
                scalar_at(control, entry.step.get()).ok_or(SettingsProblem::StepOutOfRange)?;
            if !allowed.contains(&value) {
                return Err(SettingsProblem::SpatialNotTolerated);
            }
        }
        provided.insert(control.key.as_str(), entry);
    }

    for control in epoch
        .controls
        .iter()
        .filter(|control| control.look_affecting)
    {
        if !provided.contains_key(control.key.as_str()) {
            return Err(SettingsProblem::MissingLookAffectingControl);
        }
    }
    Ok(())
}

fn step_in_range(control: &Control, step: i64) -> bool {
    match control.kind {
        ControlType::Enum => control
            .values
            .as_ref()
            .is_some_and(|values| usize::try_from(step).is_ok_and(|index| index < values.len())),
        ControlType::Bool => step == 0 || step == 1,
        ControlType::Int | ControlType::Decimal => {
            let steps = control.steps();
            !steps.is_empty() && steps.contains(&step)
        }
    }
}

fn scalar_at(control: &Control, step: i64) -> Option<ScalarValue> {
    match control.kind {
        ControlType::Enum => control
            .values
            .as_ref()?
            .get(usize::try_from(step).ok()?)
            .map(|value| ScalarValue::Text(value.to_string())),
        ControlType::Bool => match step {
            0 => Some(ScalarValue::Bool(false)),
            1 => Some(ScalarValue::Bool(true)),
            _ => None,
        },
        ControlType::Int | ControlType::Decimal => control.value_at(step).map(ScalarValue::Number),
    }
}

pub fn resolve_epoch<'a>(schema: &'a ControlSchema, firmware: &str) -> Option<&'a Epoch> {
    let version: FirmwareVersion = firmware.parse().ok()?;
    if let Some(epoch) = schema
        .epochs
        .iter()
        .find(|epoch| epoch.firmware.from <= version && version <= epoch.firmware.to)
    {
        return Some(epoch);
    }
    schema
        .epochs
        .iter()
        .filter(|epoch| epoch.firmware.to < version)
        .max_by(|a, b| a.firmware.to.cmp(&b.firmware.to))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &str = r#"
body: fujifilm/x-t5
epochs:
  - id: 1
    firmware: { from: "1.00", to: "1.99" }
    native_sims:
      - key: classic_chrome
        look: classic-chrome
        makernote: { tag: FilmMode, value: 0x0700 }
    exposure_modes:
      - key: dr100
    controls:
      - key: highlight_tone
        type: decimal
        range: [-2.0, 4.0]
        step: 0.5
        default: 0.0
        look_affecting: true
        affects: [tone]
      - key: grain
        type: enum
        values: [off, weak]
        default: off
        look_affecting: false
        spatial: true
        tolerated: [off]
      - key: paused
        type: bool
        default: false
        look_affecting: false
    solve_plan:
      - stage: tone
        controls: [highlight_tone]
"#;

    fn record() -> BodyRecord {
        BodyRecord {
            make: "Fujifilm".to_owned(),
            model: "X-T5".to_owned(),
            exif_aliases: Vec::new(),
            schema: validate_control_schema(SCHEMA).expect("test schema"),
        }
    }

    fn settings(entries: Vec<(&str, i64)>) -> ControlSettings {
        ControlSettings::new(
            entries
                .into_iter()
                .map(|(key, step)| looklab_upload::ControlEntry {
                    key: looklab_upload::ControlKey::new(key).expect("control key"),
                    step: looklab_upload::StepIndex::new(step),
                })
                .collect(),
        )
        .expect("unique settings")
    }

    #[test]
    fn epochs_resolve_by_range_and_nearest_lower() {
        let record = record();
        assert_eq!(
            resolve_epoch(&record.schema, "1.50").map(|epoch| epoch.id),
            Some(1)
        );
        assert_eq!(
            resolve_epoch(&record.schema, "2.10").map(|epoch| epoch.id),
            Some(1)
        );
        assert_eq!(resolve_epoch(&record.schema, "0.90"), None);
    }

    #[test]
    fn settings_validate_against_the_epoch() {
        let sim = SimKey::new("classic_chrome").expect("sim");
        let record = record();
        assert_eq!(
            validate_settings(
                &record,
                "1.50",
                &sim,
                &settings(vec![("highlight_tone", 2)])
            ),
            Ok(())
        );
        assert_eq!(
            validate_settings(&record, "1.50", &sim, &settings(vec![])),
            Err(SettingsProblem::MissingLookAffectingControl)
        );
        assert_eq!(
            validate_settings(
                &record,
                "1.50",
                &sim,
                &settings(vec![("highlight_tone", 99)])
            ),
            Err(SettingsProblem::StepOutOfRange)
        );
        assert_eq!(
            validate_settings(
                &record,
                "1.50",
                &sim,
                &settings(vec![("highlight_tone", 0), ("grain", 1)])
            ),
            Err(SettingsProblem::SpatialNotTolerated)
        );
        assert_eq!(
            validate_settings(&record, "1.50", &sim, &settings(vec![("color", 0)])),
            Err(SettingsProblem::UnknownControl)
        );
        assert_eq!(
            validate_settings(
                &record,
                "1.50",
                &sim,
                &settings(vec![("highlight_tone", 0), ("grain", 2)])
            ),
            Err(SettingsProblem::StepOutOfRange)
        );
        assert_eq!(
            validate_settings(
                &record,
                "1.50",
                &sim,
                &settings(vec![("highlight_tone", 0), ("paused", 7)])
            ),
            Err(SettingsProblem::StepOutOfRange)
        );
    }
}
