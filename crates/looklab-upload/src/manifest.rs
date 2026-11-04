// SPDX-License-Identifier: AGPL-3.0-or-later
//! Batch manifest: fixed-schema, postcard-encoded, canonical by field order
//! ([docs/03 §2.3](../../../docs/03-data-model.md)).

use looklab_samples::RerenderGroupId;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use crate::ids::{
    BodyId, ContributionKind, ControlSettings, FirmwareVersionString, Intent, SimKey,
};
use crate::metadata::ManifestMetadata;
use crate::nonce::ManifestHash;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BlobKind {
    Samples,
    PatchTable,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ManifestImage {
    pub meta: ManifestMetadata,
    pub rerender_group: Option<RerenderGroupId>,
    pub blob_sha256: [u8; 32],
    pub blob_kind: BlobKind,
}

#[derive(Deserialize)]
struct ManifestImageWire {
    meta: ManifestMetadata,
    rerender_group: Option<[u8; 16]>,
    blob_sha256: [u8; 32],
    blob_kind: BlobKind,
}

impl<'de> Deserialize<'de> for ManifestImage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = ManifestImageWire::deserialize(deserializer)?;
        Ok(Self {
            meta: wire.meta,
            rerender_group: wire.rerender_group.map(RerenderGroupId::new),
            blob_sha256: wire.blob_sha256,
            blob_kind: wire.blob_kind,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchManifest {
    pub kind: ContributionKind,
    pub intent: Intent,
    pub body: BodyId,
    pub firmware_version: FirmwareVersionString,
    pub sim_key: SimKey,
    pub settings: ControlSettings,
    pub images: Vec<ManifestImage>,
}

/// The bytes whose SHA-256 is the manifest hash and which the envelope carries.
pub fn canonical_manifest_bytes(manifest: &BatchManifest) -> Vec<u8> {
    postcard::to_allocvec(manifest).expect("a manifest has no unserializable value")
}

pub fn manifest_hash(manifest: &BatchManifest) -> ManifestHash {
    let digest = Sha256::digest(canonical_manifest_bytes(manifest));
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&digest);
    ManifestHash::from_bytes(bytes)
}
