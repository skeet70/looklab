// SPDX-License-Identifier: AGPL-3.0-or-later
//! Pinned body and realization parameters a published match bundle consumes (docs/04 §2).

use serde::{Deserialize, Serialize};

use crate::ids::{BodyId, ExposureModeKey};
use crate::model_subject::{BodyRealizationVersionId, ControlEpochId};
use crate::schema::FirmwareVersion;

pub const MATCH_PARAMS_FORMAT: &str = "looklab-match-params/1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchParamsFormat {
    #[serde(rename = "looklab-match-params/1")]
    V1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchStage {
    Native,
    Published,
}

/// A validated, non-empty logical white-balance mode key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WbModeKey(String);

impl WbModeKey {
    /// # Errors
    /// Empty or whitespace-bearing text.
    pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if value.trim().is_empty() || value.chars().any(char::is_whitespace) {
            return Err("a white-balance mode key is non-empty and has no whitespace");
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for WbModeKey {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<WbModeKey> for String {
    fn from(value: WbModeKey) -> Self {
        value.0
    }
}

/// A firmware identity scoped to one body epoch.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FirmwareId {
    pub body: BodyId,
    pub epoch: ControlEpochId,
    pub version: FirmwareVersion,
}

impl FirmwareId {
    pub fn as_text(&self) -> String {
        format!(
            "{}@{}:{}",
            self.body,
            self.epoch.get(),
            self.version.as_str()
        )
    }

    /// # Errors
    /// Text that is not `body@epoch:version`.
    pub fn parse(text: &str) -> Result<Self, &'static str> {
        let (head, version) = text.rsplit_once(':').ok_or("firmware id needs a version")?;
        let (body, epoch) = head
            .rsplit_once('@')
            .ok_or("firmware id needs a body and epoch")?;
        Ok(Self {
            body: BodyId::new(body).map_err(|_| "firmware id has an invalid body")?,
            epoch: ControlEpochId::new(
                epoch
                    .parse()
                    .map_err(|_| "firmware id has an invalid epoch")?,
            ),
            version: version
                .parse()
                .map_err(|_| "firmware id has an invalid version")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchTransform {
    pub version: u32,
    pub camera_to_xyz: [[f64; 3]; 3],
    /// `Δ` in [`looklab_body::Delta6`] order; the matrix already includes it.
    pub delta: [f64; 6],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExposureModeOffset {
    pub mode: ExposureModeKey,
    pub offset: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WbModeOffset {
    pub mode: WbModeKey,
    pub r: f64,
    pub b: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FirmwareOffset {
    pub firmware: FirmwareId,
    pub t_shift: f64,
    pub log_g_shift: f64,
}

/// Body physics shared by every source sim of one `(body, epoch)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchParams {
    pub format: MatchParamsFormat,
    pub stage: MatchStage,
    pub body: BodyId,
    pub transform: MatchTransform,
    pub exposure: Vec<ExposureModeOffset>,
    pub wb: Vec<WbModeOffset>,
}

/// One source sim's realization and its native-family firmware offsets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchRealizationParams {
    pub format: MatchParamsFormat,
    pub realization: BodyRealizationVersionId,
    pub family: crate::model_subject::FamilyTargetId,
    pub version: u32,
    pub control_response_version: u32,
    pub clip_k: f64,
    pub firmware: Vec<FirmwareOffset>,
}

impl MatchRealizationParams {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.version == 0 || self.control_response_version == 0 {
            return Err("realization versions are zero");
        }
        self.validate_geometry()
    }

    /// A native-stage binding may exist without a fitted control response.
    pub fn validate_native(&self) -> Result<(), &'static str> {
        if self.version == 0 {
            return Err("realization version is zero");
        }
        self.validate_geometry()
    }

    fn validate_geometry(&self) -> Result<(), &'static str> {
        if !self.clip_k.is_finite() || self.clip_k <= 0.0 {
            return Err("realization clip is not positive");
        }
        if self
            .firmware
            .iter()
            .any(|entry| !entry.t_shift.is_finite() || !entry.log_g_shift.is_finite())
        {
            return Err("firmware offset is not finite");
        }
        Ok(())
    }

    pub fn firmware_offset(
        &self,
        firmware: Option<&FirmwareId>,
    ) -> Result<Option<[f64; 2]>, &'static str> {
        match (firmware, self.firmware.as_slice()) {
            (Some(firmware), _) => Ok(self
                .firmware
                .iter()
                .find(|entry| &entry.firmware == firmware)
                .map(|entry| [entry.t_shift, entry.log_g_shift])),
            (None, []) => Ok(None),
            (None, [only]) => Ok(Some([only.t_shift, only.log_g_shift])),
            (None, _) => Err("several firmware offsets are pinned; choose one"),
        }
    }
}

impl MatchParams {
    pub fn validate(&self, body: &BodyId) -> Result<(), &'static str> {
        if &self.body != body {
            return Err("match params name another body");
        }
        if self.transform.version == 0
            || self
                .transform
                .camera_to_xyz
                .iter()
                .flatten()
                .any(|v| !v.is_finite())
        {
            return Err("match transform is not finite or unversioned");
        }
        if self.transform.delta.iter().any(|v| !v.is_finite()) {
            return Err("match transform delta is not finite");
        }
        if self.exposure.iter().any(|entry| !entry.offset.is_finite()) {
            return Err("exposure offset is not finite");
        }
        if self
            .wb
            .iter()
            .any(|entry| !entry.r.is_finite() || !entry.b.is_finite())
        {
            return Err("white-balance gain is not finite");
        }
        Ok(())
    }

    pub fn wb_offset(&self, mode: Option<&WbModeKey>) -> Result<[f64; 2], &'static str> {
        match (mode, self.wb.as_slice()) {
            (Some(mode), _) => Ok(self
                .wb
                .iter()
                .find(|entry| &entry.mode == mode)
                .map_or([0.0, 0.0], |entry| [entry.r, entry.b])),
            (None, []) => Ok([0.0, 0.0]),
            (None, [only]) => Ok([only.r, only.b]),
            (None, _) => Err("several white-balance modes are pinned; choose one"),
        }
    }
}
