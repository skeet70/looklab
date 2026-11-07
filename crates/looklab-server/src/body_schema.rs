// SPDX-License-Identifier: AGPL-3.0-or-later
//! The shipped `data/bodies` snapshot and the published community snapshots,
//! resolved by version, plus settings validation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use looklab_schema::{
    Control, ControlSchema, ControlType, Epoch, FirmwareVersion, MakernoteValues, ScalarValue,
    Tolerated, mapped_controls, monochrome_sim, unmapped_controls, validate_body,
    validate_control_schema,
};
use looklab_upload::{
    BodyId, CommunitySnapshotVersion, ControlKey, ControlSettings, ManifestMetadata, SettingReach,
    SettingsSourceError, SimKey, validate_setting_sources,
};

#[derive(Debug)]
pub struct BodyRecord {
    pub make: String,
    pub model: String,
    pub exif_aliases: Vec<String>,
    pub schema: ControlSchema,
}

/// Body records per snapshot version; version 0 is the shipped `data/bodies`.
#[derive(Debug, Default)]
pub struct Bodies {
    versions: BTreeMap<u64, BTreeMap<String, BodyRecord>>,
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
    #[error("community snapshot v{version}: {source}")]
    Version {
        version: u64,
        source: Box<BodiesError>,
    },
}

/// A manifest's snapshot version that this server cannot resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotVersionError {
    #[error("community snapshot {version} is not one this server has published")]
    Unknown { version: CommunitySnapshotVersion },
    #[error("community snapshot {version} is older than the retained {oldest}")]
    TooOld {
        version: CommunitySnapshotVersion,
        oldest: CommunitySnapshotVersion,
    },
}

impl Bodies {
    /// # Errors
    /// `bodies_dir` or a `v<version>/bodies` directory under `snapshot_dir`
    /// fails to load.
    pub fn load(bodies_dir: &Path, snapshot_dir: &Path) -> Result<Self, BodiesError> {
        let mut versions = BTreeMap::new();
        versions.insert(0, load_dir(bodies_dir)?);
        for (version, dir) in published_snapshots(snapshot_dir)? {
            if version == 0 {
                continue;
            }
            let records = load_dir(&dir.join("bodies")).map_err(|error| BodiesError::Version {
                version,
                source: Box::new(error),
            })?;
            versions.insert(version, records);
        }
        Ok(Self { versions })
    }

    /// # Errors
    /// The version is unknown or older than the retained snapshots.
    pub fn at(
        &self,
        version: CommunitySnapshotVersion,
    ) -> Result<&BTreeMap<String, BodyRecord>, SnapshotVersionError> {
        if let Some(records) = self.versions.get(&version.get()) {
            return Ok(records);
        }
        let oldest = self
            .versions
            .keys()
            .copied()
            .filter(|published| *published > 0)
            .min()
            .map(CommunitySnapshotVersion::new);
        match oldest {
            Some(oldest) if version < oldest => {
                Err(SnapshotVersionError::TooOld { version, oldest })
            }
            _ => Err(SnapshotVersionError::Unknown { version }),
        }
    }

    pub fn current_version(&self) -> CommunitySnapshotVersion {
        self.versions.keys().next_back().copied().map_or(
            CommunitySnapshotVersion::SHIPPED,
            CommunitySnapshotVersion::new,
        )
    }

    pub fn get(&self, body: &BodyId) -> Option<&BodyRecord> {
        self.current()
            .and_then(|records| records.get(body.as_str()))
    }

    pub fn records(&self) -> impl Iterator<Item = (&String, &BodyRecord)> {
        self.current()
            .into_iter()
            .flat_map(|records| records.iter())
    }

    pub fn len(&self) -> usize {
        self.current().map_or(0, BTreeMap::len)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn current(&self) -> Option<&BTreeMap<String, BodyRecord>> {
        self.versions.values().next_back()
    }
}

fn published_snapshots(dir: &Path) -> Result<Vec<(u64, PathBuf)>, BodiesError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut versions = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(version) = name
            .strip_prefix('v')
            .and_then(|digits| digits.parse().ok())
        else {
            continue;
        };
        if path.is_dir() {
            versions.push((version, path));
        }
    }
    versions.sort_by_key(|(version, _)| *version);
    Ok(versions)
}

fn load_dir(dir: &Path) -> Result<BTreeMap<String, BodyRecord>, BodiesError> {
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
    Ok(records)
}

fn join_errors(errors: &[looklab_schema::ValidationError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SettingsProblem {
    #[error("no epoch covers the firmware version")]
    NoEpoch,
    #[error("the epoch has no native simulation `{0}`")]
    UnknownSim(SimKey),
    #[error("`{0}` is a monochrome film simulation; B&W renders aren't supported yet")]
    MonochromeSim(SimKey),
    #[error("control `{0}` is not in the schema")]
    UnknownControl(ControlKey),
    #[error("control `{0}` is outside its steps")]
    StepOutOfRange(ControlKey),
    #[error("control `{0}` is outside its tolerated values")]
    SpatialNotTolerated(ControlKey),
    #[error(transparent)]
    Source(#[from] SettingsSourceError),
}

pub fn validate_settings(
    record: &BodyRecord,
    firmware: &str,
    sim_key: &SimKey,
    settings: &ControlSettings,
    card: bool,
) -> Result<(), SettingsProblem> {
    let epoch = resolve_epoch(&record.schema, firmware).ok_or(SettingsProblem::NoEpoch)?;
    let sim = epoch
        .native_sim(sim_key)
        .ok_or_else(|| SettingsProblem::UnknownSim(sim_key.clone()))?;
    if sim.monochrome {
        return Err(SettingsProblem::MonochromeSim(sim_key.clone()));
    }

    for entry in settings.entries() {
        let control = epoch
            .controls
            .iter()
            .find(|control| control.key.as_str() == entry.key.as_str())
            .ok_or_else(|| SettingsProblem::UnknownControl(entry.key.clone()))?;
        if !step_in_range(control, entry.step.get()) {
            return Err(SettingsProblem::StepOutOfRange(entry.key.clone()));
        }
        if control.spatial
            && let Some(Tolerated::Values(allowed)) = control.tolerated.as_ref()
        {
            let value = scalar_at(control, entry.step.get())
                .ok_or_else(|| SettingsProblem::StepOutOfRange(entry.key.clone()))?;
            if !allowed.contains(&value) {
                return Err(SettingsProblem::SpatialNotTolerated(entry.key.clone()));
            }
        }
    }

    validate_setting_sources(
        settings,
        card,
        &SettingReach {
            mapped: mapped_controls(epoch),
            unmapped: unmapped_controls(epoch),
        },
    )?;
    Ok(())
}

/// # Errors
/// An image's makernotes select a monochrome sim, whatever `sim_key` claims.
pub fn validate_render_sims<'a>(
    record: &BodyRecord,
    firmware: &str,
    images: impl IntoIterator<Item = &'a ManifestMetadata>,
) -> Result<(), SettingsProblem> {
    let epoch = resolve_epoch(&record.schema, firmware).ok_or(SettingsProblem::NoEpoch)?;
    for metadata in images {
        if let Some(sim) = monochrome_sim(epoch, &MakernoteValues::from(metadata)) {
            return Err(SettingsProblem::MonochromeSim(sim));
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
    use looklab_upload::ControlEntry;

    use super::*;

    const SCHEMA: &str = r#"
body: fujifilm/x-t5
epochs:
  - id: 1
    firmware: { from: "1.00", to: "1.99" }
    native_sims:
      - key: classic_chrome
        look: classic-chrome
        makernote: { tag: FilmMode, value: 0x0600 }
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
        makernote: { tag: HighlightTone, encoding: { scale: -16 } }
      - key: color_chrome_effect
        type: enum
        values: [off, weak, strong]
        default: off
        look_affecting: true
        affects: [chroma]
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

    fn entry(key: &str, step: i64, source: looklab_upload::SettingSource) -> ControlEntry {
        ControlEntry::new(
            ControlKey::new(key).expect("control key"),
            looklab_upload::StepIndex::new(step),
            source,
        )
    }

    fn settings(entries: Vec<(&str, i64)>) -> ControlSettings {
        sourced(entries, looklab_upload::SettingSource::Reproduced)
    }

    fn sourced(
        entries: Vec<(&str, i64)>,
        source: looklab_upload::SettingSource,
    ) -> ControlSettings {
        ControlSettings::new(
            entries
                .into_iter()
                .map(|(key, step)| entry(key, step, source))
                .collect(),
        )
        .expect("unique settings")
    }

    fn with(settings: ControlSettings, entry: ControlEntry) -> ControlSettings {
        let mut entries = settings
            .entries()
            .iter()
            .filter(|candidate| candidate.key != entry.key)
            .cloned()
            .collect::<Vec<_>>();
        entries.push(entry);
        ControlSettings::new(entries).expect("unique settings")
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
        let complete = with(
            settings(vec![("highlight_tone", 2)]),
            entry(
                "color_chrome_effect",
                1,
                looklab_upload::SettingSource::Declared,
            ),
        );
        assert_eq!(
            validate_settings(&record, "1.50", &sim, &complete, false),
            Ok(())
        );
        assert_eq!(
            validate_settings(
                &record,
                "1.50",
                &sim,
                &settings(vec![("highlight_tone", 2)]),
                false
            ),
            Err(SettingsProblem::Source(
                SettingsSourceError::MissingLookAffectingControl {
                    key: ControlKey::new("color_chrome_effect").expect("control key"),
                }
            ))
        );
        assert_eq!(
            validate_settings(
                &record,
                "1.50",
                &sim,
                &settings(vec![("highlight_tone", 2)]),
                true
            ),
            Ok(())
        );
        assert_eq!(
            validate_settings(
                &record,
                "1.50",
                &sim,
                &settings(vec![("highlight_tone", 2), ("color_chrome_effect", 1)]),
                false
            ),
            Err(SettingsProblem::Source(
                SettingsSourceError::ReproducedWithoutMapping {
                    key: ControlKey::new("color_chrome_effect").expect("control key"),
                }
            ))
        );
        assert_eq!(
            validate_settings(
                &record,
                "1.50",
                &sim,
                &with(
                    complete.clone(),
                    entry(
                        "highlight_tone",
                        99,
                        looklab_upload::SettingSource::Reproduced
                    )
                ),
                false
            ),
            Err(SettingsProblem::StepOutOfRange(
                ControlKey::new("highlight_tone").expect("control key")
            ))
        );
        assert_eq!(
            validate_settings(
                &record,
                "1.50",
                &sim,
                &with(
                    complete.clone(),
                    entry("grain", 1, looklab_upload::SettingSource::Reproduced)
                ),
                false
            ),
            Err(SettingsProblem::SpatialNotTolerated(
                ControlKey::new("grain").expect("control key")
            ))
        );
        assert_eq!(
            validate_settings(
                &record,
                "1.50",
                &sim,
                &with(
                    complete.clone(),
                    entry("color", 0, looklab_upload::SettingSource::Reproduced)
                ),
                false
            ),
            Err(SettingsProblem::UnknownControl(
                ControlKey::new("color").expect("control key")
            ))
        );
        assert_eq!(
            validate_settings(
                &record,
                "1.50",
                &sim,
                &with(
                    complete,
                    entry("paused", 7, looklab_upload::SettingSource::Reproduced)
                ),
                false
            ),
            Err(SettingsProblem::StepOutOfRange(
                ControlKey::new("paused").expect("control key")
            ))
        );
    }

    #[test]
    fn snapshot_versions_resolve_or_report_why_not() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/bodies");
        let dir = tempfile::tempdir().expect("temp dir");
        let snapshot = dir.path().join("community/v3/bodies");
        std::fs::create_dir_all(&snapshot).expect("snapshot dir");
        for entry in std::fs::read_dir(&repo).expect("shipped bodies") {
            let path = entry.expect("entry").path();
            let name = path.file_name().expect("file name");
            std::fs::copy(&path, snapshot.join(name)).expect("snapshot copy");
        }
        let bodies = Bodies::load(&repo, &dir.path().join("community")).expect("bodies load");
        let body = BodyId::new("fujifilm/x-t5").expect("body id");

        assert!(
            bodies
                .at(CommunitySnapshotVersion::SHIPPED)
                .expect("v0")
                .contains_key(body.as_str())
        );
        assert!(
            bodies
                .at(CommunitySnapshotVersion::new(3))
                .expect("v3")
                .contains_key(body.as_str())
        );
        assert_eq!(bodies.current_version(), CommunitySnapshotVersion::new(3));
        assert!(matches!(
            bodies.at(CommunitySnapshotVersion::new(2)),
            Err(SnapshotVersionError::TooOld { .. })
        ));
        assert!(matches!(
            bodies.at(CommunitySnapshotVersion::new(4)),
            Err(SnapshotVersionError::Unknown { .. })
        ));
    }
}
