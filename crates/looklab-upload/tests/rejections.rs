// SPDX-License-Identifier: AGPL-3.0-or-later
mod common;

use common::{Fixture, compact_limits, image, manifest};
use looklab_upload::{
    BlobKind, EnvelopeError, Limits, LookSettings, ManifestField, ManifestMetadata, MetadataError,
    MetadataField, Tagged, WhiteBalanceMultipliers, parse_envelope_strict,
};

fn parse_error(bytes: &[u8], limits: &Limits) -> EnvelopeError {
    match parse_envelope_strict(bytes, limits) {
        Ok(_) => panic!("expected a rejection"),
        Err(error) => error,
    }
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[test]
fn rejects_bad_magic() {
    let fixture = Fixture::new();
    let mut bytes = fixture.bytes();
    bytes[0] = b'X';
    assert!(matches!(
        parse_error(&bytes, &compact_limits()),
        EnvelopeError::BadMagic { .. }
    ));
}

#[test]
fn rejects_unsupported_version() {
    let fixture = Fixture::new();
    let mut bytes = fixture.bytes();
    write_u16(&mut bytes, fixture.offsets().version, 2);
    assert!(matches!(
        parse_error(&bytes, &compact_limits()),
        EnvelopeError::UnsupportedVersion(2)
    ));
}

#[test]
fn rejects_truncated_envelope() {
    let fixture = Fixture::new();
    let bytes = fixture.bytes();
    assert!(matches!(
        parse_error(&bytes[..5], &compact_limits()),
        EnvelopeError::Truncated { .. }
    ));
}

#[test]
fn rejects_oversized_body() {
    let fixture = Fixture::new();
    let bytes = fixture.bytes();
    let limits = Limits {
        max_body_bytes: 8,
        ..compact_limits()
    };
    assert!(matches!(
        parse_error(&bytes, &limits),
        EnvelopeError::BodyTooLarge { .. }
    ));
}

#[test]
fn rejects_oversized_manifest() {
    let fixture = Fixture::new();
    let bytes = fixture.bytes();
    let limits = Limits {
        max_manifest_bytes: 4,
        ..compact_limits()
    };
    assert!(matches!(
        parse_error(&bytes, &limits),
        EnvelopeError::ManifestTooLong { .. }
    ));
}

#[test]
fn rejects_manifest_codec_error() {
    let fixture = Fixture::new();
    let mut bytes = fixture.bytes();
    write_u32(&mut bytes, fixture.offsets().manifest_len, 0);
    assert!(matches!(
        parse_error(&bytes, &compact_limits()),
        EnvelopeError::ManifestCodec(_)
    ));
}

#[test]
fn rejects_manifest_trailing_bytes() {
    let fixture = Fixture::new();
    let offsets = fixture.offsets();
    let manifest_len = u32::from_le_bytes(
        fixture.bytes()[offsets.manifest_len..offsets.manifest_len + 4]
            .try_into()
            .expect("four bytes"),
    ) as usize;
    let mut bytes = fixture.bytes();
    bytes.insert(offsets.manifest + manifest_len, 0);
    write_u32(&mut bytes, offsets.manifest_len, (manifest_len as u32) + 1);
    assert!(matches!(
        parse_error(&bytes, &compact_limits()),
        EnvelopeError::ManifestTrailingBytes { len: 1 }
    ));
}

#[test]
fn rejects_non_canonical_manifest() {
    let fixture = Fixture::new();
    let offsets = fixture.offsets();
    let manifest_len = u32::from_le_bytes(
        fixture.bytes()[offsets.manifest_len..offsets.manifest_len + 4]
            .try_into()
            .expect("four bytes"),
    ) as usize;
    let mut bytes = fixture.bytes();
    bytes.insert(offsets.manifest, 0x80);
    write_u32(&mut bytes, offsets.manifest_len, (manifest_len as u32) + 1);
    assert!(matches!(
        parse_error(&bytes, &compact_limits()),
        EnvelopeError::ManifestNotCanonical
    ));
}

#[test]
fn rejects_empty_image_list() {
    let mut fixture = Fixture::new();
    fixture.manifest = manifest(vec![]);
    fixture.blobs.clear();
    assert!(matches!(
        parse_error(&fixture.bytes(), &compact_limits()),
        EnvelopeError::ImageCountOutOfRange { count: 0, .. }
    ));
}

#[test]
fn rejects_too_many_images() {
    let fixture = Fixture::new();
    let limits = Limits {
        max_images: 1,
        ..compact_limits()
    };
    assert!(matches!(
        parse_error(&fixture.bytes(), &limits),
        EnvelopeError::ImageCountOutOfRange { count: 2, .. }
    ));
}

#[test]
fn rejects_empty_credential_id() {
    let fixture = Fixture::new();
    let mut bytes = fixture.bytes();
    bytes[fixture.offsets().credential_len] = 0;
    assert!(matches!(
        parse_error(&bytes, &compact_limits()),
        EnvelopeError::CredentialIdEmpty
    ));
}

#[test]
fn rejects_oversized_authenticator_data() {
    let fixture = Fixture::new();
    let mut bytes = fixture.bytes();
    write_u16(&mut bytes, fixture.offsets().authenticator_len, 513);
    assert!(matches!(
        parse_error(&bytes, &compact_limits()),
        EnvelopeError::AuthenticatorDataTooLong { len: 513, .. }
    ));
}

#[test]
fn rejects_oversized_client_data() {
    let fixture = Fixture::new();
    let mut bytes = fixture.bytes();
    write_u16(&mut bytes, fixture.offsets().client_len, 1025);
    assert!(matches!(
        parse_error(&bytes, &compact_limits()),
        EnvelopeError::ClientDataJsonTooLong { len: 1025, .. }
    ));
}

#[test]
fn rejects_oversized_signature() {
    let fixture = Fixture::new();
    let mut bytes = fixture.bytes();
    write_u16(&mut bytes, fixture.offsets().signature_len, 513);
    assert!(matches!(
        parse_error(&bytes, &compact_limits()),
        EnvelopeError::SignatureTooLong { len: 513, .. }
    ));
}

#[test]
fn rejects_wrong_pow_count() {
    let fixture = Fixture::new();
    let mut bytes = fixture.bytes();
    bytes[fixture.offsets().pow_k] = 15;
    assert!(matches!(
        parse_error(&bytes, &compact_limits()),
        EnvelopeError::WrongPowCount {
            found: 15,
            expected: 16
        }
    ));
}

#[test]
fn rejects_bad_serial_tag() {
    let fixture = Fixture::new();
    let mut bytes = fixture.bytes();
    bytes[fixture.offsets().serial_tag] = 2;
    assert!(matches!(
        parse_error(&bytes, &compact_limits()),
        EnvelopeError::BadSerialTag(2)
    ));
}

#[test]
fn rejects_non_ascii_serial() {
    let fixture = Fixture::new();
    let offsets = fixture.offsets();
    let mut bytes = fixture.bytes();
    bytes[offsets.serial_tag] = 1;
    bytes.insert(offsets.serial_tag + 1, 1);
    bytes.insert(offsets.serial_tag + 2, 0xff);
    assert!(matches!(
        parse_error(&bytes, &compact_limits()),
        EnvelopeError::InvalidSerial(_)
    ));
}

#[test]
fn rejects_blob_count_mismatch() {
    let fixture = Fixture::new();
    let mut bytes = fixture.bytes();
    write_u16(&mut bytes, fixture.offsets().blob_count, 1);
    assert!(matches!(
        parse_error(&bytes, &compact_limits()),
        EnvelopeError::BlobCountMismatch {
            count: 1,
            images: 2
        }
    ));
}

#[test]
fn rejects_oversized_blob() {
    let fixture = Fixture::new();
    let limits = Limits {
        max_blob_bytes: 4,
        ..compact_limits()
    };
    assert!(matches!(
        parse_error(&fixture.bytes(), &limits),
        EnvelopeError::BlobTooLong { index: 0, .. }
    ));
}

#[test]
fn rejects_altered_blob() {
    let fixture = Fixture::new();
    let offsets = fixture.offsets();
    let mut bytes = fixture.bytes();
    let first_blob = offsets.blob_count + 2 + 4;
    bytes[first_blob] ^= 0xff;
    assert!(matches!(
        parse_error(&bytes, &compact_limits()),
        EnvelopeError::BlobHashMismatch { index: 0 }
    ));
}

#[test]
fn rejects_malformed_blob() {
    let blobs = vec![vec![0u8; 16]];
    let mut fixture = Fixture::new();
    fixture.manifest = manifest(vec![image(&blobs[0], BlobKind::Samples)]);
    fixture.blobs = blobs;
    assert!(matches!(
        parse_error(&fixture.bytes(), &compact_limits()),
        EnvelopeError::BlobInvalid {
            index: 0,
            kind: BlobKind::Samples,
            ..
        }
    ));
}

#[test]
fn rejects_trailing_bytes() {
    let fixture = Fixture::new();
    let mut bytes = fixture.bytes();
    bytes.push(0);
    assert!(matches!(
        parse_error(&bytes, &compact_limits()),
        EnvelopeError::TrailingBytes { count: 1 }
    ));
}

fn fixture_with_meta(meta: ManifestMetadata) -> Fixture {
    let mut fixture = Fixture::new();
    let blob = fixture.blobs[0].clone();
    let mut image = image(&blob, BlobKind::Samples);
    image.meta = meta;
    fixture.manifest = manifest(vec![image]);
    fixture.blobs = vec![blob];
    fixture
}

#[test]
fn rejects_too_many_images_before_decoding() {
    let fixture = Fixture::new();
    let offsets = fixture.offsets();
    let count_at = offsets.manifest + common::images_count_offset(&fixture.manifest);
    let mut bytes = fixture.bytes();
    bytes[count_at] = 3;
    write_u32(
        &mut bytes,
        offsets.manifest_len,
        (count_at - offsets.manifest + 1) as u32,
    );
    let limits = Limits {
        max_images: 2,
        ..compact_limits()
    };
    assert!(matches!(
        parse_error(&bytes, &limits),
        EnvelopeError::ImageCountOutOfRange { count: 3, max: 2 }
    ));
}

#[test]
fn rejects_oversized_manifest_string_before_decoding() {
    let fixture = Fixture::new();
    let offsets = fixture.offsets();
    let length_at = offsets.manifest + 2;
    let mut bytes = fixture.bytes();
    bytes[length_at] = 0x81;
    bytes[length_at + 1] = 0x01;
    assert!(matches!(
        parse_error(&bytes, &compact_limits()),
        EnvelopeError::ManifestStringTooLong {
            field: ManifestField::Body,
            len: 129,
            max: 128
        }
    ));
}

#[test]
fn rejects_non_finite_exposure_compensation() {
    let mut meta = common::metadata();
    meta.exposure_compensation_ev = Some(f64::NAN);
    assert!(matches!(
        parse_error(&fixture_with_meta(meta).bytes(), &compact_limits()),
        EnvelopeError::MetadataInvalid {
            index: 0,
            source: MetadataError::NonFinite {
                field: MetadataField::ExposureCompensation
            }
        }
    ));
}

#[test]
fn rejects_zero_iso() {
    let mut meta = common::metadata();
    meta.iso = Some(0);
    assert!(matches!(
        parse_error(&fixture_with_meta(meta).bytes(), &compact_limits()),
        EnvelopeError::MetadataInvalid {
            index: 0,
            source: MetadataError::ZeroIso
        }
    ));
}

#[test]
fn rejects_non_positive_shutter() {
    let mut meta = common::metadata();
    meta.shutter_seconds = Some(0.0);
    assert!(matches!(
        parse_error(&fixture_with_meta(meta).bytes(), &compact_limits()),
        EnvelopeError::MetadataInvalid {
            index: 0,
            source: MetadataError::NotPositive {
                field: MetadataField::Shutter
            }
        }
    ));
}

#[test]
fn rejects_non_finite_white_balance() {
    let mut meta = common::metadata();
    meta.as_shot_wb = Some(WhiteBalanceMultipliers {
        r: 1.0,
        g: f64::INFINITY,
        b: 1.0,
    });
    assert!(matches!(
        parse_error(&fixture_with_meta(meta).bytes(), &compact_limits()),
        EnvelopeError::MetadataInvalid {
            index: 0,
            source: MetadataError::NonFinite {
                field: MetadataField::AsShotWhiteBalance
            }
        }
    ));
}

#[test]
fn rejects_non_finite_look_tone() {
    let mut meta = common::metadata();
    meta.look = Some(LookSettings {
        highlight_tone: Some(Tagged {
            raw: 0,
            value: Some(f64::NAN),
        }),
        ..LookSettings::default()
    });
    assert!(matches!(
        parse_error(&fixture_with_meta(meta).bytes(), &compact_limits()),
        EnvelopeError::MetadataInvalid {
            index: 0,
            source: MetadataError::NonFinite {
                field: MetadataField::HighlightTone
            }
        }
    ));
}

#[test]
fn decodes_both_blob_kinds() {
    let fixture = Fixture::new();
    let parsed =
        parse_envelope_strict(&fixture.bytes(), &compact_limits()).expect("fixture parses");
    assert_eq!(parsed.blobs.len(), 2);
    assert!(looklab_samples::decode(&parsed.blobs[0]).is_ok());
    assert!(looklab_samples::decode_patch_table(&parsed.blobs[1]).is_ok());
}
