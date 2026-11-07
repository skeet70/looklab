// SPDX-License-Identifier: AGPL-3.0-or-later
//! Resolving a settings card from published, hash-checked artifacts.

use looklab_match::{Calibration, Permalink};
use looklab_publish::{MatchBundle, body_artifact, bundle_artifact, transform_artifact};
use looklab_wasm::MatchInputs;
use looklab_wasm::SimInputs;
use looklab_wasm::SimSources;
use looklab_wasm::match_api::SourceNames;

use crate::data::{self, LoadedVersion};

const NO_MODEL: &str = "We do not have a settings model for this camera yet. Try a different camera or help by sharing photos.";

pub fn inputs_for_camera(
    version: &LoadedVersion,
    camera: &looklab_schema::BodyId,
    emphasis: &looklab_match::Emphasis,
    epoch: u32,
    seed: u64,
) -> Result<MatchInputs, String> {
    let descriptor_name = bundle_artifact(camera, epoch).map_err(|error| error.to_string())?;
    let Some(descriptor) = data::artifact(version, descriptor_name.as_str()) else {
        return Err(NO_MODEL.to_owned());
    };
    let bundle = MatchBundle::decode(descriptor).map_err(|error| error.to_string())?;
    if &bundle.body != camera || bundle.epoch != epoch {
        return Err(
            "This look's settings model is for a different camera or firmware version.".to_owned(),
        );
    }
    if bundle.sims.is_empty() {
        return Err(NO_MODEL.to_owned());
    }
    let body_name = body_artifact(camera, epoch).map_err(|error| error.to_string())?;
    let body_yaml = String::from_utf8(artifact(version, body_name.as_str())?.to_vec())
        .map_err(|_| "Camera settings are unreadable.")?;
    let schema = looklab_schema::validate_control_schema(&body_yaml)
        .map_err(|errors| format!("{errors:?}"))?;
    if &schema.body != camera {
        return Err(NO_MODEL.to_owned());
    }
    let transform_name = transform_artifact(camera, epoch).map_err(|error| error.to_string())?;
    let transform_yaml = String::from_utf8(artifact(version, transform_name.as_str())?.to_vec())
        .map_err(|_| "Camera color conversion is unreadable.")?;
    let mut sims = Vec::with_capacity(bundle.sims.len());
    for entry in &bundle.sims {
        let covariance = match &entry.covariance {
            Some(name) => Some(artifact(version, name.as_str())?.to_vec()),
            None => None,
        };
        sims.push(SimInputs {
            sim: entry.sim.clone(),
            base_look: artifact(version, entry.base.as_str())?.to_vec(),
            crm: artifact(version, entry.crm.as_str())?.to_vec(),
            covariance,
            sources: SimSources {
                base: Some(entry.base.to_string()),
                crm: Some(entry.crm.to_string()),
                covariance: entry.covariance.as_ref().map(ToString::to_string),
            },
        });
    }
    Ok(MatchInputs {
        target_look: artifact(version, "look.looklab")?.to_vec(),
        sims,
        excluded: bundle.excluded,
        body_yaml,
        input_transform_version: looklab_wasm::match_api::transform_version(&transform_yaml)
            .unwrap_or(1),
        transform_yaml: Some(transform_yaml),
        emphasis: emphasis.as_str().to_owned(),
        epoch,
        realization_version: 0,
        seed,
        sources: SourceNames {
            target_look: Some("look.looklab".into()),
            body: Some(body_name.to_string()),
            transform: Some(transform_name.to_string()),
        },
        calibration: Calibration::default(),
    })
}

pub async fn inputs_from_permalink(permalink: &Permalink) -> Result<MatchInputs, String> {
    let target = data::match_version(
        permalink.target_slug.as_str(),
        permalink.target_version.as_str(),
    )
    .await?;
    let body =
        looklab_schema::BodyId::new(permalink.body.as_str()).map_err(|error| error.to_string())?;
    let descriptor_name =
        bundle_artifact(&body, permalink.epoch).map_err(|error| error.to_string())?;
    let Some(descriptor) = data::artifact(&target, descriptor_name.as_str()) else {
        return Err(format!(
            "{} {} has no settings bundle for {} on epoch {}, so this link's original search cannot be replayed. Run the match again to make a new link.",
            target.manifest.look, target.manifest.version, permalink.body, permalink.epoch
        ));
    };
    let bundle = MatchBundle::decode(descriptor).map_err(|error| error.to_string())?;
    if bundle.body != body || bundle.epoch != permalink.epoch {
        return Err(
            "This link's settings bundle is for a different camera or firmware version.".to_owned(),
        );
    }
    if bundle.sims.is_empty() {
        return Err(NO_MODEL.to_owned());
    }
    let winner_sim =
        looklab_schema::SimKey::new(permalink.sim.as_str()).map_err(|error| error.to_string())?;
    let winner = bundle
        .sims
        .iter()
        .find(|entry| entry.sim == winner_sim)
        .ok_or_else(|| {
            format!(
                "The pinned sim {winner_sim} is not in the settings bundle of {} {}, so this link cannot reproduce its settings.",
                target.manifest.look, target.manifest.version
            )
        })?;
    pinned_source(
        "base look",
        permalink.sources.base_look.as_deref(),
        Some(winner.base.as_str()),
    )?;
    pinned_source(
        "control-response model",
        permalink.sources.crm.as_deref(),
        Some(winner.crm.as_str()),
    )?;
    pinned_source(
        "covariance",
        permalink.sources.covariance.as_deref(),
        winner.covariance.as_ref().map(|name| name.as_str()),
    )?;
    let target_look = source_bytes(
        &target,
        permalink.sources.target_look.as_deref(),
        "target look",
    )?;
    let body_name = body_artifact(&body, permalink.epoch).map_err(|error| error.to_string())?;
    pinned_source(
        "body schema",
        permalink.sources.body.as_deref(),
        Some(body_name.as_str()),
    )?;
    let body_yaml = String::from_utf8(artifact(&target, body_name.as_str())?.to_vec())
        .map_err(|_| "Camera settings are unreadable.")?;
    let schema = looklab_schema::validate_control_schema(&body_yaml)
        .map_err(|errors| format!("{errors:?}"))?;
    if schema.body != body {
        return Err(NO_MODEL.to_owned());
    }
    let transform_name =
        transform_artifact(&body, permalink.epoch).map_err(|error| error.to_string())?;
    let transform_yaml = if data::artifact(&target, transform_name.as_str()).is_some() {
        pinned_source(
            "transform",
            permalink.sources.transform.as_deref(),
            Some(transform_name.as_str()),
        )?;
        let bytes = artifact(&target, transform_name.as_str())?;
        Some(
            String::from_utf8(bytes.to_vec())
                .map_err(|_| "Camera color conversion is unreadable.")?,
        )
    } else {
        pinned_source("transform", permalink.sources.transform.as_deref(), None)?;
        None
    };
    let mut sims = Vec::with_capacity(bundle.sims.len());
    for entry in &bundle.sims {
        sims.push(SimInputs {
            sim: entry.sim.clone(),
            base_look: artifact(&target, entry.base.as_str())?.to_vec(),
            crm: artifact(&target, entry.crm.as_str())?.to_vec(),
            covariance: entry
                .covariance
                .as_ref()
                .map(|name| artifact(&target, name.as_str()).map(<[u8]>::to_vec))
                .transpose()?,
            sources: SimSources {
                base: Some(entry.base.to_string()),
                crm: Some(entry.crm.to_string()),
                covariance: entry.covariance.as_ref().map(ToString::to_string),
            },
        });
    }
    Ok(MatchInputs {
        target_look: target_look.to_vec(),
        sims,
        excluded: bundle.excluded.clone(),
        body_yaml,
        transform_yaml,
        emphasis: permalink.emphasis.as_str().to_owned(),
        epoch: permalink.epoch,
        input_transform_version: permalink.input_transform_version,
        realization_version: permalink.realization_params_version,
        seed: permalink.seed,
        sources: SourceNames {
            target_look: permalink.sources.target_look.clone(),
            body: permalink.sources.body.clone(),
            transform: permalink.sources.transform.clone(),
        },
        calibration: permalink.calibration.clone(),
    })
}

fn pinned_source(what: &str, pinned: Option<&str>, bundled: Option<&str>) -> Result<(), String> {
    if pinned != bundled {
        return Err(format!(
            "The pinned {what} source {pinned:?} does not match this link's settings bundle {bundled:?}, so the original settings cannot be reproduced."
        ));
    }
    Ok(())
}

fn source_bytes<'a>(
    version: &'a LoadedVersion,
    name: Option<&str>,
    what: &str,
) -> Result<&'a [u8], String> {
    let name = name.ok_or_else(|| format!("the permalink has no {what} source"))?;
    artifact(version, name)
}

fn artifact<'a>(version: &'a LoadedVersion, name: &str) -> Result<&'a [u8], String> {
    data::artifact(version, name).ok_or_else(|| {
        format!(
            "{name} is not in the manifest of {} {}",
            version.manifest.look, version.manifest.version
        )
    })
}
