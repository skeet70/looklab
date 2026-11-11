// SPDX-License-Identifier: AGPL-3.0-or-later
//! A camera owner's signed confirmation or correction of a documented control.

use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{ErrorKind, ValidationError};
use crate::ids::BodyId;
use crate::manual::{
    Availability, CapabilityDomain, CapabilityKey, DocumentedCapability, ManualCatalog,
};
use crate::proposal::Sha256Hex;

pub const MANUAL_CONFIRMATION_VERSION: u32 = 1;
pub const MAX_CONFIRMATION_BYTES: usize = 64 * 1024;
pub const MAX_CONFIRMATION_OBSERVATIONS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualConfirmation {
    pub version: u32,
    pub camera: BodyId,
    pub camera_digest: Sha256Hex,
    pub firmware: ReportedFirmware,
    pub scope: ConfirmationScope,
    pub observations: Vec<ControlObservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ReportedFirmware(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("expected a non-empty firmware version")]
pub struct ReportedFirmwareError;

impl ReportedFirmware {
    /// # Errors
    /// Empty, over-long or control-character text.
    pub fn new(value: &str) -> Result<Self, ReportedFirmwareError> {
        if value.trim().is_empty() || value.len() > 200 || value.chars().any(char::is_control) {
            return Err(ReportedFirmwareError);
        }
        Ok(Self(value.trim().to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ReportedFirmware {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ReportedFirmware {
    type Error = ReportedFirmwareError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<ReportedFirmware> for String {
    fn from(value: ReportedFirmware) -> Self {
        value.0
    }
}

impl ManualConfirmation {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a confirmation has no unserializable value")
    }

    pub fn digest(&self) -> Sha256Hex {
        Sha256Hex::from_bytes(Sha256::digest(self.canonical_bytes()).into())
    }

    /// Rules that hold without a catalog.
    ///
    /// # Errors
    /// Every structure rule violation, as paths into the payload.
    pub fn validate_structure(&self) -> Result<(), Vec<ValidationError>> {
        let (errors, _) = self.structure_errors();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    fn structure_errors(&self) -> (Vec<ValidationError>, HashSet<CapabilityKey>) {
        let mut errors = Vec::new();
        if self.version != MANUAL_CONFIRMATION_VERSION {
            errors.push(ValidationError::new(
                "version",
                ErrorKind::UnsupportedConfirmationVersion {
                    found: self.version,
                },
            ));
        }
        if self.observations.is_empty() {
            errors.push(ValidationError::new(
                "observations",
                ErrorKind::EmptyText {
                    field: "observations",
                },
            ));
        }
        if self.observations.len() > MAX_CONFIRMATION_OBSERVATIONS {
            errors.push(ValidationError::new(
                "observations",
                ErrorKind::ConfirmationTooLarge,
            ));
        }
        let mut observed = HashSet::new();
        for (index, observation) in self.observations.iter().enumerate() {
            let path = format!("observations[{index}]");
            if !observed.insert(observation.key) {
                errors.push(ValidationError::new(
                    format!("{path}.key"),
                    ErrorKind::DuplicateKey {
                        key: observation.key.as_str().to_owned(),
                    },
                ));
            }
            observation.validate_structure(&path, &mut errors);
        }
        self.scope.validate_structure(&observed, &mut errors);
        if self.canonical_bytes().len() > MAX_CONFIRMATION_BYTES {
            errors.push(ValidationError::new("", ErrorKind::ConfirmationTooLarge));
        }
        (errors, observed)
    }

    /// # Errors
    /// Every rule violation, as paths into the payload.
    pub fn validate(&self, catalog: &ManualCatalog) -> Result<(), Vec<ValidationError>> {
        let (mut errors, observed) = self.structure_errors();
        let Some(camera) = catalog.camera(&self.camera) else {
            errors.push(ValidationError::new(
                "camera",
                ErrorKind::InvalidValue {
                    expected: "a documented camera id",
                },
            ));
            return Err(errors);
        };
        if self.camera_digest != camera.definition_digest() {
            errors.push(ValidationError::new(
                "camera_digest",
                ErrorKind::CameraDefinitionChanged,
            ));
        }
        for (index, observation) in self.observations.iter().enumerate() {
            let path = format!("observations[{index}]");
            let Some(capability) = camera
                .capabilities
                .iter()
                .find(|capability| capability.key == observation.key)
            else {
                errors.push(ValidationError::new(
                    format!("{path}.key"),
                    ErrorKind::UnknownDocumentedControl {
                        key: observation.key.as_str().to_owned(),
                    },
                ));
                continue;
            };
            observation.validate_documented(capability, camera, &path, &mut errors);
        }
        self.scope
            .validate_complete_scope(camera.capabilities.as_slice(), &observed, &mut errors);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfirmationScope {
    Complete,
    Controls { keys: Vec<CapabilityKey> },
}

impl<'de> Deserialize<'de> for ConfirmationScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
        enum Raw {
            Complete {},
            Controls { keys: Vec<CapabilityKey> },
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Complete {} => Self::Complete,
            Raw::Controls { keys } => Self::Controls { keys },
        })
    }
}

impl ConfirmationScope {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Controls { .. } => "controls",
        }
    }

    fn validate_structure(
        &self,
        observed: &HashSet<CapabilityKey>,
        errors: &mut Vec<ValidationError>,
    ) {
        let Self::Controls { keys } = self else {
            return;
        };
        if keys.is_empty() {
            errors.push(ValidationError::new(
                "scope.keys",
                ErrorKind::EmptyText { field: "keys" },
            ));
        }
        let mut listed = HashSet::new();
        for (index, key) in keys.iter().enumerate() {
            if !listed.insert(*key) {
                errors.push(ValidationError::new(
                    format!("scope.keys[{index}]"),
                    ErrorKind::DuplicateKey {
                        key: key.as_str().to_owned(),
                    },
                ));
            }
            if !observed.contains(key) {
                errors.push(ValidationError::new(
                    format!("scope.keys[{index}]"),
                    ErrorKind::ScopeMismatch,
                ));
            }
        }
        for key in observed {
            if !listed.contains(key) {
                errors.push(ValidationError::new("scope.keys", ErrorKind::ScopeMismatch));
            }
        }
    }

    fn validate_complete_scope(
        &self,
        capabilities: &[DocumentedCapability],
        observed: &HashSet<CapabilityKey>,
        errors: &mut Vec<ValidationError>,
    ) {
        if !matches!(self, Self::Complete) {
            return;
        }
        for capability in capabilities {
            if capability.availability == Availability::Available
                && !observed.contains(&capability.key)
            {
                errors.push(ValidationError::new(
                    "scope",
                    ErrorKind::CompleteConfirmationMissing {
                        key: capability.key.as_str().to_owned(),
                    },
                ));
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlObservation {
    pub key: CapabilityKey,
    pub definition_digest: Sha256Hex,
    pub finding: Finding,
}

impl ControlObservation {
    fn validate_structure(&self, path: &str, errors: &mut Vec<ValidationError>) {
        let Finding::Corrected {
            availability,
            domain,
        } = &self.finding
        else {
            return;
        };
        if *availability == Availability::NotDocumented {
            errors.push(ValidationError::new(
                format!("{path}.finding.availability"),
                ErrorKind::CorrectionNotDocumented,
            ));
        }
        if *availability == Availability::Unavailable && *domain != CapabilityDomain::Unspecified {
            errors.push(ValidationError::new(
                format!("{path}.finding.domain"),
                ErrorKind::ManualDomainNotUnspecified {
                    key: self.key.as_str().to_owned(),
                },
            ));
        }
        domain.validate(&format!("{path}.finding.domain"), errors);
    }

    fn validate_documented(
        &self,
        capability: &DocumentedCapability,
        camera: &crate::manual::ManualCamera,
        path: &str,
        errors: &mut Vec<ValidationError>,
    ) {
        if self.definition_digest != capability.definition_digest(camera) {
            errors.push(ValidationError::new(
                format!("{path}.definition_digest"),
                ErrorKind::ControlDefinitionChanged {
                    key: self.key.as_str().to_owned(),
                },
            ));
        }
        if matches!(&self.finding, Finding::Matches)
            && capability.availability != Availability::Available
        {
            errors.push(ValidationError::new(
                format!("{path}.finding"),
                ErrorKind::MatchesNeedsAvailable,
            ));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Finding {
    Matches,
    Corrected {
        availability: Availability,
        domain: CapabilityDomain,
    },
}

impl<'de> Deserialize<'de> for Finding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
        enum Raw {
            Matches {},
            Corrected {
                availability: Availability,
                domain: CapabilityDomain,
            },
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Matches {} => Self::Matches,
            Raw::Corrected {
                availability,
                domain,
            } => Self::Corrected {
                availability,
                domain,
            },
        })
    }
}

/// # Errors
/// A malformed or over-large payload, or any rule violation.
pub fn validate_manual_confirmation(
    payload: &str,
    catalog: &ManualCatalog,
) -> Result<ManualConfirmation, Vec<ValidationError>> {
    if payload.len() > MAX_CONFIRMATION_BYTES {
        return Err(vec![ValidationError::new(
            "",
            ErrorKind::ConfirmationTooLarge,
        )]);
    }
    crate::yaml::check(payload)?;
    let confirmation: ManualConfirmation =
        serde_norway::from_str(payload).map_err(ValidationError::syntax)?;
    confirmation.validate(catalog)?;
    Ok(confirmation)
}
