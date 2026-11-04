// SPDX-License-Identifier: AGPL-3.0-or-later
//! Seam for `looklab-trust`: tier and reputation inputs to proof-of-work difficulty.

use looklab_upload::PowParams;

use crate::config::PowConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    New,
    Endorsed,
    Seeder,
}

impl Tier {
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "new" => Some(Self::New),
            "endorsed" => Some(Self::Endorsed),
            "seeder" => Some(Self::Seeder),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Endorsed => "endorsed",
            Self::Seeder => "seeder",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CredentialStanding {
    pub tier: Tier,
    pub reputation: f64,
}

pub trait TrustPolicy: Send + Sync {
    fn pow_params(&self, standing: CredentialStanding) -> PowParams;
}

/// The docs/05 §9 defaults with a tier and reputation discount.
#[derive(Debug, Clone, Copy)]
pub struct DefaultTrustPolicy {
    base: PowConfig,
}

impl DefaultTrustPolicy {
    pub const fn new(base: PowConfig) -> Self {
        Self { base }
    }
}

impl TrustPolicy for DefaultTrustPolicy {
    fn pow_params(&self, standing: CredentialStanding) -> PowParams {
        let tier_discount = match standing.tier {
            Tier::New => 0.0,
            Tier::Endorsed => 1.0,
            Tier::Seeder => 2.0,
        };
        let reputation_discount = ((standing.reputation - 0.5) * 2.0).clamp(0.0, 2.0);
        let difficulty =
            (self.base.difficulty_bits_per_image - tier_discount - reputation_discount).max(0.0);
        PowParams {
            memory_kib: self.base.memory_kib,
            iterations: self.base.iterations,
            difficulty_bits_per_image: difficulty,
        }
    }
}
