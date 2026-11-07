// SPDX-License-Identifier: AGPL-3.0-or-later
//! Internal identifiers and the status capability token.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
#[error("the operating system did not provide random bytes")]
pub struct RandomError;

pub fn random_bytes<const N: usize>() -> Result<[u8; N], RandomError> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|_| RandomError)?;
    Ok(bytes)
}

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4().to_string())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl core::fmt::Display for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidId;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                let uuid = uuid::Uuid::parse_str(&value).map_err(|_| InvalidId)?;
                Ok(Self(uuid.hyphenated().to_string()))
            }
        }

        impl TryFrom<&str> for $name {
            type Error = InvalidId;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::try_from(value.to_owned())
            }
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("stored id is not a UUID")]
pub struct InvalidId;

uuid_id!(
    /// Internal credential id; never a public key or serial HMAC.
    CredentialId
);
uuid_id!(ContributionId);
uuid_id!(ContributionImageId);
uuid_id!(BlobAttemptId);
uuid_id!(JobId);
uuid_id!(WithdrawalId);
uuid_id!(GrantId);
uuid_id!(EndorsementId);
uuid_id!(RootAdmissionId);
uuid_id!(LookVersionId);
uuid_id!(ProposalId);
uuid_id!(ProposalMessageId);
uuid_id!(ModerationDecisionId);

/// The WebAuthn credential id as it appears on the wire.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RawCredentialId(Vec<u8>);

impl RawCredentialId {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl core::fmt::Debug for RawCredentialId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("RawCredentialId(..)")
    }
}

/// SHA-256 of a capability token.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenHash([u8; 32]);

impl TokenHash {
    pub fn of(token: &[u8]) -> Self {
        let digest = Sha256::digest(token);
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&digest);
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl core::fmt::Debug for TokenHash {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("TokenHash(..)")
    }
}

/// A status capability token, returned to its holder exactly once.
#[derive(Clone, PartialEq, Eq)]
pub struct StatusToken(String);

impl StatusToken {
    pub fn generate() -> Result<Self, RandomError> {
        let bytes = random_bytes::<32>()?;
        Ok(Self(URL_SAFE_NO_PAD.encode(bytes)))
    }

    pub fn from_base64url(value: &str) -> Option<Self> {
        let bytes = URL_SAFE_NO_PAD.decode(value).ok()?;
        (bytes.len() == 32).then(|| Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn hash(&self) -> TokenHash {
        TokenHash::of(self.0.as_bytes())
    }
}

impl core::fmt::Debug for StatusToken {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("StatusToken(..)")
    }
}

/// Key under which an in-memory passkey registration is held.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RegistrationHandle(String);

impl RegistrationHandle {
    pub fn generate() -> Result<Self, RandomError> {
        Ok(Self(URL_SAFE_NO_PAD.encode(random_bytes::<32>()?)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RegistrationHandle {
    type Error = InvalidHandle;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let bytes = URL_SAFE_NO_PAD.decode(&value).map_err(|_| InvalidHandle)?;
        if bytes.len() != 32 {
            return Err(InvalidHandle);
        }
        Ok(Self(value))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid registration handle")]
pub struct InvalidHandle;
