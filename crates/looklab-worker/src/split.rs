// SPDX-License-Identifier: AGPL-3.0-or-later
//! Firmware-epoch split proposals gated by the predicted ΔE between firmware
//! offsets (docs/02 §4.5).

use std::collections::BTreeMap;

use looklab_color::cam16::{Cam16Model, Jmh, ViewingConditions};
use looklab_color::ucs::{UcsSpace, jmh_to_ucs};
use looklab_color::{Rgb, RgbSpace, Xyz};
use looklab_fit::epoch::{PROJECTION_DIM, SplitDecision};
use looklab_fit::{
    FitResult, ImageProjection, SampleProjection, SplitOptions, SplitSample,
    detect_split_with_effect, firmware_probe_basis, predicted_effect, project_images,
};
use looklab_model::LookInput;
use looklab_schema::{FirmwareVersion, ProposalKind};
use looklab_server::admin::{self, WorkerProposal};
use looklab_trust::Region;
use sqlx::SqlitePool;

use crate::config::WorkerConfig;
use crate::db::ContributionIntent;
use crate::error::{Result, WorkerError};
use crate::pool::{LookPool, PoolImage};

const MAX_RESIDUAL_SAMPLES: usize = 1_000;

const MIN_IMAGES_PER_GROUP: usize = 3;

pub async fn detect_and_propose(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    pool: &LookPool,
    result: &FitResult,
) -> Result<()> {
    let native: Vec<&PoolImage> = pool
        .images
        .iter()
        .filter(|image| image.contribution.intent == ContributionIntent::Native)
        .collect();
    if native.len() < 2 * MIN_IMAGES_PER_GROUP {
        return Ok(());
    }
    let mut firmwares: Vec<FirmwareVersion> = native
        .iter()
        .filter_map(|image| image.contribution.firmware.parse().ok())
        .collect();
    firmwares.sort();
    firmwares.dedup();
    if firmwares.len() < 2 {
        return Ok(());
    }
    let probe_set = looklab_model::probes::v1();
    let basis = firmware_probe_basis(&result.look);
    let probe_regions: Vec<Option<Region>> = probe_set
        .probes
        .iter()
        .map(|probe| region_of(probe.xyz))
        .collect();
    let supported: Vec<usize> = result
        .probes
        .probes
        .iter()
        .enumerate()
        .filter(|(_, probe)| probe.supported)
        .map(|(index, _)| index)
        .filter(|index| *index < basis.len())
        .collect();
    if supported.is_empty() {
        return Ok(());
    }
    let model = Cam16Model::new(&ViewingConditions::reference());
    let mut rows: Vec<(&PoolImage, [f64; 2])> = Vec::new();
    let mut groups: Vec<u32> = Vec::new();
    for image in native {
        let Some(group) = image
            .contribution
            .firmware
            .parse()
            .ok()
            .and_then(|firmware| firmwares.iter().position(|known| *known == firmware))
            .and_then(|index| u32::try_from(index).ok())
        else {
            continue;
        };
        let Some(projection) =
            project_residual(&model, image, result, &basis, &probe_regions, &supported)
        else {
            continue;
        };
        rows.push((image, projection));
        groups.push(group);
    }
    if rows.len() < 2 * MIN_IMAGES_PER_GROUP {
        return Ok(());
    }
    let mut counts: BTreeMap<u32, usize> = BTreeMap::new();
    for group in &groups {
        *counts.entry(*group).or_default() += 1;
    }
    let offsets = group_offsets(&groups, &rows);
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
            let samples: Vec<SplitSample> = rows
                .iter()
                .zip(&groups)
                .filter(|(_, group)| **group == left_group || **group == right_group)
                .map(|((_, projection), group)| SplitSample {
                    group: *group,
                    projection: *projection,
                    effect_de: predicted_effect(&basis, *projection, [0.0; PROJECTION_DIM]),
                })
                .collect();
            let Some(decision) = detect_split_with_effect(&samples, &options, effect) else {
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
    let left_epoch = epoch_for_group(&groups, &rows, left, pool)?;
    let right_epoch = epoch_for_group(&groups, &rows, right, pool)?;
    if left_epoch != right_epoch {
        return Ok(());
    }
    let left_firmware = firmwares.get(left as usize);
    let right_firmware = firmwares.get(right as usize);
    let (Some(left_firmware), Some(right_firmware)) = (left_firmware, right_firmware) else {
        return Ok(());
    };
    let split_at = left_firmware.max(right_firmware);
    let new_epoch = pool
        .resources
        .schema
        .epochs
        .iter()
        .map(|epoch| epoch.id)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let payload = serde_norway::to_string(&SplitPayload {
        body: pool.body.as_str(),
        from_epoch: left_epoch,
        new_epoch,
        split_at: split_at.as_str(),
    })?;
    let evidence = serde_json::json!({
        "source": "worker",
        "look": pool.look.as_str(),
        "body": pool.body.as_str(),
        "from_firmware": left_firmware.as_str(),
        "to_firmware": right_firmware.as_str(),
        "from_epoch": left_epoch,
        "p_value": decision.p_value,
        "effect_de": decision.effect_de,
        "t_squared": decision.t_squared,
        "images_from": counts.get(&left).copied().unwrap_or(0),
        "images_to": counts.get(&right).copied().unwrap_or(0),
    });
    admin::submit_worker_proposal(
        db_pool,
        WorkerProposal {
            kind: ProposalKind::EpochSplit,
            payload,
            evidence,
        },
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

/// Least-squares firmware offset of one image.
fn project_residual(
    model: &Cam16Model,
    image: &PoolImage,
    result: &FitResult,
    basis: &[[[f64; 3]; PROJECTION_DIM]],
    probe_regions: &[Option<Region>],
    supported: &[usize],
) -> Option<[f64; PROJECTION_DIM]> {
    let space = match image.samples.header().output_space {
        looklab_samples::OutputSpace::Srgb => RgbSpace::Srgb,
        looklab_samples::OutputSpace::AdobeRgb => RgbSpace::AdobeRgb,
    };
    let mut slots: BTreeMap<Region, usize> = BTreeMap::new();
    for (slot, probe_index) in supported.iter().enumerate() {
        if let Some(region) = probe_regions.get(*probe_index).copied().flatten() {
            slots.entry(region).or_insert(slot);
        }
    }
    if slots.is_empty() {
        return None;
    }
    let step = (image.samples.n() / MAX_RESIDUAL_SAMPLES).max(1);
    let mut sums = vec![[0.0_f64; 3]; supported.len()];
    let mut counts = vec![0_usize; supported.len()];
    let in_rgb = image.samples.in_rgb();
    let out_rgb = image.samples.out_rgb();
    for index in (0..image.samples.n()).step_by(step) {
        let xyz = image.transform.mul_vec(in_rgb[index].map(f64::from));
        if !(xyz[0].is_finite() && xyz[1].is_finite() && xyz[2].is_finite()) {
            continue;
        }
        let input = LookInput::new(Xyz::from_array(xyz));
        let s = input
            .s
            .clamp(looklab_trust::EXPOSURE_MIN, looklab_trust::EXPOSURE_MAX);
        let Ok(region) = Region::from_probe(s, input.h_in) else {
            continue;
        };
        let Some(slot) = slots.get(&region) else {
            continue;
        };
        let predicted = result.look.eval(Xyz::from_array(xyz)).to_array();
        let measured = ucs_of_xyz(
            model,
            space.rgb_linear_to_xyz(Rgb::from_array(out_rgb[index].map(f64::from))),
        );
        for channel in 0..3 {
            sums[*slot][channel] += predicted[channel] - measured[channel];
        }
        counts[*slot] += 1;
    }
    let samples: Vec<SampleProjection> = supported
        .iter()
        .enumerate()
        .filter(|(slot, _)| counts[*slot] > 0)
        .map(|(slot, probe_index)| SampleProjection {
            residual: sums[slot].map(|value| value / counts[slot] as f64),
            basis: basis[*probe_index],
            weight: counts[slot] as f64,
        })
        .collect();
    if samples.is_empty() {
        return None;
    }
    let projected = project_images(&[ImageProjection { group: 0, samples }]);
    projected.first().map(|sample| sample.projection)
}

/// The fitted offset of each firmware group: the mean of its images' projections.
fn group_offsets(
    groups: &[u32],
    rows: &[(&PoolImage, [f64; PROJECTION_DIM])],
) -> BTreeMap<u32, [f64; PROJECTION_DIM]> {
    let mut sums: BTreeMap<u32, ([f64; PROJECTION_DIM], usize)> = BTreeMap::new();
    for ((_, projection), group) in rows.iter().zip(groups) {
        let entry = sums.entry(*group).or_default();
        for (sum, value) in entry.0.iter_mut().zip(projection) {
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

fn ucs_of_xyz(model: &Cam16Model, xyz: Xyz) -> [f64; 3] {
    let cam = model.xyz_to_cam16(xyz);
    jmh_to_ucs(
        Jmh {
            j: cam.j,
            m: cam.m,
            h: cam.h,
        },
        UcsSpace::Ucs,
    )
    .to_array()
}

fn region_of(xyz: Xyz) -> Option<Region> {
    let input = LookInput::new(xyz);
    Region::from_probe(
        input
            .s
            .clamp(looklab_trust::EXPOSURE_MIN, looklab_trust::EXPOSURE_MAX),
        input.h_in,
    )
    .ok()
}

fn epoch_for_group(
    groups: &[u32],
    rows: &[(&PoolImage, [f64; PROJECTION_DIM])],
    group: u32,
    pool: &LookPool,
) -> Result<u32> {
    let image = rows
        .iter()
        .zip(groups)
        .find(|(_, candidate)| **candidate == group)
        .map(|(row, _)| row.0)
        .ok_or_else(|| WorkerError::Data("split group is unknown".to_owned()))?;
    Ok(crate::body::epoch_for(&pool.resources.schema, &image.contribution.firmware)?.id)
}
