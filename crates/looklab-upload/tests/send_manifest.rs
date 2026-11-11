// SPDX-License-Identifier: AGPL-3.0-or-later
//! The versioned send manifest: canonical bytes, version, and disjoint membership.

use looklab_upload::{
    AttestationHash, ManifestHash, SEND_MANIFEST_VERSION, SendManifest, SendManifestError,
    canonical_send_manifest_bytes, send_manifest_hash,
};

fn batch(value: u8) -> ManifestHash {
    ManifestHash::from_bytes([value; 32])
}

fn attestation(value: u8) -> AttestationHash {
    AttestationHash::from_bytes([value; 32])
}

#[test]
fn canonical_manifest_is_versioned_and_round_trips() {
    let manifest =
        SendManifest::canonical_with_attestations(vec![batch(2), batch(1)], vec![attestation(3)])
            .expect("send manifest");
    assert_eq!(manifest.version, SEND_MANIFEST_VERSION);
    assert_eq!(manifest.batches, vec![batch(1), batch(2)]);
    let bytes = canonical_send_manifest_bytes(&manifest);
    let (decoded, rest) =
        postcard::take_from_bytes::<SendManifest>(&bytes).expect("canonical bytes decode");
    assert!(rest.is_empty());
    assert_eq!(decoded, manifest);
    assert_eq!(canonical_send_manifest_bytes(&decoded), bytes);
    assert_eq!(send_manifest_hash(&decoded), send_manifest_hash(&manifest));
}

#[test]
fn attestation_only_manifest_is_valid() {
    let manifest = SendManifest::canonical_with_attestations(Vec::new(), vec![attestation(4)])
        .expect("send manifest");
    assert!(manifest.batches.is_empty());
    assert_eq!(manifest.attestations, vec![attestation(4)]);
    assert!(manifest.contains_attestation(&attestation(4)));
    assert!(!manifest.contains(&batch(4)));
}

#[test]
fn attestation_may_not_repeat_a_batch_hash() {
    let manifest = SendManifest {
        version: SEND_MANIFEST_VERSION,
        batches: vec![batch(7)],
        attestations: vec![AttestationHash::from_bytes(*batch(7).as_bytes())],
    };
    assert!(matches!(
        manifest.validate(8, 8),
        Err(SendManifestError::AttestationIsBatch { index: 0 })
    ));
}

#[test]
fn an_unversioned_or_foreign_manifest_is_rejected() {
    let manifest = SendManifest {
        version: SEND_MANIFEST_VERSION + 1,
        batches: vec![batch(1)],
        attestations: Vec::new(),
    };
    assert!(matches!(
        manifest.validate(8, 8),
        Err(SendManifestError::UnsupportedVersion { .. })
    ));
}
