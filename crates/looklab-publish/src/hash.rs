// SPDX-License-Identifier: AGPL-3.0-or-later
//! Content hashes, artifact names and site paths.

use core::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::PublishError;

/// Lowercase SHA-256 hex digest of an artifact.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ArtifactHash(String);

impl ArtifactHash {
    pub fn of(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        let mut out = String::with_capacity(64);
        for byte in digest {
            out.push(nibble(byte >> 4));
            out.push(nibble(byte & 0x0f));
        }
        Self(out)
    }

    /// # Errors
    /// Returns [`PublishError::Hash`] unless `value` is 64 lowercase hex characters.
    pub fn parse(value: &str) -> Result<Self, PublishError> {
        let valid = value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(PublishError::Hash(value.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ArtifactHash {
    type Error = PublishError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<ArtifactHash> for String {
    fn from(value: ArtifactHash) -> Self {
        value.0
    }
}

impl fmt::Display for ArtifactHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn nibble(value: u8) -> char {
    match value {
        0..=9 => char::from(b'0' + value),
        _ => char::from(b'a' + (value - 10)),
    }
}

/// A file name inside a version directory or scene directory.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ArtifactName(String);

impl ArtifactName {
    /// # Errors
    /// Rejects empty names, path separators and characters outside
    /// `[A-Za-z0-9._-]`.
    pub fn new(value: impl Into<String>) -> Result<Self, PublishError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        if valid {
            Ok(Self(value))
        } else {
            Err(PublishError::Path(value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ArtifactName {
    type Error = PublishError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ArtifactName> for String {
    fn from(value: ArtifactName) -> Self {
        value.0
    }
}

impl fmt::Display for ArtifactName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A relative site path, for example `data/looks/provia.json`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SitePath(String);

impl SitePath {
    /// # Errors
    /// Rejects absolute paths, empty, `.` or `..` segments, backslashes and control characters.
    pub fn new(value: impl Into<String>) -> Result<Self, PublishError> {
        let value = value.into();
        let valid = !value.is_empty()
            && !value.starts_with('/')
            && !value.contains('\\')
            && !value.chars().any(char::is_control)
            && value.split('/').all(|segment| {
                !segment.is_empty() && segment != "." && segment != ".." && segment.len() <= 128
            });
        if valid {
            Ok(Self(value))
        } else {
            Err(PublishError::Path(value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn join(&self, segment: &str) -> Self {
        Self(format!("{}/{segment}", self.0))
    }
}

impl TryFrom<String> for SitePath {
    type Error = PublishError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<SitePath> for String {
    fn from(value: SitePath) -> Self {
        value.0
    }
}

impl fmt::Display for SitePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_matches_sha256_of_empty_input() {
        assert_eq!(
            ArtifactHash::of(b"").as_str(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn hash_rejects_uppercase_and_short_values() {
        assert!(ArtifactHash::parse("ABC").is_err());
        assert!(ArtifactHash::parse(&"A".repeat(64)).is_err());
    }

    #[test]
    fn site_path_rejects_traversal() {
        assert!(SitePath::new("../manifest.json").is_err());
        assert!(SitePath::new("data/../x").is_err());
        assert!(SitePath::new("/data/x").is_err());
        assert!(SitePath::new("data/x.json").is_ok());
    }

    #[test]
    fn artifact_name_rejects_separators() {
        assert!(ArtifactName::new("a/b").is_err());
        assert!(ArtifactName::new("look.looklab").is_ok());
    }
}
