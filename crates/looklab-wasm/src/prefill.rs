// SPDX-License-Identifier: AGPL-3.0-or-later
//! Contribution settings read from a file's makernotes.

use looklab_meta::ParsedMetadata;
use looklab_prep::pair::{FileKind, LocalImageKey};
use looklab_schema::{
    ApplicableControls, Epoch, LookId, MakernoteValues, MonoFilter, RenderClass, SimKey,
    applicable_controls, classify_render, mapped_controls, settings_from_makernote_for,
    settings_from_makernote_partial, sim_from_makernote, unmapped_controls,
    validate_control_schema,
};
use serde::Serialize;

use crate::error::FacadeError;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct MonochromeRender {
    pub file: String,
    pub sim: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeRenderClass {
    Colour(SimKey),
    Monochrome(SimKey),
    Undetermined,
}

pub fn native_render_class(epoch: &Epoch, values: &MakernoteValues) -> NativeRenderClass {
    match classify_render(epoch, values) {
        Ok(classification) => match classification.render_class {
            RenderClass::Colour => NativeRenderClass::Colour(classification.native_sim),
            RenderClass::Monochrome => NativeRenderClass::Monochrome(classification.native_sim),
        },
        Err(_) => NativeRenderClass::Undetermined,
    }
}

/// The batch fields the contribution form prefills.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SettingsPrefill {
    pub firmware: String,
    pub sim: Option<String>,
    pub render_class: Option<RenderClass>,
    pub family: Option<LookId>,
    pub mono_filter: Option<MonoFilter>,
    /// `(control key, step)` in schema order.
    pub controls: Vec<(String, i64)>,
    /// Controls the tags did not determine, with the reason.
    pub unavailable: Vec<String>,
    /// Applicable controls without a reproduced value, which need a declaration.
    pub unmapped: Vec<String>,
    /// The batch's JPEGs that render a monochrome native sim.
    pub monochrome: Vec<MonochromeRender>,
}

/// Derives the settings of `files` through the body's control schema.
///
/// # Errors
/// The schema does not validate, no file carries an EXIF firmware version, or
/// no epoch covers it.
pub fn prefill_settings(
    schema_yaml: &str,
    files: &[(&str, &[u8])],
) -> Result<SettingsPrefill, FacadeError> {
    let parsed = parsed_files(files);
    let parsed = parsed
        .iter()
        .map(|(name, metadata)| (*name, metadata))
        .collect::<Vec<_>>();
    prefill_settings_parsed(schema_yaml, &parsed)
}

/// JPEG rendering metadata takes precedence over RAW capture metadata; missing
/// JPEG firmware falls back to another file's firmware.
///
/// # Errors
/// The schema does not validate, no file carries an EXIF firmware version, or
/// no epoch covers it.
pub fn prefill_settings_parsed(
    schema_yaml: &str,
    parsed: &[(&str, &ParsedMetadata)],
) -> Result<SettingsPrefill, FacadeError> {
    let schema = validate_control_schema(schema_yaml).map_err(FacadeError::Schema)?;
    let metadata = parsed
        .iter()
        .find(|(name, _)| is_jpeg(name))
        .or_else(|| parsed.first())
        .map(|(_, metadata)| metadata)
        .ok_or_else(|| FacadeError::Input("the files carry no readable EXIF".to_owned()))?;
    let firmware = metadata
        .firmware
        .as_ref()
        .or_else(|| {
            parsed
                .iter()
                .find_map(|(_, metadata)| metadata.firmware.as_ref())
        })
        .cloned()
        .ok_or_else(|| FacadeError::Input("the files carry no EXIF firmware".to_owned()))?;
    let epoch = schema
        .epoch_for(&firmware)
        .ok_or_else(|| FacadeError::Input(format!("no epoch covers firmware {firmware}")))?;
    let values = values_of(metadata);
    let classification = classify_render(epoch, &values).ok();
    let reach = match &classification {
        Some(classification) => applicable_controls(epoch, &classification.family),
        None => ApplicableControls {
            mapped: mapped_controls(epoch),
            unmapped: unmapped_controls(epoch),
            spatial: epoch
                .controls
                .iter()
                .filter(|control| control.spatial)
                .map(|control| control.key.clone())
                .collect(),
        },
    };
    let (settings, problems) = match &classification {
        Some(classification) => {
            settings_from_makernote_for(&schema, epoch.id, &classification.family, &values)
        }
        None => settings_from_makernote_partial(&schema, epoch.id, &values),
    };
    let mut required: Vec<&looklab_schema::ControlKey> =
        reach.mapped.iter().chain(&reach.unmapped).collect();
    if classification
        .as_ref()
        .is_some_and(|classification| classification.render_class == RenderClass::Monochrome)
    {
        required.extend(reach.spatial.iter());
    }
    let monochrome = parsed
        .iter()
        .filter(|(name, _)| is_jpeg(name))
        .filter_map(
            |(name, metadata)| match native_render_class(epoch, &values_of(metadata)) {
                NativeRenderClass::Monochrome(sim) => Some(MonochromeRender {
                    file: (*name).to_owned(),
                    sim: sim.to_string(),
                }),
                NativeRenderClass::Colour(_) | NativeRenderClass::Undetermined => None,
            },
        )
        .collect();
    Ok(SettingsPrefill {
        firmware,
        sim: classification
            .as_ref()
            .map(|classification| classification.native_sim.to_string())
            .or_else(|| {
                sim_from_makernote(epoch, &values)
                    .ok()
                    .map(|sim| sim.to_string())
            }),
        render_class: classification.as_ref().map(|c| c.render_class),
        family: classification.as_ref().map(|c| c.family.clone()),
        mono_filter: classification.as_ref().and_then(|c| c.mono_filter),
        controls: settings
            .entries()
            .iter()
            .map(|setting| (setting.key.to_string(), setting.step))
            .collect(),
        unavailable: problems.iter().map(ToString::to_string).collect(),
        unmapped: required
            .into_iter()
            .filter(|key| settings.get(key).is_none())
            .map(ToString::to_string)
            .collect(),
        monochrome,
    })
}

fn parsed_files<'a>(files: &'a [(&'a str, &'a [u8])]) -> Vec<(&'a str, ParsedMetadata)> {
    files
        .iter()
        .filter_map(|(name, bytes)| looklab_meta::parse(bytes).ok().map(|meta| (*name, meta)))
        .collect()
}

fn is_jpeg(name: &str) -> bool {
    LocalImageKey::new(name).is_some_and(|key| key.kind() == FileKind::Jpeg)
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
    use looklab_meta::FujiTag;

    const SCHEMA: &str = "\
body: test/body
epochs:
  - id: 1
    firmware: { from: \"1.00\", to: \"1.99\" }
    native_sims:
      - key: acros
        look: acros
        makernote: { tag: Saturation, value: 0x0500 }
        monochrome: true
      - key: acros_ye
        look: acros
        makernote: { tag: Saturation, value: 0x0502 }
        monochrome: true
      - key: provia
        look: provia
        makernote: { tag: FilmMode, value: 0x0000 }
    exposure_modes: []
    controls: []
    solve_plan: []
";

    #[test]
    fn a_monochrome_native_sim_is_classified_apart() {
        let schema = validate_control_schema(SCHEMA).expect("the schema validates");
        let epoch = schema.epoch_for("1.00").expect("epoch");

        let mut acros = MakernoteValues::new();
        acros.insert(FujiTag::Saturation, vec![0x0500]);
        assert_eq!(
            native_render_class(epoch, &acros),
            NativeRenderClass::Monochrome("acros".try_into().expect("sim key"))
        );

        let mut acros_ye = MakernoteValues::new();
        acros_ye.insert(FujiTag::Saturation, vec![0x0502]);
        assert_eq!(
            native_render_class(epoch, &acros_ye),
            NativeRenderClass::Monochrome("acros_ye".try_into().expect("sim key"))
        );

        let mut both = MakernoteValues::new();
        both.insert(FujiTag::FilmMode, vec![0x0000]);
        both.insert(FujiTag::Saturation, vec![0x0500]);
        assert_eq!(
            native_render_class(epoch, &both),
            NativeRenderClass::Monochrome("acros".try_into().expect("sim key"))
        );

        let mut provia = MakernoteValues::new();
        provia.insert(FujiTag::FilmMode, vec![0x0000]);
        assert_eq!(
            native_render_class(epoch, &provia),
            NativeRenderClass::Colour("provia".try_into().expect("sim key"))
        );

        assert_eq!(
            native_render_class(epoch, &MakernoteValues::new()),
            NativeRenderClass::Undetermined
        );
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn prefills_the_fixture_from_its_makernotes() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/cache/fujifilm-x-s10.raf");
        let raw = std::fs::read(&path).unwrap_or_else(|error| {
            panic!(
                "read {}: {error}; run tools/fetch-fixtures.sh",
                path.display()
            )
        });
        let schema = include_str!("../../../data/bodies/fujifilm-x-t5.yaml");
        let prefill =
            prefill_settings(schema, &[("fujifilm-x-s10.raf", raw.as_slice())]).expect("prefill");
        assert_eq!(prefill.firmware, "1.00");
        assert_eq!(prefill.sim.as_deref(), Some("provia"));
        assert!(prefill.unavailable.is_empty(), "{:?}", prefill.unavailable);
        for key in &prefill.unmapped {
            assert!(
                prefill.controls.iter().all(|(name, _)| name != key),
                "{key} is both reproduced and required"
            );
        }
    }
}
