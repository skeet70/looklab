// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-body match bundles: the artifacts the settings optimizer reads for every
//! native sim of one body epoch (docs/04 §2-§3). A bundle lives in the target
//! Look's version directory: a descriptor, a schema and transform, and a base
//! realization and response model per sim that has published data.

use looklab_schema::{BodyId, SimKey};
use serde::{Deserialize, Serialize};

use crate::PublishError;
use crate::hash::ArtifactName;

pub const MATCH_BUNDLE_FORMAT: &str = "looklab-match-bundle/1";

/// The descriptor's format tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchBundleFormat {
    #[serde(rename = "looklab-match-bundle/1")]
    V1,
}

const BASE_SUFFIX: &str = "base.looklab";
const CRM_SUFFIX: &str = "crm.json";
const COVARIANCE_SUFFIX: &str = "covariance.json";

/// One native sim's inputs for the optimizer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchSim {
    pub sim: SimKey,
    pub base: ArtifactName,
    pub crm: ArtifactName,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covariance: Option<ArtifactName>,
}

/// Why a native sim is not in the ranking.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimExclusion {
    /// No published Look realizes this sim on the body.
    NoPublishedLook,
    /// The sim's Look has no published control-response model.
    NoResponse,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcludedSim {
    pub sim: SimKey,
    pub reason: SimExclusion,
}

/// The `match-<body>-<epoch>-sims.json` artifact of a version directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchBundle {
    pub format: MatchBundleFormat,
    pub body: BodyId,
    pub epoch: u32,
    pub sims: Vec<MatchSim>,
    pub excluded: Vec<ExcludedSim>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimArtifacts {
    pub base: ArtifactName,
    pub crm: ArtifactName,
    pub covariance: ArtifactName,
}

impl MatchBundle {
    /// # Errors
    /// Rejects a sim that appears both in and out of the ranking.
    pub fn new(
        body: BodyId,
        epoch: u32,
        sims: Vec<MatchSim>,
        excluded: Vec<ExcludedSim>,
    ) -> Result<Self, PublishError> {
        let bundle = Self {
            format: MatchBundleFormat::V1,
            body,
            epoch,
            sims,
            excluded,
        };
        bundle.validate()?;
        Ok(bundle)
    }

    /// # Errors
    /// Rejects bytes that are not a match bundle, or that list a sim twice.
    pub fn decode(bytes: &[u8]) -> Result<Self, PublishError> {
        let bundle: Self = serde_json::from_slice(bytes)?;
        bundle.validate()?;
        Ok(bundle)
    }

    pub fn to_json(&self) -> Result<Vec<u8>, PublishError> {
        Ok(serde_json::to_vec_pretty(self)?)
    }

    fn validate(&self) -> Result<(), PublishError> {
        let mut seen = std::collections::BTreeSet::new();
        for entry in &self.sims {
            if !seen.insert(entry.sim.as_str()) {
                return Err(PublishError::MatchBundle(format!(
                    "sim {} appears twice in the ranking",
                    entry.sim
                )));
            }
            let names = sim_artifacts(&self.body, self.epoch, &entry.sim)?;
            if entry.base != names.base || entry.crm != names.crm {
                return Err(PublishError::MatchBundle(format!(
                    "sim {} does not name its own artifacts",
                    entry.sim
                )));
            }
            if entry
                .covariance
                .as_ref()
                .is_some_and(|name| name != &names.covariance)
            {
                return Err(PublishError::MatchBundle(format!(
                    "sim {} does not name its own covariance",
                    entry.sim
                )));
            }
        }
        for entry in &self.excluded {
            if !seen.insert(entry.sim.as_str()) {
                return Err(PublishError::MatchBundle(format!(
                    "sim {} is both ranked and excluded",
                    entry.sim
                )));
            }
        }
        Ok(())
    }
}

fn prefix(body: &BodyId, epoch: u32) -> String {
    format!("match-{}-{epoch}-", body.as_str().replace('/', "-"))
}

/// The `match-<body>-<epoch>-sims.json` descriptor name.
///
/// # Errors
/// Rejects a name that is not a safe artifact file name.
pub fn bundle_artifact(body: &BodyId, epoch: u32) -> Result<ArtifactName, PublishError> {
    ArtifactName::new(format!("{}sims.json", prefix(body, epoch)))
}

/// The `match-<body>-<epoch>-body.yaml` control-schema snapshot name.
///
/// # Errors
/// Rejects a name that is not a safe artifact file name.
pub fn body_artifact(body: &BodyId, epoch: u32) -> Result<ArtifactName, PublishError> {
    ArtifactName::new(format!("{}body.yaml", prefix(body, epoch)))
}

/// The `match-<body>-<epoch>-transform.yaml` input-transform name.
///
/// # Errors
/// Rejects a name that is not a safe artifact file name.
pub fn transform_artifact(body: &BodyId, epoch: u32) -> Result<ArtifactName, PublishError> {
    ArtifactName::new(format!("{}transform.yaml", prefix(body, epoch)))
}

/// The per-sim names.
///
/// # Errors
/// Rejects a name that is not a safe artifact file name.
pub fn sim_artifacts(
    body: &BodyId,
    epoch: u32,
    sim: &SimKey,
) -> Result<SimArtifacts, PublishError> {
    let prefix = prefix(body, epoch);
    Ok(SimArtifacts {
        base: ArtifactName::new(format!("{prefix}{sim}-{BASE_SUFFIX}"))?,
        crm: ArtifactName::new(format!("{prefix}{sim}-{CRM_SUFFIX}"))?,
        covariance: ArtifactName::new(format!("{prefix}{sim}-{COVARIANCE_SUFFIX}"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ExcludedSim, MATCH_BUNDLE_FORMAT, MatchBundle, MatchBundleFormat, MatchSim, SimExclusion,
        bundle_artifact, sim_artifacts,
    };
    use crate::ArtifactName;
    use looklab_schema::{BodyId, SimKey};

    fn body() -> BodyId {
        BodyId::new("fujifilm/x-t5").expect("body")
    }

    fn sim(key: &str) -> SimKey {
        SimKey::new(key).expect("sim")
    }

    fn entry(key: &str) -> MatchSim {
        let names = sim_artifacts(&body(), 1, &sim(key)).expect("names");
        MatchSim {
            sim: sim(key),
            base: names.base,
            crm: names.crm,
            covariance: None,
        }
    }

    #[test]
    fn per_sim_names_are_scoped_by_body_and_epoch() {
        let names = sim_artifacts(&body(), 2, &sim("classic_chrome")).expect("names");
        assert_eq!(
            names.base.as_str(),
            "match-fujifilm-x-t5-2-classic_chrome-base.looklab"
        );
        assert_eq!(
            names.crm.as_str(),
            "match-fujifilm-x-t5-2-classic_chrome-crm.json"
        );
        assert_eq!(
            names.covariance.as_str(),
            "match-fujifilm-x-t5-2-classic_chrome-covariance.json"
        );
        assert_eq!(
            bundle_artifact(&body(), 2).expect("descriptor").as_str(),
            "match-fujifilm-x-t5-2-sims.json"
        );
    }

    #[test]
    fn round_trip_keeps_ranked_and_excluded_sims_apart() {
        let bundle = MatchBundle::new(
            body(),
            1,
            vec![entry("provia")],
            vec![ExcludedSim {
                sim: sim("classic_chrome"),
                reason: SimExclusion::NoPublishedLook,
            }],
        )
        .expect("bundle");
        let decoded = MatchBundle::decode(&bundle.to_json().expect("json")).expect("decode");
        assert_eq!(decoded, bundle);
        assert_eq!(decoded.format, MatchBundleFormat::V1);
        assert_eq!(decoded.sims[0].sim.as_str(), "provia");
        assert_eq!(decoded.excluded[0].reason, SimExclusion::NoPublishedLook);
    }

    #[test]
    fn a_sim_cannot_be_ranked_and_excluded_at_once() {
        let error = MatchBundle::new(
            body(),
            1,
            vec![entry("provia")],
            vec![ExcludedSim {
                sim: sim("provia"),
                reason: SimExclusion::NoResponse,
            }],
        )
        .expect_err("duplicate sim");
        assert!(matches!(
            error,
            crate::PublishError::MatchBundle(message) if message.contains("provia")
        ));
    }

    #[test]
    fn decode_rejects_a_foreign_format() {
        let mut bundle = MatchBundle::new(body(), 1, vec![entry("provia")], Vec::new())
            .expect("bundle")
            .to_json()
            .expect("json");
        let text = String::from_utf8(bundle.clone()).expect("utf-8");
        bundle = text
            .replace(MATCH_BUNDLE_FORMAT, "looklab-match-bundle/9")
            .into_bytes();
        assert!(matches!(
            MatchBundle::decode(&bundle),
            Err(crate::PublishError::Json(_))
        ));
    }

    #[test]
    fn artifact_names_stay_within_the_site_name_grammar() {
        assert!(ArtifactName::new("match-fujifilm-x-t5-1-provia-crm.json").is_ok());
    }

    fn decoded_after(
        mutate: impl FnOnce(&mut MatchBundle),
    ) -> Result<MatchBundle, crate::PublishError> {
        let mut bundle =
            MatchBundle::new(body(), 1, vec![entry("provia")], Vec::new()).expect("bundle");
        mutate(&mut bundle);
        MatchBundle::decode(&serde_json::to_vec(&bundle).expect("json"))
    }

    #[test]
    fn a_sim_cannot_name_artifacts_outside_its_bundle() {
        let parent = ArtifactName::new("..").expect("name");
        let foreign = ArtifactName::new("match-fujifilm-x-t5-2-provia-base.looklab").expect("name");
        for name in [parent.clone(), foreign] {
            assert!(matches!(
                decoded_after(|bundle| bundle.sims[0].base = name.clone()),
                Err(crate::PublishError::MatchBundle(_))
            ));
        }
        assert!(matches!(
            decoded_after(|bundle| bundle.sims[0].crm = parent.clone()),
            Err(crate::PublishError::MatchBundle(_))
        ));
        assert!(matches!(
            decoded_after(|bundle| bundle.sims[0].covariance = Some(parent)),
            Err(crate::PublishError::MatchBundle(_))
        ));
    }
}
