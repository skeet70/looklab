// SPDX-License-Identifier: AGPL-3.0-or-later
//! Match bundle assembly for a new Look release (docs/03 §5, docs/04 §2).
//! Each matchable version fixes one bundle per body epoch in its manifest.

use std::collections::BTreeMap;

use looklab_publish::{
    ArtifactBlob, ExcludedSim, MatchBundle, MatchSim, ProvenanceSummary, SimExclusion,
    body_artifact, bundle_artifact, sim_artifacts, transform_artifact,
};
use looklab_schema::{Body, BodyId, BodyKind, BodyStatus, Epoch, SimKey};
use looklab_server::admin::{self, ListingState, LookListingRecord, ManifestArtifact};
use looklab_upload::LookSlug;
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};

use crate::body::{BodyResources, TransformDir};
use crate::config::WorkerConfig;
use crate::db;
use crate::error::{Result, WorkerError};
use crate::refit::{CRM_ARTIFACT, LOOK_ARTIFACT};

/// The Look version being published, whose artifacts are not stored yet.
pub(crate) struct Candidate<'a> {
    pub body: &'a BodyId,
    pub epoch: u32,
    pub sim: &'a SimKey,
    pub look: &'a LookSlug,
    pub base: &'a [u8],
    pub crm: &'a [u8],
}

impl Candidate<'_> {
    fn matches(&self, body: &BodyId, epoch: u32, look: &LookSlug, sim: &SimKey) -> bool {
        self.body == body && self.epoch == epoch && self.look == look && self.sim == sim
    }
}

/// Every bundle artifact for the body epochs in `provenance`.
pub(crate) async fn for_release(
    pool: &SqlitePool,
    config: &WorkerConfig,
    provenance: &ProvenanceSummary,
    candidate: &Candidate<'_>,
) -> Result<Vec<ArtifactBlob>> {
    let listings = load_listings(pool).await?;
    let data = crate::body::data_dir(config)?;
    let mut blobs = Vec::new();
    for use_ in &provenance.bodies {
        let body_id = BodyId::new(&use_.body)
            .map_err(|_| WorkerError::Data(format!("invalid body id `{}`", use_.body)))?;
        let resources = data.load(&body_id)?;
        if !bundled_body(&resources.body) {
            continue;
        }
        for epoch_id in &use_.epochs {
            let Some(epoch) = resources
                .schema
                .epochs
                .iter()
                .find(|epoch| epoch.id == *epoch_id)
            else {
                continue;
            };
            assemble(
                pool, config, &listings, &resources, epoch, candidate, &mut blobs,
            )
            .await?;
        }
    }
    Ok(blobs)
}

fn bundled_body(body: &Body) -> bool {
    body.status == BodyStatus::Active && body.kind == BodyKind::Camera
}

async fn load_listings(pool: &SqlitePool) -> Result<BTreeMap<String, LookListingRecord>> {
    Ok(admin::look_listings(pool)
        .await?
        .into_iter()
        .map(|record| (record.look.as_str().to_owned(), record))
        .collect())
}

fn listed(listings: &BTreeMap<String, LookListingRecord>, look: &LookSlug) -> bool {
    match listings.get(look.as_str()).map(|record| record.listing) {
        Some(ListingState::Hidden | ListingState::Removed) => false,
        Some(ListingState::Listed) | None => true,
    }
}

/// Adds every artifact of one `(body, epoch)` bundle.
async fn assemble(
    pool: &SqlitePool,
    config: &WorkerConfig,
    listings: &BTreeMap<String, LookListingRecord>,
    resources: &BodyResources,
    epoch: &Epoch,
    candidate: &Candidate<'_>,
    blobs: &mut Vec<ArtifactBlob>,
) -> Result<()> {
    if epoch.native_sims.is_empty() {
        return Ok(());
    }
    let body = &resources.body.id;
    let transform = TransformDir::new(&config.transforms).snapshot(body)?;
    blobs.push(ArtifactBlob::new(
        body_artifact(body, epoch.id)?,
        resources.schema.source().as_bytes().to_vec(),
    ));
    blobs.push(ArtifactBlob::new(
        transform_artifact(body, epoch.id)?,
        transform,
    ));
    let mut sims = Vec::new();
    let mut excluded = Vec::new();
    for sim in &epoch.native_sims {
        let look = LookSlug::new(sim.look.as_str()).map_err(|_| {
            WorkerError::Data(format!(
                "native sim has an invalid Look `{}`",
                sim.look.as_str()
            ))
        })?;
        if !candidate.matches(body, epoch.id, &look, &sim.key) && !listed(listings, &look) {
            excluded.push(ExcludedSim {
                sim: sim.key.clone(),
                reason: SimExclusion::NoPublishedLook,
            });
            continue;
        }
        match source(pool, config, body, epoch.id, &look, &sim.key, candidate).await? {
            Source::Ready { base, crm } => {
                let names = sim_artifacts(body, epoch.id, &sim.key)?;
                blobs.push(ArtifactBlob::new(names.base.clone(), base));
                blobs.push(ArtifactBlob::new(names.crm.clone(), crm));
                sims.push(MatchSim {
                    sim: sim.key.clone(),
                    base: names.base,
                    crm: names.crm,
                    covariance: None,
                });
            }
            Source::NoLook => excluded.push(ExcludedSim {
                sim: sim.key.clone(),
                reason: SimExclusion::NoPublishedLook,
            }),
            Source::NoResponse => excluded.push(ExcludedSim {
                sim: sim.key.clone(),
                reason: SimExclusion::NoResponse,
            }),
        }
    }
    let bundle = MatchBundle::new(body.clone(), epoch.id, sims, excluded)?;
    blobs.push(ArtifactBlob::new(
        bundle_artifact(body, epoch.id)?,
        bundle.to_json()?,
    ));
    Ok(())
}

enum Source {
    Ready { base: Vec<u8>, crm: Vec<u8> },
    NoLook,
    NoResponse,
}

/// The sim's published realization on `body` for `epoch`, or the in-memory candidate.
async fn source(
    pool: &SqlitePool,
    config: &WorkerConfig,
    body: &BodyId,
    epoch: u32,
    look: &LookSlug,
    sim: &SimKey,
    candidate: &Candidate<'_>,
) -> Result<Source> {
    if candidate.matches(body, epoch, look, sim) {
        return Ok(Source::Ready {
            base: candidate.base.to_vec(),
            crm: candidate.crm.to_vec(),
        });
    }
    let rows = sqlx::query(
        "SELECT m.artifacts FROM look_versions v
         JOIN version_manifests m ON m.look_version_id = v.id
         WHERE v.look = ? AND v.status IN ('published', 'rooted')
           AND EXISTS (
             SELECT 1 FROM release_body_params p
             WHERE p.look = v.look AND p.version = v.version
               AND p.body = ? AND p.stage = 'native')
         ORDER BY v.version DESC",
    )
    .bind(look.as_str())
    .bind(body.as_str())
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(Source::NoLook);
    }
    let mut unusable = false;
    for row in &rows {
        let artifacts: Vec<ManifestArtifact> =
            serde_json::from_str(&row.get::<String, _>("artifacts"))?;
        let (Some(crm), Some(base)) = (
            artifact(&artifacts, CRM_ARTIFACT),
            artifact(&artifacts, LOOK_ARTIFACT),
        ) else {
            unusable = true;
            continue;
        };
        let crm_bytes = read_blob(config, crm)?;
        let Ok(model) = looklab_model::realization::decode_crm(&crm_bytes) else {
            unusable = true;
            continue;
        };
        if model.body != *body || model.epoch != epoch {
            continue;
        }
        if model.base_sim != *sim {
            return Ok(Source::NoResponse);
        }
        return Ok(Source::Ready {
            base: read_blob(config, base)?,
            crm: crm_bytes,
        });
    }
    Ok(if unusable {
        Source::NoResponse
    } else {
        Source::NoLook
    })
}

fn artifact<'a>(artifacts: &'a [ManifestArtifact], name: &str) -> Option<&'a ManifestArtifact> {
    artifacts.iter().find(|artifact| artifact.name == name)
}

fn read_blob(config: &WorkerConfig, artifact: &ManifestArtifact) -> Result<Vec<u8>> {
    let path = config.blobs.join(&artifact.sha256);
    let bytes = std::fs::read(&path)
        .map_err(|error| WorkerError::Data(format!("{}: {error}", path.display())))?;
    let digest = db::hex(&Sha256::digest(&bytes));
    if digest != artifact.sha256 {
        return Err(WorkerError::Data(format!(
            "{}: bytes hash to {digest}, manifest records {}",
            artifact.name, artifact.sha256
        )));
    }
    Ok(bytes)
}
