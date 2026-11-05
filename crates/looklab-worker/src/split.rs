// SPDX-License-Identifier: AGPL-3.0-or-later
//! Firmware-epoch split proposals from per-image residual projections (docs/02 §4.5).

use std::collections::BTreeSet;

use looklab_color::cam16::{Cam16Model, Jmh, ViewingConditions};
use looklab_color::ucs::{UcsSpace, jmh_to_ucs};
use looklab_color::{Rgb, RgbSpace, Xyz};
use looklab_fit::FitResult;
use looklab_fit::epoch::{SplitOptions, SplitSample, detect_split};
use looklab_model::LookInput;
use looklab_schema::{FirmwareVersion, ProposalKind};
use looklab_server::admin::{self, WorkerProposal};
use looklab_trust::Region;
use sqlx::SqlitePool;

use crate::config::WorkerConfig;
use crate::db::ContributionIntent;
use crate::error::Result;
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
    let probe_xyz: std::collections::HashMap<String, Xyz> = looklab_model::probes::v1()
        .probes
        .into_iter()
        .map(|probe| (probe.id.as_str().to_owned(), probe.xyz))
        .collect();
    let supported: BTreeSet<Region> = result
        .probes
        .ids
        .iter()
        .zip(&result.probes.probes)
        .filter(|(_, probe)| probe.supported)
        .filter_map(|(id, _)| probe_xyz.get(id.as_str()).and_then(|xyz| region_of(*xyz)))
        .collect();
    let model = Cam16Model::new(&ViewingConditions::reference());
    let mut rows = Vec::new();
    for image in native {
        if let Some(mean) = mean_residual(&model, image, result, &supported) {
            rows.push((image, mean));
        }
    }
    if rows.len() < 2 * MIN_IMAGES_PER_GROUP {
        return Ok(());
    }
    let groups: Vec<u32> = rows
        .iter()
        .filter_map(|(image, _)| {
            image
                .contribution
                .firmware
                .parse()
                .ok()
                .and_then(|firmware| firmwares.iter().position(|known| *known == firmware))
                .and_then(|index| u32::try_from(index).ok())
        })
        .collect();
    let axes = principal_axes(&rows.iter().map(|(_, mean)| mean.mean).collect::<Vec<_>>());
    let shift = firmware_axis(&rows, &groups);
    let build = |first: usize, second: usize| -> Vec<SplitSample> {
        groups
            .iter()
            .zip(&rows)
            .map(|(group, (_, mean))| SplitSample {
                group: *group,
                projection: [
                    dot(&axes[first], &mean.mean),
                    dot(&axes[second], &mean.mean),
                ],
                effect_de: dot(&mean.mean, &shift),
            })
            .collect()
    };
    let samples = build(0, 1);
    let options = SplitOptions {
        seed: config.seed,
        ..SplitOptions::default()
    };
    let decision = detect_split(&samples, &options).or_else(|| {
        // A nearly collinear group can make one projection pair singular.
        let alternative = build(1, 2);
        detect_split(&alternative, &options)
    });
    let Some(decision) = decision else {
        return Ok(());
    };
    let (left, right) = decision.groups;
    let left_epoch = epoch_for_group(&rows, &groups, left, pool)?;
    let right_epoch = epoch_for_group(&rows, &groups, right, pool)?;
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
    let counts = |group: u32| {
        groups
            .iter()
            .filter(|candidate| **candidate == group)
            .count()
    };
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
        "images_from": counts(left),
        "images_to": counts(right),
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
    .map_err(crate::error::WorkerError::Admin)?;
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

struct MeanResidual {
    mean: [f64; 3],
}

/// The direction between the two firmware groups.
fn firmware_axis(rows: &[(&PoolImage, MeanResidual)], groups: &[u32]) -> [f64; 3] {
    let mut ordered: Vec<u32> = groups.to_vec();
    ordered.sort_unstable();
    ordered.dedup();
    let mut means = Vec::new();
    for group in ordered {
        let count = groups
            .iter()
            .filter(|candidate| **candidate == group)
            .count();
        if count < MIN_IMAGES_PER_GROUP {
            continue;
        }
        let mut mean = [0.0_f64; 3];
        for ((_, residual), candidate) in rows.iter().zip(groups) {
            if *candidate == group {
                for (sum, channel) in mean.iter_mut().zip(residual.mean) {
                    *sum += channel;
                }
            }
        }
        means.push(mean.map(|value| value / count as f64));
        if means.len() == 2 {
            break;
        }
    }
    if means.len() < 2 {
        return [1.0, 0.0, 0.0];
    }
    normalize([
        means[1][0] - means[0][0],
        means[1][1] - means[0][1],
        means[1][2] - means[0][2],
    ])
}

fn mean_residual(
    model: &Cam16Model,
    image: &PoolImage,
    result: &FitResult,
    supported: &BTreeSet<Region>,
) -> Option<MeanResidual> {
    let space = match image.samples.header().output_space {
        looklab_samples::OutputSpace::Srgb => RgbSpace::Srgb,
        looklab_samples::OutputSpace::AdobeRgb => RgbSpace::AdobeRgb,
    };
    let step = (image.samples.n() / MAX_RESIDUAL_SAMPLES).max(1);
    let mut sum = [0.0; 3];
    let mut count = 0.0;
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
        if !supported.contains(&region) {
            continue;
        }
        let predicted = result.look.eval(Xyz::from_array(xyz)).to_array();
        let measured = ucs_of_xyz(
            model,
            space.rgb_linear_to_xyz(Rgb::from_array(out_rgb[index].map(f64::from))),
        );
        for channel in 0..3 {
            sum[channel] += predicted[channel] - measured[channel];
        }
        count += 1.0;
    }
    if count < 2.0 {
        return None;
    }
    Some(MeanResidual {
        mean: sum.map(|value| value / count),
    })
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

fn dot(left: &[f64; 3], right: &[f64; 3]) -> f64 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
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
    rows: &[(&PoolImage, MeanResidual)],
    groups: &[u32],
    group: u32,
    pool: &LookPool,
) -> Result<u32> {
    let index = groups
        .iter()
        .position(|candidate| *candidate == group)
        .ok_or_else(|| crate::error::WorkerError::Data("split group is unknown".to_owned()))?;
    let image = rows[index].0;
    Ok(crate::body::epoch_for(&pool.resources.schema, &image.contribution.firmware)?.id)
}

/// Three orthonormal axes of the image mean-residual cloud.
fn principal_axes(vectors: &[[f64; 3]]) -> [[f64; 3]; 3] {
    if vectors.len() < 2 {
        return [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    }
    let mut mean = [0.0; 3];
    for vector in vectors {
        for channel in 0..3 {
            mean[channel] += vector[channel];
        }
    }
    for value in &mut mean {
        *value /= vectors.len() as f64;
    }
    let mut covariance = [[0.0_f64; 3]; 3];
    for vector in vectors {
        let delta = [
            vector[0] - mean[0],
            vector[1] - mean[1],
            vector[2] - mean[2],
        ];
        for row in 0..3 {
            for column in 0..3 {
                covariance[row][column] += delta[row] * delta[column];
            }
        }
    }
    let first = power_iteration(&covariance, [1.0, 0.37, 0.19]);
    let eigenvalue = dot(&first, &mat_vec(&covariance, &first));
    let mut deflated = covariance;
    for row in 0..3 {
        for column in 0..3 {
            deflated[row][column] -= eigenvalue * first[row] * first[column];
        }
    }
    let second = power_iteration(&deflated, [0.23, 1.0, 0.41]);
    let third = normalize([
        first[1] * second[2] - first[2] * second[1],
        first[2] * second[0] - first[0] * second[2],
        first[0] * second[1] - first[1] * second[0],
    ]);
    [first, second, third]
}

fn mat_vec(matrix: &[[f64; 3]; 3], vector: &[f64; 3]) -> [f64; 3] {
    let mut out = [0.0; 3];
    for row in 0..3 {
        for column in 0..3 {
            out[row] += matrix[row][column] * vector[column];
        }
    }
    out
}

fn power_iteration(matrix: &[[f64; 3]; 3], start: [f64; 3]) -> [f64; 3] {
    let mut vector = normalize(start);
    for _ in 0..64 {
        let next = normalize(mat_vec(matrix, &vector));
        if dot(&next, &vector) < 0.0 {
            vector = next.map(|value| -value);
        } else {
            vector = next;
        }
    }
    vector
}

fn normalize(vector: [f64; 3]) -> [f64; 3] {
    let length = dot(&vector, &vector).sqrt();
    if length <= f64::EPSILON {
        [0.0, 0.0, 0.0]
    } else {
        vector.map(|value| value / length)
    }
}
