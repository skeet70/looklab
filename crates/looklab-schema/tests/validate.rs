// SPDX-License-Identifier: AGPL-3.0-or-later
//! Validation tests for the shipped data files and each schema rule.

use looklab_schema::{
    ErrorKind, MappingStatus, Rational, validate_body, validate_control_schema,
    validate_interface_profile,
};

const HEADER: &str =
    "body: test/body\nepochs:\n  - id: 1\n    firmware: { from: \"1.00\", to: \"1.99\" }\n";

fn schema_with(control: &str) -> String {
    format!(
        "{HEADER}    native_sims: []\n    exposure_modes: []\n    controls:\n{control}    solve_plan: []\n"
    )
}

fn kinds(control: &str) -> Vec<ErrorKind> {
    validate_control_schema(&schema_with(control))
        .unwrap_err()
        .into_iter()
        .map(|error| error.kind)
        .collect()
}

fn has_kind(kinds: &[ErrorKind], expected: &ErrorKind) -> bool {
    kinds.iter().any(|kind| kind == expected)
}

#[test]
fn shipped_files_validate() {
    let schema =
        validate_control_schema(include_str!("../../../data/bodies/fujifilm-x-t5.yaml")).unwrap();
    assert_eq!(schema.body.as_str(), "fujifilm/x-t5");
    assert_eq!(schema.epochs.len(), 1);

    let body = validate_body(include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml")).unwrap();
    assert_eq!(body.id.as_str(), "fujifilm/x-t5");
    assert_eq!(body.mapping_status, MappingStatus::Provisional);

    let profile = validate_interface_profile(include_str!(
        "../../../data/interfaces/example-firmware-sim-v1.yaml"
    ))
    .unwrap();
    assert_eq!(profile.id.as_str(), "example-firmware/sim-v1");
    assert_eq!(profile.limits.max_bytes, 262_144);
}

#[test]
fn exposure_compensation_steps_are_thirds() {
    let schema =
        validate_control_schema(include_str!("../../../data/bodies/fujifilm-x-t5.yaml")).unwrap();
    let control = schema.epochs[0]
        .controls
        .iter()
        .find(|control| control.key.as_str() == "exposure_compensation")
        .unwrap();
    assert_eq!(control.steps(), -9..=9);
    assert_eq!(control.value_at(1), Some(Rational::new(1, 3).unwrap()));
}

#[test]
fn rejects_numeric_default_of_wrong_type() {
    let kinds = kinds(
        "      - key: n\n        type: int\n        range: [0, 4]\n        step: 1\n        default: off\n",
    );
    assert!(has_kind(
        &kinds,
        &ErrorKind::InvalidValue { expected: "number" }
    ));
}

#[test]
fn rejects_default_out_of_range() {
    let yaml = schema_with(
        "      - key: n\n        type: int\n        range: [0, 4]\n        step: 1\n        default: 5\n",
    );
    let errors = validate_control_schema(&yaml).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.kind == ErrorKind::DefaultOutOfRange
                && error.path == "epochs[0].controls[0].default")
    );
}

#[test]
fn rejects_default_not_on_step() {
    let kinds = kinds(
        "      - key: n\n        type: decimal\n        range: [0.0, 1.0]\n        step: 0.5\n        default: 0.3\n",
    );
    assert!(has_kind(&kinds, &ErrorKind::NotOnStep));
}

#[test]
fn rejects_range_endpoint_not_on_step() {
    let kinds = kinds(
        "      - key: n\n        type: decimal\n        range: [0.1, 1.0]\n        step: 0.5\n        default: 0.5\n",
    );
    assert!(has_kind(&kinds, &ErrorKind::NotOnStep));
}

#[test]
fn rejects_enum_default_not_in_values() {
    let kinds =
        kinds("      - key: n\n        type: enum\n        values: [a, b]\n        default: c\n");
    assert!(has_kind(&kinds, &ErrorKind::EnumDefaultNotInValues));
}

#[test]
fn rejects_unknown_affects_vocabulary() {
    let kinds = kinds(
        "      - key: n\n        type: int\n        range: [0, 4]\n        step: 1\n        default: 0\n        affects: [color]\n",
    );
    assert!(has_kind(
        &kinds,
        &ErrorKind::UnknownAffect {
            value: "color".to_owned()
        }
    ));
}

#[test]
fn rejects_look_affecting_without_affects() {
    let kinds = kinds(
        "      - key: n\n        type: int\n        range: [0, 4]\n        step: 1\n        default: 0\n        look_affecting: true\n",
    );
    assert!(has_kind(&kinds, &ErrorKind::AffectsRequired));
}

#[test]
fn rejects_spatial_without_tolerated() {
    let kinds = kinds(
        "      - key: n\n        type: int\n        range: [0, 4]\n        step: 1\n        default: 0\n        spatial: true\n",
    );
    assert!(has_kind(&kinds, &ErrorKind::SpatialToleratedRequired));
}

#[test]
fn rejects_missing_decimal_step() {
    let kinds = kinds(
        "      - key: n\n        type: decimal\n        range: [0.0, 1.0]\n        default: 0.0\n",
    );
    assert!(has_kind(&kinds, &ErrorKind::MissingField { field: "step" }));
}

#[test]
fn rejects_zero_step() {
    let kinds = kinds(
        "      - key: n\n        type: decimal\n        range: [0.0, 1.0]\n        step: 0\n        default: 0.0\n",
    );
    assert!(has_kind(&kinds, &ErrorKind::StepNotPositive));
}

#[test]
fn rejects_field_not_allowed_for_type() {
    let kinds = kinds(
        "      - key: n\n        type: enum\n        values: [a]\n        range: [0, 1]\n        default: a\n",
    );
    assert!(has_kind(
        &kinds,
        &ErrorKind::FieldNotAllowed {
            field: "range",
            control_type: looklab_schema::ControlType::Enum,
        }
    ));
}

#[test]
fn rejects_duplicate_control_keys() {
    let kinds = kinds(
        "      - key: n\n        type: int\n        range: [0, 4]\n        step: 1\n        default: 0\n      - key: n\n        type: int\n        range: [0, 4]\n        step: 1\n        default: 0\n",
    );
    assert!(has_kind(
        &kinds,
        &ErrorKind::DuplicateKey {
            key: "n".to_owned()
        }
    ));
}

#[test]
fn rejects_duplicate_sim_keys() {
    let yaml = format!(
        "{HEADER}    native_sims:\n      - key: s\n        look: a\n        makernote: {{ tag: T, value: 1 }}\n      - key: s\n        look: b\n        makernote: {{ tag: T, value: 2 }}\n    exposure_modes: []\n    controls: []\n    solve_plan: []\n"
    );
    let errors = validate_control_schema(&yaml).unwrap_err();
    assert!(errors.iter().any(|error| matches!(
        &error.kind,
        ErrorKind::DuplicateKey { key } if key == "s"
    )));
}

#[test]
fn rejects_duplicate_exposure_mode_keys() {
    let yaml = format!(
        "{HEADER}    native_sims: []\n    exposure_modes:\n      - key: dr100\n      - key: dr100\n    controls: []\n    solve_plan: []\n"
    );
    let errors = validate_control_schema(&yaml).unwrap_err();
    assert!(errors.iter().any(|error| matches!(
        &error.kind,
        ErrorKind::DuplicateKey { key } if key == "dr100"
    )));
}

#[test]
fn rejects_inverted_firmware_range() {
    let yaml = "body: test/body\nepochs:\n  - id: 1\n    firmware: { from: \"2.00\", to: \"1.00\" }\n    native_sims: []\n    exposure_modes: []\n    controls: []\n    solve_plan: []\n";
    let errors = validate_control_schema(yaml).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.kind == ErrorKind::FirmwareRangeInverted)
    );
}

#[test]
fn rejects_overlapping_firmware_ranges() {
    let yaml = "body: test/body\nepochs:\n  - id: 1\n    firmware: { from: \"1.00\", to: \"2.00\" }\n    native_sims: []\n    exposure_modes: []\n    controls: []\n    solve_plan: []\n  - id: 2\n    firmware: { from: \"1.50\", to: \"3.00\" }\n    native_sims: []\n    exposure_modes: []\n    controls: []\n    solve_plan: []\n";
    let errors = validate_control_schema(yaml).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.kind == ErrorKind::FirmwareRangeOverlap)
    );
}

#[test]
fn rejects_solve_plan_unknown_control() {
    let yaml = format!(
        "{HEADER}    native_sims: []\n    exposure_modes: []\n    controls:\n      - key: n\n        type: int\n        range: [0, 4]\n        step: 1\n        default: 0\n    solve_plan:\n      - stage: tone\n        controls: [missing]\n"
    );
    let errors = validate_control_schema(&yaml).unwrap_err();
    assert!(errors.iter().any(|error| matches!(
        &error.kind,
        ErrorKind::SolvePlanUnknownControl { key } if key == "missing"
    )));
}

#[test]
fn rejects_solve_plan_repeating_a_control() {
    let yaml = format!(
        "{HEADER}    native_sims: []\n    exposure_modes: []\n    controls:\n      - key: n\n        type: int\n        range: [0, 4]\n        step: 1\n        default: 0\n    solve_plan:\n      - stage: tone\n        controls: [n]\n      - stage: color\n        controls: [n]\n"
    );
    let errors = validate_control_schema(&yaml).unwrap_err();
    assert!(errors.iter().any(|error| matches!(
        &error.kind,
        ErrorKind::SolvePlanDuplicateControl { key } if key == "n"
    )));
}

#[test]
fn rejects_unknown_fields() {
    let schema = schema_with(
        "      - key: n\n        type: int\n        range: [0, 4]\n        step: 1\n        default: 0\n        bogus: 1\n",
    );
    assert!(validate_control_schema(&schema).is_err());
    assert!(
        validate_body(
            "id: a/b\nmake: A\nmodel: B\nkind: camera\ncapabilities: { in_camera_raw_conversion: true, jpeg_color_spaces: [srgb], heif: false }\nstatus: active\nmapping_status: provisional\nbogus: 1\n"
        )
        .is_err()
    );

    let profile = include_str!("../../../data/interfaces/example-firmware-sim-v1.yaml")
        .replace("    sizes: [17, 33]", "    sizes: [17, 33]\n    bogus: 1");
    assert!(validate_interface_profile(&profile).is_err());
}

#[cfg(feature = "fs")]
#[test]
fn shipped_directory_validates() {
    let errors =
        looklab_schema::validate_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data")).unwrap();
    assert!(errors.is_empty(), "{errors:?}");
}
