// SPDX-License-Identifier: AGPL-3.0-or-later
//! Rollback planning ([docs/05 §8](../../../docs/05-trust-and-integrity.md#8-release-gates-and-rollback)).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::error::{Result, TrustError};
use crate::ids::{ArtifactHash, ContributionId, VersionId};

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
}

/// What the worker does for "revert to version N, reject contribution(s) C, refit".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RevertPlan {
    pub target_version: VersionId,
    /// Content of the target version, published as a new version immediately.
    pub publish_as_new: ArtifactHash,
    pub rejected_contributions: Vec<ContributionId>,
}

pub fn plan_revert(
    history: &[LookVersionRecord],
    target_version: VersionId,
    reject: impl IntoIterator<Item = ContributionId>,
) -> Result<RevertPlan> {
    let target = history
        .iter()
        .find(|record| record.id == target_version)
        .ok_or(TrustError::UnknownVersion(target_version))?;

    let rejected_contributions: Vec<ContributionId> = reject
        .into_iter()
        .collect::<BTreeSet<ContributionId>>()
        .into_iter()
        .collect();

    Ok(RevertPlan {
        target_version,
        publish_as_new: target.content,
        rejected_contributions,
    })
}
