// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_prep::{LocalImageKey, PrepError, SamplingSeed, SeedFile};
use looklab_samples::PriorTransformVersion;

fn key(name: &str) -> LocalImageKey {
    LocalImageKey::new(name).expect("valid file name")
}

fn seed() -> SamplingSeed {
    SamplingSeed::fresh().expect("os randomness")
}

#[test]
fn seed_file_round_trips_through_json() {
    let first = seed();
    let second = seed();
    let mut file = SeedFile::new(PriorTransformVersion::new(7));
    file.insert(key("DSCF0201.RAF"), second);
    file.insert(key("DSCF0200.RAF"), seed());
    file.insert(key("DSCF0200.RAF"), first);

    let json = file.to_json().expect("serializes");
    let parsed = SeedFile::from_json(&json).expect("parses");

    assert_eq!(parsed, file);
    assert_eq!(
        parsed.prior_transform_version(),
        PriorTransformVersion::new(7)
    );
    assert_eq!(parsed.seed_for(&key("DSCF0200.RAF")), Some(first));
    assert_eq!(parsed.seed_for(&key("DSCF0201.RAF")), Some(second));
    assert_eq!(parsed.seed_for(&key("other.RAF")), None);
    assert_eq!(parsed.entries().len(), 2);
}

#[test]
fn seed_file_entries_are_sorted() {
    let mut file = SeedFile::new(PriorTransformVersion::new(1));
    file.insert(key("B.RAF"), seed());
    file.insert(key("A.RAF"), seed());
    let names: Vec<&str> = file
        .entries()
        .iter()
        .map(|entry| entry.key.as_str())
        .collect();
    assert_eq!(names, vec!["A.RAF", "B.RAF"]);
}

#[test]
fn seed_file_rejects_a_bad_format_string() {
    let json = r#"{"format":"looklab-seeds/2","prior_transform_version":1,"entries":[]}"#;
    assert!(matches!(
        SeedFile::from_json(json),
        Err(PrepError::SeedFileFormat)
    ));
}

#[test]
fn seed_file_rejects_a_bad_seed_hex() {
    let json = r#"{"format":"looklab-seeds/1","prior_transform_version":1,"entries":[{"key":"A.RAF","seed":"zz"}]}"#;
    assert!(matches!(
        SeedFile::from_json(json),
        Err(PrepError::SeedFileEntry { index: 0 })
    ));
}

#[test]
fn seed_file_rejects_a_path() {
    let json = r#"{"format":"looklab-seeds/1","prior_transform_version":1,"entries":[{"key":"../A.RAF","seed":"0000000000000000000000000000000000000000000000000000000000000000"}]}"#;
    assert!(matches!(
        SeedFile::from_json(json),
        Err(PrepError::SeedFileEntry { index: 0 })
    ));
}

#[test]
fn fresh_seeds_come_from_the_operating_system() {
    assert_ne!(seed().as_bytes(), seed().as_bytes());
}

#[test]
fn debug_redacts_seed_bytes() {
    assert_eq!(format!("{:?}", seed()), "SamplingSeed(<redacted>)");
}
