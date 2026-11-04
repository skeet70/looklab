// SPDX-License-Identifier: AGPL-3.0-or-later
//! Resolving a settings card from published, hash-checked artifacts.

use looklab_match::Permalink;
use looklab_wasm::MatchInputs;
use looklab_wasm::match_api::SourceNames;

use crate::data::{self, LoadedVersion};

const DEFAULT_SEED: u64 = 0x5eed_1234;

pub fn inputs_from_version(
    version: &LoadedVersion,
    emphasis: &str,
    epoch: u32,
    seed: u64,
) -> Result<MatchInputs, String> {
    let target_look = artifact(version, "look.looklab")?;
    let base_look = artifact(version, "base.looklab")?;
    let body_yaml = String::from_utf8(artifact(version, "body.yaml")?.to_vec())
        .map_err(|_| "body.yaml is not UTF-8".to_owned())?;
    let transform_yaml = String::from_utf8(artifact(version, "transform.yaml")?.to_vec())
        .map_err(|_| "transform.yaml is not UTF-8".to_owned())?;
    let input_transform_version =
        looklab_wasm::match_api::transform_version(&transform_yaml).unwrap_or(1);
    Ok(MatchInputs {
        target_look: target_look.to_vec(),
        base_look: Some(base_look.to_vec()),
        body_yaml,
        crm: artifact(version, "crm.json")?.to_vec(),
        transform_yaml: Some(transform_yaml),
        covariance: Some(artifact(version, "covariance.json")?.to_vec()),
        emphasis: emphasis.to_owned(),
        epoch,
        input_transform_version,
        realization_version: 0,
        seed,
        sources: SourceNames {
            target_look: Some("look.looklab".to_owned()),
            base_look: Some("base.looklab".to_owned()),
            body: Some("body.yaml".to_owned()),
            crm: Some("crm.json".to_owned()),
            transform: Some("transform.yaml".to_owned()),
            covariance: Some("covariance.json".to_owned()),
        },
    })
}

pub async fn inputs_from_permalink(permalink: &Permalink) -> Result<MatchInputs, String> {
    let target = data::version(
        permalink.target_slug.as_str(),
        permalink.target_version.as_str(),
    )
    .await?;
    let base = if permalink.base_slug.as_str() == permalink.target_slug.as_str()
        && permalink.base_version.as_str() == permalink.target_version.as_str()
    {
        target.clone()
    } else {
        data::version(
            permalink.base_slug.as_str(),
            permalink.base_version.as_str(),
        )
        .await?
    };
    let target_look = source_bytes(&target, permalink.sources.target_look.as_deref(), "target look")?;
    let base_look = source_bytes(&base, permalink.sources.base_look.as_deref(), "base look")?;
    let body_yaml = String::from_utf8(source_bytes(
        &target,
        permalink.sources.body.as_deref(),
        "body schema",
    )?
    .to_vec())
    .map_err(|_| "body schema is not UTF-8".to_owned())?;
    let crm = source_bytes(&target, permalink.sources.crm.as_deref(), "control-response model")?;
    let transform_yaml = match permalink.sources.transform.as_deref() {
        Some(name) => {
            let bytes = artifact(&target, name)?;
            Some(String::from_utf8(bytes.to_vec()).map_err(|_| "transform is not UTF-8".to_owned())?)
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
        seed: DEFAULT_SEED,
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
