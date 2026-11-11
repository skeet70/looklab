// SPDX-License-Identifier: AGPL-3.0-or-later
//! Send manifest: the batch and attestation digests one passkey approval covers
//! ([docs/03 §2.3](../../../docs/03-data-model.md)).
//!
//! Canonical order is strictly increasing lexicographic order within each
//! collection, independent of the client's enumeration.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::nonce::{AttestationHash, ManifestHash, SendManifestHash};

pub const DEFAULT_MAX_BATCHES: usize = 64;
pub const DEFAULT_MAX_ATTESTATIONS: usize = 8;
/// The versioned layout of the canonical send manifest bytes.
pub const SEND_MANIFEST_VERSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendManifest {
    pub version: u8,
    pub batches: Vec<ManifestHash>,
    pub attestations: Vec<AttestationHash>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SendManifestError {
    #[error("send manifest version {found}, expected {expected}")]
    UnsupportedVersion { found: u8, expected: u8 },
    #[error("a send manifest lists no batch or attestation")]
    Empty,
    #[error("send manifest attestation {index} is also a batch")]
    AttestationIsBatch { index: usize },
    #[error("a send manifest lists {count} batches, outside 0..={max}")]
    TooMany { count: usize, max: usize },
    #[error("a send manifest lists {count} attestations, outside 0..={max}")]
    TooManyAttestations { count: usize, max: usize },
    #[error("send manifest batch {index} repeats an earlier hash")]
    Duplicate { index: usize },
    #[error("send manifest batch {index} is not in canonical order")]
    NotSorted { index: usize },
    #[error("send manifest attestation {index} repeats an earlier hash")]
    DuplicateAttestation { index: usize },
    #[error("send manifest attestation {index} is not in canonical order")]
    AttestationNotSorted { index: usize },
}

impl SendManifest {
    pub fn canonical(batches: Vec<ManifestHash>) -> Result<Self, SendManifestError> {
        Self::canonical_with_attestations(batches, Vec::new())
    }

    pub fn canonical_with_attestations(
        batches: Vec<ManifestHash>,
        attestations: Vec<AttestationHash>,
    ) -> Result<Self, SendManifestError> {
        let mut batches = batches;
        let mut attestations = attestations;
        batches.sort_unstable();
        attestations.sort_unstable();
        let manifest = Self {
            version: SEND_MANIFEST_VERSION,
            batches,
            attestations,
        };
        manifest.validate(DEFAULT_MAX_BATCHES, DEFAULT_MAX_ATTESTATIONS)?;
        Ok(manifest)
    }

    pub fn validate(
        &self,
        max_batches: usize,
        max_attestations: usize,
    ) -> Result<(), SendManifestError> {
        if self.version != SEND_MANIFEST_VERSION {
            return Err(SendManifestError::UnsupportedVersion {
                found: self.version,
                expected: SEND_MANIFEST_VERSION,
            });
        }
        if self.batches.is_empty() && self.attestations.is_empty() {
            return Err(SendManifestError::Empty);
        }
        if self.batches.len() > max_batches {
            return Err(SendManifestError::TooMany {
                count: self.batches.len(),
                max: max_batches,
            });
        }
        if self.attestations.len() > max_attestations {
            return Err(SendManifestError::TooManyAttestations {
                count: self.attestations.len(),
                max: max_attestations,
            });
        }
        check_sorted(&self.batches).map_err(|index| match index {
            SortedError::Duplicate(index) => SendManifestError::Duplicate { index },
            SortedError::NotSorted(index) => SendManifestError::NotSorted { index },
        })?;
        check_sorted(&self.attestations).map_err(|index| match index {
            SortedError::Duplicate(index) => SendManifestError::DuplicateAttestation { index },
            SortedError::NotSorted(index) => SendManifestError::AttestationNotSorted { index },
        })?;
        for (index, attestation) in self.attestations.iter().enumerate() {
            if self
                .batches
                .binary_search_by(|batch| batch.as_bytes().cmp(attestation.as_bytes()))
                .is_ok()
            {
                return Err(SendManifestError::AttestationIsBatch { index });
            }
        }
        Ok(())
    }

    pub fn contains(&self, hash: &ManifestHash) -> bool {
        self.batches.binary_search(hash).is_ok()
    }

    pub fn contains_attestation(&self, hash: &AttestationHash) -> bool {
        self.attestations.binary_search(hash).is_ok()
    }
}

enum SortedError {
    Duplicate(usize),
    NotSorted(usize),
}

fn check_sorted<T: Ord>(values: &[T]) -> Result<(), SortedError> {
    for (index, pair) in values.windows(2).enumerate() {
        if pair[0] == pair[1] {
            return Err(SortedError::Duplicate(index + 1));
        }
        if pair[0] > pair[1] {
            return Err(SortedError::NotSorted(index + 1));
        }
    }
    Ok(())
}

/// The bytes whose SHA-256 is the send manifest hash and which the envelope carries.
pub fn canonical_send_manifest_bytes(manifest: &SendManifest) -> Vec<u8> {
    postcard::to_allocvec(manifest).expect("a send manifest has no unserializable value")
}

pub fn send_manifest_hash(manifest: &SendManifest) -> SendManifestHash {
    let digest = Sha256::digest(canonical_send_manifest_bytes(manifest));
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&digest);
    SendManifestHash::from_bytes(bytes)
}
