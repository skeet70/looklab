// SPDX-License-Identifier: AGPL-3.0-or-later
//! Validation errors for the data files.

use crate::control::ControlType;
use crate::ids::SourceId;

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
    #[error("`{family}` is not a look family of this epoch")]
    UnknownFamily { family: String },
    #[error("look family `{family}` mixes monochrome and colour native sims")]
    FamilyRenderClassConflict { family: String },
    #[error("`{family}` is not a monochrome family of this epoch")]
    MonochromeFamilyNotDeclared { family: String },
    #[error("monochrome capability lists `{control}` but the control is absent")]
    MonochromeControlMissing { control: &'static str },
    #[error("control `{control}` is present but the monochrome capability does not list it")]
    MonochromeControlNotDeclared { control: &'static str },
    #[error("control `{control}` scope does not match the monochrome capability")]
    MonochromeControlMismatch { control: &'static str },
    #[error("monochrome capability retains grain size but no `grain_size` control exists")]
    MonochromeGrainSizeMissing,
    #[error("a `grain_size` control exists but the monochrome capability does not retain it")]
    MonochromeGrainSizeNotDeclared,
    #[error("`grain_size` must be a spatial, mapped, family-independent control")]
    MonochromeGrainSizeMismatch,
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
    #[error("`{tag}` is not a known makernote tag")]
    UnknownMakernoteTag { tag: String },
    #[error("a makernote mapping needs `encoding` or `values`")]
    MissingMakernoteEncoding,
    #[error("a makernote mapping takes `encoding` or `values`, not both")]
    ConflictingMakernoteEncoding,
    #[error("a paired makernote tag needs `component`")]
    MakernoteComponentRequired,
    #[error("`component` is only meaningful for a paired makernote tag")]
    MakernoteComponentNotAllowed,
    #[error("`component` {component} is outside a tag of {components} values")]
    MakernoteComponentOutOfRange { component: u8, components: u8 },
    #[error("a scale needs a numeric control")]
    MakernoteScaleNotNumeric,
    #[error("the scale is zero")]
    MakernoteScaleZero,
    #[error("the scaled raw value of step {value} is not an integer")]
    MakernoteRawNotInteger { value: String },
    #[error("`{value}` is not a value of the control")]
    MakernoteValueUnknown { value: String },
    #[error("the control value `{value}` has no makernote value")]
    MakernoteValueMissing { value: String },
    #[error("the mapped value has no raw tag value")]
    MakernoteValueHasNoRaw,
    #[error("`{value}` is outside the control's range")]
    MakernoteValueOutOfRange { value: String },
    #[error("raw value {raw} maps to more than one control value")]
    MakernoteDuplicateRaw { raw: i64 },
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
    #[error("manual catalog version {found} is not supported")]
    UnsupportedCatalogVersion { found: u32 },
    #[error("citation source `{source_id}` is not defined on this camera")]
    UnknownCitationSource { source_id: SourceId },
    #[error("`{field}` must not be empty")]
    EmptyText { field: &'static str },
    #[error("`{value}` is not an HTTPS URL")]
    InvalidUrl { value: String },
    #[error("`{value}` is not an exact rational")]
    InvalidRational { value: String },
    #[error("`{key}` needs at least one citation")]
    CitationRequired { key: String },
    #[error("`{key}` must carry an enum domain")]
    ManualKeyNotEnum { key: String },
    #[error("`{key}` has no documented value and needs an unspecified domain")]
    ManualDomainNotUnspecified { key: String },
    #[error("year {year} is outside the catalog's 2011-onward scope")]
    YearOutOfScope { year: u16 },
    #[error("a correction cannot set availability `not_documented`")]
    CorrectionNotDocumented,
    #[error("a `matches` finding needs an available documented control")]
    MatchesNeedsAvailable,
    #[error("confirmation scope names `{key}`, which is not documented for this camera")]
    UnknownDocumentedControl { key: String },
    #[error("complete confirmation omits available control `{key}`")]
    CompleteConfirmationMissing { key: String },
    #[error("confirmation scope and observations disagree")]
    ScopeMismatch,
    #[error("the confirmation payload is larger than allowed")]
    ConfirmationTooLarge,
    #[error("confirmation version {found} is not supported")]
    UnsupportedConfirmationVersion { found: u32 },
    #[error("the camera definition changed since this confirmation was prepared")]
    CameraDefinitionChanged,
    #[error("`{key}` changed since this confirmation was prepared")]
    ControlDefinitionChanged { key: String },
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
