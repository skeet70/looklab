// SPDX-License-Identifier: AGPL-3.0-or-later
//! Declared and reproduced control values against a snapshot's control reach.

use looklab_upload::{
    ControlEntry, ControlKey, ControlSettings, SettingReach, SettingSource, SettingsSourceError,
    validate_setting_sources,
};

fn key(value: &str) -> ControlKey {
    ControlKey::new(value).expect("control key")
}

fn reach() -> SettingReach {
    SettingReach {
        mapped: vec![key("highlight_tone")],
        unmapped: vec![key("color_chrome_effect")],
    }
}

fn settings(entries: Vec<ControlEntry>) -> ControlSettings {
    ControlSettings::new(entries).expect("unique settings")
}

fn the_key(error: SettingsSourceError) -> String {
    match error {
        SettingsSourceError::MissingLookAffectingControl { key }
        | SettingsSourceError::ReproducedWithoutMapping { key } => key.to_string(),
    }
}

#[test]
fn a_declared_value_covers_a_control_the_snapshot_does_not_map() {
    let settings = settings(vec![
        ControlEntry::reproduced(key("highlight_tone"), looklab_upload::StepIndex::new(1)),
        ControlEntry::declared(
            key("color_chrome_effect"),
            looklab_upload::StepIndex::new(2),
        ),
    ]);
    assert_eq!(validate_setting_sources(&settings, false, &reach()), Ok(()));
}

#[test]
fn an_absent_look_affecting_control_fails() {
    let settings = settings(vec![ControlEntry::reproduced(
        key("highlight_tone"),
        looklab_upload::StepIndex::new(1),
    )]);
    let error = validate_setting_sources(&settings, false, &reach()).expect_err("missing control");
    assert!(matches!(
        error,
        SettingsSourceError::MissingLookAffectingControl { .. }
    ));
    assert_eq!(the_key(error), "color_chrome_effect");
}

#[test]
fn a_reproduced_claim_needs_a_snapshot_mapping() {
    let settings = settings(vec![
        ControlEntry::reproduced(key("highlight_tone"), looklab_upload::StepIndex::new(1)),
        ControlEntry::reproduced(
            key("color_chrome_effect"),
            looklab_upload::StepIndex::new(0),
        ),
    ]);
    let error = validate_setting_sources(&settings, false, &reach()).expect_err("no mapping");
    assert!(matches!(
        error,
        SettingsSourceError::ReproducedWithoutMapping { .. }
    ));
    assert_eq!(the_key(error), "color_chrome_effect");
}

#[test]
fn a_card_render_is_exempt_from_covering_every_control() {
    let settings = settings(vec![ControlEntry::declared(
        key("highlight_tone"),
        looklab_upload::StepIndex::new(3),
    )]);
    assert_eq!(validate_setting_sources(&settings, true, &reach()), Ok(()));
    assert_ne!(validate_setting_sources(&settings, false, &reach()), Ok(()));
}

#[test]
fn a_card_render_cannot_claim_a_reproduced_value_without_a_mapping() {
    let settings = settings(vec![ControlEntry::new(
        key("color_chrome_effect"),
        looklab_upload::StepIndex::new(0),
        SettingSource::Reproduced,
    )]);
    assert!(matches!(
        validate_setting_sources(&settings, true, &reach()),
        Err(SettingsSourceError::ReproducedWithoutMapping { .. })
    ));
}
