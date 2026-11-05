// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_upload::{
    BodyId, Challenge, ControlEntry, ControlKey, ControlSettings, FirmwareVersionString, IdError,
    LookSlug, ManifestHash, Nonce, SettingsError, SimKey, StepIndex,
};

#[test]
fn challenge_known_answer() {
    let nonce = Nonce::from_bytes([0x00; 32]);
    let manifest_hash = ManifestHash::from_bytes([0x11; 32]);
    let expected = [
        0x88, 0x78, 0xb1, 0x5a, 0x7d, 0x6a, 0x3a, 0x4f, 0x46, 0x4e, 0x8f, 0x9f, 0x42, 0x59, 0x1d,
        0xbc, 0x0c, 0xf4, 0xbe, 0xde, 0xa0, 0xec, 0x30, 0x90, 0x03, 0xd2, 0xb2, 0xee, 0x53, 0x65,
        0x5e, 0xf8,
    ];
    assert_eq!(Challenge::new(&nonce, &manifest_hash).as_bytes(), &expected);
}

#[test]
fn body_ids_are_make_and_model() {
    assert_eq!(
        BodyId::new("fujifilm/x-t5").expect("valid").as_str(),
        "fujifilm/x-t5"
    );
    assert_eq!(
        BodyId::new("fujifilm/x-t5").expect("valid").make(),
        "fujifilm"
    );
    assert_eq!(BodyId::new("fujifilm/x-t5").expect("valid").model(), "x-t5");
    for invalid in [
        "",
        "fujifilm",
        "fujifilm/",
        "/x-t5",
        "a/b/c",
        "Fuji/X-T5",
        "fuji/x t5",
    ] {
        assert!(BodyId::new(invalid).is_err(), "{invalid:?} should fail");
    }
    assert_eq!(BodyId::new("fujifilm").unwrap_err(), IdError::BodyIdShape);
}

#[test]
fn slugs_reject_uppercase_and_punctuation() {
    assert!(LookSlug::new("classic-chrome").is_ok());
    assert!(SimKey::new("classic_chrome").is_ok());
    assert!(ControlKey::new("highlight_tone").is_ok());
    for invalid in [
        "",
        "Classic-Chrome",
        "classic chrome",
        "classic.chrome",
        &"a".repeat(65),
    ] {
        assert!(LookSlug::new(invalid).is_err(), "{invalid:?} should fail");
    }
    assert!(matches!(
        LookSlug::new("Bad"),
        Err(IdError::InvalidCharacter { .. })
    ));
    assert!(matches!(
        LookSlug::new(&"a".repeat(65)),
        Err(IdError::TooLong { .. })
    ));
}

#[test]
fn firmware_version_is_printable_ascii() {
    assert!(FirmwareVersionString::new("2.10").is_ok());
    assert!(FirmwareVersionString::new("").is_err());
    assert!(FirmwareVersionString::new("2.10\n").is_err());
    assert!(FirmwareVersionString::new(&"9".repeat(65)).is_err());
}

fn entry(key: &str, step: i64) -> ControlEntry {
    ControlEntry::reproduced(
        ControlKey::new(key).expect("fixture key"),
        StepIndex::new(step),
    )
}

#[test]
fn settings_are_sorted_and_unique() {
    let settings = ControlSettings::new(vec![entry("shadow_tone", 1), entry("color", 2)])
        .expect("unique keys");
    let keys: Vec<_> = settings
        .entries()
        .iter()
        .map(|entry| entry.key.as_str())
        .collect();
    assert_eq!(keys, ["color", "shadow_tone"]);

    assert!(matches!(
        ControlSettings::new(vec![entry("color", 1), entry("color", 2)]),
        Err(SettingsError::DuplicateKey { .. })
    ));
    assert_eq!(
        ControlSettings::try_from(vec![entry("shadow_tone", 1), entry("color", 2)]),
        Err(SettingsError::NotSorted)
    );
}

#[test]
fn settings_roundtrip_through_postcard() {
    let settings =
        ControlSettings::new(vec![entry("color", -2), entry("grain", 1)]).expect("unique keys");
    let bytes = postcard::to_allocvec(&settings).expect("settings serialize");
    let (decoded, rest) =
        postcard::take_from_bytes::<ControlSettings>(&bytes).expect("settings deserialize");
    assert!(rest.is_empty());
    assert_eq!(decoded, settings);
}
