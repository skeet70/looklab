// SPDX-License-Identifier: AGPL-3.0-or-later
//! Catalog, confirmation and agreement rules.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use looklab_schema::{
    AGREEMENT_GROUPS, Availability, CapabilityDomain, CapabilityKey, ConfirmationBasis,
    ConfirmationScope, ConfirmationState, ControlObservation, ErrorKind, Finding, FirmwareScope,
    IdentityGroupId, MAX_CONFIRMATION_BYTES, ManualCamera, ManualCatalog, ManualConfirmation,
    ManualEvidence, ManualUrl, ModeratorEvidence, ModeratorEvidenceKind, ModeratorFirmwareScope,
    ModeratorRecord, ReportedFirmware, SourceId, SourceKind, ValidationError, evaluate_camera,
    evaluate_control, firmware_scope_accepts, validate_manual_catalog,
    validate_manual_confirmation,
};

const CAMERA_ID: &str = "fujifilm/x-t5";

const NATIVE: &str = "  - key: native_simulations
    label: FILM SIMULATION
    category: rendering
    availability: available
    domain:
      type: enum
      values:
      - PROVIA
    citations:
    - source: manual
      locator: p. 1
";

const HEIF_ABSENT: &str = "  - key: heif_capture
    label: HEIF
    category: capture
    availability: not_documented
";

fn camera_yaml(scope: &str, capabilities: &str) -> String {
    format!(
        "catalog_version: 1
cameras:
- id: {CAMERA_ID}
  make: Fujifilm
  model: X-T5
  sources:
  - id: manual
    kind: owner_manual
    url: https://example.com/x-t5
    title: X-T5 manual
    firmware_scope: '{scope}'
  capabilities:
{capabilities}"
    )
}

fn validate(capabilities: &str) -> Result<ManualCatalog, Vec<ValidationError>> {
    validate_manual_catalog(&camera_yaml("unspecified", capabilities))
}

fn catalog(capabilities: &str) -> ManualCatalog {
    validate(capabilities).expect("a valid catalog")
}

fn exposure_compensation(default: &str) -> String {
    format!(
        "  - key: exposure_compensation
    label: EXPOSURE COMPENSATION
    category: rendering
    availability: available
    domain:
      type: rational
      min: '-1'
      max: '1'
      step: 1/3
      default: '{default}'
    citations:
    - source: manual
      locator: p. 2
"
    )
}

fn capability(camera: &ManualCamera, key: CapabilityKey) -> &looklab_schema::DocumentedCapability {
    camera
        .capabilities
        .iter()
        .find(|capability| capability.key == key)
        .expect("the capability under test")
}

fn observation(camera: &ManualCamera, key: CapabilityKey) -> ControlObservation {
    let capability = capability(camera, key);
    ControlObservation {
        key,
        definition_digest: capability.definition_digest(camera),
        finding: Finding::Matches,
    }
}

fn confirmation(
    camera: &ManualCamera,
    scope: ConfirmationScope,
    observations: Vec<ControlObservation>,
) -> ManualConfirmation {
    ManualConfirmation {
        version: looklab_schema::MANUAL_CONFIRMATION_VERSION,
        camera: camera.id.clone(),
        camera_digest: camera.definition_digest(),
        firmware: ReportedFirmware::new("1.00").expect("a firmware version"),
        scope,
        observations,
    }
}

fn evidence(
    camera: &ManualCamera,
    capability: &looklab_schema::DocumentedCapability,
    group: &IdentityGroupId,
    firmware: &str,
    submitted_at: i64,
) -> ManualEvidence {
    ManualEvidence {
        identity_group: group.clone(),
        firmware: ReportedFirmware::new(firmware).expect("a firmware version"),
        control: capability.key,
        definition_digest: capability.definition_digest(camera),
        finding: Finding::Matches,
        submitted_at,
    }
}

fn corrected_evidence(
    camera: &ManualCamera,
    capability: &looklab_schema::DocumentedCapability,
    group: &IdentityGroupId,
    firmware: &str,
    submitted_at: i64,
    availability: Availability,
    domain: CapabilityDomain,
) -> ManualEvidence {
    ManualEvidence {
        identity_group: group.clone(),
        firmware: ReportedFirmware::new(firmware).expect("a firmware version"),
        control: capability.key,
        definition_digest: capability.definition_digest(camera),
        finding: Finding::Corrected {
            availability,
            domain,
        },
        submitted_at,
    }
}

fn group(name: &str) -> IdentityGroupId {
    IdentityGroupId::new(name).expect("an identity group")
}

fn groups(names: [&str; 3]) -> Vec<IdentityGroupId> {
    names.into_iter().map(group).collect()
}

fn moderator(scope: ModeratorFirmwareScope) -> ModeratorRecord {
    ModeratorRecord {
        confirmed: true,
        evidence: ModeratorEvidence {
            kind: ModeratorEvidenceKind::Video,
            url: None,
            description: "menu walkthrough".to_owned(),
            firmware_scope: scope,
        },
    }
}

fn enum_domain(values: &[&str]) -> CapabilityDomain {
    CapabilityDomain::Enum {
        values: values.iter().map(|value| (*value).to_owned()).collect(),
        default: None,
    }
}

fn corrections(
    camera: &ManualCamera,
    capability: &looklab_schema::DocumentedCapability,
    firmware: &str,
    availability: Availability,
    domain: CapabilityDomain,
) -> Vec<ManualEvidence> {
    groups(["a", "b", "c"])
        .iter()
        .enumerate()
        .map(|(index, group)| {
            corrected_evidence(
                camera,
                capability,
                group,
                firmware,
                index as i64,
                availability,
                domain.clone(),
            )
        })
        .collect()
}

fn has_kind(errors: &[ValidationError], expected: &ErrorKind) -> bool {
    errors.iter().any(|error| &error.kind == expected)
}

fn firmware_result<'a>(
    control: &'a looklab_schema::ControlConfirmation,
    firmware: &str,
) -> &'a looklab_schema::FirmwareConfirmation {
    control
        .firmware
        .iter()
        .find(|result| result.firmware.as_str() == firmware)
        .expect("a firmware result")
}

fn assert_unknown_field_rejected(
    catalog: &ManualCatalog,
    base: &Value,
    label: &str,
    mutate: impl FnOnce(&mut Value),
) {
    let mut value = base.clone();
    mutate(&mut value);
    let payload = serde_json::to_string(&value).expect("JSON");
    let errors = validate_manual_confirmation(&payload, catalog).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| matches!(error.kind, ErrorKind::Syntax(_))),
        "{label}: {errors:?}"
    );
}

#[test]
fn rational_defaults_stay_in_range_and_on_a_step() {
    let errors = validate(&format!("{NATIVE}{}", exposure_compensation("0.2"))).unwrap_err();
    assert!(has_kind(&errors, &ErrorKind::NotOnStep), "{errors:?}");

    let errors = validate(&format!("{NATIVE}{}", exposure_compensation("2"))).unwrap_err();
    assert!(
        has_kind(&errors, &ErrorKind::DefaultOutOfRange),
        "{errors:?}"
    );

    let errors = validate(&format!("{NATIVE}{}", exposure_compensation("-1/3"))).unwrap();
    assert_eq!(errors.cameras[0].capabilities.len(), 2);
}

#[test]
fn integer_and_selectable_steps_are_checked() {
    let steps = "  - key: color_temperature
    label: COLOR TEMPERATURE
    category: rendering
    availability: available
    domain:
      type: integer
      min: 2500
      max: 10000
      steps:
      - 10
      - 100
      default: 2510
    citations:
    - source: manual
      locator: p. 3
";
    assert!(validate(&format!("{NATIVE}{steps}")).is_ok());

    let off_step = steps.replace("default: 2510", "default: 2505");
    let errors = validate(&format!("{NATIVE}{off_step}")).unwrap_err();
    assert!(has_kind(&errors, &ErrorKind::NotOnStep), "{errors:?}");

    let both = steps.replace(
        "      steps:\n      - 10\n      - 100",
        "      step: 10\n      steps:\n      - 10",
    );
    let errors = validate(&format!("{NATIVE}{both}")).unwrap_err();
    assert!(
        errors.iter().any(|error| matches!(
            &error.kind,
            ErrorKind::InvalidValue { expected } if expected.contains("not both")
        )),
        "{errors:?}"
    );

    let partial_axis = "  - key: white_balance_shift
    label: WB SHIFT
    category: rendering
    availability: available
    domain:
      type: axes
      axes:
      - label: R
        min: -9
    citations:
    - source: manual
      locator: p. 4
";
    let errors = validate(&format!("{NATIVE}{partial_axis}")).unwrap_err();
    assert!(
        errors.iter().any(|error| matches!(
            &error.kind,
            ErrorKind::InvalidValue { expected } if expected.contains("all three")
        )),
        "{errors:?}"
    );
}

#[test]
fn not_documented_needs_no_value_and_no_citation() {
    let absent = "  - key: heif_capture
    label: HEIF
    category: capture
    availability: not_documented
    notes: No HEIF entry appears in the image quality menu.
";
    assert!(validate(&format!("{NATIVE}{absent}")).is_ok());

    let invented = absent.replace(
        "    availability: not_documented\n",
        "    availability: not_documented\n    domain:\n      type: boolean\n",
    );
    let errors = validate(&format!("{NATIVE}{invented}")).unwrap_err();
    assert!(
        has_kind(
            &errors,
            &ErrorKind::ManualDomainNotUnspecified {
                key: "heif_capture".to_owned(),
            },
        ),
        "{errors:?}"
    );

    let uncited = exposure_compensation("-1/3").replace(
        "    citations:\n    - source: manual\n      locator: p. 2\n",
        "",
    );
    let errors = validate(&format!("{NATIVE}{uncited}")).unwrap_err();
    assert!(
        has_kind(
            &errors,
            &ErrorKind::CitationRequired {
                key: "exposure_compensation".to_owned(),
            },
        ),
        "{errors:?}"
    );
}

#[test]
fn citations_must_name_a_defined_https_source() {
    let unknown = exposure_compensation("-1/3").replace("source: manual", "source: missing");
    let errors = validate(&format!("{NATIVE}{unknown}")).unwrap_err();
    assert!(
        has_kind(
            &errors,
            &ErrorKind::UnknownCitationSource {
                source_id: SourceId::new("missing").expect("a source id"),
            },
        ),
        "{errors:?}"
    );

    let insecure = camera_yaml("unspecified", NATIVE).replace(
        "url: https://example.com/x-t5",
        "url: http://example.com/x-t5",
    );
    let errors = validate_manual_catalog(&insecure).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| matches!(error.kind, ErrorKind::Syntax(_))),
        "{errors:?}"
    );
}

#[test]
fn definition_digests_scope_to_the_control_and_its_cited_sources() {
    let mut catalog = catalog(&format!("{NATIVE}{}", exposure_compensation("-1/3")));
    let mut camera = catalog.cameras[0].clone();
    let target = capability(&camera, CapabilityKey::ExposureCompensation).clone();
    let digest = target.definition_digest(&camera);
    let prepared = observation(&camera, CapabilityKey::ExposureCompensation);

    camera.capabilities[0].label = "Picture Style".to_owned();
    assert_eq!(target.definition_digest(&camera), digest);
    catalog.cameras[0] = camera.clone();
    let partial = confirmation(
        &camera,
        ConfirmationScope::Controls {
            keys: vec![CapabilityKey::ExposureCompensation],
        },
        vec![prepared.clone()],
    );
    assert!(partial.validate(&catalog).is_ok());

    let mut extra_source = camera.clone();
    extra_source.sources.push(looklab_schema::ManualSource {
        id: SourceId::new("firmware").expect("a source id"),
        kind: SourceKind::FirmwareNotes,
        url: ManualUrl::new("https://example.com/firmware").expect("an HTTPS URL"),
        title: "Firmware notes".to_owned(),
        edition: None,
        firmware_scope: FirmwareScope::new("unspecified").expect("a firmware scope"),
    });
    assert_eq!(target.definition_digest(&extra_source), digest);

    let mut moved_citation = camera.clone();
    moved_citation.capabilities[1].citations[0].locator = "p. 9".to_owned();
    assert_ne!(
        moved_citation.capabilities[1].definition_digest(&moved_citation),
        digest
    );

    let mut revised = camera.clone();
    revised.sources[0].title = "Revised manual".to_owned();
    catalog.cameras[0] = revised.clone();
    assert_ne!(target.definition_digest(&revised), digest);
    let stale = confirmation(
        &revised,
        ConfirmationScope::Controls {
            keys: vec![CapabilityKey::ExposureCompensation],
        },
        vec![prepared],
    );
    let errors = stale.validate(&catalog).unwrap_err();
    assert!(
        has_kind(
            &errors,
            &ErrorKind::ControlDefinitionChanged {
                key: "exposure_compensation".to_owned(),
            },
        ),
        "{errors:?}"
    );

    let stale_camera = confirmation(
        &camera,
        ConfirmationScope::Controls {
            keys: vec![CapabilityKey::ExposureCompensation],
        },
        vec![observation(&camera, CapabilityKey::ExposureCompensation)],
    );
    let errors = stale_camera.validate(&catalog).unwrap_err();
    assert!(
        has_kind(&errors, &ErrorKind::CameraDefinitionChanged),
        "{errors:?}"
    );
}

#[test]
fn confirmation_scope_covers_exactly_the_observed_controls() {
    let catalog = catalog(&format!("{NATIVE}{}", exposure_compensation("-1/3")));
    let camera = &catalog.cameras[0];
    let native = observation(camera, CapabilityKey::NativeSimulations);
    let exposure = observation(camera, CapabilityKey::ExposureCompensation);

    let scope = ConfirmationScope::Controls {
        keys: vec![CapabilityKey::NativeSimulations],
    };
    let partial = confirmation(camera, scope.clone(), vec![native.clone()]);
    assert!(partial.validate(&catalog).is_ok());

    let extra = confirmation(camera, scope, vec![native.clone(), exposure.clone()]);
    let errors = extra.validate(&catalog).unwrap_err();
    assert!(has_kind(&errors, &ErrorKind::ScopeMismatch), "{errors:?}");

    let unobserved = confirmation(
        camera,
        ConfirmationScope::Controls {
            keys: vec![
                CapabilityKey::NativeSimulations,
                CapabilityKey::ExposureCompensation,
            ],
        },
        vec![native.clone()],
    );
    let errors = unobserved.validate(&catalog).unwrap_err();
    assert!(has_kind(&errors, &ErrorKind::ScopeMismatch), "{errors:?}");

    let complete = confirmation(camera, ConfirmationScope::Complete, vec![native]);
    let errors = complete.validate(&catalog).unwrap_err();
    assert!(
        has_kind(
            &errors,
            &ErrorKind::CompleteConfirmationMissing {
                key: "exposure_compensation".to_owned(),
            },
        ),
        "{errors:?}"
    );

    let empty = confirmation(
        camera,
        ConfirmationScope::Controls {
            keys: vec![CapabilityKey::NativeSimulations],
        },
        Vec::new(),
    );
    let errors = empty.validate(&catalog).unwrap_err();
    assert!(
        has_kind(
            &errors,
            &ErrorKind::EmptyText {
                field: "observations",
            },
        ),
        "{errors:?}"
    );
}

#[test]
fn confirmation_structure_checks_reject_without_a_catalog() {
    let catalog = catalog(&format!("{NATIVE}{}", exposure_compensation("-1/3")));
    let camera = &catalog.cameras[0];
    let native = observation(camera, CapabilityKey::NativeSimulations);
    let scope = ConfirmationScope::Controls {
        keys: vec![CapabilityKey::NativeSimulations],
    };
    assert!(
        confirmation(camera, scope.clone(), vec![native.clone()])
            .validate_structure()
            .is_ok()
    );

    let mut version = confirmation(camera, scope.clone(), vec![native.clone()]);
    version.version = looklab_schema::MANUAL_CONFIRMATION_VERSION + 1;
    assert!(has_kind(
        &version.validate_structure().unwrap_err(),
        &ErrorKind::UnsupportedConfirmationVersion {
            found: looklab_schema::MANUAL_CONFIRMATION_VERSION + 1,
        }
    ));

    let empty = confirmation(camera, scope.clone(), Vec::new());
    assert!(has_kind(
        &empty.validate_structure().unwrap_err(),
        &ErrorKind::EmptyText {
            field: "observations",
        }
    ));

    let duplicate = confirmation(camera, scope.clone(), vec![native.clone(), native.clone()]);
    assert!(has_kind(
        &duplicate.validate_structure().unwrap_err(),
        &ErrorKind::DuplicateKey {
            key: "native_simulations".to_owned(),
        }
    ));

    let duplicate_scope = confirmation(
        camera,
        ConfirmationScope::Controls {
            keys: vec![
                CapabilityKey::NativeSimulations,
                CapabilityKey::NativeSimulations,
            ],
        },
        vec![native.clone()],
    );
    assert!(has_kind(
        &duplicate_scope.validate_structure().unwrap_err(),
        &ErrorKind::DuplicateKey {
            key: "native_simulations".to_owned(),
        }
    ));

    let unobserved = confirmation(
        camera,
        ConfirmationScope::Controls {
            keys: vec![CapabilityKey::ExposureCompensation],
        },
        vec![native.clone()],
    );
    assert!(has_kind(
        &unobserved.validate_structure().unwrap_err(),
        &ErrorKind::ScopeMismatch
    ));

    let unlisted = confirmation(
        camera,
        ConfirmationScope::Controls { keys: Vec::new() },
        vec![native],
    );
    let errors = unlisted.validate_structure().unwrap_err();
    assert!(has_kind(&errors, &ErrorKind::EmptyText { field: "keys" }));
    assert!(has_kind(&errors, &ErrorKind::ScopeMismatch));
}

#[test]
fn confirmation_structure_checks_corrected_findings_and_size() {
    let catalog = catalog(NATIVE);
    let camera = &catalog.cameras[0];
    let capability = capability(camera, CapabilityKey::NativeSimulations);
    let corrected = |availability: Availability, domain: CapabilityDomain| ControlObservation {
        key: CapabilityKey::NativeSimulations,
        definition_digest: capability.definition_digest(camera),
        finding: Finding::Corrected {
            availability,
            domain,
        },
    };
    let scope = ConfirmationScope::Controls {
        keys: vec![CapabilityKey::NativeSimulations],
    };

    let not_documented = confirmation(
        camera,
        scope.clone(),
        vec![corrected(
            Availability::NotDocumented,
            CapabilityDomain::Unspecified,
        )],
    );
    assert!(has_kind(
        &not_documented.validate_structure().unwrap_err(),
        &ErrorKind::CorrectionNotDocumented
    ));

    let unavailable_with_domain = confirmation(
        camera,
        scope.clone(),
        vec![corrected(
            Availability::Unavailable,
            enum_domain(&["PROVIA"]),
        )],
    );
    assert!(has_kind(
        &unavailable_with_domain.validate_structure().unwrap_err(),
        &ErrorKind::ManualDomainNotUnspecified {
            key: "native_simulations".to_owned(),
        }
    ));

    let large_value = "x".repeat(MAX_CONFIRMATION_BYTES);
    let oversized = confirmation(
        camera,
        scope,
        vec![corrected(
            Availability::Available,
            enum_domain(&[large_value.as_str()]),
        )],
    );
    assert!(has_kind(
        &oversized.validate_structure().unwrap_err(),
        &ErrorKind::ConfirmationTooLarge
    ));
}

#[test]
fn firmware_scope_gates_quorum() {
    let catalog = validate_manual_catalog(&camera_yaml(
        "Ver.2.00-2.99",
        &format!("{NATIVE}{}", exposure_compensation("-1/3")),
    ))
    .expect("a valid catalog");
    let camera = &catalog.cameras[0];
    let target = capability(camera, CapabilityKey::ExposureCompensation);

    assert!(firmware_scope_accepts(
        camera,
        target,
        &ReportedFirmware::new("2.50").expect("a firmware version")
    ));
    assert!(!firmware_scope_accepts(
        camera,
        target,
        &ReportedFirmware::new("3.00").expect("a firmware version")
    ));

    let in_scope = groups(["a", "b", "c"])
        .iter()
        .enumerate()
        .map(|(index, group)| evidence(camera, target, group, "2.50", index as i64))
        .collect::<Vec<_>>();
    let control = evaluate_control(camera, target, &in_scope, None);
    assert_eq!(control.state, ConfirmationState::Confirmed);
    assert_eq!(control.agreeing_groups, AGREEMENT_GROUPS as u32);

    let out_of_scope = groups(["a", "b", "c"])
        .iter()
        .enumerate()
        .map(|(index, group)| evidence(camera, target, group, "3.00", index as i64))
        .collect::<Vec<_>>();
    let control = evaluate_control(camera, target, &out_of_scope, None);
    assert_eq!(control.state, ConfirmationState::Unconfirmed);
    assert_eq!(control.agreeing_groups, 0);
}

#[test]
fn identical_corrections_confirm_the_reported_options() {
    let catalog = catalog(&format!("{NATIVE}{}", exposure_compensation("-1/3")));
    let camera = &catalog.cameras[0];
    let target = capability(camera, CapabilityKey::NativeSimulations);
    let corrected = enum_domain(&["PROVIA", "REALA ACE"]);
    let entries = corrections(
        camera,
        target,
        "1.00",
        Availability::Available,
        corrected.clone(),
    );

    let control = evaluate_control(camera, target, &entries, None);
    assert_eq!(control.state, ConfirmationState::Confirmed);
    assert_eq!(control.agreeing_groups, 3);
    assert!(control.contradictions.is_empty());
    let result = firmware_result(&control, "1.00");
    assert_eq!(result.state, ConfirmationState::Confirmed);
    assert_eq!(result.basis, Some(ConfirmationBasis::Corrected));
    let actual = result
        .confirmed_actual
        .as_ref()
        .expect("a confirmed actual");
    assert_eq!(actual.availability, Availability::Available);
    assert_eq!(actual.domain, corrected);
    assert_eq!(actual.definition_digest, target.definition_digest(camera));
    assert_eq!(
        actual.firmware_scopes,
        vec![FirmwareScope::new("1.00").expect("a firmware scope")]
    );
    assert_eq!(target.domain, enum_domain(&["PROVIA"]));
}

#[test]
fn corrections_matching_the_manual_use_the_documented_basis() {
    let catalog = catalog(NATIVE);
    let camera = &catalog.cameras[0];
    let target = capability(camera, CapabilityKey::NativeSimulations);
    let documented = enum_domain(&["PROVIA"]);
    let entries = corrections(
        camera,
        target,
        "1.00",
        Availability::Available,
        documented.clone(),
    );

    let control = evaluate_control(camera, target, &entries, None);
    assert_eq!(control.state, ConfirmationState::Confirmed);
    let result = firmware_result(&control, "1.00");
    assert_eq!(result.basis, Some(ConfirmationBasis::Documented));
    let actual = result
        .confirmed_actual
        .as_ref()
        .expect("a confirmed actual");
    assert_eq!(actual.domain, documented);
}

#[test]
fn corrections_confirm_a_formerly_not_documented_control() {
    let catalog = catalog(&format!("{NATIVE}{HEIF_ABSENT}"));
    let camera = &catalog.cameras[0];
    let target = capability(camera, CapabilityKey::HeifCapture);
    let corrected = enum_domain(&["HEIF"]);
    let entries = corrections(
        camera,
        target,
        "1.00",
        Availability::Available,
        corrected.clone(),
    );

    let control = evaluate_control(camera, target, &entries, None);
    assert_eq!(control.state, ConfirmationState::Confirmed);
    let result = firmware_result(&control, "1.00");
    assert_eq!(result.basis, Some(ConfirmationBasis::Corrected));
    let actual = result
        .confirmed_actual
        .as_ref()
        .expect("a confirmed actual");
    assert_eq!(actual.availability, Availability::Available);
    assert_eq!(actual.domain, corrected);
    assert_eq!(target.availability, Availability::NotDocumented);
    assert_eq!(target.domain, CapabilityDomain::Unspecified);
}

#[test]
fn corrections_confirm_a_changed_range() {
    let catalog = catalog(&format!("{NATIVE}{}", exposure_compensation("-1/3")));
    let camera = &catalog.cameras[0];
    let target = capability(camera, CapabilityKey::ExposureCompensation);
    let corrected = CapabilityDomain::Rational {
        min: "-2".to_owned(),
        max: "2".to_owned(),
        step: Some("1/2".to_owned()),
        steps: Vec::new(),
        default: None,
    };
    let entries = corrections(
        camera,
        target,
        "1.00",
        Availability::Available,
        corrected.clone(),
    );

    let control = evaluate_control(camera, target, &entries, None);
    assert_eq!(control.state, ConfirmationState::Confirmed);
    let result = firmware_result(&control, "1.00");
    assert_eq!(result.basis, Some(ConfirmationBasis::Corrected));
    let actual = result
        .confirmed_actual
        .as_ref()
        .expect("a confirmed actual");
    assert_eq!(actual.domain, corrected);
}

#[test]
fn two_corrections_are_not_a_quorum() {
    let catalog = catalog(&format!("{NATIVE}{HEIF_ABSENT}"));
    let camera = &catalog.cameras[0];
    let target = capability(camera, CapabilityKey::HeifCapture);
    let domain = enum_domain(&["HEIF"]);
    let entries = vec![
        corrected_evidence(
            camera,
            target,
            &group("a"),
            "1.00",
            1,
            Availability::Available,
            domain.clone(),
        ),
        corrected_evidence(
            camera,
            target,
            &group("b"),
            "1.00",
            2,
            Availability::Available,
            domain,
        ),
    ];

    let control = evaluate_control(camera, target, &entries, None);
    assert_eq!(control.state, ConfirmationState::Unconfirmed);
    assert_eq!(control.agreeing_groups, 2);
    let result = firmware_result(&control, "1.00");
    assert_eq!(result.state, ConfirmationState::Unconfirmed);
    assert_eq!(result.basis, None);
    assert!(result.confirmed_actual.is_none());
}

#[test]
fn a_differing_active_claim_blocks_corrected_confirmation() {
    let catalog = catalog(NATIVE);
    let camera = &catalog.cameras[0];
    let target = capability(camera, CapabilityKey::NativeSimulations);
    let mut entries = corrections(
        camera,
        target,
        "1.00",
        Availability::Available,
        enum_domain(&["PROVIA", "REALA ACE"]),
    );
    entries.push(evidence(camera, target, &group("d"), "1.00", 4));

    let control = evaluate_control(camera, target, &entries, None);
    assert_eq!(control.state, ConfirmationState::Contradicted);
    assert_eq!(control.agreeing_groups, 3);
    assert_eq!(control.contradictions.len(), 1);
    let result = firmware_result(&control, "1.00");
    assert_eq!(result.state, ConfirmationState::Contradicted);
    assert_eq!(result.basis, None);
    assert!(result.confirmed_actual.is_none());
}

#[test]
fn corrected_quorum_respects_firmware_scope() {
    let catalog =
        validate_manual_catalog(&camera_yaml("Ver.2.00-2.99", NATIVE)).expect("a valid catalog");
    let camera = &catalog.cameras[0];
    let target = capability(camera, CapabilityKey::NativeSimulations);
    let corrected = enum_domain(&["PROVIA", "REALA ACE"]);

    let in_scope = corrections(
        camera,
        target,
        "2.50",
        Availability::Available,
        corrected.clone(),
    );
    let control = evaluate_control(camera, target, &in_scope, None);
    assert_eq!(control.state, ConfirmationState::Confirmed);
    let result = firmware_result(&control, "2.50");
    assert_eq!(
        result
            .confirmed_actual
            .as_ref()
            .expect("a confirmed actual")
            .domain,
        corrected
    );

    let out_of_scope = corrections(camera, target, "3.00", Availability::Available, corrected);
    let control = evaluate_control(camera, target, &out_of_scope, None);
    assert_eq!(control.state, ConfirmationState::Unconfirmed);
    assert_eq!(control.agreeing_groups, 0);
    assert!(control.firmware.is_empty());
}

#[test]
fn a_partial_correction_does_not_invent_options() {
    let catalog = catalog(&format!("{NATIVE}{HEIF_ABSENT}"));
    let camera = &catalog.cameras[0];
    let target = capability(camera, CapabilityKey::HeifCapture);
    let entries = corrections(
        camera,
        target,
        "1.00",
        Availability::Available,
        CapabilityDomain::Unspecified,
    );

    let control = evaluate_control(camera, target, &entries, None);
    assert_eq!(control.state, ConfirmationState::Confirmed);
    let result = firmware_result(&control, "1.00");
    assert_eq!(result.basis, Some(ConfirmationBasis::Corrected));
    let actual = result
        .confirmed_actual
        .as_ref()
        .expect("a confirmed actual");
    assert_eq!(actual.availability, Availability::Available);
    assert_eq!(actual.domain, CapabilityDomain::Unspecified);
}

#[test]
fn different_firmware_options_are_not_contradictions() {
    let catalog = catalog(NATIVE);
    let camera = &catalog.cameras[0];
    let target = capability(camera, CapabilityKey::NativeSimulations);
    let mut entries = corrections(
        camera,
        target,
        "1.00",
        Availability::Available,
        enum_domain(&["PROVIA"]),
    );
    entries.extend(corrections(
        camera,
        target,
        "2.00",
        Availability::Available,
        enum_domain(&["REALA ACE"]),
    ));

    let control = evaluate_control(camera, target, &entries, None);
    assert_eq!(control.state, ConfirmationState::Confirmed);
    assert_eq!(control.firmware.len(), 2);
    assert!(control.contradictions.is_empty());
    let first = firmware_result(&control, "1.00");
    let second = firmware_result(&control, "2.00");
    assert_eq!(first.state, ConfirmationState::Confirmed);
    assert_eq!(second.state, ConfirmationState::Confirmed);
    assert_eq!(
        first.confirmed_actual.as_ref().expect("an actual").domain,
        enum_domain(&["PROVIA"])
    );
    assert_eq!(
        second.confirmed_actual.as_ref().expect("an actual").domain,
        enum_domain(&["REALA ACE"])
    );
    assert_eq!(
        first
            .confirmed_actual
            .as_ref()
            .expect("an actual")
            .firmware_scopes,
        vec![FirmwareScope::new("1.00").expect("a firmware scope")]
    );
    assert_eq!(
        second
            .confirmed_actual
            .as_ref()
            .expect("an actual")
            .firmware_scopes,
        vec![FirmwareScope::new("2.00").expect("a firmware scope")]
    );
}

#[test]
fn unspecified_scope_does_not_merge_firmware_votes() {
    let catalog = catalog(NATIVE);
    let camera = &catalog.cameras[0];
    let target = capability(camera, CapabilityKey::NativeSimulations);
    let entries = vec![
        corrected_evidence(
            camera,
            target,
            &group("a"),
            "1.00",
            1,
            Availability::Available,
            enum_domain(&["PROVIA"]),
        ),
        corrected_evidence(
            camera,
            target,
            &group("b"),
            "2.00",
            2,
            Availability::Available,
            enum_domain(&["PROVIA"]),
        ),
        corrected_evidence(
            camera,
            target,
            &group("c"),
            "3.00",
            3,
            Availability::Available,
            enum_domain(&["PROVIA"]),
        ),
    ];

    let control = evaluate_control(camera, target, &entries, None);
    assert_eq!(control.state, ConfirmationState::Unconfirmed);
    assert_eq!(control.agreeing_groups, 1);
    assert_eq!(control.firmware.len(), 3);
    assert!(
        control
            .firmware
            .iter()
            .all(|result| result.state == ConfirmationState::Unconfirmed)
    );
    assert!(
        control
            .firmware
            .iter()
            .all(|result| result.confirmed_actual.is_none())
    );
}

#[test]
fn camera_human_confirmation_requires_one_common_firmware() {
    let catalog = catalog(&format!("{NATIVE}{}", exposure_compensation("-1/3")));
    let camera = &catalog.cameras[0];
    let native = capability(camera, CapabilityKey::NativeSimulations);
    let exposure = capability(camera, CapabilityKey::ExposureCompensation);
    let mut entries = groups(["a", "b", "c"])
        .iter()
        .enumerate()
        .map(|(index, group)| evidence(camera, native, group, "1.00", index as i64))
        .collect::<Vec<_>>();
    entries.extend(
        groups(["a", "b", "c"])
            .iter()
            .enumerate()
            .map(|(index, group)| evidence(camera, exposure, group, "1.00", index as i64)),
    );

    let summary = evaluate_camera(camera, &entries, &BTreeMap::new());
    assert!(summary.human_confirmed());

    let mut mixed = entries.clone();
    for entry in &mut mixed {
        if entry.control == CapabilityKey::ExposureCompensation {
            entry.firmware = ReportedFirmware::new("2.00").expect("a firmware version");
        }
    }
    let summary = evaluate_camera(camera, &mixed, &BTreeMap::new());
    assert!(!summary.human_confirmed());

    let native_only = entries
        .iter()
        .filter(|entry| entry.control == CapabilityKey::NativeSimulations)
        .cloned()
        .collect::<Vec<_>>();
    let summary = evaluate_camera(camera, &native_only, &BTreeMap::new());
    assert!(!summary.human_confirmed());

    let catalog = self::catalog(&format!("{NATIVE}{HEIF_ABSENT}"));
    let camera = &catalog.cameras[0];
    let native = capability(camera, CapabilityKey::NativeSimulations);
    let entries = groups(["a", "b", "c"])
        .iter()
        .enumerate()
        .map(|(index, group)| evidence(camera, native, group, "1.00", index as i64))
        .collect::<Vec<_>>();
    let summary = evaluate_camera(camera, &entries, &BTreeMap::new());
    assert!(summary.human_confirmed());
}

#[test]
fn camera_confirmation_keeps_every_common_confirmed_firmware() {
    let catalog = catalog(&format!("{NATIVE}{}", exposure_compensation("-1/3")));
    let camera = &catalog.cameras[0];
    let native = capability(camera, CapabilityKey::NativeSimulations);
    let exposure = capability(camera, CapabilityKey::ExposureCompensation);
    let mut entries = Vec::new();
    for (firmware, names) in [("1.00", ["a", "b", "c"]), ("2.00", ["d", "e", "f"])] {
        entries.extend(
            groups(names)
                .iter()
                .enumerate()
                .map(|(index, group)| evidence(camera, native, group, firmware, index as i64)),
        );
        entries.extend(
            groups(names)
                .iter()
                .enumerate()
                .map(|(index, group)| evidence(camera, exposure, group, firmware, index as i64)),
        );
    }
    let summary = evaluate_camera(camera, &entries, &BTreeMap::new());
    assert_eq!(
        summary.common_confirmed_firmware(),
        vec![
            ReportedFirmware::new("1.00").expect("a firmware version"),
            ReportedFirmware::new("2.00").expect("a firmware version"),
        ]
    );
    assert!(summary.human_confirmed());
}

#[test]
fn disjoint_firmware_coverage_does_not_confirm_the_camera() {
    let catalog = catalog(&format!("{NATIVE}{}", exposure_compensation("-1/3")));
    let camera = &catalog.cameras[0];
    let native = capability(camera, CapabilityKey::NativeSimulations);
    let exposure = capability(camera, CapabilityKey::ExposureCompensation);
    let mut entries = groups(["a", "b", "c"])
        .iter()
        .enumerate()
        .map(|(index, group)| evidence(camera, native, group, "1.00", index as i64))
        .collect::<Vec<_>>();
    entries.extend(
        groups(["a", "b", "c"])
            .iter()
            .enumerate()
            .map(|(index, group)| evidence(camera, exposure, group, "2.00", index as i64)),
    );
    let summary = evaluate_camera(camera, &entries, &BTreeMap::new());
    assert!(summary.common_confirmed_firmware().is_empty());
    assert!(!summary.human_confirmed());
}

#[test]
fn exact_moderator_scope_satisfies_the_scoped_camera_rule() {
    let catalog = catalog(&format!("{NATIVE}{}", exposure_compensation("-1/3")));
    let camera = &catalog.cameras[0];
    let native = capability(camera, CapabilityKey::NativeSimulations);
    let entries = groups(["a", "b", "c"])
        .iter()
        .enumerate()
        .map(|(index, group)| evidence(camera, native, group, "1.00", index as i64))
        .collect::<Vec<_>>();
    let moderators = BTreeMap::from([(
        CapabilityKey::ExposureCompensation,
        moderator(ModeratorFirmwareScope::exact("1.00").expect("a firmware scope")),
    )]);
    let summary = evaluate_camera(camera, &entries, &moderators);
    assert_eq!(
        summary.common_confirmed_firmware(),
        vec![ReportedFirmware::new("1.00").expect("a firmware version")]
    );
    assert!(summary.human_confirmed());

    let disjoint = BTreeMap::from([(
        CapabilityKey::ExposureCompensation,
        moderator(ModeratorFirmwareScope::exact("2.00").expect("a firmware scope")),
    )]);
    let summary = evaluate_camera(camera, &entries, &disjoint);
    assert!(summary.common_confirmed_firmware().is_empty());
    assert!(!summary.human_confirmed());
}

#[test]
fn unspecified_moderator_scope_is_reviewed_but_not_confirmed_across_versions() {
    let catalog = catalog(&format!("{NATIVE}{}", exposure_compensation("-1/3")));
    let camera = &catalog.cameras[0];
    let native = capability(camera, CapabilityKey::NativeSimulations);
    let entries = groups(["a", "b", "c"])
        .iter()
        .enumerate()
        .map(|(index, group)| evidence(camera, native, group, "1.00", index as i64))
        .collect::<Vec<_>>();
    let moderators = BTreeMap::from([(
        CapabilityKey::ExposureCompensation,
        moderator(ModeratorFirmwareScope::unspecified()),
    )]);
    let summary = evaluate_camera(camera, &entries, &moderators);
    assert!(summary.common_confirmed_firmware().is_empty());
    assert!(!summary.human_confirmed());
    let control = summary
        .control(CapabilityKey::ExposureCompensation)
        .expect("a summary entry");
    assert_eq!(control.state, ConfirmationState::ModeratorConfirmed);
    assert!(
        control
            .moderator
            .as_ref()
            .is_some_and(|evidence| evidence.firmware_scope.is_unspecified())
    );
}

#[test]
fn moderator_scope_rejects_unknown_fields_and_dishonest_units() {
    for payload in [
        r#"{"kind":"unspecified","extra":true}"#,
        r#"{"kind":"exact","firmware":"1.00","extra":true}"#,
        r#"{"kind":"range","from":"1.00","to":"2.00","extra":true}"#,
    ] {
        assert!(
            serde_json::from_str::<ModeratorFirmwareScope>(payload).is_err(),
            "{payload}"
        );
    }
    assert_eq!(
        serde_json::from_str::<ModeratorFirmwareScope>(r#"{"kind":"unspecified"}"#)
            .expect("an unspecified scope"),
        ModeratorFirmwareScope::unspecified()
    );
    assert!(
        serde_json::from_str::<ModeratorFirmwareScope>(
            r#"{"kind":"exact","firmware":"unspecified"}"#
        )
        .is_err()
    );
    assert!(
        serde_json::from_str::<ModeratorFirmwareScope>(
            r#"{"kind":"range","from":"2.00","to":"1.00"}"#
        )
        .is_err()
    );
    assert!(
        ModeratorFirmwareScope::range("1.00", "2.00")
            .expect("a firmware range")
            .exact_firmware()
            .is_none()
    );
}

#[test]
fn stale_control_definitions_do_not_contribute() {
    let catalog = catalog(&format!("{NATIVE}{}", exposure_compensation("-1/3")));
    let camera = &catalog.cameras[0];
    let target = capability(camera, CapabilityKey::ExposureCompensation);
    let entries = groups(["a", "b", "c"])
        .iter()
        .enumerate()
        .map(|(index, group)| evidence(camera, target, group, "1.00", index as i64))
        .collect::<Vec<_>>();

    let mut changed = camera.clone();
    changed.capabilities[1].label = "Exposure".to_owned();
    let changed_target = capability(&changed, CapabilityKey::ExposureCompensation);
    let control = evaluate_control(&changed, changed_target, &entries, None);
    assert_eq!(control.state, ConfirmationState::Unconfirmed);
    assert_eq!(control.agreeing_groups, 0);
}

#[test]
fn contradictions_block_and_known_groups_dedup() {
    let catalog = catalog(&format!("{NATIVE}{}", exposure_compensation("-1/3")));
    let camera = &catalog.cameras[0];
    let target = capability(camera, CapabilityKey::ExposureCompensation);

    let agreeing = groups(["a", "b", "c"])
        .iter()
        .enumerate()
        .map(|(index, group)| evidence(camera, target, group, "1.00", index as i64))
        .collect::<Vec<_>>();
    let control = evaluate_control(camera, target, &agreeing, None);
    assert_eq!(control.state, ConfirmationState::Confirmed);
    assert!(control.contradictions.is_empty());

    let mut contradicted = agreeing.clone();
    contradicted.push(corrected_evidence(
        camera,
        target,
        &group("d"),
        "1.00",
        4,
        Availability::Unavailable,
        CapabilityDomain::Unspecified,
    ));
    let control = evaluate_control(camera, target, &contradicted, None);
    assert_eq!(control.state, ConfirmationState::Contradicted);
    assert_eq!(control.agreeing_groups, 3);
    assert_eq!(control.contradictions.len(), 1);

    let mut changed = vec![
        evidence(camera, target, &group("a"), "1.00", 1),
        evidence(camera, target, &group("b"), "1.00", 2),
        evidence(camera, target, &group("c"), "1.00", 1),
        corrected_evidence(
            camera,
            target,
            &group("c"),
            "1.00",
            3,
            Availability::Unavailable,
            CapabilityDomain::Unspecified,
        ),
    ];
    changed.sort_by_key(|entry| entry.submitted_at);
    let control = evaluate_control(camera, target, &changed, None);
    assert_eq!(control.state, ConfirmationState::Contradicted);
    assert_eq!(control.agreeing_groups, 2);

    let repeated = vec![
        evidence(camera, target, &group("a"), "1.00", 1),
        evidence(camera, target, &group("a"), "1.00", 2),
        evidence(camera, target, &group("b"), "1.00", 3),
    ];
    let control = evaluate_control(camera, target, &repeated, None);
    assert_eq!(control.state, ConfirmationState::Unconfirmed);
    assert_eq!(control.agreeing_groups, 2);
}

#[test]
fn evidence_for_another_control_does_not_confirm_this_one() {
    let catalog = catalog(&format!("{NATIVE}{}", exposure_compensation("-1/3")));
    let camera = &catalog.cameras[0];
    let native = capability(camera, CapabilityKey::NativeSimulations);
    let entries = groups(["a", "b", "c"])
        .iter()
        .enumerate()
        .map(|(index, group)| evidence(camera, native, group, "1.00", index as i64))
        .collect::<Vec<_>>();
    let summary = evaluate_camera(camera, &entries, &BTreeMap::new());
    assert_eq!(
        summary
            .control(CapabilityKey::NativeSimulations)
            .expect("a summary entry")
            .state,
        ConfirmationState::Confirmed
    );
    assert_eq!(
        summary
            .control(CapabilityKey::ExposureCompensation)
            .expect("a summary entry")
            .state,
        ConfirmationState::Unconfirmed
    );

    let moderator = ModeratorRecord {
        confirmed: true,
        evidence: ModeratorEvidence {
            kind: ModeratorEvidenceKind::Video,
            url: None,
            description: "menu walkthrough".to_owned(),
            firmware_scope: ModeratorFirmwareScope::exact("1.00").expect("a firmware scope"),
        },
    };
    let control = evaluate_control(
        camera,
        capability(camera, CapabilityKey::ExposureCompensation),
        &[],
        Some(&moderator),
    );
    assert_eq!(control.state, ConfirmationState::ModeratorConfirmed);
    assert!(control.moderator.is_some());
    assert!(control.firmware.is_empty());
}

#[test]
fn nested_unknown_confirmation_fields_are_rejected() {
    let catalog = catalog(NATIVE);
    let camera = &catalog.cameras[0];
    let target = capability(camera, CapabilityKey::NativeSimulations);
    let observation = ControlObservation {
        key: CapabilityKey::NativeSimulations,
        definition_digest: target.definition_digest(camera),
        finding: Finding::Corrected {
            availability: Availability::Available,
            domain: enum_domain(&["PROVIA"]),
        },
    };
    let base = confirmation(
        camera,
        ConfirmationScope::Controls {
            keys: vec![CapabilityKey::NativeSimulations],
        },
        vec![observation],
    );
    let value = serde_json::to_value(&base).expect("a serializable confirmation");
    let payload = serde_json::to_string(&value).expect("JSON");
    assert!(validate_manual_confirmation(&payload, &catalog).is_ok());

    assert_unknown_field_rejected(&catalog, &value, "scope", |value| {
        value["scope"]["extra"] = json!(true);
    });
    assert_unknown_field_rejected(&catalog, &value, "finding", |value| {
        value["observations"][0]["finding"]["extra"] = json!(true);
    });
    assert_unknown_field_rejected(&catalog, &value, "domain", |value| {
        value["observations"][0]["finding"]["domain"]["extra"] = json!(true);
    });
    assert_unknown_field_rejected(&catalog, &value, "observation", |value| {
        value["observations"][0]["extra"] = json!(true);
    });
    assert_unknown_field_rejected(&catalog, &value, "top level", |value| {
        value["extra"] = json!(true);
    });
    assert_unknown_field_rejected(&catalog, &value, "unit finding", |value| {
        value["observations"][0]["finding"] = json!({"kind": "matches", "extra": true});
    });
    assert_unknown_field_rejected(&catalog, &value, "unit scope", |value| {
        value["scope"] = json!({"kind": "complete", "extra": true});
    });
    assert_unknown_field_rejected(&catalog, &value, "unit domain", |value| {
        value["observations"][0]["finding"]["domain"] =
            json!({"type": "unspecified", "extra": true});
    });
}
