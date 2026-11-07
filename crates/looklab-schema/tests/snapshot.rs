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
    fixture_manifest_with(source_url, Some(FIXTURE_SHA256))
}

fn fixture_manifest_with(source_url: Option<&str>, sha256: Option<&str>) -> Vec<u8> {
    let mut manifest = String::from(
        "[[fixture]]\nfile = \"fujifilm-x-t5.raf\"\nmake = \"Fujifilm\"\nmodel = \"X-T5\"\n\
         license = \"CC0-1.0\"\ncommitted = false\n",
    );
    if let Some(sha256) = sha256 {
        manifest.push_str(&format!("sha256 = \"{sha256}\"\n"));
    }
    if let Some(url) = source_url {
        manifest.push_str(&format!("source_url = \"{url}\"\n"));
    }
    manifest.into_bytes()
}

const FIXTURE_SHA256: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

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

#[test]
fn a_manifest_entry_without_a_digest_is_rejected() {
    let dir = tempfile::tempdir().expect("temp dir");
    publish(
        dir.path(),
        &[(
            looklab_schema::FIXTURE_MANIFEST.to_owned(),
            fixture_manifest_with(Some("https://example.com/fujifilm-x-t5.raf"), None),
        )],
    );
    let failures = looklab_schema::validate_snapshot_dir(dir.path()).expect("walk");
    assert!(
        failures.iter().any(|failure| failure.errors.iter().any(|error| {
            error.path == "fixture[0]"
                && matches!(error.kind, looklab_schema::ErrorKind::FixtureManifest(_))
        })),
        "{failures:?}"
    );
}

fn published_fixture(file: &str, sha256: &str, committed: &str) -> Vec<u8> {
    format!(
        "[[fixture]]\nfile = \"{file}\"\nmake = \"Fujifilm\"\nmodel = \"X-T5\"\n\
         sha256 = \"{sha256}\"\nlicense = \"CC0-1.0\"\ncommitted = {committed}\n"
    )
    .into_bytes()
}

fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn write_fixture(root: &Path, path: &str, bytes: &[u8]) {
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("directory");
    std::fs::write(file, bytes).expect("fixture");
}

#[test]
fn a_published_fixture_is_stored_at_its_placement_path() {
    let bytes = b"donated raw bytes";
    let sha256 = digest(bytes);

    let lfs = tempfile::tempdir().expect("temp dir");
    publish(
        lfs.path(),
        &[(
            looklab_schema::FIXTURE_MANIFEST.to_owned(),
            published_fixture("donated.raf", &sha256, "\"lfs\""),
        )],
    );
    write_fixture(lfs.path(), "fixtures/donated/donated.raf", bytes);
    assert!(
        looklab_schema::validate_snapshot_dir(lfs.path())
            .expect("walk")
            .is_empty()
    );

    let plain = tempfile::tempdir().expect("temp dir");
    publish(
        plain.path(),
        &[(
            looklab_schema::FIXTURE_MANIFEST.to_owned(),
            published_fixture("donated.raf", &sha256, "\"lfs\""),
        )],
    );
    write_fixture(plain.path(), "fixtures/donated.raf", bytes);
    let failures = looklab_schema::validate_snapshot_dir(plain.path()).expect("walk");
    assert!(
        failures.iter().any(|failure| failure
            .errors
            .iter()
            .any(|error| matches!(error.kind, looklab_schema::ErrorKind::FixtureManifest(_)))),
        "{failures:?}"
    );

    let git = tempfile::tempdir().expect("temp dir");
    publish(
        git.path(),
        &[(
            looklab_schema::FIXTURE_MANIFEST.to_owned(),
            published_fixture("small.dng", &sha256, "true"),
        )],
    );
    write_fixture(git.path(), "fixtures/small.dng", bytes);
    assert!(
        looklab_schema::validate_snapshot_dir(git.path())
            .expect("walk")
            .is_empty()
    );

    let corrupt = tempfile::tempdir().expect("temp dir");
    publish(
        corrupt.path(),
        &[(
            looklab_schema::FIXTURE_MANIFEST.to_owned(),
            published_fixture("donated.raf", &sha256, "\"lfs\""),
        )],
    );
    write_fixture(
        corrupt.path(),
        "fixtures/donated/donated.raf",
        b"other bytes",
    );
    assert!(
        !looklab_schema::validate_snapshot_dir(corrupt.path())
            .expect("walk")
            .is_empty()
    );

    let escape = tempfile::tempdir().expect("temp dir");
    publish(
        escape.path(),
        &[(
            looklab_schema::FIXTURE_MANIFEST.to_owned(),
            published_fixture("../escape.raf", &sha256, "\"lfs\""),
        )],
    );
    assert!(
        !looklab_schema::validate_snapshot_dir(escape.path())
            .expect("walk")
            .is_empty()
    );
}

fn name(file: &str) -> looklab_schema::FixtureFileName<'_> {
    looklab_schema::FixtureFileName::parse(file).expect("a plain file name")
}

fn git_filter(repo: &Path, path: &str) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["check-attr", "filter", "--", path])
        .output()
        .expect("git runs");
    assert!(output.status.success(), "{output:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[test]
fn a_donated_fixture_exports_to_a_git_lfs_tracked_path() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");

    let lfs = looklab_schema::Placement::Lfs
        .snapshot_path(name("donated.raf"))
        .expect("an LFS fixture exports bytes");
    assert_eq!(lfs, "fixtures/donated/donated.raf");
    assert_eq!(git_filter(&repo, &lfs), format!("{lfs}: filter: lfs"));
    let committed = format!("data/community/v1/{lfs}");
    assert_eq!(
        git_filter(&repo, &committed),
        format!("{committed}: filter: lfs")
    );

    let git = looklab_schema::Placement::Git
        .snapshot_path(name("small.dng"))
        .expect("a git fixture exports bytes");
    assert_eq!(git, "fixtures/small.dng");
    assert_eq!(
        git_filter(&repo, &git),
        format!("{git}: filter: unspecified")
    );
}
