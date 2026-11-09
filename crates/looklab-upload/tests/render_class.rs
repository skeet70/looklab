// SPDX-License-Identifier: AGPL-3.0-or-later
mod common;

use common::{Fixture, compact_limits};
use looklab_upload::{
    BatchManifest, BlobKind, CardDeclaration, ControlKey, EnvelopeError, Intent,
    MonochromeAuthorization, RenderClass, RenderClassError, SendManifest, SettingReach,
    SettingsSourceError, canonical_manifest_bytes, canonical_send_manifest_bytes, manifest_hash,
    parse_envelope_strict,
};

fn monochrome(manifest: &BatchManifest) -> BatchManifest {
    BatchManifest {
        render_class: RenderClass::Monochrome,
        monochrome_authorization: Some(MonochromeAuthorization::ModelDevelopment180Days),
        ..manifest.clone()
    }
}

fn fixture_with(manifest: BatchManifest) -> Fixture {
    let mut fixture = Fixture::new();
    fixture.manifest = manifest;
    fixture
}

/// Replaces the send and batch manifest sections, keeping every length fixed.
fn splice(manifest: &BatchManifest, fixture: &Fixture) -> Vec<u8> {
    let offsets = fixture.offsets();
    let manifest_bytes = canonical_manifest_bytes(manifest);
    let send = SendManifest::canonical(vec![manifest_hash(manifest)]).expect("send manifest");
    let send_bytes = canonical_send_manifest_bytes(&send);
    assert_eq!(
        manifest_bytes.len(),
        offsets.credential_len - offsets.manifest
    );
    assert_eq!(
        send_bytes.len(),
        offsets.manifest_len - offsets.send_manifest
    );
    let mut bytes = fixture.bytes();
    bytes[offsets.send_manifest..offsets.send_manifest + send_bytes.len()]
        .copy_from_slice(&send_bytes);
    bytes[offsets.manifest..offsets.manifest + manifest_bytes.len()]
        .copy_from_slice(&manifest_bytes);
    bytes
}

#[test]
fn a_monochrome_batch_needs_an_explicit_authorization() {
    let fixture = fixture_with(BatchManifest {
        render_class: RenderClass::Monochrome,
        monochrome_authorization: None,
        ..Fixture::new().manifest
    });
    let error = parse_envelope_strict(
        &splice(&fixture.manifest, &Fixture::new()),
        &compact_limits(),
    )
    .err()
    .expect("a monochrome batch without authorization is rejected");
    assert!(matches!(
        error,
        EnvelopeError::RenderClassInvalid {
            source: RenderClassError::MissingAuthorization
        }
    ));
}

#[test]
fn a_colour_batch_rejects_a_monochrome_authorization() {
    let manifest = BatchManifest {
        render_class: RenderClass::Colour,
        monochrome_authorization: Some(MonochromeAuthorization::ModelDevelopment180Days),
        ..Fixture::new().manifest
    };
    let base = fixture_with(monochrome(&Fixture::new().manifest));
    let error = parse_envelope_strict(&splice(&manifest, &base), &compact_limits())
        .err()
        .expect("a colour batch with authorization is rejected");
    assert!(matches!(
        error,
        EnvelopeError::RenderClassInvalid {
            source: RenderClassError::UnexpectedAuthorization
        }
    ));
}

#[test]
fn a_monochrome_batch_round_trips_with_its_authorization() {
    let fixture = fixture_with(monochrome(&Fixture::new().manifest));
    let parsed = parse_envelope_strict(&fixture.bytes(), &compact_limits()).expect("parses");
    assert_eq!(parsed.manifest.render_class, RenderClass::Monochrome);
    assert_eq!(
        parsed.manifest.monochrome_authorization,
        Some(MonochromeAuthorization::ModelDevelopment180Days)
    );
    assert_eq!(
        parsed.manifest_bytes,
        canonical_manifest_bytes(&fixture.manifest)
    );
}

#[test]
fn a_monochrome_card_render_keeps_its_card_intent() {
    let declaration = CardDeclaration::UserDeclared {
        firmware_id: looklab_meta::FirmwareId::new("acme-sim").expect("firmware id"),
        firmware_version: looklab_meta::FirmwareVersion::new("2.0.1").expect("firmware version"),
    };
    let fixture = fixture_with(monochrome(&BatchManifest {
        intent: Intent::Card(declaration.clone()),
        ..Fixture::new().manifest
    }));
    let parsed = parse_envelope_strict(&fixture.bytes(), &compact_limits()).expect("parses");
    assert_eq!(parsed.manifest.intent, Intent::Card(declaration));
    assert_eq!(parsed.manifest.render_class, RenderClass::Monochrome);
}

#[test]
fn the_classification_and_authorization_are_signed() {
    let colour = Fixture::new().manifest;
    let mono = monochrome(&colour);
    assert_ne!(
        canonical_manifest_bytes(&colour),
        canonical_manifest_bytes(&mono)
    );
    assert_ne!(manifest_hash(&colour), manifest_hash(&mono));
}

#[test]
fn envelope_version_four_rejects_three() {
    assert_eq!(looklab_upload::VERSION, 4);
    let fixture = Fixture::new();
    let mut bytes = fixture.bytes();
    let offsets = fixture.offsets();
    bytes[offsets.version..offsets.version + 2].copy_from_slice(&3u16.to_le_bytes());
    let error = parse_envelope_strict(&bytes, &compact_limits())
        .err()
        .expect("a version 3 envelope is rejected");
    assert!(matches!(error, EnvelopeError::UnsupportedVersion(3)));
}

#[test]
fn a_monochrome_card_batch_must_still_source_every_control() {
    let declaration = CardDeclaration::UserDeclared {
        firmware_id: looklab_meta::FirmwareId::new("acme-sim").expect("firmware id"),
        firmware_version: looklab_meta::FirmwareVersion::new("2.0.1").expect("firmware version"),
    };
    let card = BatchManifest {
        intent: Intent::Card(declaration),
        ..common::manifest(vec![common::image(
            &common::sample_blob(),
            BlobKind::Samples,
        )])
    };
    let reach = SettingReach {
        mapped: Vec::new(),
        unmapped: Vec::new(),
        spatial: vec![ControlKey::new("grain_size").expect("control key")],
    };
    assert_eq!(card.validate_setting_sources(&reach), Ok(()));
    let error = monochrome(&card)
        .validate_setting_sources(&reach)
        .expect_err("a monochrome card render must source its spatial controls");
    assert!(matches!(
        error,
        SettingsSourceError::MissingLookAffectingControl { .. }
    ));
}

#[test]
fn the_wire_names_are_snake_case() {
    assert_eq!(
        serde_json::to_string(&RenderClass::Monochrome).expect("serializes"),
        "\"monochrome\""
    );
    assert_eq!(
        serde_json::to_string(&RenderClass::Colour).expect("serializes"),
        "\"colour\""
    );
    assert_eq!(
        serde_json::to_string(&MonochromeAuthorization::ModelDevelopment180Days)
            .expect("serializes"),
        "\"model_development_180_days\""
    );
}
