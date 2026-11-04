// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tiers, grants, endorsements, revocation and the audit event each produces
//! ([docs/05 §3, §11]).

use std::collections::{BTreeMap, BTreeSet};

use looklab_upload::{BodyId, LookSlug};
use serde::{Deserialize, Serialize};

use crate::error::{Result, TrustError};
use crate::ids::{ArtifactHash, CredentialId, VersionId};
use crate::params::TrustParams;
use crate::reputation::{Reputation, ReputationUpdate};
use crate::rollback::RevertPlan;
use crate::standing::{Scope, Standing, Tier};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Moderator,
    Admin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grant {
    Seeder,
    Moderator,
    Admin,
}

/// Who performed a logged action. `Cli` is the bootstrap admin path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    Credential(CredentialId),
    Cli,
}

impl From<CredentialId> for Actor {
    fn from(id: CredentialId) -> Self {
        Self::Credential(id)
    }
}

impl core::fmt::Display for Actor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Actor::Credential(id) => id.fmt(f),
            Actor::Cli => f.write_str("cli"),
        }
    }
}

/// A reference to evidence behind an endorsement, for example a chart-session id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EvidenceRef(String);

impl EvidenceRef {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Endorsement {
    pub scope: Scope,
    pub reason: String,
    pub evidence: Vec<EvidenceRef>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CredentialState {
    pub tier: Tier,
    pub roles: BTreeSet<Role>,
    pub reputation: Reputation,
    /// `None` means the grant is unrestricted. An endorsement always sets a scope.
    pub scope: Option<Scope>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrustState {
    credentials: BTreeMap<CredentialId, CredentialState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProposalId(u128);

impl ProposalId {
    pub const fn from_u128(value: u128) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModerationOutcome {
    Approved,
    Rejected,
}

/// One append-only audit-log row. The server writes the row the operation returns.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AuditEvent {
    GrantCreated {
        actor: Actor,
        subject: CredentialId,
        grant: Grant,
    },
    GrantRevoked {
        actor: Actor,
        subject: CredentialId,
        grant: Grant,
    },
    EndorsementIssued {
        actor: Actor,
        subject: CredentialId,
        endorsement: Endorsement,
    },
    EndorsementRevoked {
        actor: Actor,
        subject: CredentialId,
    },
    RootAdmitted {
        actor: Actor,
        subject: CredentialId,
        look: LookSlug,
        body: BodyId,
        reason: String,
    },
    RootRevoked {
        actor: Actor,
        subject: CredentialId,
        look: LookSlug,
        body: BodyId,
    },
    ModerationDecision {
        actor: Actor,
        proposal: ProposalId,
        outcome: ModerationOutcome,
        reason: String,
    },
    ReAnchor {
        actor: Actor,
        look: LookSlug,
        version: VersionId,
        reason: String,
    },
    Revert {
        actor: Actor,
        look: LookSlug,
        plan: RevertPlan,
    },
    Split {
        actor: Actor,
        look: LookSlug,
        from: VersionId,
        into: Vec<LookSlug>,
        reason: String,
    },
    ReleaseManifest {
        look: LookSlug,
        version: VersionId,
        artifact: ArtifactHash,
    },
}

impl TrustState {
    pub fn new() -> Self {
        Self {
            credentials: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, id: CredentialId, params: &TrustParams) -> Option<CredentialState> {
        self.insert(
            id,
            CredentialState {
                tier: Tier::New,
                roles: BTreeSet::new(),
                reputation: Tier::New.starting_reputation(params),
                scope: None,
            },
        )
    }

    pub fn insert(&mut self, id: CredentialId, state: CredentialState) -> Option<CredentialState> {
        self.credentials.insert(id, state)
    }

    pub fn credential(&self, id: CredentialId) -> Option<&CredentialState> {
        self.credentials.get(&id)
    }

    pub fn credentials(&self) -> impl Iterator<Item = (CredentialId, &CredentialState)> {
        self.credentials.iter().map(|(&id, state)| (id, state))
    }

    pub fn standing(&self, id: CredentialId, look: &LookSlug, body: &BodyId) -> Option<Standing> {
        self.credentials
            .get(&id)
            .map(|state| Standing::resolve(state.tier, state.scope.as_ref(), look, body))
    }

    /// New-tier credentials at or above the endorsement-queue threshold, best first.
    pub fn nominations(&self, params: &TrustParams) -> Vec<CredentialId> {
        let mut candidates: Vec<(CredentialId, f64)> = self
            .credentials
            .iter()
            .filter(|(_, state)| {
                state.tier == Tier::New && state.reputation.value() >= params.nomination_threshold
            })
            .map(|(&id, state)| (id, state.reputation.value()))
            .collect();
        candidates.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        candidates.into_iter().map(|(id, _)| id).collect()
    }

    pub fn apply_grant(
        &self,
        actor: Actor,
        subject: CredentialId,
        grant: Grant,
        params: &TrustParams,
    ) -> Result<(Self, AuditEvent)> {
        self.require_admin(actor, "grant trust")?;
        let mut state = self.clone();
        let record = state
            .credentials
            .get_mut(&subject)
            .ok_or(TrustError::UnknownCredential(subject))?;
        match grant {
            Grant::Seeder => {
                record.tier = Tier::Seeder;
                if record.reputation.value() < params.reputation_seeder_start {
                    record.reputation = Reputation::saturating(params.reputation_seeder_start);
                }
            }
            Grant::Moderator => {
                record.roles.insert(Role::Moderator);
            }
            Grant::Admin => {
                record.roles.insert(Role::Admin);
            }
        }
        let event = AuditEvent::GrantCreated {
            actor,
            subject,
            grant,
        };
        Ok((state, event))
    }

    pub fn revoke_grant(
        &self,
        actor: Actor,
        subject: CredentialId,
        grant: Grant,
    ) -> Result<(Self, AuditEvent)> {
        self.require_admin(actor, "revoke a grant")?;
        let mut state = self.clone();
        let record = state
            .credentials
            .get_mut(&subject)
            .ok_or(TrustError::UnknownCredential(subject))?;
        match grant {
            Grant::Seeder => {
                if record.tier == Tier::Seeder {
                    record.tier = Tier::New;
                }
            }
            Grant::Moderator => {
                record.roles.remove(&Role::Moderator);
            }
            Grant::Admin => {
                record.roles.remove(&Role::Admin);
            }
        }
        let event = AuditEvent::GrantRevoked {
            actor,
            subject,
            grant,
        };
        Ok((state, event))
    }

    pub fn apply_endorsement(
        &self,
        actor: Actor,
        subject: CredentialId,
        endorsement: Endorsement,
    ) -> Result<(Self, AuditEvent)> {
        let Actor::Credential(issuer) = actor else {
            return Err(TrustError::NotAuthorized {
                actor: actor.to_string(),
                action: "endorse",
            });
        };
        let issuer_state = self
            .credentials
            .get(&issuer)
            .ok_or(TrustError::UnknownCredential(issuer))?;
        if issuer_state.tier != Tier::Seeder {
            return Err(TrustError::NotAuthorized {
                actor: actor.to_string(),
                action: "endorse",
            });
        }
        let within_scope = match &issuer_state.scope {
            Some(scope) => scope.contains(&endorsement.scope),
            None => true,
        };
        if !within_scope {
            return Err(TrustError::ScopeNotWithinIssuer);
        }
        let mut state = self.clone();
        let record = state
            .credentials
            .get_mut(&subject)
            .ok_or(TrustError::UnknownCredential(subject))?;
        if record.tier == Tier::Seeder {
            return Err(TrustError::CannotEndorseSeeder(subject));
        }
        if record.tier == Tier::New {
            record.tier = Tier::Endorsed;
        }
        record.scope = Some(endorsement.scope.clone());
        let event = AuditEvent::EndorsementIssued {
            actor,
            subject,
            endorsement,
        };
        Ok((state, event))
    }

    pub fn revoke_endorsement(
        &self,
        actor: Actor,
        subject: CredentialId,
    ) -> Result<(Self, AuditEvent)> {
        let authorized = match actor {
            Actor::Cli => true,
            Actor::Credential(id) => self.credentials.get(&id).is_some_and(|state| {
                state.roles.contains(&Role::Admin) || state.tier == Tier::Seeder
            }),
        };
        if !authorized {
            return Err(TrustError::NotAuthorized {
                actor: actor.to_string(),
                action: "revoke an endorsement",
            });
        }
        let mut state = self.clone();
        let record = state
            .credentials
            .get_mut(&subject)
            .ok_or(TrustError::UnknownCredential(subject))?;
        if record.tier == Tier::Endorsed {
            record.tier = Tier::New;
            record.scope = None;
        }
        Ok((state, AuditEvent::EndorsementRevoked { actor, subject }))
    }

    /// Reputation is not an audited trust transition.
    pub fn record_reputation(
        &self,
        subject: CredentialId,
        update: ReputationUpdate,
        params: &TrustParams,
    ) -> Result<(Self, Reputation)> {
        let mut state = self.clone();
        let record = state
            .credentials
            .get_mut(&subject)
            .ok_or(TrustError::UnknownCredential(subject))?;
        record.reputation = record.reputation.updated(params, update);
        let reputation = record.reputation;
        Ok((state, reputation))
    }

    fn require_admin(&self, actor: Actor, action: &'static str) -> Result<()> {
        let allowed = match actor {
            Actor::Cli => true,
            Actor::Credential(id) => self
                .credentials
                .get(&id)
                .is_some_and(|state| state.roles.contains(&Role::Admin)),
        };
        if allowed {
            Ok(())
        } else {
            Err(TrustError::NotAuthorized {
                actor: actor.to_string(),
                action,
            })
        }
    }
}

impl Default for TrustState {
    fn default() -> Self {
        Self::new()
    }
}
