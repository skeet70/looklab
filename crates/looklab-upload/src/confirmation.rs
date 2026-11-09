// SPDX-License-Identifier: AGPL-3.0-or-later
//! The canonical HTTP bodies for an owner's manual confirmation and withdrawal.

use looklab_schema::{BodyId, ManualConfirmation};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualConfirmationRequest {
    pub nonce: String,
    pub credential_id: String,
    pub authenticator_data: String,
    pub client_data_json: String,
    pub signature: String,
    pub send_manifest: String,
    pub confirmation: ManualConfirmation,
}

impl ManualConfirmationRequest {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a confirmation request has no unserializable value")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualWithdrawalRequest {
    pub nonce: String,
    pub credential_id: String,
    pub authenticator_data: String,
    pub client_data_json: String,
    pub signature: String,
    pub camera: BodyId,
}

impl ManualWithdrawalRequest {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a withdrawal request has no unserializable value")
    }
}
