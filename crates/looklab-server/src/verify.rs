// SPDX-License-Identifier: AGPL-3.0-or-later
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use looklab_upload::{
    Challenge, ExpectedAssertion, ManifestHash, Nonce, SignatureAlgorithm, StructuralError,
    StructuralVerdict, WebAuthnAssertion, check_structure,
};
use sha2::{Digest, Sha256};
use webauthn_rs::prelude::{COSEAlgorithm, Credential, Passkey};

use crate::error::{ApiError, ErrorCode};

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("the assertion does not belong to this credential")]
    CredentialMismatch,
    #[error("the credential's signature algorithm is not supported")]
    AlgorithmUnsupported,
    #[error("the assertion is structurally invalid: {0}")]
    Structure(#[from] StructuralError),
    #[error("the assertion signature does not verify")]
    Signature,
    #[error("the assertion does not set the user-verified flag")]
    UserVerification,
    #[error("the assertion's backup-eligibility flag changed")]
    BackupEligibility,
    #[error("the assertion reports a backup state without backup eligibility")]
    BackupState,
    #[error("the assertion counter did not increase")]
    CounterReplay,
}

pub fn verify_assertion(
    passkey: &Passkey,
    assertion: &WebAuthnAssertion,
    challenge: Challenge,
    origin: &str,
    rp_id: &str,
) -> Result<Passkey, VerifyError> {
    if passkey.cred_id().as_slice() != assertion.credential_id.as_slice() {
        return Err(VerifyError::CredentialMismatch);
    }
    let algorithm = match passkey.cred_algorithm() {
        COSEAlgorithm::ES256 | COSEAlgorithm::ES384 => SignatureAlgorithm::Ecdsa,
        COSEAlgorithm::RS256 => SignatureAlgorithm::Rsa,
        _ => return Err(VerifyError::AlgorithmUnsupported),
    };

    let expected = ExpectedAssertion {
        challenge,
        origin,
        rp_id,
        algorithm,
        top_origin: None,
    };
    match check_structure(assertion, &expected) {
        StructuralVerdict::Pass | StructuralVerdict::ReviewNeeded { .. } => {}
        StructuralVerdict::Fail(error) => return Err(VerifyError::Structure(error)),
    }

    let mut signed = assertion.authenticator_data.clone();
    signed.extend_from_slice(&Sha256::digest(&assertion.client_data_json));
    let verified = passkey
        .get_public_key()
        .verify_signature(&assertion.signature, &signed)
        .map_err(|_| VerifyError::Signature)?;
    if !verified {
        return Err(VerifyError::Signature);
    }

    let authenticator_data = &assertion.authenticator_data;
    if authenticator_data.len() < 37 {
        return Err(VerifyError::Structure(
            StructuralError::AuthenticatorDataTooShort {
                len: authenticator_data.len(),
            },
        ));
    }
    let flags = authenticator_data[32];
    let user_verified = flags & 0x04 != 0;
    let backup_eligible = flags & 0x08 != 0;
    let backup_state = flags & 0x10 != 0;
    if !user_verified {
        return Err(VerifyError::UserVerification);
    }

    let counter = u32::from_be_bytes([
        authenticator_data[33],
        authenticator_data[34],
        authenticator_data[35],
        authenticator_data[36],
    ]);
    let mut credential: Credential = passkey.clone().into();
    if credential.backup_eligible != backup_eligible {
        return Err(VerifyError::BackupEligibility);
    }
    if backup_state && !credential.backup_eligible {
        return Err(VerifyError::BackupState);
    }
    if counter != 0 && counter <= credential.counter {
        return Err(VerifyError::CounterReplay);
    }
    credential.counter = credential.counter.max(counter);
    credential.backup_state = backup_state;
    Ok(credential.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestDomain {
    Session,
    SessionRevoke,
    Withdraw,
    Grant,
    RevokeGrant,
    Endorse,
    RevokeEndorsement,
    AdmitRoot,
    RevokeRoot,
    ModSession,
    Proposal,
    ProposalConfirmation,
    ProposalThread,
    FixtureUpload,
    WithdrawManualConfirmation,
    ModerateManualConfirmation,
}

impl RequestDomain {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::SessionRevoke => "session-revoke",
            Self::Withdraw => "withdraw",
            Self::Grant => "grant",
            Self::RevokeGrant => "revoke-grant",
            Self::Endorse => "endorse",
            Self::RevokeEndorsement => "revoke-endorsement",
            Self::AdmitRoot => "admit-root",
            Self::RevokeRoot => "revoke-root",
            Self::ModSession => "mod-session",
            Self::Proposal => "proposal",
            Self::ProposalConfirmation => "proposal-confirmation",
            Self::ProposalThread => "proposal-thread",
            Self::FixtureUpload => "fixture-upload",
            Self::WithdrawManualConfirmation => "withdraw-manual-confirmation",
            Self::ModerateManualConfirmation => "moderate-manual-confirmation",
        }
    }
}

pub fn request_digest(domain: RequestDomain, payload: &[u8]) -> ManifestHash {
    let mut hasher = Sha256::new();
    hasher.update(b"looklab-request-v1:");
    hasher.update(domain.as_str().as_bytes());
    hasher.update(b":");
    hasher.update(payload);
    let digest = hasher.finalize();
    ManifestHash::from_bytes(digest.into())
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedRequestPayload {
    pub nonce: String,
    pub credential_id: String,
    pub authenticator_data: String,
    pub client_data_json: String,
    pub signature: String,
}

impl SignedRequestPayload {
    pub fn decode(&self) -> Result<(WebAuthnAssertion, Nonce), ApiError> {
        let malformed = ApiError::new(ErrorCode::MalformedEnvelope);
        let nonce_bytes = URL_SAFE_NO_PAD.decode(&self.nonce).map_err(|_| malformed)?;
        let nonce = Nonce::from_slice(&nonce_bytes).ok_or(malformed)?;
        let credential_id = decode_field(&self.credential_id, 255)?;
        let authenticator_data = decode_field(&self.authenticator_data, 512)?;
        let client_data_json = decode_field(&self.client_data_json, 1024)?;
        let signature = decode_field(&self.signature, 512)?;
        Ok((
            WebAuthnAssertion {
                credential_id,
                authenticator_data,
                client_data_json,
                signature,
            },
            nonce,
        ))
    }
}

fn decode_field(value: &str, max: usize) -> Result<Vec<u8>, ApiError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    if bytes.is_empty() || bytes.len() > max {
        return Err(ApiError::new(ErrorCode::MalformedEnvelope));
    }
    Ok(bytes)
}
