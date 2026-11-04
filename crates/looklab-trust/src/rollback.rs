// SPDX-License-Identifier: AGPL-3.0-or-later
//! Rollback planning ([docs/05 §8]).
//!
//! [docs/05 §8]: ../../../docs/05-trust-and-integrity.md

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::error::{Result, TrustError};
use crate::ids::{ArtifactHash, CredentialId, VersionId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionStatus {
    Provisional,
    Rooted,
    Published,
    Deprecated,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LookVersionRecord {
    pub id: VersionId,
    pub content: ArtifactHash,
    pub status: VersionStatus,
    pub contributors: Vec<CredentialId>,
}

/// What the worker does for "revert to version N, exclude credential(s) K, refit".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RevertPlan {
    pub target_version: VersionId,
    /// Content of the target version, published as a new version immediately.
    pub publish_as_new: ArtifactHash,
    pub refit_excluding: Vec<CredentialId>,
    /// Credentials of the target that the exclusion removes from the refit.
    pub excluded_contributors_of_target: Vec<CredentialId>,
}

pub fn plan_revert(
    history: &[LookVersionRecord],
    target_version: VersionId,
    exclude: impl IntoIterator<Item = CredentialId>,
) -> Result<RevertPlan> {
    let target = history
        .iter()
        .find(|record| record.id == target_version)
        .ok_or(TrustError::UnknownVersion(target_version))?;

    let refit_excluding: Vec<CredentialId> = exclude
        .into_iter()
        .collect::<BTreeSet<CredentialId>>()
        .into_iter()
        .collect();
    let excluded_contributors_of_target: Vec<CredentialId> = target
        .contributors
        .iter()
        .copied()
        .filter(|credential| refit_excluding.contains(credential))
        .collect::<BTreeSet<CredentialId>>()
        .into_iter()
        .collect();

    Ok(RevertPlan {
        target_version,
        publish_as_new: target.content,
        refit_excluding,
        excluded_contributors_of_target,
    })
}
