// SPDX-License-Identifier: AGPL-3.0-or-later
//! Identifier parsing and effective-contributor resolution.

use looklab_meta::{RawSerial, SerialHmacKey};
use looklab_trust::{
    ArtifactHash, CredentialId, EffectiveContributorId, ImageRef, SerialLink,
    resolve_effective_contributors,
};

fn credential(value: u128) -> CredentialId {
    CredentialId::from_u128(value)
}

fn hmac(serial: &[u8]) -> looklab_meta::SerialHmac {
    let key = SerialHmacKey::new([7u8; 32]);
    RawSerial::new(serial)
        .expect("ASCII serial")
        .hmac(&key, "MAKE", "MODEL")
}

#[test]
fn credentials_round_trip_through_hex() {
    let id = CredentialId::from_u128(0x0123_4567_89ab_cdef_0011_2233_4455_6677);
    let text = id.to_string();
    assert_eq!(text.len(), 32);
    assert_eq!(text.parse::<CredentialId>().expect("hex id"), id);
}

#[test]
fn image_refs_convert_from_str() {
    let id = ImageRef::from("image-1");
    assert_eq!(id.as_str(), "image-1");
}

#[test]
fn artifact_hashes_round_trip_through_hex() {
    let hash = ArtifactHash::new([0xab; 32]);
    let text = hash.to_string();
    assert_eq!(text.len(), 64);
    assert_eq!(text.parse::<ArtifactHash>().expect("hex hash"), hash);
    assert!("not-a-hash".parse::<ArtifactHash>().is_err());
}

#[test]
fn serial_links_union_transitively() {
    let links = vec![
        SerialLink {
            credential: credential(2),
            hmac: hmac(b"SERIAL-A"),
        },
        SerialLink {
            credential: credential(1),
            hmac: hmac(b"SERIAL-A"),
        },
        SerialLink {
            credential: credential(2),
            hmac: hmac(b"SERIAL-B"),
        },
        SerialLink {
            credential: credential(3),
            hmac: hmac(b"SERIAL-B"),
        },
    ];
    let resolved = resolve_effective_contributors(
        [credential(1), credential(2), credential(3), credential(4)],
        &links,
    );

    assert_eq!(
        resolved[&credential(1)],
        EffectiveContributorId::from(credential(1))
    );
    assert_eq!(
        resolved[&credential(2)],
        EffectiveContributorId::from(credential(1))
    );
    assert_eq!(
        resolved[&credential(3)],
        EffectiveContributorId::from(credential(1))
    );
    assert_eq!(
        resolved[&credential(4)],
        EffectiveContributorId::from(credential(4))
    );
}

#[test]
fn resolution_does_not_depend_on_link_order() {
    let serial_x = hmac(b"SERIAL-X");
    let serial_y = hmac(b"SERIAL-Y");
    let links = vec![
        SerialLink {
            credential: credential(5),
            hmac: serial_x,
        },
        SerialLink {
            credential: credential(1),
            hmac: serial_y,
        },
        SerialLink {
            credential: credential(3),
            hmac: serial_x,
        },
    ];
    let mut reversed = links
        .iter()
        .map(|link| SerialLink {
            credential: link.credential,
            hmac: link.hmac,
        })
        .collect::<Vec<_>>();
    reversed.reverse();

    let ids = [credential(1), credential(3), credential(5)];
    let first = resolve_effective_contributors(ids, &links);
    let second = resolve_effective_contributors(ids, &reversed);
    assert_eq!(first, second);
    assert_eq!(
        first[&credential(5)],
        EffectiveContributorId::from(credential(3))
    );
    assert_eq!(
        first[&credential(1)],
        EffectiveContributorId::from(credential(1))
    );
}
