// SPDX-License-Identifier: AGPL-3.0-or-later
//! Body page data: identity, epochs, control schema and optional realizations (docs/08 §2).

use looklab_model::file::{LookVersion, Slug};
use looklab_schema::{
    Affects, Body, BodyKind as SchemaBodyKind, BodyStatus as SchemaBodyStatus, ControlKey,
    ControlSchema, ControlType, EnumValue, ExposureModeKey, MakernoteTag,
    MappingStatus as SchemaMappingStatus, ScalarValue, SelectionScope, SimKey, StageName,
    Tolerated,
};
use serde::{Deserialize, Serialize};

use crate::PublishError;
use crate::manifest::LookStatus;
use crate::qualification::{BundleSelection, RealizationCard};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyKind {
    Camera,
    Software,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyStatus {
    Proposed,
    Active,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MappingStatus {
    Provisional,
    FixtureBacked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransformSource {
    Ssf,
    Dng,
    Db,
    Chart,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialTier {
    New,
    Endorsed,
    Seeder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Affect {
    Tone,
    Tint,
    Chroma,
    Hue,
    Lightness,
    Wb,
    Exposure,
}

impl From<Affects> for Affect {
    fn from(value: Affects) -> Self {
        match value {
            Affects::Tone => Self::Tone,
            Affects::Tint => Self::Tint,
            Affects::Chroma => Self::Chroma,
            Affects::Hue => Self::Hue,
            Affects::Lightness => Self::Lightness,
            Affects::Wb => Self::Wb,
            Affects::Exposure => Self::Exposure,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum ControlDefaultPage {
    Number(String),
    Enum(EnumValue),
    Bool(bool),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilitiesPage {
    pub in_camera_raw_conversion: bool,
    pub jpeg_color_spaces: Vec<String>,
    pub heif: bool,
}

/// Confidence in a body's input transform: its source, anchoring count and the tier of the anchors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputTransformPage {
    pub version: u32,
    pub source: TransformSource,
    pub anchor_count: u32,
    pub tier: CredentialTier,
}

/// A plain-language realization difference from the reference body (docs/08 §2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealizationNote {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u32>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeLookCard {
    pub epoch: u32,
    pub sim_key: SimKey,
    pub look: Slug,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<LookVersion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<LookStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realization: Option<RealizationCard>,
    #[serde(default = "unassessed_scope")]
    pub scope: SelectionScope,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
}

fn unassessed_scope() -> SelectionScope {
    SelectionScope::Unassessed
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeSimPage {
    pub key: SimKey,
    pub look: Slug,
    pub makernote_tag: MakernoteTag,
    pub makernote_value: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlRangePage {
    pub min: String,
    pub max: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlTypePage {
    Int,
    Decimal,
    Enum,
    Bool,
}

impl From<ControlType> for ControlTypePage {
    fn from(value: ControlType) -> Self {
        match value {
            ControlType::Int => Self::Int,
            ControlType::Decimal => Self::Decimal,
            ControlType::Enum => Self::Enum,
            ControlType::Bool => Self::Bool,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "values")]
pub enum ToleratedPage {
    Any,
    Values(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlConstraint {
    pub value: EnumValue,
    pub min_iso: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlPage {
    pub key: ControlKey,
    pub kind: ControlTypePage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<ControlRangePage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    pub values: Vec<EnumValue>,
    pub default: ControlDefaultPage,
    pub look_affecting: bool,
    pub affects: Vec<Affect>,
    pub spatial: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tolerated: Option<ToleratedPage>,
    pub constraints: Vec<ControlConstraint>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SolvePlanStepPage {
    pub stage: StageName,
    pub controls: Vec<ControlKey>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpochPage {
    pub id: u32,
    pub firmware_from: String,
    pub firmware_to: String,
    pub native_sims: Vec<NativeSimPage>,
    pub exposure_modes: Vec<ExposureModeKey>,
    pub controls: Vec<ControlPage>,
    pub solve_plan: Vec<SolvePlanStepPage>,
}

/// Whether normal RAW+JPEG contributions can enter a colour fit for a body.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreparationReadiness {
    /// No readiness decision was published for this body.
    #[default]
    Unknown,
    /// The required input transform is present.
    Ready,
    /// A catalog entry exists, but a required fit resource is absent.
    Unavailable,
}

/// The missing resource that keeps a body from being fitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreparationBlocker {
    InputTransformMissing,
}

/// A body's fit readiness.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparationPage {
    pub readiness: PreparationReadiness,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<PreparationBlocker>,
}

impl PreparationPage {
    pub fn ready() -> Self {
        Self {
            readiness: PreparationReadiness::Ready,
            blocker: None,
        }
    }

    pub fn unavailable(blocker: PreparationBlocker) -> Self {
        Self {
            readiness: PreparationReadiness::Unavailable,
            blocker: Some(blocker),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.readiness == PreparationReadiness::Ready
    }
}

/// Everything the body page renders, as a JSON document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodyPage {
    pub id: looklab_schema::BodyId,
    pub make: String,
    pub model: String,
    /// The control-schema YAML this page was built from.
    pub schema: String,
    pub exif_aliases: Vec<String>,
    pub kind: BodyKind,
    pub status: BodyStatus,
    pub mapping_status: MappingStatus,
    pub capabilities: CapabilitiesPage,
    #[serde(default)]
    pub preparation: PreparationPage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_transform: Option<InputTransformPage>,
    pub epochs: Vec<EpochPage>,
    pub realization_notes: Vec<RealizationNote>,
    pub native_looks: Vec<NativeLookCard>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bundles: Vec<BundleSelection>,
}

/// Body-level facts that are not in the schema snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BodyExtras {
    pub preparation: PreparationPage,
    pub input_transform: Option<InputTransformPage>,
    pub realization_notes: Vec<RealizationNote>,
    pub bundles: Vec<BundleSelection>,
}

impl BodyPage {
    /// # Errors
    /// Returns [`PublishError::BodyMismatch`] when the schema names a different body.
    pub fn from_schema(
        body: &Body,
        schema: &ControlSchema,
        extras: &BodyExtras,
    ) -> Result<Self, PublishError> {
        if schema.body != body.id {
            return Err(PublishError::BodyMismatch {
                body: body.id.to_string(),
                schema: schema.body.to_string(),
            });
        }
        let mut epochs = Vec::with_capacity(schema.epochs.len());
        let mut native_looks = Vec::new();
        for epoch in &schema.epochs {
            let mut native_sims = Vec::with_capacity(epoch.native_sims.len());
            for sim in &epoch.native_sims {
                let look = Slug::new(sim.look.as_str())?;
                native_sims.push(NativeSimPage {
                    key: sim.key.clone(),
                    look: look.clone(),
                    makernote_tag: sim.makernote.tag.clone(),
                    makernote_value: sim.makernote.value.0,
                });
                native_looks.push(NativeLookCard {
                    epoch: epoch.id,
                    sim_key: sim.key.clone(),
                    look,
                    version: None,
                    status: None,
                    realization: None,
                    scope: SelectionScope::Unassessed,
                    blockers: Vec::new(),
                });
            }
            epochs.push(EpochPage {
                id: epoch.id,
                firmware_from: epoch.firmware.from.as_str().to_owned(),
                firmware_to: epoch.firmware.to.as_str().to_owned(),
                native_sims,
                exposure_modes: epoch
                    .exposure_modes
                    .iter()
                    .map(|mode| mode.key.clone())
                    .collect(),
                controls: epoch.controls.iter().map(control_page).collect(),
                solve_plan: epoch
                    .solve_plan
                    .iter()
                    .map(|step| SolvePlanStepPage {
                        stage: step.stage.clone(),
                        controls: step.controls.clone(),
                    })
                    .collect(),
            });
        }
        Ok(Self {
            id: body.id.clone(),
            make: body.make.clone(),
            model: body.model.clone(),
            schema: schema.source().to_owned(),
            exif_aliases: body.exif_aliases.clone(),
            kind: match body.kind {
                SchemaBodyKind::Camera => BodyKind::Camera,
                SchemaBodyKind::Software => BodyKind::Software,
            },
            status: match body.status {
                SchemaBodyStatus::Proposed => BodyStatus::Proposed,
                SchemaBodyStatus::Active => BodyStatus::Active,
            },
            mapping_status: match body.mapping_status {
                SchemaMappingStatus::Provisional => MappingStatus::Provisional,
                SchemaMappingStatus::FixtureBacked => MappingStatus::FixtureBacked,
            },
            capabilities: CapabilitiesPage {
                in_camera_raw_conversion: body.capabilities.in_camera_raw_conversion,
                jpeg_color_spaces: body
                    .capabilities
                    .jpeg_color_spaces
                    .iter()
                    .map(|space| space.as_str().to_owned())
                    .collect(),
                heif: body.capabilities.heif,
            },
            input_transform: extras.input_transform.clone(),
            preparation: extras.preparation.clone(),
            epochs,
            realization_notes: extras.realization_notes.clone(),
            native_looks,
            bundles: extras.bundles.clone(),
        })
    }
}

fn control_page(control: &looklab_schema::Control) -> ControlPage {
    ControlPage {
        key: control.key.clone(),
        kind: control.kind.into(),
        range: control.range.as_ref().map(|range| ControlRangePage {
            min: range.min.to_string(),
            max: range.max.to_string(),
        }),
        step: control.step.as_ref().map(ToString::to_string),
        values: control.values.clone().unwrap_or_default(),
        default: control_default(&control.default),
        look_affecting: control.look_affecting,
        affects: control.affects.iter().copied().map(Affect::from).collect(),
        spatial: control.spatial,
        tolerated: control.tolerated.as_ref().map(|tolerated| match tolerated {
            Tolerated::Any => ToleratedPage::Any,
            Tolerated::Values(values) => {
                ToleratedPage::Values(values.iter().map(scalar_text).collect())
            }
        }),
        constraints: control
            .constraints
            .as_ref()
            .map(|constraints| {
                constraints
                    .min_iso
                    .iter()
                    .map(|(value, min_iso)| ControlConstraint {
                        value: value.clone(),
                        min_iso: *min_iso,
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn control_default(value: &looklab_schema::ControlDefault) -> ControlDefaultPage {
    match value {
        looklab_schema::ControlDefault::Number(number) => {
            ControlDefaultPage::Number(number.to_string())
        }
        looklab_schema::ControlDefault::Enum(value) => ControlDefaultPage::Enum(value.clone()),
        looklab_schema::ControlDefault::Bool(value) => ControlDefaultPage::Bool(*value),
    }
}

fn scalar_text(value: &ScalarValue) -> String {
    match value {
        ScalarValue::Bool(value) => value.to_string(),
        ScalarValue::Number(value) => value.to_string(),
        ScalarValue::Text(value) => value.clone(),
    }
}
