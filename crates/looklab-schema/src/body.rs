// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Body entity file.

use serde::{Deserialize, Serialize};

use crate::error::ValidationError;
use crate::ids::{BodyId, IdError};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Body {
    pub id: BodyId,
    pub make: String,
    pub model: String,
    #[serde(default)]
    pub exif_aliases: Vec<String>,
    pub kind: BodyKind,
    pub capabilities: Capabilities,
    pub status: BodyStatus,
    pub mapping_status: MappingStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    pub in_camera_raw_conversion: bool,
    pub jpeg_color_spaces: Vec<ColorSpace>,
    pub heif: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(try_from = "String")]
pub struct ColorSpace(String);

impl ColorSpace {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ColorSpace {
    type Error = IdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() {
            return Err(IdError::Empty);
        }
        if value.chars().any(char::is_whitespace) {
            return Err(IdError::Whitespace);
        }
        Ok(Self(value))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyKind {
    Camera,
    Software,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyStatus {
    Proposed,
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MappingStatus {
    Provisional,
    FixtureBacked,
}

impl Body {
    pub fn matches_fixture(&self, make: &str, model: &str) -> bool {
        self.make.trim().eq_ignore_ascii_case(make.trim())
            && self.model.trim().eq_ignore_ascii_case(model.trim())
    }
}

pub fn validate_body(yaml: &str) -> Result<Body, Vec<ValidationError>> {
    crate::yaml::check(yaml)?;
    serde_norway::from_str(yaml).map_err(ValidationError::syntax)
}
