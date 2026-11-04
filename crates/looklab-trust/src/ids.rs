// SPDX-License-Identifier: AGPL-3.0-or-later
//! Identifier newtypes owned by trust, and effective-contributor resolution.

use core::fmt;
use core::str::FromStr;

use looklab_meta::SerialHmac;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Result, TrustError};

/// Internal credential id. Rendered as 32 lowercase hex digits; the public key is never part of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CredentialId(u128);

impl CredentialId {
    pub const fn from_u128(value: u128) -> Self {
        Self(value)
    }

    pub const fn to_u128(self) -> u128 {
        self.0
    }
}

impl fmt::Display for CredentialId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

impl FromStr for CredentialId {
    type Err = TrustError;

    fn from_str(text: &str) -> Result<Self> {
        u128::from_str_radix(text, 16)
            .map(Self)
            .map_err(|_| TrustError::InvalidId(text.to_owned()))
    }
}

impl Serialize for CredentialId {
    fn serialize<S: Serializer>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for CredentialId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> core::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(D::Error::custom)
    }
}

/// A group of credentials linked by a shared serial HMAC.
///
/// The representative is the smallest [`CredentialId`] in the group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EffectiveContributorId(CredentialId);

impl EffectiveContributorId {
    pub const fn representative(self) -> CredentialId {
        self.0
    }
}

impl From<CredentialId> for EffectiveContributorId {
    fn from(id: CredentialId) -> Self {
        Self(id)
    }
}

impl fmt::Display for EffectiveContributorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for EffectiveContributorId {
    fn serialize<S: Serializer>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EffectiveContributorId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> core::result::Result<Self, D::Error> {
        CredentialId::deserialize(deserializer).map(Self)
    }
}

/// One contribution image, as the server addresses it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ImageRef(String);

impl ImageRef {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<&str> for ImageRef {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for ImageRef {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Monotone Look version number within one Look.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VersionId(u32);

impl VersionId {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for VersionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Content address of a published artifact or version manifest.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArtifactHash([u8; 32]);

impl ArtifactHash {
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'a' + nibble - 10) as char,
    }
}

impl fmt::Display for ArtifactHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{}{}", hex_digit(byte >> 4), hex_digit(byte & 0x0f))?;
        }
        Ok(())
    }
}

impl fmt::Debug for ArtifactHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for ArtifactHash {
    type Err = TrustError;

    fn from_str(text: &str) -> Result<Self> {
        if text.len() != 64 {
            return Err(TrustError::InvalidId(text.to_owned()));
        }
        let mut bytes = [0u8; 32];
        for (index, chunk) in text.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let high = (chunk[0] as char)
                .to_digit(16)
                .ok_or_else(|| TrustError::InvalidId(text.to_owned()))?;
            let low = (chunk[1] as char)
                .to_digit(16)
                .ok_or_else(|| TrustError::InvalidId(text.to_owned()))?;
            bytes[index] = ((high as u8) << 4) | low as u8;
        }
        Ok(Self(bytes))
    }
}

impl Serialize for ArtifactHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ArtifactHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> core::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(D::Error::custom)
    }
}

/// A credential's reported serial HMAC. Never serialized, logged or published.
pub struct SerialLink {
    pub credential: CredentialId,
    pub hmac: SerialHmac,
}

/// Maps every credential to the effective contributor it belongs to.
///
/// A credential with no serial link is its own group. The HMAC is an internal join key and
/// is not retained.
pub fn resolve_effective_contributors(
    credentials: impl IntoIterator<Item = CredentialId>,
    links: &[SerialLink],
) -> std::collections::BTreeMap<CredentialId, EffectiveContributorId> {
    use std::collections::BTreeMap;

    let mut parent: BTreeMap<CredentialId, CredentialId> = BTreeMap::new();
    for credential in credentials {
        parent.insert(credential, credential);
    }
    for link in links {
        parent.insert(link.credential, link.credential);
    }

    let mut by_hmac: BTreeMap<[u8; 32], CredentialId> = BTreeMap::new();
    for link in links {
        let root = find(&mut parent, link.credential);
        match by_hmac.get(link.hmac.as_bytes()) {
            Some(&first) => {
                let other = find(&mut parent, first);
                let (low, high) = if root < other {
                    (root, other)
                } else {
                    (other, root)
                };
                if low != high {
                    parent.insert(high, low);
                }
            }
            None => {
                by_hmac.insert(*link.hmac.as_bytes(), root);
            }
        }
    }

    let credentials: Vec<CredentialId> = parent.keys().copied().collect();
    credentials
        .into_iter()
        .map(|credential| {
            let root = find(&mut parent, credential);
            (credential, EffectiveContributorId(root))
        })
        .collect()
}

fn find(
    parent: &mut std::collections::BTreeMap<CredentialId, CredentialId>,
    start: CredentialId,
) -> CredentialId {
    let mut root = start;
    while let Some(&next) = parent.get(&root) {
        if next == root {
            break;
        }
        root = next;
    }
    let mut current = start;
    while current != root {
        let Some(&next) = parent.get(&current) else {
            break;
        };
        parent.insert(current, root);
        current = next;
    }
    root
}
