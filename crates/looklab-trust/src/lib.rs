// SPDX-License-Identifier: AGPL-3.0-or-later
//! Contribution weights, tiers, reputation, caps, release gates and rollback planning.
//!
//! Pure computation over typed inputs; the server and worker map database rows into
//! these types and persist the audit events operations return.

pub mod error;
pub mod evidence;
pub mod gates;
#[cfg(feature = "harness")]
pub mod harness;
pub mod ids;
pub mod params;
pub mod region;
pub mod reputation;
pub mod rollback;
pub mod root;
pub mod standing;
pub mod state;
pub mod weight;

pub use error::{Result, TrustError};
pub use evidence::{EvidenceGrid, coverage_gain, evidence};
pub use gates::{
    DriftGateReport, GateInput, GateReference, GateReport, GatedProbe, ProbeEstimate,
    RootGateReport, RootObservation, VersionProbes, VersionRole, Violation, convert_exposure_gauge,
    exposure_gauge_excluded, gate_release,
};
pub use ids::{
    ArtifactHash, CredentialId, EffectiveContributorId, ImageRef, SerialLink, SerialReuse,
    VersionId, resolve_effective_contributors, serial_reuse,
};
pub use params::TrustParams;
pub use region::{EXPOSURE_CELLS, EXPOSURE_MAX, EXPOSURE_MIN, HUE_CELLS, Region, RegionCoverage};
pub use reputation::{
    REPUTATION_MAX, REPUTATION_MIN, REPUTATION_NEUTRAL, Reputation, ReputationUpdate,
};
pub use rollback::{LookVersionRecord, RevertPlan, VersionStatus, plan_revert};
pub use root::{
    EndorsedContributor, RootAdmission, RootDataKind, RootSession, RootTarget, admission_eligible,
    rooted, sunset_ready, sunset_sessions,
};
pub use standing::{Scope, ScopeRule, Standing, Tier};
pub use state::{
    Actor, AuditEvent, CredentialState, Endorsement, EvidenceRef, Grant, ModerationOutcome,
    ProposalId, Role, TrustState,
};
pub use weight::{
    BodyImageWeight, BodyWeightInput, BodyWeightReport, CapReason, CapReduction, CapReport,
    CaptureTuple, ImageWeight, LookWeightInput, LookWeightReport, RegionWeights, StagePartition,
    WeightImage, body_contributor_cap, look_contributor_cap, partition_stages, weight_body,
    weight_look,
};
