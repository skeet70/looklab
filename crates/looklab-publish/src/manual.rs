// SPDX-License-Identifier: AGPL-3.0-or-later
//! Published documentation records and their confirmation state.

use std::collections::BTreeMap;

use looklab_schema::{
    BodyId, ControlConfirmation, DocumentedCapability, ManualCamera, ManualConfirmationSummary,
    ManualSource, Sha256Hex, evaluate_camera, evaluate_control,
};
use serde::{Deserialize, Serialize};

use crate::body::PreparationPage;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualCapabilityPage {
    #[serde(flatten)]
    pub documented: DocumentedCapability,
    pub definition_digest: Sha256Hex,
    pub confirmation: ControlConfirmation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualPage {
    pub id: BodyId,
    pub make: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub introduced_year: Option<u16>,
    pub catalog_version: u32,
    pub catalog_digest: Sha256Hex,
    pub camera_digest: Sha256Hex,
    pub documentation_derived: bool,
    pub human_confirmed: bool,
    pub preparation: PreparationPage,
    pub sources: Vec<ManualSource>,
    pub capabilities: Vec<ManualCapabilityPage>,
    pub notes: Vec<String>,
    pub confirmation: ManualConfirmationSummary,
}

impl ManualPage {
    pub fn from_camera(
        camera: &ManualCamera,
        catalog_version: u32,
        catalog_digest: Sha256Hex,
        preparation: PreparationPage,
        confirmation: ManualConfirmationSummary,
    ) -> Self {
        let capabilities = camera
            .capabilities
            .iter()
            .map(|capability| ManualCapabilityPage {
                documented: capability.clone(),
                definition_digest: capability.definition_digest(camera),
                confirmation: confirmation
                    .control(capability.key)
                    .cloned()
                    .unwrap_or_else(|| evaluate_control(camera, capability, &[], None)),
            })
            .collect();
        let human_confirmed = confirmation.human_confirmed();
        Self {
            id: camera.id.clone(),
            make: camera.make.clone(),
            model: camera.model.clone(),
            introduced_year: camera.introduced_year,
            catalog_version,
            catalog_digest,
            camera_digest: camera.definition_digest(),
            documentation_derived: true,
            human_confirmed,
            preparation,
            sources: camera.sources.clone(),
            capabilities,
            notes: camera.notes.clone(),
            confirmation,
        }
    }

    pub fn documented(
        camera: &ManualCamera,
        catalog_version: u32,
        catalog_digest: Sha256Hex,
        preparation: PreparationPage,
    ) -> Self {
        let confirmation = evaluate_camera(camera, &[], &BTreeMap::new());
        Self::from_camera(
            camera,
            catalog_version,
            catalog_digest,
            preparation,
            confirmation,
        )
    }

    pub fn summary(&self) -> ManualSummary {
        ManualSummary {
            id: self.id.clone(),
            make: self.make.clone(),
            model: self.model.clone(),
            introduced_year: self.introduced_year,
            source_count: self.sources.len(),
            documented_controls: self.capabilities.len(),
            confirmed_controls: self.confirmation.confirmed,
            contradicted_controls: self.confirmation.contradicted,
            recorded_checks: self
                .confirmation
                .controls
                .iter()
                .map(|control| control.intake.recorded)
                .sum(),
            unlinked_checks: self
                .confirmation
                .controls
                .iter()
                .map(|control| control.intake.unlinked)
                .sum(),
            preparation: self.preparation.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualSummary {
    pub id: BodyId,
    pub make: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub introduced_year: Option<u16>,
    pub source_count: usize,
    pub documented_controls: usize,
    pub confirmed_controls: u32,
    pub contradicted_controls: u32,
    #[serde(default)]
    pub recorded_checks: u32,
    #[serde(default)]
    pub unlinked_checks: u32,
    pub preparation: PreparationPage,
}
