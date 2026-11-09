// SPDX-License-Identifier: AGPL-3.0-or-later
//! The canonical payload a moderator's passkey signs for one documented control.

use looklab_schema::{BodyId, CapabilityKey, ModeratorEvidence, Sha256Hex};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualModeration {
    pub camera: BodyId,
    pub key: CapabilityKey,
    pub definition_digest: Sha256Hex,
    pub confirmed: bool,
    pub evidence: ModeratorEvidence,
}

impl ManualModeration {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a moderation payload has no unserializable value")
    }
}
