// SPDX-License-Identifier: AGPL-3.0-or-later
//! Signed-request domains and digests, byte-compatible with
//! `looklab-server/src/verify.rs` ([docs/06 §4](../../../docs/06-privacy-and-payload.md)).
//! The server signs `H(nonce ‖ H("looklab-request-v1:" ‖ domain ‖ ":" ‖ payload))`;
//! the module computes that in Rust so the web layer never builds the preimage.

use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    pub const fn as_str(self) -> &'static str {
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

pub fn request_digest(domain: RequestDomain, payload: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"looklab-request-v1:");
    hasher.update(domain.as_str().as_bytes());
    hasher.update(b":");
    hasher.update(payload);
    hasher.finalize().into()
}

pub fn challenge(nonce: &[u8; 32], digest: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(nonce);
    hasher.update(digest);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn domain_digests_are_pinned() {
        // sha256("looklab-request-v1:proposal:abc")
        assert_eq!(
            hex(&request_digest(RequestDomain::Proposal, b"abc")),
            "5d05c05477cbe7ef05d54ee395c02cec646a0e0c27235b27ed2dfbca7c8f85f0"
        );
        // sha256("looklab-request-v1:mod-session:")
        assert_eq!(
            hex(&request_digest(RequestDomain::ModSession, b"")),
            "6841a3ce779b7a077b3a71f42409bb4abc13c5c34e4ad010ae458450c7ce29e0"
        );
        // sha256("looklab-request-v1:proposal-thread:{}")
        assert_eq!(
            hex(&request_digest(RequestDomain::ProposalThread, b"{}")),
            "dfbeb93192d24890d7300c847de46718b2cc0b3bcc5f472a66fe7e4a26388540"
        );
        // sha256("looklab-request-v1:fixture-upload:")
        assert_eq!(
            hex(&request_digest(RequestDomain::FixtureUpload, b"")),
            "9246b775ae9396d771c2238d3f7bd3ca04a7ba988e5dbc63e28b5bb61da42aad"
        );
    }
}
