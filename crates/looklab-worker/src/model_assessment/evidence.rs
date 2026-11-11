// SPDX-License-Identifier: AGPL-3.0-or-later
//! Candidate evidence selection and the shared held-out group partition.

use std::collections::BTreeSet;

use looklab_model::realization::Settings;
use looklab_schema::{
    AssessedPoint, BodyRealizationVersionId, CellId, SettingCombination, SettingValue,
};
use looklab_server::model_store::{EvidenceGroupId, EvidenceMembership, EvidenceRole};
use sqlx::SqlitePool;

use crate::config::WorkerConfig;
use crate::error::{Result, WorkerError};
use crate::pool::{self, EvidenceScope, LookPool, PhysicalContext, PoolImage};

pub(crate) struct EvidenceImage {
    pub pool_index: usize,
    pub group: EvidenceGroupId,
    pub context: PhysicalContext,
    pub settings: Settings,
    pub combination: SettingCombination,
}

impl EvidenceImage {
    pub(crate) fn point(&self) -> AssessedPoint {
        AssessedPoint {
            settings: self.combination.clone(),
            firmware: Some(self.context.firmware.clone()),
            wb_mode: Some(self.context.wb_mode.clone()),
        }
    }
}

pub(crate) struct Evidence {
    pub pool: LookPool,
    pub images: Vec<EvidenceImage>,
    pub assessment_groups: BTreeSet<EvidenceGroupId>,
    pub training_groups: BTreeSet<EvidenceGroupId>,
}

pub(crate) fn context_of(image: &PoolImage) -> Option<PhysicalContext> {
    pool::physical_context(image)
}

pub(crate) fn pinned_modes(evidence: &Evidence) -> crate::assessment::PinnedModes {
    let mut pinned = crate::assessment::PinnedModes::default();
    for image in &evidence.images {
        if !pinned.firmware.contains(&image.context.firmware) {
            pinned.firmware.push(image.context.firmware.clone());
        }
        if !pinned.wb_modes.contains(&image.context.wb_mode) {
            pinned.wb_modes.push(image.context.wb_mode.clone());
        }
    }
    pinned
}

pub(crate) fn points(evidence: &Evidence) -> Vec<AssessedPoint> {
    points_of(&evidence.images)
}

pub(crate) fn points_of<'a>(
    images: impl IntoIterator<Item = &'a EvidenceImage>,
) -> Vec<AssessedPoint> {
    let mut points = Vec::new();
    for image in images {
        let point = image.point();
        if !points.contains(&point) {
            points.push(point);
        }
    }
    points.sort();
    points
}

/// The look-affecting controls the image actually recorded.
pub(crate) fn complete_combination(
    image: &PoolImage,
    epoch: &looklab_schema::Epoch,
) -> Option<SettingCombination> {
    let mut combination = SettingCombination::new();
    for control in &epoch.controls {
        if !control.look_affecting {
            continue;
        }
        let value = image.settings.get(&control.key)?;
        combination.insert(control.key.clone(), setting_value(value));
    }
    Some(combination)
}

fn setting_value(value: &looklab_model::realization::ControlValue) -> SettingValue {
    match value {
        looklab_model::realization::ControlValue::Step(step) => SettingValue::Step(*step),
        looklab_model::realization::ControlValue::Enum(value) => SettingValue::Enum(value.clone()),
        looklab_model::realization::ControlValue::Bool(value) => SettingValue::Bool(*value),
    }
}

pub(crate) fn allowed_contributions(
    membership: Option<&EvidenceMembership>,
) -> BTreeSet<looklab_server::ContributionId> {
    let Some(membership) = membership else {
        return BTreeSet::new();
    };
    membership
        .roles
        .iter()
        .filter_map(|assignment| {
            looklab_server::ContributionId::try_from(assignment.contribution.as_str().to_owned())
                .ok()
        })
        .collect()
}

fn allowed_images(membership: Option<&EvidenceMembership>) -> BTreeSet<String> {
    membership
        .map(|membership| {
            membership
                .blobs
                .iter()
                .map(|blob| blob.image.as_str().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) async fn load(
    pool: &SqlitePool,
    config: &WorkerConfig,
    candidate: &BodyRealizationVersionId,
    membership: Option<&EvidenceMembership>,
    reassess_selected: bool,
) -> Result<Evidence> {
    let look = looklab_upload::LookSlug::new(candidate.target.family.as_str())
        .map_err(|_| WorkerError::Data("candidate family is not a Look slug".to_owned()))?;
    let look_pool = pool::build(
        pool,
        config,
        &candidate.realization.body,
        &look,
        &candidate.realization,
        &EvidenceScope::Experimental,
    )
    .await?;
    let allowed = allowed_contributions(membership);
    let allowed_images = allowed_images(membership);
    let epoch = look_pool
        .resources
        .schema
        .epochs
        .iter()
        .find(|epoch| epoch.id == candidate.realization.epoch.get())
        .ok_or_else(|| {
            WorkerError::Data(format!(
                "epoch {} is not in the body schema",
                candidate.realization.epoch.get()
            ))
        })?;
    let mut images = Vec::new();
    for (pool_index, image) in look_pool.images.iter().enumerate() {
        if !image.is_eligible() || !image.admitted.iter().any(|admitted| *admitted) {
            continue;
        }
        if !reassess_selected && !allowed.is_empty() && !allowed.contains(&image.contribution.id) {
            continue;
        }
        if !reassess_selected
            && !allowed_images.is_empty()
            && !allowed_images.contains(&image.image.id.as_str())
        {
            continue;
        }
        let Some(context) = context_of(image) else {
            continue;
        };
        let Some(combination) = complete_combination(image, epoch) else {
            continue;
        };
        let group = crate::model_fit::evidence_group(image)?;
        images.push(EvidenceImage {
            pool_index,
            group,
            context,
            settings: image.settings.clone(),
            combination,
        });
    }
    let groups: BTreeSet<EvidenceGroupId> =
        images.iter().map(|image| image.group.clone()).collect();
    let group_list: Vec<EvidenceGroupId> = groups.iter().cloned().collect();
    let training_groups: BTreeSet<EvidenceGroupId> = if let Some(membership) = membership {
        membership
            .groups
            .iter()
            .filter(|group| group.role == EvidenceRole::Training)
            .map(|group| group.group.clone())
            .collect()
    } else {
        let assessment = crate::model_fit::assessment_partition(
            &looklab_schema::SubjectKey::for_realization(&candidate.realization),
            &group_list,
        );
        groups
            .iter()
            .filter(|group| !assessment.contains(*group))
            .cloned()
            .collect()
    };
    let assessment_groups: BTreeSet<EvidenceGroupId> = if reassess_selected {
        groups
            .iter()
            .filter(|group| !training_groups.contains(*group))
            .cloned()
            .collect()
    } else if let Some(membership) = membership
        && membership
            .groups
            .iter()
            .any(|group| group.role == EvidenceRole::Assessment)
    {
        membership
            .groups
            .iter()
            .filter(|group| group.role == EvidenceRole::Assessment)
            .map(|group| group.group.clone())
            .collect()
    } else {
        crate::model_fit::assessment_partition(
            &looklab_schema::SubjectKey::for_realization(&candidate.realization),
            &group_list,
        )
    };
    Ok(Evidence {
        pool: look_pool,
        images,
        assessment_groups,
        training_groups,
    })
}

pub(crate) fn cell_for_scene(scene: looklab_color::Xyz) -> Option<CellId> {
    let input = looklab_model::LookInput::new(scene);
    CellId::new(format!("cell-{}", looklab_fit::model_error::cell(&input))).ok()
}
