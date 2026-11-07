// SPDX-License-Identifier: AGPL-3.0-or-later
//! Makernote mappings: schema validation and settings derivation.

use looklab_meta::FujiTag;
use looklab_schema::{
    ControlKey, ErrorKind, MakernoteValues, MappingError, settings_from_makernote,
    settings_from_makernote_partial, sim_from_makernote, validate_control_schema,
};

const HEADER: &str =
    "body: test/body\nepochs:\n  - id: 1\n    firmware: { from: \"1.00\", to: \"1.99\" }\n";

fn schema_with(control: &str) -> String {
    format!(
        "{HEADER}    native_sims: []\n    exposure_modes: []\n    controls:\n{control}    solve_plan: []\n"
    )
}

fn kinds_of(control: &str) -> Vec<ErrorKind> {
    validate_control_schema(&schema_with(control))
        .unwrap_err()
        .into_iter()
        .map(|error| error.kind)
        .collect()
}

fn shipped() -> looklab_schema::ControlSchema {
    validate_control_schema(include_str!("../../../data/bodies/fujifilm-x-t5.yaml")).unwrap()
}

fn key(name: &str) -> ControlKey {
    ControlKey::new(name).unwrap()
}

fn fixture_values() -> MakernoteValues {
    let mut values = MakernoteValues::new();
    values.insert(FujiTag::DevelopmentDynamicRange, vec![100]);
    values.insert(FujiTag::HighlightTone, vec![0]);
    values.insert(FujiTag::ShadowTone, vec![0]);
    values.insert(FujiTag::Saturation, vec![0]);
    values.insert(FujiTag::ColorChromeEffect, vec![32]);
    values.insert(FujiTag::ColorChromeFxBlue, vec![64]);
    values.insert(FujiTag::WhiteBalanceFineTune, vec![20, -40]);
    values.insert(FujiTag::GrainEffectRoughness, vec![48]);
    values.insert(FujiTag::Clarity, vec![-2000]);
    values.insert(FujiTag::NoiseReduction, vec![0x2c0]);
    values.set_exposure_compensation_ev(1.0 / 3.0);
    values
}

#[test]
fn the_shipped_schema_maps_every_control() {
    let schema = shipped();
    for control in &schema.epochs[0].controls {
        assert!(
            control.makernote.is_some(),
            "{} has no mapping",
            control.key
        );
    }
    let settings = settings_from_makernote(&schema, 1, &fixture_values()).expect("settings");
    let steps: Vec<(&str, i64)> = settings
        .entries()
        .iter()
        .map(|setting| (setting.key.as_str(), setting.step))
        .collect();
    assert_eq!(
        steps,
        vec![
            ("dynamic_range", 0),
            ("highlight_tone", 0),
            ("shadow_tone", 0),
            ("color", 0),
            ("color_chrome_effect", 1),
            ("color_chrome_fx_blue", 2),
            ("wb_shift_r", 1),
            ("wb_shift_b", -2),
            ("exposure_compensation", 1),
            ("grain", 2),
            ("clarity", -2),
            ("noise_reduction", -3),
        ]
    );
}

#[test]
fn the_sim_comes_from_the_declared_native_sims() {
    let schema = shipped();
    let epoch = &schema.epochs[0];
    let mut values = MakernoteValues::new();
    values.insert(FujiTag::FilmMode, vec![0x0600]);
    assert_eq!(
        sim_from_makernote(epoch, &values).unwrap().as_str(),
        "classic_chrome"
    );

    values.insert(FujiTag::FilmMode, vec![0x0500]);
    assert_eq!(
        sim_from_makernote(epoch, &values),
        Err(MappingError::UnknownSim)
    );
}

#[test]
fn an_absent_tag_falls_back() {
    let schema = shipped();
    let mut values = MakernoteValues::new();
    values.insert(FujiTag::DynamicRangeSetting, vec![0x200]);
    let (settings, _) = settings_from_makernote_partial(&schema, 1, &values);
    let setting = settings.get(&key("dynamic_range")).unwrap();
    assert_eq!(setting.step, 1);
    assert_eq!(setting.source.name(), "DynamicRangeSetting");
}

#[test]
fn a_missing_tag_fails_closed() {
    let schema = shipped();
    let values = MakernoteValues::new();
    let error = settings_from_makernote(&schema, 1, &values).unwrap_err();
    assert!(
        matches!(error, MappingError::MissingTag { .. }),
        "{error:?}"
    );
}

#[test]
fn an_off_grid_exposure_value_is_not_a_tag_value() {
    let schema = shipped();
    let mut values = MakernoteValues::new();
    values.set_exposure_compensation_ev(0.2);
    let error = settings_from_makernote(&schema, 1, &values).unwrap_err();
    assert!(
        matches!(error, MappingError::MissingTag { .. }),
        "{error:?}"
    );
}

#[test]
fn a_control_without_a_mapping_is_omitted() {
    let yaml = schema_with(
        "      - key: n\n        type: int\n        range: [0, 4]\n        step: 1\n        default: 0\n",
    );
    let schema = validate_control_schema(&yaml).unwrap();
    let settings = settings_from_makernote(&schema, 1, &MakernoteValues::new()).unwrap();
    assert!(settings.entries().is_empty());
}

#[test]
fn a_scale_mapping_accepts_every_step_and_rejects_others() {
    let yaml = schema_with(
        "      - key: tone\n        type: decimal\n        range: [-2.0, 4.0]\n        step: 0.5\n        default: 0.0\n        makernote: { tag: HighlightTone, encoding: { scale: -16 } }\n",
    );
    let schema = validate_control_schema(&yaml).unwrap();
    let mut values = MakernoteValues::new();
    values.insert(FujiTag::HighlightTone, vec![-24]);
    assert_eq!(
        settings_from_makernote(&schema, 1, &values)
            .unwrap()
            .get(&key("tone"))
            .unwrap()
            .step,
        3
    );

    values.insert(FujiTag::HighlightTone, vec![-4]);
    let error = settings_from_makernote(&schema, 1, &values).unwrap_err();
    assert!(matches!(error, MappingError::NotOnStep { .. }), "{error:?}");
}

#[test]
fn a_table_mapping_needs_a_covered_default() {
    let kinds = kinds_of(
        "      - key: effect\n        type: enum\n        values: [off, weak, strong]\n        default: off\n        makernote: { tag: ColorChromeEffect, values: { off: 0, weak: 32 } }\n",
    );
    assert!(kinds.contains(&ErrorKind::MakernoteValueMissing {
        value: "strong".to_owned()
    }));
}

#[test]
fn a_table_mapping_rejects_unknown_values_and_duplicate_raws() {
    let kinds = kinds_of(
        "      - key: effect\n        type: enum\n        values: [off, weak, strong]\n        default: off\n        makernote: { tag: ColorChromeEffect, values: { off: 0, weak: 32, strong: 64, extreme: 96 } }\n",
    );
    assert!(kinds.contains(&ErrorKind::MakernoteValueUnknown {
        value: "extreme".to_owned()
    }));

    let kinds = kinds_of(
        "      - key: effect\n        type: enum\n        values: [off, weak, strong]\n        default: off\n        makernote: { tag: ColorChromeEffect, values: { off: 0, weak: 0, strong: 64 } }\n",
    );
    assert!(kinds.contains(&ErrorKind::MakernoteDuplicateRaw { raw: 0 }));
}

#[test]
fn numeric_table_keys_must_be_steps_in_range() {
    let kinds = kinds_of(
        "      - key: n\n        type: int\n        range: [0, 4]\n        step: 1\n        default: 0\n        makernote: { tag: Clarity, values: { 0: 0, 2: 2000, 5: 5000 } }\n",
    );
    assert!(kinds.contains(&ErrorKind::MakernoteValueOutOfRange {
        value: "5".to_owned()
    }));

    let kinds = kinds_of(
        "      - key: n\n        type: int\n        range: [0, 4]\n        step: 1\n        default: 0\n        makernote: { tag: Clarity, values: { 0.5: 500 } }\n",
    );
    assert!(kinds.contains(&ErrorKind::NotOnStep));
}

#[test]
fn a_scale_needs_a_numeric_control_and_integer_raws() {
    let kinds = kinds_of(
        "      - key: effect\n        type: enum\n        values: [off, weak, strong]\n        default: off\n        makernote: { tag: ColorChromeEffect, encoding: { scale: 1 } }\n",
    );
    assert!(kinds.contains(&ErrorKind::MakernoteScaleNotNumeric));

    let kinds = kinds_of(
        "      - key: n\n        type: decimal\n        range: [0.0, 1.0]\n        step: 1/3\n        default: 0.0\n        makernote: { tag: Clarity, encoding: { scale: 1 } }\n",
    );
    assert!(kinds.contains(&ErrorKind::MakernoteRawNotInteger {
        value: "1/3".to_owned()
    }));
}

#[test]
fn a_mapping_names_one_encoding_and_one_known_tag() {
    let kinds = kinds_of(
        "      - key: n\n        type: int\n        range: [0, 4]\n        step: 1\n        default: 0\n        makernote: { tag: HighlightTone }\n",
    );
    assert_eq!(kinds, vec![ErrorKind::MissingMakernoteEncoding]);

    let kinds = kinds_of(
        "      - key: n\n        type: int\n        range: [0, 4]\n        step: 1\n        default: 0\n        makernote: { tag: HighlightTone, encoding: { scale: 1 }, values: { 0: 0 } }\n",
    );
    assert_eq!(kinds, vec![ErrorKind::ConflictingMakernoteEncoding]);

    let kinds = kinds_of(
        "      - key: n\n        type: int\n        range: [0, 4]\n        step: 1\n        default: 0\n        makernote: { tag: NotATag, encoding: { scale: 1 } }\n",
    );
    assert_eq!(
        kinds,
        vec![ErrorKind::UnknownMakernoteTag {
            tag: "NotATag".to_owned()
        }]
    );
}

#[test]
fn paired_tags_require_and_bound_the_component() {
    let kinds = kinds_of(
        "      - key: n\n        type: int\n        range: [-9, 9]\n        step: 1\n        default: 0\n        makernote: { tag: WhiteBalanceFineTune, encoding: { scale: 20 } }\n",
    );
    assert_eq!(kinds, vec![ErrorKind::MakernoteComponentRequired]);

    let kinds = kinds_of(
        "      - key: n\n        type: int\n        range: [-9, 9]\n        step: 1\n        default: 0\n        makernote: { tag: WhiteBalanceFineTune, component: 2, encoding: { scale: 20 } }\n",
    );
    assert_eq!(
        kinds,
        vec![ErrorKind::MakernoteComponentOutOfRange {
            component: 2,
            components: 2
        }]
    );

    let kinds = kinds_of(
        "      - key: n\n        type: decimal\n        range: [-2.0, 4.0]\n        step: 0.5\n        default: 0.0\n        makernote: { tag: HighlightTone, component: 0, encoding: { scale: -16 } }\n",
    );
    assert_eq!(kinds, vec![ErrorKind::MakernoteComponentNotAllowed]);
}
