// SPDX-License-Identifier: AGPL-3.0-or-later
//! Firmware-epoch split proposals gated by the predicted ΔE between firmware
//! offsets (docs/02 §4.5).

use std::collections::BTreeMap;

use looklab_fit::epoch::{PROJECTION_DIM, SplitDecision, SplitSample};
use looklab_fit::{
    FirmwareId, ImageProjection, SplitOptions, detect_split_with_effect, firmware_probe_basis,
    predicted_effect, project_images,
};
use looklab_model::Look;
use looklab_schema::{
    ControlEpochId, DependencyGeneration, DependencyRef, FamilyTargetId, FirmwareVersion,
    ProposalKind,
};
use looklab_server::admin::{self, WorkerProposal};
use looklab_server::model_store::JobLease;
use sqlx::SqlitePool;

use crate::config::WorkerConfig;
use crate::error::{Result, WorkerError};
use crate::jobs::JobOwnership;
use crate::pool::LookPool;

const MIN_IMAGES_PER_GROUP: usize = 3;

/// The firmware projections of one body realization and the target they were fitted against.
pub struct FirmwareProjection<'a> {
    pub pool: &'a LookPool,
    pub target: &'a FamilyTargetId,
    pub look: &'a Look,
    pub firmware: &'a [FirmwareId],
    pub projections: &'a [ImageProjection],
}

pub async fn detect_and_propose(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    ownership: &JobOwnership,
    input: FirmwareProjection<'_>,
    dependencies: &[DependencyRef],
    snapshot: &[DependencyGeneration],
) -> Result<()> {
    if input.firmware.len() != input.projections.len() {
        return Err(WorkerError::Data(
            "firmware tags and projections must have equal length".to_owned(),
        ));
    }
    if input.target.family.as_str() != input.pool.look.as_str() {
        return Err(WorkerError::Data(
            "split target does not match the realization family".to_owned(),
        ));
    }
    let epoch = ControlEpochId::new(input.pool.realization.epoch.get());
    if input
        .firmware
        .iter()
        .any(|firmware| firmware.body != input.pool.body || firmware.epoch != epoch)
    {
        return Err(WorkerError::Data(
            "split projections must belong to one body realization".to_owned(),
        ));
    }
    let mut firmwares: Vec<FirmwareId> = input.firmware.to_vec();
    firmwares.sort();
    firmwares.dedup();
    if firmwares.len() < 2 || input.projections.len() < 2 * MIN_IMAGES_PER_GROUP {
        return Ok(());
    }
    let samples = project_images(input.projections);
    if samples.len() < 2 * MIN_IMAGES_PER_GROUP {
        return Ok(());
    }
    let mut counts: BTreeMap<u32, usize> = BTreeMap::new();
    for sample in &samples {
        *counts.entry(sample.group).or_default() += 1;
    }
    let offsets = group_offsets(&samples);
    let basis = firmware_probe_basis(input.look);
    let options = SplitOptions {
        seed: config.seed,
        ..SplitOptions::default()
    };
    let mut best: Option<SplitDecision> = None;
    for index in 0..firmwares.len() {
        for other in (index + 1)..firmwares.len() {
            let (Ok(left_group), Ok(right_group)) = (u32::try_from(index), u32::try_from(other))
            else {
                continue;
            };
            if counts.get(&left_group).copied().unwrap_or(0) < MIN_IMAGES_PER_GROUP
                || counts.get(&right_group).copied().unwrap_or(0) < MIN_IMAGES_PER_GROUP
            {
                continue;
            }
            let (Some(left_offset), Some(right_offset)) =
                (offsets.get(&left_group), offsets.get(&right_group))
            else {
                continue;
            };
            let effect = predicted_effect(&basis, *left_offset, *right_offset);
            if effect < options.min_effect_de {
                continue;
            }
            let pair: Vec<SplitSample> = samples
                .iter()
                .filter(|sample| sample.group == left_group || sample.group == right_group)
                .copied()
                .collect();
            let Some(decision) = detect_split_with_effect(&pair, &options, effect) else {
                continue;
            };
            let better = best
                .as_ref()
                .is_none_or(|current| decision.p_value < current.p_value);
            if better {
                best = Some(decision);
            }
        }
    }
    let Some(decision) = best else {
        return Ok(());
    };
    let (left, right) = decision.groups;
    let (Some(left_firmware), Some(right_firmware)) =
        (firmwares.get(left as usize), firmwares.get(right as usize))
    else {
        return Ok(());
    };
    let split_at: FirmwareVersion = left_firmware
        .version
        .clone()
        .max(right_firmware.version.clone());
    let from_epoch = input.pool.realization.epoch.get();
    let new_epoch = input
        .pool
        .resources
        .schema
        .epochs
        .iter()
        .map(|epoch| epoch.id)
        .max()
        .unwrap_or(0)
        .max(from_epoch)
        .saturating_add(1);
    let payload = serde_norway::to_string(&SplitPayload {
        body: input.pool.body.as_str(),
        from_epoch,
        new_epoch,
        split_at: split_at.as_str(),
    })?;
    let sources = crate::source_identity::sources();
    let evidence = serde_json::json!({
        "source": "worker",
        "look": input.pool.look.as_str(),
        "target_version": input.target.version.get(),
        "body": input.pool.body.as_str(),
        "realization": {
            "epoch": from_epoch,
            "native_sim": input.pool.realization.native_sim.as_str(),
        },
        "from_firmware": left_firmware.version.as_str(),
        "to_firmware": right_firmware.version.as_str(),
        "from_epoch": from_epoch,
        "p_value": decision.p_value,
        "effect_de": decision.effect_de,
        "t_squared": decision.t_squared,
        "images_from": counts.get(&left).copied().unwrap_or(0),
        "images_to": counts.get(&right).copied().unwrap_or(0),
        "sources": {
            "fitter": sources.fitter.as_str(),
            "evaluator": sources.evaluator.as_str(),
            "model": sources.model.as_str(),
        },
    });
    let server_job = ownership.server_id();
    let sources = crate::source_identity::sources().storage();
    let lease = JobLease {
        job: &server_job,
        attempts: ownership.attempts,
    };
    admin::submit_worker_proposal_fenced(
        db_pool,
        WorkerProposal {
            kind: ProposalKind::EpochSplit,
            payload,
            evidence,
        },
        lease,
        &sources,
        dependencies,
        snapshot,
    )
    .await
    .map_err(WorkerError::Admin)?;
    Ok(())
}

struct SplitPayload<'a> {
    body: &'a str,
    from_epoch: u32,
    new_epoch: u32,
    split_at: &'a str,
}

impl serde::Serialize for SplitPayload<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> core::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("EpochSplitProposal", 4)?;
        state.serialize_field("body", self.body)?;
        state.serialize_field("from_epoch", &self.from_epoch)?;
        state.serialize_field("new_epoch", &self.new_epoch)?;
        state.serialize_field("split_at", self.split_at)?;
        state.end()
    }
}

/// The fitted offset of each firmware group: the mean of its images' projections.
fn group_offsets(samples: &[SplitSample]) -> BTreeMap<u32, [f64; PROJECTION_DIM]> {
    let mut sums: BTreeMap<u32, ([f64; PROJECTION_DIM], usize)> = BTreeMap::new();
    for sample in samples {
        let entry = sums.entry(sample.group).or_default();
        for (sum, value) in entry.0.iter_mut().zip(sample.projection) {
            *sum += value;
        }
        entry.1 += 1;
    }
    sums.into_iter()
        .map(|(group, (sum, count))| {
            let count = count as f64;
            (group, sum.map(|value| value / count))
        })
        .collect()
}
