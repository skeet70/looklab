// SPDX-License-Identifier: AGPL-3.0-or-later
//! Root data for release gates and weighting (docs/05 §§6-8).

use std::collections::{BTreeMap, BTreeSet};

use looklab_color::cam16::{Cam16Model, Jmh, ViewingConditions};
use looklab_color::ucs::{UcsSpace, jmh_to_ucs};
use looklab_color::{Mat3, Rgb, RgbSpace, Xyz};
use looklab_model::LookInput;
use looklab_model::probes;
use looklab_samples::{OutputSpace, SampleSet};
use looklab_schema::SimKey;
use looklab_trust::{Region, RootDataKind, RootObservation, RootSession, RootTarget, rooted};
use looklab_upload::ContributionKind;
use sqlx::SqlitePool;

use crate::body::resolve_look;
use crate::config::WorkerConfig;
use crate::db;
use crate::error::{Result, WorkerError};
use crate::pool::LookPool;

const MAX_OBSERVATION_SAMPLES: usize = 4_000;

const MIN_PROBE_SAMPLES: usize = 3;

const EXPOSURE_TOLERANCE: f64 = 0.15;

const CHROMA_TOLERANCE: f64 = 2.5;

const HUE_TOLERANCE_DEGREES: f64 = 4.0;

#[derive(Debug, Default, Clone)]
pub struct RootWeights {
    pub anchored_weight: BTreeMap<Region, f64>,
    pub rooted: bool,
}

#[derive(Debug, Default, Clone)]
pub struct RootData {
    pub admissions: Vec<looklab_server::RootAdmissionId>,
    pub observations: Vec<RootObservation>,
    pub weights: RootWeights,
}

pub async fn load(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    pool: &LookPool,
) -> Result<RootData> {
    let active = looklab_server::admin::active_root_admissions(db_pool)
        .await
        .map_err(WorkerError::Admin)?;
    if active.is_empty() {
        return Ok(RootData::default());
    }
    let by_contribution: BTreeMap<_, _> = active
        .iter()
        .map(|admission| (&admission.contribution, admission))
        .collect();

    let rows = db::body_pool(db_pool, &pool.body).await?;
    let exclusions = db::look_credential_exclusions(db_pool, pool.look.as_str()).await?;
    let mut sessions = Vec::new();
    let mut admissions = Vec::new();
    let mut measured: BTreeMap<_, Vec<[f64; 3]>> = BTreeMap::new();

    for (contribution, images) in rows {
        if exclusions.contains(&contribution.credential) {
            continue;
        }
        let Some(admission) = by_contribution.get(&contribution.id) else {
            continue;
        };
        if contribution.intent != db::ContributionIntent::Native {
            continue;
        }
        if !matches!(
            contribution.kind,
            ContributionKind::Chart | ContributionKind::Gradient | ContributionKind::Pattern
        ) {
            continue;
        }
        if !admission.scope.covers(&pool.look, &pool.body) {
            continue;
        }
        let look = resolve_look(
            &pool.resources,
            &contribution.firmware,
            &contribution.sim_key,
            contribution.intent,
            contribution.target_look.as_ref(),
        )?;
        if look != pool.look {
            continue;
        }
        let credential = db::credential(db_pool, &contribution.credential).await?;
        let mut session_regions = BTreeSet::new();
        for image in &images {
            let path = db::blob_path(&config.blobs, &image.samples_hash);
            let bytes = std::fs::read(&path)
                .map_err(|error| WorkerError::Data(format!("{}: {error}", path.display())))?;
            let samples = looklab_samples::decode(&bytes)?;
            let transform = pool
                .transforms
                .matrix_for(None, samples.header().wb_multipliers)?;
            session_regions.extend(regions_of(&samples, &transform)?);
            measure(&samples, &transform, &mut measured)?;
        }
        admissions.push(admission.id.clone());
        sessions.push(RootSession {
            credential: credential.trust,
            tier: credential.tier,
            scope: credential.scope.clone(),
            anchored: true,
            kind: RootDataKind::Session(contribution.kind),
            target: RootTarget::Look {
                look: pool.look.clone(),
                body: pool.body.clone(),
            },
            covered_regions: session_regions,
        });
    }

    let mut anchored_weight: BTreeMap<Region, f64> = BTreeMap::new();
    for session in &sessions {
        for region in &session.covered_regions {
            *anchored_weight.entry(*region).or_insert(0.0) += 1.0;
        }
    }
    let supported: BTreeSet<Region> = pool
        .images
        .iter()
        .filter(|image| image.contribution.intent == db::ContributionIntent::Native)
        .flat_map(|image| image.coverage.regions())
        .collect();
    let is_rooted = rooted(&sessions, &supported);

    let observations = measured
        .into_iter()
        .map(|(id, values)| {
            let count = values.len() as f64;
            let mut jab = [0.0; 3];
            for value in &values {
                for (sum, channel) in jab.iter_mut().zip(value) {
                    *sum += channel;
                }
            }
            RootObservation {
                id,
                jab: jab.map(|value| value / count),
            }
        })
        .collect();

    Ok(RootData {
        admissions,
        observations,
        weights: RootWeights {
            anchored_weight,
            rooted: is_rooted,
        },
    })
}

/// The epoch and sim the body's control-response model is fitted against.
pub fn base_sim(pool: &LookPool) -> Result<(u32, SimKey)> {
    let image = pool
        .images
        .iter()
        .find(|image| image.contribution.intent == db::ContributionIntent::Native)
        .or_else(|| pool.images.first())
        .ok_or_else(|| WorkerError::Data("empty pool".to_owned()))?;
    let sim = SimKey::new(&image.contribution.sim_key)
        .map_err(|_| WorkerError::Data("invalid sim key".to_owned()))?;
    Ok((image.epoch, sim))
}

fn regions_of(samples: &SampleSet, transform: &Mat3) -> Result<Vec<Region>> {
    let mut regions = BTreeSet::new();
    for rgb in samples.in_rgb() {
        let xyz = transform.mul_vec(rgb.map(f64::from));
        if !(xyz[0].is_finite() && xyz[1].is_finite() && xyz[2].is_finite()) {
            continue;
        }
        let input = LookInput::new(Xyz::from_array(xyz));
        let s = input
            .s
            .clamp(looklab_trust::EXPOSURE_MIN, looklab_trust::EXPOSURE_MAX);
        regions.insert(Region::from_probe(s, input.h_in)?);
    }
    Ok(regions.into_iter().collect())
}

fn measure(
    samples: &SampleSet,
    transform: &Mat3,
    measured: &mut BTreeMap<looklab_model::ProbeId, Vec<[f64; 3]>>,
) -> Result<()> {
    let probe_set = probes::v1();
    let model = Cam16Model::new(&ViewingConditions::reference());
    let space = match samples.header().output_space {
        OutputSpace::Srgb => RgbSpace::Srgb,
        OutputSpace::AdobeRgb => RgbSpace::AdobeRgb,
    };
    let step = (samples.n() / MAX_OBSERVATION_SAMPLES).max(1);
    let in_rgb = samples.in_rgb();
    let out_rgb = samples.out_rgb();
    let mut matches: BTreeMap<usize, Vec<[f64; 3]>> = BTreeMap::new();
    for index in (0..samples.n()).step_by(step) {
        let xyz = transform.mul_vec(in_rgb[index].map(f64::from));
        if !(xyz[0].is_finite() && xyz[1].is_finite() && xyz[2].is_finite()) {
            continue;
        }
        let input = LookInput::new(Xyz::from_array(xyz));
        let out = space.rgb_linear_to_xyz(Rgb::from_array(out_rgb[index].map(f64::from)));
        let cam = model.xyz_to_cam16(out);
        let jab = jmh_to_ucs(
            Jmh {
                j: cam.j,
                m: cam.m,
                h: cam.h,
            },
            UcsSpace::Ucs,
        );
        for (probe_index, probe) in probe_set.probes.iter().enumerate() {
            let target = LookInput::new(probe.xyz);
            if (target.s - input.s).abs() > EXPOSURE_TOLERANCE
                || (target.c_in - input.c_in).abs() > CHROMA_TOLERANCE
            {
                continue;
            }
            if target.c_in > 1.0 {
                let mut delta = (target.h_in - input.h_in).abs() % 360.0;
                if delta > 180.0 {
                    delta = 360.0 - delta;
                }
                if delta > HUE_TOLERANCE_DEGREES {
                    continue;
                }
            }
            matches.entry(probe_index).or_default().push(jab.to_array());
        }
    }
    for (probe_index, values) in matches {
        if values.len() < MIN_PROBE_SAMPLES {
            continue;
        }
        measured
            .entry(probe_set.probes[probe_index].id.clone())
            .or_default()
            .extend(values);
    }
    Ok(())
}
