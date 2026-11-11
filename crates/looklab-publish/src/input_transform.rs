// SPDX-License-Identifier: AGPL-3.0-or-later
//! Published body input-transform priors (docs/04 §7).

use core::fmt;

use looklab_schema::BodyId;
use serde::{Deserialize, Serialize};

use crate::PublishError;
use crate::hash::{ArtifactHash, SitePath};

/// A positive input-transform version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct InputTransformVersion(u32);

impl InputTransformVersion {
    /// # Errors
    /// Rejects zero.
    pub fn new(value: u32) -> Result<Self, PublishError> {
        if value == 0 {
            Err(PublishError::InputTransform(
                "input transform version must be positive".to_owned(),
            ))
        } else {
            Ok(Self(value))
        }
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for InputTransformVersion {
    type Error = PublishError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<InputTransformVersion> for u32 {
    fn from(value: InputTransformVersion) -> Self {
        value.0
    }
}

impl fmt::Display for InputTransformVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputTransformEntry {
    pub body: BodyId,
    pub version: InputTransformVersion,
    pub path: SitePath,
    pub sha256: ArtifactHash,
    pub bytes: u64,
}

impl InputTransformEntry {
    /// # Errors
    /// Rejects bytes whose digest or size differs.
    pub fn verify(&self, bytes: &[u8]) -> Result<(), PublishError> {
        if self.bytes != bytes.len() as u64 {
            return Err(PublishError::InputTransform(format!(
                "input transform for {} has {} bytes, the entry lists {}",
                self.body,
                bytes.len(),
                self.bytes
            )));
        }
        let actual = ArtifactHash::of(bytes);
        if actual != self.sha256 {
            return Err(PublishError::InputTransform(format!(
                "input transform for {} hashes to {actual}, the entry lists {}",
                self.body, self.sha256
            )));
        }
        Ok(())
    }
}

/// A transform revision to publish, with its exact bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct PublishedInputTransform {
    pub entry: InputTransformEntry,
    pub bytes: Vec<u8>,
    pub current: bool,
}

impl PublishedInputTransform {
    /// # Errors
    /// Rejects empty bytes or a hash that is not a safe path component.
    pub fn new(
        body: BodyId,
        version: InputTransformVersion,
        bytes: Vec<u8>,
        current: bool,
    ) -> Result<Self, PublishError> {
        if bytes.is_empty() {
            return Err(PublishError::InputTransform(
                "input transform bytes are empty".to_owned(),
            ));
        }
        let sha256 = ArtifactHash::of(&bytes);
        let entry = InputTransformEntry {
            body,
            version,
            path: input_transform_path(&sha256)?,
            sha256,
            bytes: bytes.len() as u64,
        };
        Ok(Self {
            entry,
            bytes,
            current,
        })
    }
}

/// # Errors
/// Rejects a hash that is not a safe path component.
pub fn input_transform_path(sha256: &ArtifactHash) -> Result<SitePath, PublishError> {
    SitePath::new(format!("data/input-transforms/{}.yaml", sha256.as_str()))
}
