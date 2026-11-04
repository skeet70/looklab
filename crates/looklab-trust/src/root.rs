// SPDX-License-Identifier: AGPL-3.0-or-later
//! Root-set policy: eligibility, coverage and sunset ([docs/05 §7]).

use std::collections::BTreeSet;

use looklab_upload::{BodyId, ContributionKind, LookSlug};
use serde::{Deserialize, Serialize};

use crate::ids::CredentialId;
use crate::params::TrustParams;
use crate::region::Region;
use crate::standing::{Scope, Standing, Tier};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootDataKind {
    Session(ContributionKind),
    SpectralSensitivity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootTarget {
    Look { look: LookSlug, body: BodyId },
    Body(BodyId),
}

/// An anchored session offered for root admission.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RootSession {
    pub credential: CredentialId,
    pub tier: Tier,
    pub scope: Option<Scope>,
    pub anchored: bool,
    pub kind: RootDataKind,
    pub target: RootTarget,
    pub covered_regions: BTreeSet<Region>,
}

/// The seeder admission that accompanies a root session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RootAdmission {
    pub seeder: CredentialId,
    pub seeder_tier: Tier,
    pub seeder_scope: Option<Scope>,
}

/// Whether an anchored session may be admitted as root data.
///
/// Only chart, gradient and pattern sessions root a Look; only chart sessions and
/// published spectral sensitivity data root a body input transform.
pub fn admission_eligible(session: &RootSession, admission: &RootAdmission) -> bool {
    if !session.anchored || admission.seeder_tier != Tier::Seeder {
        return false;
    }
    if session.tier == Tier::New {
        return false;
    }
    match &session.target {
        RootTarget::Look { look, body } => {
            if Standing::resolve(session.tier, session.scope.as_ref(), look, body) == Standing::New
            {
                return false;
            }
            if admission
                .seeder_scope
                .as_ref()
                .is_some_and(|scope| !scope.covers(look, body))
            {
                return false;
            }
            matches!(
                session.kind,
                RootDataKind::Session(
                    ContributionKind::Chart
                        | ContributionKind::Gradient
                        | ContributionKind::Pattern
                )
            )
        }
        RootTarget::Body(body) => {
            if session
                .scope
                .as_ref()
                .is_some_and(|scope| !scope.covers_body(body))
            {
                return false;
            }
            if admission
                .seeder_scope
                .as_ref()
                .is_some_and(|scope| !scope.covers_body(body))
            {
                return false;
            }
            matches!(
                session.kind,
                RootDataKind::Session(ContributionKind::Chart) | RootDataKind::SpectralSensitivity
            )
        }
    }
}

/// A Look is rooted once admitted sessions cover every supported region.
pub fn rooted(sessions: &[RootSession], supported_regions: &BTreeSet<Region>) -> bool {
    if sessions.is_empty() || supported_regions.is_empty() {
        return false;
    }
    supported_regions.iter().all(|region| {
        sessions
            .iter()
            .any(|session| session.covered_regions.contains(region))
    })
}

/// An endorsed contributor and the bodies they contributed on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndorsedContributor {
    pub credential: CredentialId,
    pub bodies: BTreeSet<BodyId>,
}

/// Sunset condition: enough endorsed contributors across enough bodies that the root
/// can be re-derived from their admitted anchored sessions.
pub fn sunset_ready(contributors: &[EndorsedContributor], params: &TrustParams) -> bool {
    let credentials: BTreeSet<CredentialId> =
        contributors.iter().map(|entry| entry.credential).collect();
    let bodies: BTreeSet<&BodyId> = contributors
        .iter()
        .flat_map(|entry| entry.bodies.iter())
        .collect();
    credentials.len() >= params.root_sunset_contributors
        && bodies.len() >= params.root_sunset_bodies
}

/// The admitted anchored sessions the re-derived root may use: those of endorsed
/// contributors only.
pub fn sunset_sessions<'a>(
    sessions: &'a [RootSession],
    contributors: &[EndorsedContributor],
) -> Vec<&'a RootSession> {
    let eligible: BTreeSet<CredentialId> =
        contributors.iter().map(|entry| entry.credential).collect();
    sessions
        .iter()
        .filter(|session| session.tier == Tier::Endorsed && eligible.contains(&session.credential))
        .collect()
}
