// SPDX-License-Identifier: AGPL-3.0-or-later
mod common;

use common::{Fixture, compact_limits, image, manifest};
use looklab_upload::{
    BatchManifest, BlobKind, EnvelopeParts, Limits, ManifestImage, Nonce, PowSolutions, RawSerial,
    SendManifest, WebAuthnAssertion, canonical_manifest_bytes, encode_envelope,
    parse_envelope_strict, peek_nonce,
};
use proptest::prelude::*;

#[test]
fn envelope_roundtrips() {
    let fixture = Fixture::new();
    let bytes = fixture.bytes();
    assert_eq!(peek_nonce(&bytes).expect("nonce is present"), fixture.nonce);

    let parsed = parse_envelope_strict(&bytes, &compact_limits()).expect("fixture parses");
    assert_eq!(parsed.nonce, fixture.nonce);
    assert_eq!(parsed.manifest, fixture.manifest);
    assert_eq!(
        parsed.manifest_bytes,
        canonical_manifest_bytes(&fixture.manifest)
    );
    assert_eq!(parsed.assertion, fixture.assertion);
    assert_eq!(parsed.pow, fixture.pow);
    assert_eq!(parsed.blobs, fixture.blobs);
    assert!(parsed.serial.is_none());
    assert_eq!(
        parsed.manifest_hash(),
        looklab_upload::manifest_hash(&fixture.manifest)
    );
    assert_eq!(
        parsed.challenge(),
        looklab_upload::Challenge::for_send(&fixture.nonce, &parsed.send_manifest_hash)
    );

    let reencoded = encode_envelope(&EnvelopeParts {
        nonce: parsed.nonce,
        send_manifest: &parsed.send_manifest,
        manifest: &parsed.manifest,
        assertion: &parsed.assertion,
        pow: &parsed.pow,
        serial: parsed.serial.as_ref(),
        blobs: &parsed.blobs,
    });
    assert_eq!(reencoded, bytes);
}

#[test]
fn envelope_roundtrips_with_serial() {
    let mut fixture = Fixture::new();
    fixture.serial = RawSerial::new(b"1234567");
    let bytes = fixture.bytes();
    let parsed = parse_envelope_strict(&bytes, &compact_limits()).expect("fixture parses");
    assert!(parsed.serial.is_some());

    let reencoded = encode_envelope(&EnvelopeParts {
        nonce: parsed.nonce,
        send_manifest: &parsed.send_manifest,
        manifest: &parsed.manifest,
        assertion: &parsed.assertion,
        pow: &parsed.pow,
        serial: parsed.serial.as_ref(),
        blobs: &parsed.blobs,
    });
    assert_eq!(reencoded, bytes);
}

#[test]
fn encoding_is_deterministic() {
    let fixture = Fixture::new();
    assert_eq!(fixture.bytes(), fixture.bytes());
}

#[test]
fn peek_nonce_rejects_short_input() {
    let fixture = Fixture::new();
    let bytes = fixture.bytes();
    for len in 0..40 {
        let error = peek_nonce(&bytes[..len]).expect_err("short input is rejected");
        assert!(matches!(
            error,
            looklab_upload::EnvelopeError::Truncated { needed: 40, .. }
        ));
    }
    assert_eq!(
        peek_nonce(&bytes[..40]).expect("nonce is present"),
        fixture.nonce
    );
}

#[test]
fn limits_are_overridable() {
    let fixture = Fixture::new();
    let bytes = fixture.bytes();
    let limits = Limits {
        max_body_bytes: bytes.len(),
        max_manifest_bytes: usize::MAX,
        max_images: 100,
        max_blob_bytes: 1 << 20,
        max_batches: 8,
    };
    assert!(parse_envelope_strict(&bytes, &limits).is_ok());
}

fn arb_rgb() -> impl Strategy<Value = [f32; 3]> {
    (-0.4f32..60.0).prop_map(|value| [value, value * 0.5, value * 0.25])
}

fn arb_sample_blob() -> impl Strategy<Value = Vec<u8>> {
    (1usize..=4).prop_flat_map(|n| {
        (
            prop::collection::vec(arb_rgb(), n),
            prop::collection::vec(arb_rgb(), n),
            prop::collection::vec(0.01f32..1000.0, n),
            prop::collection::vec(0u8..=7, n),
        )
            .prop_map(move |(in_rgb, out_rgb, weights, flags)| {
                let header = looklab_samples::SampleHeader {
                    n: n as u32,
                    ..common::sample_header(None)
                };
                let flags = flags
                    .into_iter()
                    .map(|bits| looklab_samples::SampleFlags::from_bits(bits).expect("known bits"))
                    .collect();
                let set = looklab_samples::SampleSet::new(header, in_rgb, out_rgb, weights, flags)
                    .expect("generated samples are in range");
                looklab_samples::encode(&set).expect("generated samples encode")
            })
    })
}

fn arb_patch_blob() -> impl Strategy<Value = Vec<u8>> {
    (0u16..8).prop_map(|id| {
        let header = looklab_samples::PatchTableHeader {
            n: 1,
            input_space: looklab_samples::InputSpace::CameraRgbWbLinear,
            output_space: looklab_samples::OutputSpace::Srgb,
            prep_version: looklab_samples::PrepVersion::new(1),
        };
        let row = looklab_samples::PatchRow {
            id: looklab_samples::PatchId::new(id),
            in_mean: [0.1, 0.2, 0.3],
            in_std: [0.01, 0.02, 0.03],
            out_mean: [0.4, 0.5, 0.6],
            out_std: [0.02, 0.02, 0.02],
        };
        let table = looklab_samples::PatchTable::new(header, vec![row]).expect("fixture row");
        looklab_samples::encode_patch_table(&table).expect("fixture patch table")
    })
}

fn arb_image() -> impl Strategy<Value = (ManifestImage, Vec<u8>)> {
    prop_oneof![
        arb_sample_blob().prop_map(|blob| (image(&blob, BlobKind::Samples), blob)),
        arb_patch_blob().prop_map(|blob| (image(&blob, BlobKind::PatchTable), blob)),
    ]
}

fn arb_manifest() -> impl Strategy<Value = (BatchManifest, Vec<Vec<u8>>)> {
    prop::collection::vec(arb_image(), 1..=3).prop_map(|entries| {
        let mut images = Vec::with_capacity(entries.len());
        let mut blobs = Vec::with_capacity(entries.len());
        for (image, blob) in entries {
            images.push(image);
            blobs.push(blob);
        }
        (manifest(images), blobs)
    })
}

fn arb_assertion() -> impl Strategy<Value = WebAuthnAssertion> {
    (1usize..=255, 0usize..=512, 0usize..=1024, 0usize..=512).prop_map(
        |(credential, auth, client, signature)| WebAuthnAssertion {
            credential_id: vec![0xaa; credential],
            authenticator_data: vec![0xbb; auth],
            client_data_json: vec![0xcc; client],
            signature: vec![0xdd; signature],
        },
    )
}

fn arb_pow() -> impl Strategy<Value = PowSolutions> {
    prop::collection::vec(any::<u64>(), 16).prop_map(|values| {
        let mut array = [0u64; 16];
        array.copy_from_slice(&values);
        PowSolutions::new(array)
    })
}

fn arb_serial() -> impl Strategy<Value = Option<String>> {
    prop::option::of("[!-~]{1,64}")
}

fn arb_nonce() -> impl Strategy<Value = Nonce> {
    prop::collection::vec(any::<u8>(), 32).prop_map(|bytes| {
        let mut array = [0u8; 32];
        array.copy_from_slice(&bytes);
        Nonce::from_bytes(array)
    })
}

#[derive(Debug)]
struct Generated {
    nonce: Nonce,
    send_manifest: SendManifest,
    manifest: BatchManifest,
    assertion: WebAuthnAssertion,
    pow: PowSolutions,
    serial: Option<String>,
    blobs: Vec<Vec<u8>>,
}

impl Generated {
    fn serial(&self) -> Option<RawSerial> {
        self.serial
            .as_ref()
            .and_then(|text| RawSerial::new(text.as_bytes()))
    }

    fn bytes(&self) -> Vec<u8> {
        let serial = self.serial();
        encode_envelope(&EnvelopeParts {
            nonce: self.nonce,
            send_manifest: &self.send_manifest,
            manifest: &self.manifest,
            assertion: &self.assertion,
            pow: &self.pow,
            serial: serial.as_ref(),
            blobs: &self.blobs,
        })
    }
}

fn arb_envelope() -> impl Strategy<Value = Generated> {
    (
        arb_manifest(),
        arb_nonce(),
        arb_assertion(),
        arb_pow(),
        arb_serial(),
    )
        .prop_map(
            |((manifest, blobs), nonce, assertion, pow, serial)| {
                let send_manifest = SendManifest::canonical(vec![looklab_upload::manifest_hash(
                    &manifest,
                )])
                .expect("send manifest");
                Generated {
                    nonce,
                    send_manifest,
                    manifest,
                    assertion,
                    pow,
                    serial,
                    blobs,
                }
            },
        )
}

proptest! {
    #[test]
    fn encoded_envelopes_roundtrip(generated in arb_envelope()) {
        let bytes = generated.bytes();
        prop_assert_eq!(
            peek_nonce(&bytes).expect("nonce is present"),
            generated.nonce
        );
        let parsed = parse_envelope_strict(&bytes, &compact_limits())
            .map_err(|error| TestCaseError::fail(format!("{error}")))?;
        prop_assert_eq!(parsed.nonce, generated.nonce);
        prop_assert_eq!(&parsed.manifest, &generated.manifest);
        prop_assert_eq!(
            &parsed.manifest_bytes,
            &canonical_manifest_bytes(&generated.manifest)
        );
        prop_assert_eq!(&parsed.assertion, &generated.assertion);
        prop_assert_eq!(&parsed.pow, &generated.pow);
        prop_assert_eq!(&parsed.blobs, &generated.blobs);
        prop_assert_eq!(parsed.serial.is_some(), generated.serial.is_some());
        let reencoded = encode_envelope(&EnvelopeParts {
            nonce: parsed.nonce,
            send_manifest: &parsed.send_manifest,
            manifest: &parsed.manifest,
            assertion: &parsed.assertion,
            pow: &parsed.pow,
            serial: parsed.serial.as_ref(),
            blobs: &parsed.blobs,
        });
        prop_assert_eq!(reencoded, bytes);
    }
}
