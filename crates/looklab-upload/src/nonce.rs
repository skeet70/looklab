// SPDX-License-Identifier: AGPL-3.0-or-later
//! Server nonce, manifest hash and the WebAuthn challenge derived from them.
//!
//! `H` is SHA-256 throughout ([docs/03 §2.3](../../../docs/03-data-model.md)).

use core::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

macro_rules! bytes32_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name([u8; 32]);

        impl $name {
            pub const fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }

            pub fn from_slice(bytes: &[u8]) -> Option<Self> {
                let array: [u8; 32] = bytes.try_into().ok()?;
                Some(Self(array))
            }

            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in self.0 {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
        }
    };
}

bytes32_type!(
    /// A nonce issued by the server, consumed by exactly one envelope.
    Nonce
);
bytes32_type!(
    /// SHA-256 of the canonical batch manifest bytes.
    ManifestHash
);
bytes32_type!(
    /// SHA-256 of the canonical send manifest bytes.
    SendManifestHash
);
bytes32_type!(
    /// SHA-256 of `nonce ‖ manifest_hash`, signed through WebAuthn.
    Challenge
);

impl Challenge {
    pub fn new(nonce: &Nonce, manifest_hash: &ManifestHash) -> Self {
        Self::digest(nonce, manifest_hash.as_bytes())
    }

    /// The one challenge per send.
    pub fn for_send(nonce: &Nonce, send_manifest_hash: &SendManifestHash) -> Self {
        Self::digest(nonce, send_manifest_hash.as_bytes())
    }

    fn digest(nonce: &Nonce, hash: &[u8; 32]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(nonce.as_bytes());
        hasher.update(hash);
        let digest = hasher.finalize();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&digest);
        Self::from_bytes(bytes)
    }
}
