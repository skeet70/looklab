// SPDX-License-Identifier: AGPL-3.0-or-later
//! The published audit log (docs/05 §11).

use core::fmt;

use serde::{Deserialize, Serialize};

macro_rules! string_ref {
    ($name:ident, $what:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// # Errors
            /// Rejects an empty value.
            pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
                let value = value.into();
                if value.is_empty() {
                    Err(concat!($what, " is empty"))
                } else {
                    Ok(Self(value))
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = &'static str;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_ref!(CredentialRef, "credential reference");
string_ref!(ProposalRef, "proposal reference");
string_ref!(ContributionRef, "contribution reference");
string_ref!(LookRef, "look reference");
string_ref!(VersionRef, "version reference");
string_ref!(Timestamp, "timestamp");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Seeder,
    Moderator,
    Admin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approve,
    Reject,
    RequestInfo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitKind {
    Epoch,
    Look,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuditEvent {
    Grant {
        subject: CredentialRef,
        role: Role,
        granted_by: CredentialRef,
        reason: String,
    },
    Revocation {
        subject: CredentialRef,
        role: Role,
        revoked_by: CredentialRef,
        reason: String,
    },
    Endorsement {
        issuer: CredentialRef,
        subject: CredentialRef,
        reason: String,
    },
    RootAdmission {
        admitted_by: CredentialRef,
        contribution: ContributionRef,
        reason: String,
    },
    ModerationDecision {
        proposal: ProposalRef,
        moderator: CredentialRef,
        decision: Decision,
        reason: String,
    },
    ReAnchor {
        look: LookRef,
        version: VersionRef,
        reason: String,
    },
    Revert {
        look: LookRef,
        to_version: VersionRef,
        excluded: Vec<CredentialRef>,
        reason: String,
    },
    Split {
        kind: SplitKind,
        detail: String,
    },
    LookListing {
        look: LookRef,
        state: crate::site::LookListing,
        moderator: CredentialRef,
        reason: String,
    },
    Release {
        look: LookRef,
        version: VersionRef,
        manifest_sha256: crate::ArtifactHash,
    },
}

impl AuditEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Grant { .. } => "grant",
            Self::Revocation { .. } => "revocation",
            Self::Endorsement { .. } => "endorsement",
            Self::RootAdmission { .. } => "root_admission",
            Self::ModerationDecision { .. } => "moderation_decision",
            Self::ReAnchor { .. } => "re_anchor",
            Self::Revert { .. } => "revert",
            Self::Split { .. } => "split",
            Self::LookListing { .. } => "look_listing",
            Self::Release { .. } => "release",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub seq: u64,
    pub ts: Timestamp,
    pub event: AuditEvent,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditLog {
    pub entries: Vec<AuditEntry>,
}

impl AuditLog {
    pub fn new(mut entries: Vec<AuditEntry>) -> Self {
        entries.sort_by_key(|entry| entry.seq);
        Self { entries }
    }
}
