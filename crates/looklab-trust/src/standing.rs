// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tiers, scoped standing and endorsement scope ([docs/05 §3]).

use std::collections::BTreeSet;

use looklab_upload::{BodyId, LookSlug};
use serde::{Deserialize, Serialize};

use crate::params::TrustParams;
use crate::reputation::Reputation;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    New,
    Endorsed,
    Seeder,
}

impl Tier {
    pub fn starting_reputation(self, params: &TrustParams) -> Reputation {
        let value = match self {
            Tier::New | Tier::Endorsed => params.reputation_new_start,
            Tier::Seeder => params.reputation_seeder_start,
        };
        Reputation::saturating(value)
    }
}

/// The tier that applies to one `(look, body)` after the scope check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Standing {
    New,
    Endorsed,
    Seeder,
}

impl Standing {
    /// Outside its scope an endorsed or seeder credential counts as `new`.
    pub fn resolve(tier: Tier, scope: Option<&Scope>, look: &LookSlug, body: &BodyId) -> Self {
        let covered = match scope {
            Some(scope) => scope.covers(look, body),
            None => true,
        };
        match (tier, covered) {
            (Tier::New, _) => Standing::New,
            (Tier::Endorsed | Tier::Seeder, false) => Standing::New,
            (Tier::Endorsed, true) => Standing::Endorsed,
            (Tier::Seeder, true) => Standing::Seeder,
        }
    }

    pub fn tier(self) -> Tier {
        match self {
            Standing::New => Tier::New,
            Standing::Endorsed => Tier::Endorsed,
            Standing::Seeder => Tier::Seeder,
        }
    }

    pub fn factor(self, params: &TrustParams) -> f64 {
        match self {
            Standing::New => params.tier_factor_new,
            Standing::Endorsed | Standing::Seeder => params.tier_factor_trusted,
        }
    }
}

/// An endorsement or grant scope: a set of Looks and a set of bodies.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Scope {
    pub bodies: BTreeSet<BodyId>,
    pub looks: BTreeSet<LookSlug>,
}

impl Scope {
    pub fn new(bodies: BTreeSet<BodyId>, looks: BTreeSet<LookSlug>) -> Self {
        Self { bodies, looks }
    }

    pub fn covers(&self, look: &LookSlug, body: &BodyId) -> bool {
        self.looks.contains(look) || self.bodies.contains(body)
    }

    fn restricts(&self) -> bool {
        !(self.bodies.is_empty() && self.looks.is_empty())
    }

    /// An empty scope restricts nothing, so it cannot authorize a scoped endorsement.
    pub fn contains(&self, other: &Scope) -> bool {
        if !other.restricts() {
            return false;
        }
        other.bodies.is_subset(&self.bodies) && other.looks.is_subset(&self.looks)
    }
}
