// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sampling seeds and the local seed file.
//!
//! [docs/03 §2.1](../../../docs/03-data-model.md), [docs/06 §5](../../../docs/06-privacy-and-payload.md).

use core::fmt;

use looklab_samples::PriorTransformVersion;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;
use serde::{Deserialize, Serialize};

use crate::error::PrepError;
use crate::pair::LocalImageKey;

/// The CSPRNG seed of one image or re-render group.
/// SENSITIVE: with a seed and the upload, anyone can undo the shuffle. Never uploaded, logged, or derived from server data.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SamplingSeed([u8; 32]);

impl SamplingSeed {
    /// Operating-system randomness. The only production constructor; there is
    /// no conversion from a server nonce or byte array.
    ///
    /// ```compile_fail
    /// use looklab_prep::SamplingSeed;
    /// let server_nonce = [0u8; 32];
    /// let seed = SamplingSeed::from(server_nonce);
    /// ```
    pub fn fresh() -> Result<Self, PrepError> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| PrepError::SeedUnavailable)?;
        Ok(Self(bytes))
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Fixed seeds for tests and `looklab-cli verify-payload` fixtures only.
    #[cfg(any(test, feature = "test-seeds"))]
    pub const fn from_test_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub(crate) fn rng(&self) -> ChaCha20Rng {
        ChaCha20Rng::from_seed(self.0)
    }
}

impl fmt::Debug for SamplingSeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SamplingSeed(<redacted>)")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedEntry {
    pub key: LocalImageKey,
    pub seed: SamplingSeed,
}

/// The local JSON seed file that makes the payload reproducible byte for byte.
/// SENSITIVE: it reveals sampling order; never upload or paste it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedFile {
    prior_transform_version: PriorTransformVersion,
    entries: Vec<SeedEntry>,
}

const FORMAT: &str = "looklab-seeds/1";

#[derive(Serialize, Deserialize)]
struct SeedFileWire {
    format: String,
    prior_transform_version: u32,
    entries: Vec<SeedEntryWire>,
}

#[derive(Serialize, Deserialize)]
struct SeedEntryWire {
    key: String,
    seed: String,
}

impl SeedFile {
    pub fn new(prior_transform_version: PriorTransformVersion) -> Self {
        Self {
            prior_transform_version,
            entries: Vec::new(),
        }
    }

    pub fn from_json(text: &str) -> Result<Self, PrepError> {
        let wire: SeedFileWire = serde_json::from_str(text)?;
        if wire.format != FORMAT {
            return Err(PrepError::SeedFileFormat);
        }
        let mut file = Self::new(PriorTransformVersion::new(wire.prior_transform_version));
        for (index, entry) in wire.entries.into_iter().enumerate() {
            let key = LocalImageKey::new(&entry.key).ok_or(PrepError::SeedFileEntry { index })?;
            let seed = parse_hex(&entry.seed).ok_or(PrepError::SeedFileEntry { index })?;
            file.insert(key, SamplingSeed(seed));
        }
        Ok(file)
    }

    pub fn to_json(&self) -> Result<String, PrepError> {
        let wire = SeedFileWire {
            format: FORMAT.to_owned(),
            prior_transform_version: self.prior_transform_version.get(),
            entries: self
                .entries
                .iter()
                .map(|entry| SeedEntryWire {
                    key: entry.key.as_str().to_owned(),
                    seed: to_hex(&entry.seed.0),
                })
                .collect(),
        };
        Ok(serde_json::to_string_pretty(&wire)?)
    }

    /// Inserts or replaces the seed of `key`, keeping the entries in key order.
    pub fn insert(&mut self, key: LocalImageKey, seed: SamplingSeed) {
        let index = self.entries.binary_search_by(|entry| entry.key.cmp(&key));
        match index {
            Ok(index) => self.entries[index].seed = seed,
            Err(index) => self.entries.insert(index, SeedEntry { key, seed }),
        }
    }

    pub fn seed_for(&self, key: &LocalImageKey) -> Option<SamplingSeed> {
        self.entries
            .binary_search_by(|entry| entry.key.cmp(key))
            .ok()
            .map(|index| self.entries[index].seed)
    }

    pub fn entries(&self) -> &[SeedEntry] {
        &self.entries
    }

    pub fn prior_transform_version(&self) -> PriorTransformVersion {
        self.prior_transform_version
    }
}

fn to_hex(bytes: &[u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn parse_hex(text: &str) -> Option<[u8; 32]> {
    let text = text.trim();
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        let high = hex_digit(text.as_bytes()[index * 2])?;
        let low = hex_digit(text.as_bytes()[index * 2 + 1])?;
        *byte = (high << 4) | low;
    }
    Some(out)
}

fn hex_digit(byte: u8) -> Option<u8> {
    Some(match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => return None,
    })
}
