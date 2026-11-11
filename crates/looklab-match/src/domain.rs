// SPDX-License-Identifier: AGPL-3.0-or-later
//! Joint assessed settings and probe domains for qualified matching
//! (docs/04 §1, docs/04 §2 selection scope).

use std::collections::BTreeSet;

use looklab_model::LookInput;
use looklab_model::realization::{ControlValue, Settings};
use looklab_schema::{
    AssessedPoint, AssessmentOutcome, CellId, CombinationBasis, FirmwareId,
    QualificationCapability, SettingCombination, SettingValue, WbModeKey,
};

use crate::error::{MatchError, Result};
use crate::settings::Plan;

/// The largest `ranked sims × assessed combinations` product a qualified match evaluates.
pub const MAX_ASSESSED_EVALUATIONS: usize = 4096;

/// The complete settings points and probe cells one passing controls report assessed.
#[derive(Clone, Debug, PartialEq)]
pub struct AssessedDomain {
    settings: Vec<Settings>,
    supported: BTreeSet<CellId>,
}

impl AssessedDomain {
    #[expect(
        clippy::too_many_arguments,
        reason = "the assessed scope and pins are distinct inputs"
    )]
    pub fn from_parts(
        plan: &Plan,
        capability: QualificationCapability,
        outcome: AssessmentOutcome,
        scope: &looklab_schema::ReportScope,
        supported_mask: &looklab_schema::SupportedMask,
        requested_firmware: Option<&FirmwareId>,
        requested_wb: Option<&WbModeKey>,
        pinned_firmware: &[FirmwareId],
        pinned_wb: &[WbModeKey],
    ) -> Result<Self> {
        if capability != QualificationCapability::ControlsMatching {
            return Err(MatchError::Schema(
                "a qualified match needs a controls and matching report".to_owned(),
            ));
        }
        if outcome != AssessmentOutcome::Pass {
            return Err(MatchError::Schema(format!(
                "controls report outcome is {outcome:?}, not pass"
            )));
        }
        if scope.combination_basis != CombinationBasis::Enumerated {
            return Err(MatchError::Schema(
                "assessed settings must be enumerated, not an implicit product".to_owned(),
            ));
        }
        if scope.points.is_empty() {
            return Err(MatchError::Schema(
                "the controls report assesses no settings".to_owned(),
            ));
        }
        let mut settings = Vec::with_capacity(scope.points.len());
        for assessed in &scope.points {
            if !physical_context_covers(
                assessed,
                requested_firmware,
                requested_wb,
                pinned_firmware,
                pinned_wb,
            ) {
                continue;
            }
            if assessed.settings.is_empty() && !scope.defaults_assessed {
                return Err(MatchError::Schema(
                    "the default combination is not assessed".to_owned(),
                ));
            }
            let point = plan.settings_for(&assessed.settings)?;
            if !settings.contains(&point) {
                settings.push(point);
            }
        }
        if settings.is_empty() {
            return Err(MatchError::Schema(
                "no assessed point covers the requested physical context".to_owned(),
            ));
        }
        if supported_mask.is_empty() {
            return Err(MatchError::NoSupportedProbes);
        }
        let supported = supported_mask.iter().cloned().collect();
        Ok(Self {
            settings,
            supported,
        })
    }

    pub fn combinations(&self) -> &[Settings] {
        &self.settings
    }

    pub fn supports(&self, cell: &CellId) -> bool {
        self.supported.contains(cell)
    }

    pub fn intersect_cells(&mut self, other: &Self) {
        self.supported.retain(|cell| other.supported.contains(cell));
    }

    /// The joint `cell-{index}` identity of a probe, as the assessment records it.
    pub fn cell_of(probe: &looklab_model::Probe) -> Option<CellId> {
        let input = LookInput::new(probe.xyz);
        CellId::new(format!("cell-{}", looklab_fit::model_error::cell(&input))).ok()
    }

    /// # Errors
    /// A report whose supported cells cover no probe in the set.
    pub fn filter_probes_with_indices(
        &self,
        probes: &looklab_model::ProbeSet,
    ) -> Result<(looklab_model::ProbeSet, Vec<usize>)> {
        let mut kept = Vec::new();
        let mut indices = Vec::new();
        for (index, probe) in probes.probes.iter().enumerate() {
            if Self::cell_of(probe).is_some_and(|cell| self.supported.contains(&cell)) {
                kept.push(probe.clone());
                indices.push(index);
            }
        }
        if kept.is_empty() {
            return Err(MatchError::NoSupportedProbes);
        }
        Ok((
            looklab_model::ProbeSet {
                version: probes.version,
                notes: probes.notes.clone(),
                probes: kept,
            },
            indices,
        ))
    }

    /// # Errors
    /// A report whose supported cells cover no probe in the set.
    pub fn filter_probes(
        &self,
        probes: &looklab_model::ProbeSet,
    ) -> Result<looklab_model::ProbeSet> {
        Ok(self.filter_probes_with_indices(probes)?.0)
    }

    /// # Errors
    /// An aggregate evaluated-combination count above [`MAX_ASSESSED_EVALUATIONS`].
    pub fn check_budget(&self, sims: usize) -> Result<usize> {
        let evaluations = self.settings.len().saturating_mul(sims.max(1));
        if evaluations > MAX_ASSESSED_EVALUATIONS {
            return Err(MatchError::AssessedDomainTooLarge {
                combinations: self.settings.len(),
                sims,
                budget: MAX_ASSESSED_EVALUATIONS,
            });
        }
        Ok(evaluations)
    }
}

fn physical_context_covers(
    point: &AssessedPoint,
    requested_firmware: Option<&FirmwareId>,
    requested_wb: Option<&WbModeKey>,
    pinned_firmware: &[FirmwareId],
    pinned_wb: &[WbModeKey],
) -> bool {
    let firmware = match (&point.firmware, requested_firmware) {
        (Some(mode), Some(requested)) => mode == requested && pinned_firmware.contains(mode),
        (Some(mode), None) => pinned_firmware.len() == 1 && pinned_firmware.contains(mode),
        (None, Some(requested)) => pinned_firmware.len() == 1 && &pinned_firmware[0] == requested,
        (None, None) => pinned_firmware.len() == 1,
    };
    let wb = match (&point.wb_mode, requested_wb) {
        (Some(mode), Some(requested)) => mode == requested && pinned_wb.contains(mode),
        (Some(mode), None) => pinned_wb.len() == 1 && pinned_wb.contains(mode),
        (None, Some(requested)) => pinned_wb.len() == 1 && &pinned_wb[0] == requested,
        (None, None) => pinned_wb.len() == 1,
    };
    firmware && wb
}

pub fn setting_value(value: &SettingValue) -> ControlValue {
    match value {
        SettingValue::Step(step) => ControlValue::Step(*step),
        SettingValue::Enum(value) => ControlValue::Enum(value.clone()),
        SettingValue::Bool(value) => ControlValue::Bool(*value),
    }
}

impl Plan {
    /// The complete settings point a report combination names; unspecified
    /// controls keep the schema default.
    ///
    /// # Errors
    /// A control outside this epoch or a value outside its domain.
    pub fn settings_for(&self, combination: &SettingCombination) -> Result<Settings> {
        let mut settings = self.default.clone();
        for (key, value) in combination.iter() {
            let index = *self.by_key.get(key).ok_or_else(|| {
                MatchError::Schema(format!(
                    "assessed control {key} is not in epoch {}",
                    self.epoch
                ))
            })?;
            let value = setting_value(value);
            if self.domains[index].index_of(&value).is_none() {
                return Err(MatchError::Schema(format!(
                    "assessed value for {key} is not in its domain"
                )));
            }
            settings.set(key.clone(), value);
        }
        Ok(settings)
    }
}
