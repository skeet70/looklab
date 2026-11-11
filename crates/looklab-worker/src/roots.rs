// SPDX-License-Identifier: AGPL-3.0-or-later
//! Root data for release gates and weighting (docs/05 §§6-8).

use std::collections::{BTreeMap, BTreeSet};

use looklab_color::cam16::{Cam16Model, Jmh, ViewingConditions};
use looklab_color::ucs::{UcsSpace, jmh_to_ucs};
use looklab_color::{Mat3, Rgb, RgbSpace, Xyz};
use looklab_model::LookInput;
use looklab_model::probes;
use looklab_samples::{OutputSpace, SampleSet};
use looklab_trust::{Region, RootDataKind, RootObservation, RootSession, RootTarget, rooted};
use looklab_upload::ContributionKind;

use crate::db::ContributionIntent;
use crate::error::Result;
use crate::pool::{EvidenceRole, LookPool};

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

pub fn load(pool: &LookPool, scores: &crate::weights::ConsistencyScores) -> Result<RootData> {
    load_family(std::slice::from_ref(pool), scores)
}

/// Root sessions and observations over the eligible realization pools.
pub fn load_family(
    pools: &[LookPool],
    scores: &crate::weights::ConsistencyScores,
) -> Result<RootData> {
    if pools.is_empty() {
        return Ok(RootData::default());
    }
    let active =
        crate::pool::EvidenceObservation::merge(pools.iter().map(|pool| &pool.observation))?
            .root_admissions;
    if active.is_empty() {
        return Ok(RootData::default());
    }
    let by_contribution: BTreeMap<_, _> = active
        .iter()
        .map(|admission| (&admission.contribution, admission))
        .collect();

    let mut pending: BTreeMap<
        looklab_server::ContributionId,
        (
            BTreeSet<Region>,
            &crate::pool::PoolImage,
            &looklab_server::admin::ActiveRootAdmission,
            &LookPool,
        ),
    > = BTreeMap::new();
    let mut measured: BTreeMap<_, Vec<[f64; 3]>> = BTreeMap::new();
    let mut seen_pools = BTreeSet::new();

    for pool in pools {
        if !seen_pools.insert((pool.body.clone(), pool.realization.clone())) {
            continue;
        }
        for image in &pool.images {
            if image.role != EvidenceRole::Native
                || !image.is_eligible()
                || image.contribution.intent != ContributionIntent::Native
                || scores.effective(&image.image.id, image.outcome)
                    == Some(crate::db::ImageOutcome::Quarantined)
            {
                continue;
            }
            if !matches!(
                image.contribution.kind,
                ContributionKind::Chart | ContributionKind::Gradient | ContributionKind::Pattern
            ) {
                continue;
            }
            let Some(admission) = by_contribution.get(&image.contribution.id) else {
                continue;
            };
            if !admission.scope.covers(&pool.look, &pool.body) {
                continue;
            }
            measure(
                &image.samples,
                &image.transform,
                &image.admitted,
                &mut measured,
            )?;
            let entry = pending
                .entry(image.contribution.id.clone())
                .or_insert_with(|| (BTreeSet::new(), image, admission, pool));
            entry.0.extend(image.coverage.regions());
        }
    }

    let mut sessions = Vec::new();
    let mut admissions = Vec::new();
    for (_, (covered_regions, image, admission, pool)) in pending {
        admissions.push(admission.id.clone());
        sessions.push(RootSession {
            credential: image.credential.trust,
            tier: image.credential.tier,
            scope: image.credential.scope.clone(),
            anchored: true,
            kind: RootDataKind::Session(image.contribution.kind),
            target: RootTarget::Look {
                look: pool.look.clone(),
                body: pool.body.clone(),
            },
            covered_regions,
        });
    }

    let mut anchored_weight: BTreeMap<Region, f64> = BTreeMap::new();
    for session in &sessions {
        for region in &session.covered_regions {
            *anchored_weight.entry(*region).or_insert(0.0) += 1.0;
        }
    }
    let supported: BTreeSet<Region> = pools
        .iter()
        .flat_map(|pool| pool.images.iter())
        .filter(|image| {
            image.role == EvidenceRole::Native
                && image.is_eligible()
                && scores.effective(&image.image.id, image.outcome)
                    != Some(crate::db::ImageOutcome::Quarantined)
        })
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

fn measure(
    samples: &SampleSet,
    transform: &Mat3,
    admitted: &[bool],
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
        if !admitted.get(index).copied().unwrap_or(false) {
            continue;
        }
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
