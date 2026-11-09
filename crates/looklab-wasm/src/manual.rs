// SPDX-License-Identifier: AGPL-3.0-or-later
//! Canonical confirmation payloads and request bodies for the browser client.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use looklab_schema::{
    BodyId, CapabilityKey, ConfirmationScope, ControlObservation, MANUAL_CONFIRMATION_VERSION,
    ManualConfirmation, ManualUrl, ModeratorEvidence, ModeratorEvidenceKind,
    ModeratorFirmwareScope, ReportedFirmware, Sha256Hex,
};
use serde::Serialize;

use crate::AssertionFields;
use crate::error::FacadeError;

/// A confirmation together with the canonical bytes its attestation digest covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManualConfirmationEnvelope {
    confirmation: ManualConfirmation,
}

impl ManualConfirmationEnvelope {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        self.confirmation.canonical_bytes()
    }

    pub fn digest(&self) -> [u8; 32] {
        *self.confirmation.digest().as_bytes()
    }

    /// The body the confirmation endpoint accepts.
    ///
    /// # Errors
    /// A nonce that is not 32 bytes or a request that cannot be serialized.
    pub fn request_body(
        &self,
        nonce: &[u8],
        assertion: &AssertionFields,
        send_manifest: &[u8],
    ) -> Result<Vec<u8>, FacadeError> {
        let nonce: [u8; 32] = nonce
            .try_into()
            .map_err(|_| FacadeError::Input("nonce is not 32 bytes".to_owned()))?;
        Ok(looklab_upload::ManualConfirmationRequest {
            nonce: URL_SAFE_NO_PAD.encode(nonce),
            credential_id: URL_SAFE_NO_PAD.encode(&assertion.credential_id),
            authenticator_data: URL_SAFE_NO_PAD.encode(&assertion.authenticator_data),
            client_data_json: URL_SAFE_NO_PAD.encode(&assertion.client_data_json),
            signature: URL_SAFE_NO_PAD.encode(&assertion.signature),
            send_manifest: URL_SAFE_NO_PAD.encode(send_manifest),
            confirmation: self.confirmation.clone(),
        }
        .canonical_bytes())
    }
}

/// Builds the canonical confirmation a send manifest binds by digest.
///
/// # Errors
/// Any structure rule violation.
pub fn build_manual_confirmation(
    camera: BodyId,
    camera_digest: Sha256Hex,
    firmware: ReportedFirmware,
    scope: ConfirmationScope,
    observations: Vec<ControlObservation>,
) -> Result<ManualConfirmationEnvelope, FacadeError> {
    let confirmation = ManualConfirmation {
        version: MANUAL_CONFIRMATION_VERSION,
        camera,
        camera_digest,
        firmware,
        scope,
        observations,
    };
    confirmation.validate_structure().map_err(|errors| {
        FacadeError::Input(
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        )
    })?;
    Ok(ManualConfirmationEnvelope { confirmation })
}

/// The canonical withdrawal request body.
pub fn manual_withdraw_request(
    camera: &BodyId,
    nonce: &str,
    credential_id: &str,
    authenticator_data: &str,
    client_data_json: &str,
    signature: &str,
) -> Result<Vec<u8>, FacadeError> {
    Ok(looklab_upload::ManualWithdrawalRequest {
        nonce: nonce.to_owned(),
        credential_id: credential_id.to_owned(),
        authenticator_data: authenticator_data.to_owned(),
        client_data_json: client_data_json.to_owned(),
        signature: signature.to_owned(),
        camera: camera.clone(),
    }
    .canonical_bytes())
}

/// # Errors
/// A payload that is not a `ManualConfirmation`.
pub fn manual_confirmation_bytes(payload: &[u8]) -> Result<Vec<u8>, FacadeError> {
    let confirmation: ManualConfirmation = serde_json::from_slice(payload)?;
    Ok(confirmation.canonical_bytes())
}

/// # Errors
/// A payload that is not a `ManualConfirmation`.
pub fn manual_confirmation_digest(payload: &[u8]) -> Result<[u8; 32], FacadeError> {
    let confirmation: ManualConfirmation = serde_json::from_slice(payload)?;
    Ok(*confirmation.digest().as_bytes())
}

/// The canonical payload a moderator's passkey signs, with its request body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManualModerationEnvelope {
    moderation: looklab_upload::ManualModeration,
}

impl ManualModerationEnvelope {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        self.moderation.canonical_bytes()
    }

    /// # Errors
    /// A request that cannot be serialized.
    pub fn request_body(
        &self,
        nonce: &str,
        credential_id: &str,
        authenticator_data: &str,
        client_data_json: &str,
        signature: &str,
    ) -> Result<Vec<u8>, FacadeError> {
        Ok(serde_json::to_vec(&ModerationRequest {
            nonce,
            credential_id,
            authenticator_data,
            client_data_json,
            signature,
            definition_digest: &self.moderation.definition_digest,
            confirmed: self.moderation.confirmed,
            evidence: ModerationEvidenceRequest {
                kind: self.moderation.evidence.kind,
                url: self.moderation.evidence.url.as_ref(),
                description: &self.moderation.evidence.description,
                firmware_scope: &self.moderation.evidence.firmware_scope,
            },
        })?)
    }
}

#[derive(Serialize)]
struct ModerationRequest<'a> {
    nonce: &'a str,
    credential_id: &'a str,
    authenticator_data: &'a str,
    client_data_json: &'a str,
    signature: &'a str,
    definition_digest: &'a Sha256Hex,
    confirmed: bool,
    evidence: ModerationEvidenceRequest<'a>,
}

#[derive(Serialize)]
struct ModerationEvidenceRequest<'a> {
    kind: ModeratorEvidenceKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<&'a ManualUrl>,
    description: &'a str,
    firmware_scope: &'a ModeratorFirmwareScope,
}

/// Builds the payload a moderator signs for one documented control.
///
/// # Errors
/// A malformed URL.
pub fn build_manual_moderation(
    camera: BodyId,
    key: CapabilityKey,
    definition_digest: Sha256Hex,
    confirmed: bool,
    firmware_scope: ModeratorFirmwareScope,
    kind: ModeratorEvidenceKind,
    url: Option<&str>,
    description: &str,
) -> Result<ManualModerationEnvelope, FacadeError> {
    let url = url
        .map(ManualUrl::new)
        .transpose()
        .map_err(|error| FacadeError::Input(error.to_string()))?;
    Ok(ManualModerationEnvelope {
        moderation: looklab_upload::ManualModeration {
            camera,
            key,
            definition_digest,
            confirmed,
            evidence: ModeratorEvidence {
                kind,
                url,
                description: description.to_owned(),
                firmware_scope,
            },
        },
    })
}
