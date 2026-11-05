// SPDX-License-Identifier: AGPL-3.0-or-later
//! Resolving a settings card from published, hash-checked artifacts.

use looklab_match::Permalink;
use looklab_wasm::MatchInputs;
use looklab_wasm::match_api::SourceNames;

use crate::data::{self, LoadedVersion};

pub fn inputs_for_camera(
    version: &LoadedVersion,
    camera: &looklab_schema::BodyId,
    emphasis: &looklab_match::Emphasis,
    epoch: u32,
    seed: u64,
) -> Result<MatchInputs, String> {
    let prefix = format!("match-{}-{}-", camera.as_str().replace('/', "-"), epoch);
    let source = |name: &str| {
        let scoped = format!("{prefix}{name}");
        if data::artifact(version, &scoped).is_some() {
            scoped
        } else {
            name.to_owned()
        }
    };
    let base = source("base.looklab");
    let body = source("body.yaml");
    let crm = source("crm.json");
    let transform = source("transform.yaml");
    let covariance = source("covariance.json");
    let body_yaml = String::from_utf8(artifact(version, &body)?.to_vec())
        .map_err(|_| "Camera settings are unreadable.")?;
    let schema = looklab_schema::validate_control_schema(&body_yaml)
        .map_err(|errors| format!("{errors:?}"))?;
    if &schema.body != camera {
        return Err("We do not have a settings model for this camera yet. Try a different camera or help by sharing photos.".into());
    }
    let transform_yaml = String::from_utf8(artifact(version, &transform)?.to_vec())
        .map_err(|_| "Camera color conversion is unreadable.")?;
    Ok(MatchInputs {
        target_look: artifact(version, "look.looklab")?.to_vec(),
        base_look: Some(artifact(version, &base)?.to_vec()),
        body_yaml,
        crm: artifact(version, &crm)?.to_vec(),
        input_transform_version: looklab_wasm::match_api::transform_version(&transform_yaml)
            .unwrap_or(1),
        transform_yaml: Some(transform_yaml),
        covariance: Some(artifact(version, &covariance)?.to_vec()),
        emphasis: emphasis.as_str().to_owned(),
        epoch,
        realization_version: 0,
        seed,
        sources: SourceNames {
            target_look: Some("look.looklab".into()),
            base_look: Some(base),
            body: Some(body),
            crm: Some(crm),
            transform: Some(transform),
            covariance: Some(covariance),
        },
    })
}

pub async fn inputs_from_permalink(permalink: &Permalink) -> Result<MatchInputs, String> {
    let target = data::match_version(
        permalink.target_slug.as_str(),
        permalink.target_version.as_str(),
    )
    .await?;
    let base = if permalink
        .sources
        .base_look
        .as_deref()
        .is_some_and(|name| data::artifact(&target, name).is_some())
    {
        target.clone()
    } else {
        data::match_version(
            permalink.base_slug.as_str(),
            permalink.base_version.as_str(),
        )
        .await?
    };
    let target_look = source_bytes(
        &target,
        permalink.sources.target_look.as_deref(),
        "target look",
    )?;
    let base_look = source_bytes(&base, permalink.sources.base_look.as_deref(), "base look")?;
    let body_yaml = String::from_utf8(
        source_bytes(&target, permalink.sources.body.as_deref(), "body schema")?.to_vec(),
    )
    .map_err(|_| "body schema is not UTF-8".to_owned())?;
    let crm = source_bytes(
        &target,
        permalink.sources.crm.as_deref(),
        "control-response model",
    )?;
    let transform_yaml = match permalink.sources.transform.as_deref() {
        Some(name) => {
            let bytes = artifact(&target, name)?;
            Some(
                String::from_utf8(bytes.to_vec())
                    .map_err(|_| "transform is not UTF-8".to_owned())?,
            )
        }
        None => None,
    };
    let covariance = match permalink.sources.covariance.as_deref() {
        Some(name) => Some(artifact(&target, name)?.to_vec()),
        None => None,
    };
    Ok(MatchInputs {
        target_look: target_look.to_vec(),
        base_look: Some(base_look.to_vec()),
        body_yaml,
        crm: crm.to_vec(),
        transform_yaml,
        covariance,
        emphasis: permalink.emphasis.as_str().to_owned(),
        epoch: permalink.epoch,
        input_transform_version: permalink.input_transform_version,
        realization_version: permalink.realization_params_version,
        seed: permalink.seed,
        sources: SourceNames {
            target_look: permalink.sources.target_look.clone(),
            base_look: permalink.sources.base_look.clone(),
            body: permalink.sources.body.clone(),
            crm: permalink.sources.crm.clone(),
            transform: permalink.sources.transform.clone(),
            covariance: permalink.sources.covariance.clone(),
        },
    })
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
