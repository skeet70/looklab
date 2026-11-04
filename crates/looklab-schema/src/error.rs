// SPDX-License-Identifier: AGPL-3.0-or-later
//! Validation errors for the data files.

use crate::control::ControlType;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ErrorKind {
    #[error("YAML: {0}")]
    Syntax(String),
    #[error("YAML document rejected: {0}")]
    YamlRejected(String),
    #[error("missing `{field}`")]
    MissingField { field: &'static str },
    #[error("expected {expected}")]
    InvalidValue { expected: &'static str },
    #[error("`{field}` is not allowed on a {control_type} control")]
    FieldNotAllowed {
        field: &'static str,
        control_type: ControlType,
    },
    #[error("range must have 2 endpoints, found {found}")]
    RangeArity { found: usize },
    #[error("range minimum exceeds maximum")]
    RangeInverted,
    #[error("step must be positive")]
    StepNotPositive,
    #[error("value is not a multiple of the step")]
    NotOnStep,
    #[error("default is outside the range")]
    DefaultOutOfRange,
    #[error("enum default is not one of the values")]
    EnumDefaultNotInValues,
    #[error("unknown `affects` value `{value}`")]
    UnknownAffect { value: String },
    #[error("`look_affecting: true` requires at least one `affects` value")]
    AffectsRequired,
    #[error("duplicate key `{key}`")]
    DuplicateKey { key: String },
    #[error("solve plan names undeclared control `{key}`")]
    SolvePlanUnknownControl { key: String },
    #[error("solve plan repeats control `{key}`")]
    SolvePlanDuplicateControl { key: String },
    #[error("firmware range is inverted")]
    FirmwareRangeInverted,
    #[error("firmware range overlaps another epoch")]
    FirmwareRangeOverlap,
    #[error("`spatial: true` requires `tolerated`")]
    SpatialToleratedRequired,
    #[error("snapshot manifest: {0}")]
    SnapshotManifest(String),
    #[error("snapshot content hash is {found}, the manifest declares {declared}")]
    SnapshotHashMismatch { declared: String, found: String },
    #[error("the document declares `{declared}`, which does not match its path `{path}`")]
    DocumentOwner { declared: String, path: String },
    #[error("`fixture_backed` needs a fixture donation for {entity}")]
    FixtureEvidenceMissing { entity: String },
    #[error("the fixture manifest is invalid: {0}")]
    FixtureManifest(String),
}

/// A rule violation at a YAML path such as `epochs[0].controls[3].default`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{path}: {kind}")]
pub struct ValidationError {
    pub path: String,
    pub kind: ErrorKind,
}

impl ValidationError {
    pub fn new(path: impl Into<String>, kind: ErrorKind) -> Self {
        Self {
            path: path.into(),
            kind,
        }
    }

    pub(crate) fn syntax(error: serde_norway::Error) -> Vec<Self> {
        vec![Self::new("", ErrorKind::Syntax(error.to_string()))]
    }
}
