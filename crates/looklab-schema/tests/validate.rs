// SPDX-License-Identifier: AGPL-3.0-or-later
//! Validation tests for the shipped data files and each schema rule.

use looklab_schema::{
    ControlDefault, ErrorKind, FirmwareVersion, Makernote, MakernoteValue, MappingStatus,
    NativeSim, Rational, ScalarValue, Tolerated, apply_sim_mapping, validate_body,
    validate_control_schema, validate_interface_profile,
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
    assert_eq!(profile.limits.max_bytes, 2_097_152);
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
fn preserves_exact_yaml_decimals() {
    let yaml = schema_with(
        "      - key: n\n        type: decimal\n        range: [0.0, 9007199254740993.0]\n        step: 1\n        default: 9007199254740993.0\n",
    );
    let schema = validate_control_schema(&yaml).unwrap();
    let control = &schema.epochs[0].controls[0];
    let exact = Rational::parse("9007199254740993").unwrap();
    assert_eq!(control.range.as_ref().unwrap().max, exact);
    assert_eq!(control.default, ControlDefault::Number(exact));

    let yaml = schema_with(
        "      - key: n\n        type: decimal\n        range: [0.0, 1.0]\n        step: 0.00000000000000001\n        default: 0.10000000000000001\n",
    );
    let schema = validate_control_schema(&yaml).unwrap();
    let control = &schema.epochs[0].controls[0];
    assert_eq!(
        control.step,
        Some(Rational::parse("0.00000000000000001").unwrap())
    );
    assert_eq!(
        control.default,
        ControlDefault::Number(Rational::parse("0.10000000000000001").unwrap())
    );
    assert_ne!(
        control.default,
        ControlDefault::Number(Rational::new(1, 10).unwrap())
    );
}

#[test]
fn preserves_exact_tolerated_values() {
    let yaml = schema_with(
        "      - key: n\n        type: int\n        range: [0, 9007199254740993]\n        step: 1\n        default: 0\n        spatial: true\n        tolerated: [9007199254740993.0]\n",
    );
    let schema = validate_control_schema(&yaml).unwrap();
    let control = &schema.epochs[0].controls[0];
    assert_eq!(
        control.tolerated,
        Some(Tolerated::Values(vec![ScalarValue::Number(
            Rational::parse("9007199254740993").unwrap()
        )]))
    );
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

    let profile = include_str!("../../../data/interfaces/example-firmware-sim-v1.yaml").replace(
        "    sizes: [17, 33, 65]",
        "    sizes: [17, 33, 65]\n    bogus: 1",
    );
    assert!(validate_interface_profile(&profile).is_err());
}

#[cfg(feature = "fs")]
#[test]
fn shipped_directory_validates() {
    let errors =
        looklab_schema::validate_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data")).unwrap();
    assert!(errors.is_empty(), "{errors:?}");
}

const INTERFACE: &str = include_str!("../../../data/interfaces/example-firmware-sim-v1.yaml");

#[test]
fn interface_lut_sides_and_node_counts_are_bounded() {
    let errors =
        validate_interface_profile(&INTERFACE.replace("sizes: [17, 33, 65]", "sizes: [70000]"))
            .unwrap_err();
    assert_eq!(errors[0].path, "accepts[0].sizes[0]");

    let errors = validate_interface_profile(
        &INTERFACE.replace("sizes: [17, 33, 65]", "sizes: [256, 256, 256, 256]"),
    )
    .unwrap_err();
    assert_eq!(errors[0].path, "accepts[0].sizes");

    let errors =
        validate_interface_profile(&INTERFACE.replace("points_max: 256", "points_max: 70000"))
            .unwrap_err();
    assert_eq!(errors[0].path, "accepts[2].points_max");

    validate_interface_profile(INTERFACE).expect("the shipped profile");
}

#[test]
fn yaml_anchors_are_rejected_at_every_entry_point() {
    let body = "id: a/b\nmake: A\nmodel: B\nexif_aliases: &names [B]\nkind: camera\ncapabilities: { in_camera_raw_conversion: true, jpeg_color_spaces: [srgb], heif: false }\nstatus: active\nmapping_status: provisional\n";
    assert!(validate_body(body).is_err());

    let schema = schema_with(
        "      - key: n\n        type: int\n        range: [0, 4]\n        step: 1\n        default: &d 0\n",
    );
    assert!(validate_control_schema(&schema).is_err());

    let profile = INTERFACE.replace("max_bytes: 2097152", "max_bytes: &limit 2097152");
    assert!(validate_interface_profile(&profile).is_err());
}

#[test]
fn a_collection_over_the_node_bound_is_rejected() {
    let aliases: Vec<String> = (0..4100).map(|index| format!("a{index}")).collect();
    let body = format!(
        "id: a/b\nmake: A\nmodel: B\nexif_aliases: [{}]\nkind: camera\ncapabilities: {{ in_camera_raw_conversion: true, jpeg_color_spaces: [srgb], heif: false }}\nstatus: active\nmapping_status: provisional\n",
        aliases.join(", ")
    );
    assert!(validate_body(&body).is_err());
}

#[test]
fn a_mapping_applies_into_its_epoch_and_the_document_still_validates() {
    let schema = include_str!("../../../data/bodies/fujifilm-x-t5.yaml");
    let from: FirmwareVersion = "1.00".parse().expect("firmware");
    let sim = NativeSim {
        key: "reala_ace".try_into().expect("sim key"),
        look: "reala-ace".try_into().expect("look"),
        makernote: Makernote {
            tag: "FilmMode".try_into().expect("tag"),
            value: MakernoteValue(1792),
        },
    };
    let updated = apply_sim_mapping(schema, 1, &from, None, &sim).expect("applied");
    let parsed = validate_control_schema(&updated).expect("the updated document validates");
    let reala = parsed.epochs[0]
        .native_sims
        .iter()
        .find(|candidate| candidate.key.as_str() == "reala_ace")
        .expect("the new sim");
    assert_eq!(reala.look.as_str(), "reala-ace");
    assert_eq!(reala.makernote.value, MakernoteValue(1792));
    assert_eq!(parsed.epochs[0].native_sims.len(), 3);
}

#[test]
fn a_mapping_for_another_firmware_range_is_rejected() {
    let schema = include_str!("../../../data/bodies/fujifilm-x-t5.yaml");
    let from: FirmwareVersion = "2.00".parse().expect("firmware");
    let sim = NativeSim {
        key: "reala_ace".try_into().expect("sim key"),
        look: "reala-ace".try_into().expect("look"),
        makernote: Makernote {
            tag: "FilmMode".try_into().expect("tag"),
            value: MakernoteValue(1792),
        },
    };
    let errors = apply_sim_mapping(schema, 1, &from, None, &sim).unwrap_err();
    assert_eq!(errors[0].path, "firmware_from");

    let errors = apply_sim_mapping(schema, 9, &from, None, &sim).unwrap_err();
    assert_eq!(errors[0].path, "epoch");
}
