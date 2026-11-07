// SPDX-License-Identifier: AGPL-3.0-or-later
//! Header pre-parse for the fixed docs/03 §2.3 layout, so the nonce, challenge
//! and PoW can be checked before the strict parse; that parser stays authoritative.

use looklab_upload::webauthn::{
    MAX_AUTHENTICATOR_DATA, MAX_CLIENT_DATA_JSON, MAX_CREDENTIAL_ID, MAX_SIGNATURE,
};
use looklab_upload::{
    BatchManifest, Challenge, Limits, MAGIC, ManifestHash, Nonce, POW_K, PowSolutions,
    SendManifest, SendManifestHash, VERSION, WebAuthnAssertion, canonical_send_manifest_bytes,
    send_manifest_hash,
};
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub struct EnvelopeHeader {
    pub nonce: Nonce,
    pub send_manifest: SendManifest,
    pub send_manifest_hash: SendManifestHash,
    pub manifest_hash: ManifestHash,
    pub manifest: BatchManifest,
    pub assertion: WebAuthnAssertion,
    pub pow: PowSolutions,
}

impl EnvelopeHeader {
    pub fn n_images(&self) -> u32 {
        u32::try_from(self.manifest.images.len()).unwrap_or(u32::MAX)
    }

    pub fn pow_challenge(&self) -> Challenge {
        Challenge::new(&self.nonce, &self.manifest_hash)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HeaderError {
    #[error("the envelope ends before its header does")]
    Truncated,
    #[error("the envelope is not an upload envelope")]
    NotAnEnvelope,
    #[error("the manifest exceeds the configured limit")]
    ManifestTooLong,
    #[error("the manifest is not a valid batch manifest")]
    ManifestInvalid,
    #[error("the batch image count is out of range")]
    ImageCountOutOfRange,
    #[error("the WebAuthn assertion exceeds the envelope limits")]
    AssertionTooLong,
    #[error("the proof-of-work block is not the expected size")]
    PowInvalid,
}

pub fn parse_header(bytes: &[u8], limits: &Limits) -> Result<EnvelopeHeader, HeaderError> {
    if bytes.len() < MAGIC.len() + 2 {
        return Err(HeaderError::Truncated);
    }
    if bytes[..MAGIC.len()] != MAGIC {
        return Err(HeaderError::NotAnEnvelope);
    }
    let version = u16::from_le_bytes([bytes[6], bytes[7]]);
    if version != VERSION {
        return Err(HeaderError::NotAnEnvelope);
    }
    let nonce_bytes = take(bytes, 8, 32)?;
    let nonce = Nonce::from_slice(nonce_bytes).ok_or(HeaderError::Truncated)?;

    let send_manifest_len =
        u32::from_le_bytes(take(bytes, 40, 4)?.try_into().expect("4 bytes")) as usize;
    if send_manifest_len > limits.max_manifest_bytes {
        return Err(HeaderError::ManifestTooLong);
    }
    let send_manifest_bytes = take(bytes, 44, send_manifest_len)?;
    let (send_manifest, rest) = postcard::take_from_bytes::<SendManifest>(send_manifest_bytes)
        .map_err(|_| HeaderError::ManifestInvalid)?;
    if !rest.is_empty() || canonical_send_manifest_bytes(&send_manifest) != send_manifest_bytes {
        return Err(HeaderError::ManifestInvalid);
    }
    send_manifest
        .validate(limits.max_batches)
        .map_err(|_| HeaderError::ManifestInvalid)?;
    let send_hash = send_manifest_hash(&send_manifest);

    let manifest_offset = 44 + send_manifest_len;
    let manifest_len = u32::from_le_bytes(
        take(bytes, manifest_offset, 4)?
            .try_into()
            .expect("4 bytes"),
    ) as usize;
    if manifest_len > limits.max_manifest_bytes {
        return Err(HeaderError::ManifestTooLong);
    }
    let manifest_bytes = take(bytes, manifest_offset + 4, manifest_len)?;
    let (manifest, rest) = postcard::take_from_bytes::<BatchManifest>(manifest_bytes)
        .map_err(|_| HeaderError::ManifestInvalid)?;
    if !rest.is_empty() {
        return Err(HeaderError::ManifestInvalid);
    }
    if manifest.images.is_empty() || manifest.images.len() > limits.max_images {
        return Err(HeaderError::ImageCountOutOfRange);
    }
    let mut hasher = Sha256::new();
    hasher.update(manifest_bytes);
    let digest = hasher.finalize();
    let manifest_hash = ManifestHash::from_bytes(digest.into());
    if !send_manifest.contains(&manifest_hash) {
        return Err(HeaderError::ManifestInvalid);
    }

    let mut reader = Reader::new(bytes, manifest_offset + 4 + manifest_len);
    let credential_len = usize::from(reader.u8()?);
    if credential_len == 0 || credential_len > MAX_CREDENTIAL_ID {
        return Err(HeaderError::AssertionTooLong);
    }
    let credential_id = reader.take(credential_len)?.to_vec();
    let authenticator_data = reader.u16_prefixed(MAX_AUTHENTICATOR_DATA)?;
    let client_data_json = reader.u16_prefixed(MAX_CLIENT_DATA_JSON)?;
    let signature = reader.u16_prefixed(MAX_SIGNATURE)?;

    let k = reader.u8()?;
    if usize::from(k) != POW_K {
        return Err(HeaderError::PowInvalid);
    }
    let mut solutions = [0u64; POW_K];
    for slot in &mut solutions {
        *slot = reader.u64()?;
    }

    Ok(EnvelopeHeader {
        nonce,
        send_manifest_hash: send_hash,
        send_manifest,
        manifest_hash,
        manifest,
        assertion: WebAuthnAssertion {
            credential_id,
            authenticator_data,
            client_data_json,
            signature,
        },
        pow: PowSolutions::new(solutions),
    })
}

fn take(bytes: &[u8], offset: usize, len: usize) -> Result<&[u8], HeaderError> {
    let end = offset.checked_add(len).ok_or(HeaderError::Truncated)?;
    bytes.get(offset..end).ok_or(HeaderError::Truncated)
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8], pos: usize) -> Self {
        Self { bytes, pos }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], HeaderError> {
        let slice = take(self.bytes, self.pos, len)?;
        self.pos += len;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, HeaderError> {
        Ok(self.take(1)?[0])
    }

    fn u16_prefixed(&mut self, max: usize) -> Result<Vec<u8>, HeaderError> {
        let len = usize::from(u16::from_le_bytes(
            self.take(2)?.try_into().expect("2 bytes"),
        ));
        if len > max {
            return Err(HeaderError::AssertionTooLong);
        }
        Ok(self.take(len)?.to_vec())
    }

    fn u64(&mut self) -> Result<u64, HeaderError> {
        let bytes: [u8; 8] = self.take(8)?.try_into().expect("8 bytes");
        Ok(u64::from_le_bytes(bytes))
    }
}
