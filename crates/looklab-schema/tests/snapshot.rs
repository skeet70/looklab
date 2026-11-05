// SPDX-License-Identifier: AGPL-3.0-or-later
//! Snapshot directory validation needs the `fs` feature.

#![cfg(feature = "fs")]
#![allow(clippy::disallowed_methods)]

use std::path::Path;

use looklab_schema::{BodyId, validate_body, validate_interface_profile};

const BODY: &str = include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml");
const SCHEMA: &str = include_str!("../../../data/bodies/fujifilm-x-t5.yaml");
const INTERFACE: &str = include_str!("../../../data/interfaces/example-app-lut3d-v1.yaml");

fn body_path() -> String {
    let body = validate_body(BODY).expect("body");
    looklab_schema::body_export_path(&body.id)
}

fn schema_path() -> String {
    let schema = looklab_schema::validate_control_schema(SCHEMA).expect("schema");
    looklab_schema::schema_export_path(&schema.body)
}

fn interface_path() -> String {
    let profile = validate_interface_profile(INTERFACE).expect("interface");
    looklab_schema::interface_export_path(&profile.id)
}

fn publish(root: &Path, documents: &[(String, Vec<u8>)]) {
    for (path, bytes) in documents {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("directory");
        std::fs::write(file, bytes).expect("document");
    }
    let hash = looklab_schema::content_hash(documents);
    std::fs::write(
        root.join(looklab_schema::SNAPSHOT_MANIFEST),
        format!("{{\"version\":1,\"content_hash\":\"{hash}\"}}"),
    )
    .expect("manifest");
}

#[test]
fn a_snapshot_directory_validates_and_reports_a_stale_hash() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    let documents = vec![
        (body_path(), BODY.as_bytes().to_vec()),
        (schema_path(), SCHEMA.as_bytes().to_vec()),
        (interface_path(), INTERFACE.as_bytes().to_vec()),
    ];
    publish(root, &documents);
    assert!(
        looklab_schema::validate_snapshot_dir(root)
            .expect("walk")
            .is_empty()
    );

    std::fs::write(root.join("bodies/fujifilm-x-t5.yaml"), "body: x\n").expect("stale");
    let failures = looklab_schema::validate_snapshot_dir(root).expect("walk");
    assert_eq!(failures.len(), 2, "{failures:?}");
    assert!(
        failures
            .iter()
            .any(|failure| failure.file.ends_with("snapshot.json"))
    );
}

#[test]
fn distinct_body_ids_do_not_share_an_export_path() {
    let dotted: BodyId = "fujifilm/x-t5".try_into().expect("body id");
    let flat: BodyId = "fujifilm-x/t5".try_into().expect("body id");
    assert_ne!(
        looklab_schema::body_export_path(&dotted),
        looklab_schema::body_export_path(&flat)
    );
    assert_ne!(
        looklab_schema::schema_export_path(&dotted),
        looklab_schema::schema_export_path(&flat)
    );
}

#[test]
fn a_document_filed_under_another_id_is_rejected() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    let impostor = BODY.replace("id: fujifilm/x-t5", "id: fujifilm-x/t5");
    publish(
        root,
        &[(
            "bodies/fujifilm-x-t5.body.yaml".to_owned(),
            impostor.into_bytes(),
        )],
    );
    let failures = looklab_schema::validate_snapshot_dir(root).expect("walk");
    assert!(
        failures.iter().any(|failure| failure
            .errors
            .iter()
            .any(|error| matches!(error.kind, looklab_schema::ErrorKind::DocumentOwner { .. }))),
        "{failures:?}"
    );
}

fn fixture_manifest(source_url: Option<&str>) -> Vec<u8> {
    let mut manifest = String::from(
        "[[fixture]]\nfile = \"fujifilm-x-t5.raf\"\nmake = \"Fujifilm\"\nmodel = \"X-T5\"\n\
         license = \"CC0-1.0\"\ncommitted = false\n",
    );
    if let Some(url) = source_url {
        manifest.push_str(&format!("source_url = \"{url}\"\n"));
    }
    manifest.into_bytes()
}

#[test]
fn a_fixture_backed_body_needs_a_matching_manifest_entry() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    let backed = BODY.replace(
        "mapping_status: provisional",
        "mapping_status: fixture_backed",
    );
    publish(root, &[(body_path(), backed.clone().into_bytes())]);
    let failures = looklab_schema::validate_snapshot_dir(root).expect("walk");
    assert!(
        failures
            .iter()
            .any(|failure| failure.errors.iter().any(|error| matches!(
                error.kind,
                looklab_schema::ErrorKind::FixtureEvidenceMissing { .. }
            ))),
        "a claim without a manifest: {failures:?}"
    );

    let documents = vec![
        (body_path(), backed.into_bytes()),
        (
            looklab_schema::FIXTURE_MANIFEST.to_owned(),
            fixture_manifest(Some("https://example.com/fujifilm-x-t5.raf")),
        ),
    ];
    publish(root, &documents);
    assert!(
        looklab_schema::validate_snapshot_dir(root)
            .expect("walk")
            .is_empty()
    );
}

#[test]
fn a_manifest_download_without_a_url_is_rejected() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    publish(
        root,
        &[(
            looklab_schema::FIXTURE_MANIFEST.to_owned(),
            fixture_manifest(None),
        )],
    );
    let failures = looklab_schema::validate_snapshot_dir(root).expect("walk");
    assert!(
        failures.iter().any(|failure| failure
            .errors
            .iter()
            .any(|error| matches!(error.kind, looklab_schema::ErrorKind::FixtureManifest(_)))),
        "{failures:?}"
    );
}
