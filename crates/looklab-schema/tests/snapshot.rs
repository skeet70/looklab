// SPDX-License-Identifier: AGPL-3.0-or-later
//! Snapshot directory validation needs the `fs` feature.

#![cfg(feature = "fs")]
#![allow(clippy::disallowed_methods)]

const BODY: &str = include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml");
const SCHEMA: &str = include_str!("../../../data/bodies/fujifilm-x-t5.yaml");
const INTERFACE: &str = include_str!("../../../data/interfaces/example-firmware-sim-v1.yaml");

#[test]
fn a_snapshot_directory_validates_and_reports_a_stale_hash() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    std::fs::create_dir_all(root.join("bodies")).expect("bodies");
    std::fs::create_dir_all(root.join("interfaces")).expect("interfaces");
    std::fs::write(root.join("bodies/fujifilm-x-t5.body.yaml"), BODY).expect("body");
    std::fs::write(root.join("bodies/fujifilm-x-t5.yaml"), SCHEMA).expect("schema");
    std::fs::write(
        root.join("interfaces/example-firmware-sim-v1.yaml"),
        INTERFACE,
    )
    .expect("interface");

    let files = vec![
        (
            "bodies/fujifilm-x-t5.body.yaml".to_owned(),
            BODY.as_bytes().to_vec(),
        ),
        (
            "bodies/fujifilm-x-t5.yaml".to_owned(),
            SCHEMA.as_bytes().to_vec(),
        ),
        (
            "interfaces/example-firmware-sim-v1.yaml".to_owned(),
            INTERFACE.as_bytes().to_vec(),
        ),
    ];
    let hash = looklab_schema::content_hash(&files);
    std::fs::write(
        root.join("snapshot.json"),
        format!("{{\"version\":1,\"content_hash\":\"{hash}\"}}"),
    )
    .expect("manifest");
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
