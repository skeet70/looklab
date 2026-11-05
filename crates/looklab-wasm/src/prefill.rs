// SPDX-License-Identifier: AGPL-3.0-or-later
//! Contribution settings read from a file's makernotes.

use looklab_meta::ParsedMetadata;
use looklab_schema::{
    MakernoteValues, settings_from_makernote_partial, sim_from_makernote, validate_control_schema,
};
use serde::Serialize;

use crate::error::FacadeError;

/// The batch fields the contribution form prefills.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SettingsPrefill {
    pub firmware: String,
    pub sim: Option<String>,
    /// `(control key, step)` in schema order.
    pub controls: Vec<(String, i64)>,
    /// Controls the tags did not determine, with the reason.
    pub unavailable: Vec<String>,
}

/// Derives the settings of `files` through the body's control schema.
///
/// # Errors
/// The schema does not validate, no file carries an EXIF firmware version, or
/// no epoch covers it.
pub fn prefill_settings(
    schema_yaml: &str,
    files: &[(String, Vec<u8>)],
) -> Result<SettingsPrefill, FacadeError> {
    let schema = validate_control_schema(schema_yaml).map_err(FacadeError::Schema)?;
    let (firmware, metadata) = first_metadata(files)
        .ok_or_else(|| FacadeError::Input("the files carry no EXIF firmware".to_owned()))?;
    let epoch = schema
        .epoch_for(&firmware)
        .ok_or_else(|| FacadeError::Input(format!("no epoch covers firmware {firmware}")))?;
    let values = values_of(&metadata);
    let (settings, problems) = settings_from_makernote_partial(&schema, epoch.id, &values);
    Ok(SettingsPrefill {
        firmware,
        sim: sim_from_makernote(epoch, &values)
            .ok()
            .map(|sim| sim.to_string()),
        controls: settings
            .entries()
            .iter()
            .map(|setting| (setting.key.to_string(), setting.step))
            .collect(),
        unavailable: problems.iter().map(ToString::to_string).collect(),
    })
}

fn first_metadata(files: &[(String, Vec<u8>)]) -> Option<(String, ParsedMetadata)> {
    files.iter().find_map(|(_, bytes)| {
        let metadata = looklab_meta::parse(bytes).ok()?;
        let firmware = metadata.firmware.clone()?;
        Some((firmware, metadata))
    })
}

fn values_of(metadata: &ParsedMetadata) -> MakernoteValues {
    let mut values = metadata
        .fuji
        .as_ref()
        .map(|fuji| MakernoteValues::from(&fuji.look))
        .unwrap_or_default();
    if let Some(ev) = metadata.exif.exposure_compensation {
        values.set_exposure_compensation_ev(ev.ev());
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefills_the_fixture_from_its_makernotes() {
        let raw = include_bytes!("../../../fixtures/fujifilm-x-s10.raf").to_vec();
        let schema = include_str!("../../../data/bodies/fujifilm-x-t5.yaml");
        let prefill =
            prefill_settings(schema, &[("fujifilm-x-s10.raf".to_owned(), raw)]).expect("prefill");
        assert_eq!(prefill.firmware, "1.00");
        assert_eq!(prefill.sim.as_deref(), Some("provia"));
        assert_eq!(prefill.controls.len(), 12);
        assert!(prefill.unavailable.is_empty(), "{:?}", prefill.unavailable);
    }
}
