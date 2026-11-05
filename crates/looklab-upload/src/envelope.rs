// SPDX-License-Identifier: AGPL-3.0-or-later
//! Canonical upload envelope ([docs/03 §2.3](../../../docs/03-data-model.md)).
//!
//! Encoding is total for values that satisfy the field limits; decoding rejects
//! every deviation from the layout with its own error variant.

use looklab_meta::{
    MAX_FIRMWARE_ID_LEN, MAX_FIRMWARE_VERSION_LEN, MAX_LOOK_SLUG_LEN, MAX_LOOK_VERSION_LEN,
    RawSerial,
};
use sha2::{Digest, Sha256};

use crate::ids::{MAX_BODY_ID_LEN, MAX_FIRMWARE_LEN, MAX_SLUG_LEN};
use crate::manifest::{
    BatchManifest, BlobKind, IntentError, canonical_manifest_bytes, manifest_hash,
};
use crate::metadata::MetadataError;
use crate::nonce::{Challenge, ManifestHash, Nonce};
use crate::pow::{POW_K, PowSolutions};
use crate::webauthn::{
    MAX_AUTHENTICATOR_DATA, MAX_CLIENT_DATA_JSON, MAX_CREDENTIAL_ID, MAX_SIGNATURE,
    WebAuthnAssertion,
};

pub const MAGIC: [u8; 6] = *b"LLUPLD";
pub const VERSION: u16 = 1;

const NONCE_OFFSET: usize = MAGIC.len() + 2;

pub const DEFAULT_MAX_BODY_BYTES: usize = 256 * 1024 * 1024;
pub const DEFAULT_MAX_MANIFEST_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MAX_IMAGES: usize = 100;
pub const DEFAULT_MAX_BLOB_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Limits {
    pub max_body_bytes: usize,
    pub max_manifest_bytes: usize,
    pub max_images: usize,
    pub max_blob_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_manifest_bytes: DEFAULT_MAX_MANIFEST_BYTES,
            max_images: DEFAULT_MAX_IMAGES,
            max_blob_bytes: DEFAULT_MAX_BLOB_BYTES,
        }
    }
}

#[derive(Clone, Copy)]
pub struct EnvelopeParts<'a> {
    pub nonce: Nonce,
    pub manifest: &'a BatchManifest,
    pub assertion: &'a WebAuthnAssertion,
    pub pow: &'a PowSolutions,
    pub serial: Option<&'a RawSerial>,
    pub blobs: &'a [Vec<u8>],
}

pub struct Envelope {
    pub nonce: Nonce,
    pub manifest: BatchManifest,
    pub manifest_bytes: Vec<u8>,
    pub assertion: WebAuthnAssertion,
    pub pow: PowSolutions,
    pub serial: Option<RawSerial>,
    pub blobs: Vec<Vec<u8>>,
}

impl Envelope {
    pub fn manifest_hash(&self) -> ManifestHash {
        manifest_hash(&self.manifest)
    }

    pub fn challenge(&self) -> Challenge {
        Challenge::new(&self.nonce, &self.manifest_hash())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestField {
    LookSlug,
    Body,
    FirmwareVersion,
    SimKey,
    ControlKey,
    CardIdentifier,
}

impl core::fmt::Display for ManifestField {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::LookSlug => "look slug",
            Self::Body => "body id",
            Self::FirmwareVersion => "firmware version",
            Self::SimKey => "sim key",
            Self::ControlKey => "control key",
            Self::CardIdentifier => "card identifier",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    #[error("envelope of {len} bytes exceeds the {max}-byte limit")]
    BodyTooLarge { len: usize, max: usize },
    #[error("envelope ends before byte {needed} (length {len})")]
    Truncated { needed: usize, len: usize },
    #[error("bad magic {found:02x?}, expected {expected:02x?}")]
    BadMagic { found: [u8; 6], expected: [u8; 6] },
    #[error("unsupported envelope version {0}")]
    UnsupportedVersion(u16),
    #[error("manifest of {len} bytes exceeds the {max}-byte limit")]
    ManifestTooLong { len: usize, max: usize },
    #[error("manifest is not a valid postcard BatchManifest")]
    ManifestCodec(#[source] postcard::Error),
    #[error("manifest has {len} unexpected trailing bytes")]
    ManifestTrailingBytes { len: usize },
    #[error("manifest is not in canonical encoding")]
    ManifestNotCanonical,
    #[error("batch has {count} images, outside 1..={max}")]
    ImageCountOutOfRange { count: usize, max: usize },
    #[error("{field} of {len} bytes exceeds the {max}-byte limit")]
    ManifestStringTooLong {
        field: ManifestField,
        len: usize,
        max: usize,
    },
    #[error("image {index} metadata is invalid: {source}")]
    MetadataInvalid {
        index: usize,
        #[source]
        source: MetadataError,
    },
    #[error("batch intent does not match its images: {source}")]
    IntentInvalid {
        #[source]
        source: IntentError,
    },
    #[error("credential id is empty")]
    CredentialIdEmpty,
    #[error("authenticator data of {len} bytes exceeds the {max}-byte limit")]
    AuthenticatorDataTooLong { len: usize, max: usize },
    #[error("client_data_json of {len} bytes exceeds the {max}-byte limit")]
    ClientDataJsonTooLong { len: usize, max: usize },
    #[error("signature of {len} bytes exceeds the {max}-byte limit")]
    SignatureTooLong { len: usize, max: usize },
    #[error("proof-of-work has {found} sub-puzzles, expected {expected}")]
    WrongPowCount { found: u8, expected: u8 },
    #[error("serial tag {0} is neither 0 (absent) nor 1 (present)")]
    BadSerialTag(u8),
    #[error("serial is not valid ASCII")]
    InvalidSerial(#[source] looklab_meta::MetaError),
    #[error("blob count {count} does not match the manifest's {images} images")]
    BlobCountMismatch { count: usize, images: usize },
    #[error("blob {index} of {len} bytes exceeds the {max}-byte limit")]
    BlobTooLong {
        index: usize,
        len: usize,
        max: usize,
    },
    #[error("blob {index} does not hash to the manifest's sha256")]
    BlobHashMismatch { index: usize },
    #[error("blob {index} is not a valid {kind:?} blob")]
    BlobInvalid {
        index: usize,
        kind: BlobKind,
        #[source]
        source: looklab_samples::Error,
    },
    #[error("{count} unexpected bytes after the last blob")]
    TrailingBytes { count: usize },
}

/// Encodes the canonical layout.
///
/// # Panics
///
/// If a field length exceeds its envelope limit; [`parse_envelope_strict`]
/// enforces the same limits on the wire.
pub fn encode_envelope(parts: &EnvelopeParts<'_>) -> Vec<u8> {
    let manifest_bytes = canonical_manifest_bytes(parts.manifest);
    assert!(
        parts.blobs.len() == parts.manifest.images.len(),
        "blob count must match the manifest image count"
    );
    assert!(
        parts.assertion.credential_id.len() <= MAX_CREDENTIAL_ID,
        "credential id over the envelope limit"
    );
    assert!(
        !parts.assertion.credential_id.is_empty(),
        "credential id must be non-empty"
    );
    assert!(
        parts.assertion.authenticator_data.len() <= MAX_AUTHENTICATOR_DATA,
        "authenticator data over the envelope limit"
    );
    assert!(
        parts.assertion.client_data_json.len() <= MAX_CLIENT_DATA_JSON,
        "client_data_json over the envelope limit"
    );
    assert!(
        parts.assertion.signature.len() <= MAX_SIGNATURE,
        "signature over the envelope limit"
    );

    let mut out = Vec::with_capacity(
        NONCE_OFFSET + 32 + 4 + manifest_bytes.len() + 1024 + parts.blobs.len() * 1024,
    );
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(parts.nonce.as_bytes());
    out.extend_from_slice(&(manifest_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&manifest_bytes);

    out.push(parts.assertion.credential_id.len() as u8);
    out.extend_from_slice(&parts.assertion.credential_id);
    push_u16_prefixed(&mut out, &parts.assertion.authenticator_data);
    push_u16_prefixed(&mut out, &parts.assertion.client_data_json);
    push_u16_prefixed(&mut out, &parts.assertion.signature);

    out.push(POW_K as u8);
    for solution in parts.pow.iter() {
        out.extend_from_slice(&solution.to_le_bytes());
    }

    match parts.serial {
        Some(serial) => serial.write_envelope_field(&mut out),
        None => out.push(0),
    }

    assert!(
        parts.blobs.len() <= u16::MAX as usize,
        "blob count over the envelope limit"
    );
    out.extend_from_slice(&(parts.blobs.len() as u16).to_le_bytes());
    for blob in parts.blobs {
        out.extend_from_slice(&(blob.len() as u32).to_le_bytes());
        out.extend_from_slice(blob);
    }
    out
}

fn push_u16_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// Reads the nonce at its fixed offset.
pub fn peek_nonce(bytes: &[u8]) -> Result<Nonce, EnvelopeError> {
    let needed = NONCE_OFFSET + 32;
    if bytes.len() < needed {
        return Err(EnvelopeError::Truncated {
            needed,
            len: bytes.len(),
        });
    }
    Nonce::from_slice(&bytes[NONCE_OFFSET..needed]).ok_or(EnvelopeError::Truncated {
        needed,
        len: bytes.len(),
    })
}

pub fn parse_envelope_strict(bytes: &[u8], limits: &Limits) -> Result<Envelope, EnvelopeError> {
    if bytes.len() > limits.max_body_bytes {
        return Err(EnvelopeError::BodyTooLarge {
            len: bytes.len(),
            max: limits.max_body_bytes,
        });
    }
    let mut reader = Reader::new(bytes);

    let magic = reader.take(MAGIC.len())?;
    let magic: [u8; 6] = magic.try_into().expect("slice of the requested length");
    if magic != MAGIC {
        return Err(EnvelopeError::BadMagic {
            found: magic,
            expected: MAGIC,
        });
    }
    let version = reader.u16()?;
    if version != VERSION {
        return Err(EnvelopeError::UnsupportedVersion(version));
    }
    let nonce = Nonce::from_slice(reader.take(32)?).expect("slice of the requested length");

    let manifest_len = reader.u32()? as usize;
    if manifest_len > limits.max_manifest_bytes {
        return Err(EnvelopeError::ManifestTooLong {
            len: manifest_len,
            max: limits.max_manifest_bytes,
        });
    }
    let manifest_bytes = reader.take(manifest_len)?;
    if let Some(count) = scan_manifest_images(manifest_bytes)?
        && (count == 0 || count > limits.max_images)
    {
        return Err(EnvelopeError::ImageCountOutOfRange {
            count,
            max: limits.max_images,
        });
    }
    let (manifest, rest) = postcard::take_from_bytes::<BatchManifest>(manifest_bytes)
        .map_err(EnvelopeError::ManifestCodec)?;
    if !rest.is_empty() {
        return Err(EnvelopeError::ManifestTrailingBytes { len: rest.len() });
    }
    if canonical_manifest_bytes(&manifest) != manifest_bytes {
        return Err(EnvelopeError::ManifestNotCanonical);
    }
    if manifest.images.is_empty() || manifest.images.len() > limits.max_images {
        return Err(EnvelopeError::ImageCountOutOfRange {
            count: manifest.images.len(),
            max: limits.max_images,
        });
    }
    for (index, image) in manifest.images.iter().enumerate() {
        image
            .meta
            .validate()
            .map_err(|source| EnvelopeError::MetadataInvalid { index, source })?;
    }
    manifest
        .validate_intent()
        .map_err(|source| EnvelopeError::IntentInvalid { source })?;

    let credential_len = usize::from(reader.u8()?);
    if credential_len == 0 {
        return Err(EnvelopeError::CredentialIdEmpty);
    }
    let credential_id = reader.take(credential_len)?.to_vec();
    let authenticator_data = reader.u16_prefixed(MAX_AUTHENTICATOR_DATA, |len, max| {
        EnvelopeError::AuthenticatorDataTooLong { len, max }
    })?;
    let client_data_json = reader.u16_prefixed(MAX_CLIENT_DATA_JSON, |len, max| {
        EnvelopeError::ClientDataJsonTooLong { len, max }
    })?;
    let signature = reader.u16_prefixed(MAX_SIGNATURE, |len, max| {
        EnvelopeError::SignatureTooLong { len, max }
    })?;

    let k = reader.u8()?;
    if usize::from(k) != POW_K {
        return Err(EnvelopeError::WrongPowCount {
            found: k,
            expected: POW_K as u8,
        });
    }
    let mut solutions = [0u64; POW_K];
    for slot in &mut solutions {
        *slot = reader.u64()?;
    }
    let pow = PowSolutions::new(solutions);

    let serial = match reader.u8()? {
        0 => None,
        1 => {
            let len = usize::from(reader.u8()?);
            let ascii = reader.take(len)?;
            Some(RawSerial::from_envelope_ascii(ascii).map_err(EnvelopeError::InvalidSerial)?)
        }
        tag => return Err(EnvelopeError::BadSerialTag(tag)),
    };

    let blob_count = usize::from(reader.u16()?);
    if blob_count != manifest.images.len() {
        return Err(EnvelopeError::BlobCountMismatch {
            count: blob_count,
            images: manifest.images.len(),
        });
    }
    let mut blobs = Vec::with_capacity(blob_count);
    for (index, image) in manifest.images.iter().enumerate() {
        let len = reader.u32()? as usize;
        if len > limits.max_blob_bytes {
            return Err(EnvelopeError::BlobTooLong {
                index,
                len,
                max: limits.max_blob_bytes,
            });
        }
        let blob = reader.take(len)?;
        let digest = Sha256::digest(blob);
        if digest[..] != image.blob_sha256 {
            return Err(EnvelopeError::BlobHashMismatch { index });
        }
        match image.blob_kind {
            BlobKind::Samples => looklab_samples::decode(blob).map(|_| ()),
            BlobKind::PatchTable => looklab_samples::decode_patch_table(blob).map(|_| ()),
        }
        .map_err(|source| EnvelopeError::BlobInvalid {
            index,
            kind: image.blob_kind,
            source,
        })?;
        blobs.push(blob.to_vec());
    }
    if !reader.is_empty() {
        return Err(EnvelopeError::TrailingBytes {
            count: reader.remaining(),
        });
    }

    Ok(Envelope {
        nonce,
        manifest,
        manifest_bytes: manifest_bytes.to_vec(),
        assertion: WebAuthnAssertion {
            credential_id,
            authenticator_data,
            client_data_json,
            signature,
        },
        pow,
        serial,
        blobs,
    })
}

/// Walks the fixed manifest prefix to the image-count varint without allocating.
/// `None` means the bytes could not be scanned and the decoder should report.
fn scan_manifest_images(bytes: &[u8]) -> Result<Option<usize>, EnvelopeError> {
    let mut scan = Scan::new(bytes);
    let Some(_kind) = scan.varint() else {
        return Ok(None);
    };
    let Some(intent) = scan.varint() else {
        return Ok(None);
    };
    if intent == 1 {
        let Some(len) = scan.varint() else {
            return Ok(None);
        };
        check_string_len(len, ManifestField::LookSlug, MAX_SLUG_LEN)?;
        if !scan.skip(len) {
            return Ok(None);
        }
    }
    if intent == 2 {
        let Some(variant) = scan.varint() else {
            return Ok(None);
        };
        let limits: &[usize] = match variant {
            0 => &[
                MAX_FIRMWARE_ID_LEN,
                MAX_FIRMWARE_VERSION_LEN,
                MAX_LOOK_SLUG_LEN,
                MAX_LOOK_VERSION_LEN,
            ],
            1 => &[MAX_FIRMWARE_ID_LEN, MAX_FIRMWARE_VERSION_LEN],
            _ => return Ok(None),
        };
        for &max in limits {
            let Some(len) = scan.varint() else {
                return Ok(None);
            };
            check_string_len(len, ManifestField::CardIdentifier, max)?;
            if !scan.skip(len) {
                return Ok(None);
            }
        }
        if variant == 0 && !scan.skip(8) {
            return Ok(None);
        }
    }
    for (field, max) in [
        (ManifestField::Body, MAX_BODY_ID_LEN),
        (ManifestField::FirmwareVersion, MAX_FIRMWARE_LEN),
        (ManifestField::SimKey, MAX_SLUG_LEN),
    ] {
        let Some(len) = scan.varint() else {
            return Ok(None);
        };
        check_string_len(len, field, max)?;
        if !scan.skip(len) {
            return Ok(None);
        }
    }
    // community_snapshot_version
    if scan.varint().is_none() {
        return Ok(None);
    }
    let Some(controls) = scan.varint() else {
        return Ok(None);
    };
    if controls > bytes.len() as u64 {
        return Ok(None);
    }
    for _ in 0..controls {
        let Some(len) = scan.varint() else {
            return Ok(None);
        };
        check_string_len(len, ManifestField::ControlKey, MAX_SLUG_LEN)?;
        if !scan.skip(len) {
            return Ok(None);
        }
        if scan.varint().is_none() || scan.varint().is_none() {
            return Ok(None);
        }
    }
    let Some(images) = scan.varint() else {
        return Ok(None);
    };
    Ok(Some(usize::try_from(images).unwrap_or(usize::MAX)))
}

fn check_string_len(len: u64, field: ManifestField, max: usize) -> Result<(), EnvelopeError> {
    if len > max as u64 {
        return Err(EnvelopeError::ManifestStringTooLong {
            field,
            len: usize::try_from(len).unwrap_or(usize::MAX),
            max,
        });
    }
    Ok(())
}

struct Scan<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Scan<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for index in 0..10 {
            let byte = *self.bytes.get(self.pos)?;
            self.pos += 1;
            let payload = u64::from(byte & 0x7f);
            if index == 9 && payload > 1 {
                return None;
            }
            value |= payload << (7 * index);
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    fn skip(&mut self, len: u64) -> bool {
        let Ok(len) = usize::try_from(len) else {
            return false;
        };
        match self.pos.checked_add(len) {
            Some(end) if end <= self.bytes.len() => {
                self.pos = end;
                true
            }
            _ => false,
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    fn is_empty(&self) -> bool {
        self.pos == self.bytes.len()
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], EnvelopeError> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len());
        let Some(end) = end else {
            return Err(EnvelopeError::Truncated {
                needed: self.pos.saturating_add(len),
                len: self.bytes.len(),
            });
        };
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, EnvelopeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, EnvelopeError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, EnvelopeError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, EnvelopeError> {
        let bytes = self.take(8)?;
        let mut array = [0u8; 8];
        array.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(array))
    }

    fn u16_prefixed(
        &mut self,
        max: usize,
        too_long: impl Fn(usize, usize) -> EnvelopeError,
    ) -> Result<Vec<u8>, EnvelopeError> {
        let len = usize::from(self.u16()?);
        if len > max {
            return Err(too_long(len, max));
        }
        Ok(self.take(len)?.to_vec())
    }
}
