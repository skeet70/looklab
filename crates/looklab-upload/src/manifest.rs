// SPDX-License-Identifier: AGPL-3.0-or-later
//! Batch manifest: fixed-schema, postcard-encoded, canonical by field order
//! ([docs/03 §2.3](../../../docs/03-data-model.md)).

use looklab_samples::RerenderGroupId;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use crate::ids::{
    BodyId, CardDeclaration, CommunitySnapshotVersion, ContributionKind, ControlKey,
    ControlSettings, FirmwareVersionString, Intent, SettingSource, SimKey,
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
    pub community_snapshot_version: CommunitySnapshotVersion,
    pub settings: ControlSettings,
    pub images: Vec<ManifestImage>,
}

/// The look-affecting controls of the named snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingReach {
    /// Controls the schema maps from a makernote tag.
    pub mapped: Vec<ControlKey>,
    /// Controls the schema maps from no tag.
    pub unmapped: Vec<ControlKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SettingsSourceError {
    #[error("look-affecting control `{key}` has no value; declare it")]
    MissingLookAffectingControl { key: ControlKey },
    #[error("control `{key}` is reproduced but its snapshot maps no tag for it")]
    ReproducedWithoutMapping { key: ControlKey },
}

/// A batch whose intent disagrees with its images' render tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IntentError {
    #[error("image {index} carries a render tag but the batch intent is not card")]
    TaggedImageNeedsCard { index: usize },
    #[error("image {index} carries a render tag in a user-declared card batch")]
    UserDeclaredWithTag { index: usize },
    #[error("image {index} render tag does not match the batch card declaration")]
    CardTagMismatch { index: usize },
    #[error(
        "image {index} carries a malformed render tag; declare card intent with its firmware or exclude it"
    )]
    MalformedCardTag { index: usize },
    #[error("a tagged card batch has no image carrying its render tag")]
    MissingCardTag,
}

/// Rejects settings the named snapshot cannot support. A card render
/// (`card`) is exempt from covering every look-affecting control.
pub fn validate_setting_sources(
    settings: &ControlSettings,
    card: bool,
    reach: &SettingReach,
) -> Result<(), SettingsSourceError> {
    for entry in settings.entries() {
        if entry.source == SettingSource::Reproduced && reach.unmapped.contains(&entry.key) {
            return Err(SettingsSourceError::ReproducedWithoutMapping {
                key: entry.key.clone(),
            });
        }
    }
    if card {
        return Ok(());
    }
    for key in reach.mapped.iter().chain(&reach.unmapped) {
        if settings.get(key).is_none() {
            return Err(SettingsSourceError::MissingLookAffectingControl { key: key.clone() });
        }
    }
    Ok(())
}

impl BatchManifest {
    /// Rejects settings the named snapshot cannot support.
    pub fn validate_setting_sources(
        &self,
        reach: &SettingReach,
    ) -> Result<(), SettingsSourceError> {
        validate_setting_sources(
            &self.settings,
            matches!(self.intent, Intent::Card(_)),
            reach,
        )
    }

    /// Rejects a batch whose render tags and card declaration disagree
    /// ([docs/07 §4](../../../docs/07-contribution.md)).
    pub fn validate_intent(&self) -> Result<(), IntentError> {
        match &self.intent {
            Intent::Card(CardDeclaration::Tagged(expected)) => {
                let mut found = false;
                for (index, image) in self.images.iter().enumerate() {
                    if image.meta.render_tag_malformed {
                        return Err(IntentError::MalformedCardTag { index });
                    }
                    if let Some(tag) = &image.meta.render_tag {
                        found = true;
                        if tag != expected {
                            return Err(IntentError::CardTagMismatch { index });
                        }
                    }
                }
                if !found {
                    return Err(IntentError::MissingCardTag);
                }
            }
            Intent::Card(CardDeclaration::UserDeclared { .. }) => {
                if let Some(index) = self
                    .images
                    .iter()
                    .position(|image| image.meta.render_tag.is_some())
                {
                    return Err(IntentError::UserDeclaredWithTag { index });
                }
            }
            Intent::Native | Intent::Recipe { .. } => {
                if let Some(index) = self
                    .images
                    .iter()
                    .position(|image| image.meta.render_tag.is_some())
                {
                    return Err(IntentError::TaggedImageNeedsCard { index });
                }
                if let Some(index) = self
                    .images
                    .iter()
                    .position(|image| image.meta.render_tag_malformed)
                {
                    return Err(IntentError::MalformedCardTag { index });
                }
            }
        }
        Ok(())
    }
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
