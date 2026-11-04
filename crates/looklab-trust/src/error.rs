// SPDX-License-Identifier: AGPL-3.0-or-later

use thiserror::Error;

use crate::ids::{CredentialId, VersionId};

#[derive(Clone, Debug, Error, PartialEq)]
pub enum TrustError {
    #[error("credential {0} is not registered")]
    UnknownCredential(CredentialId),
    #[error("{actor} may not {action}")]
    NotAuthorized { actor: String, action: &'static str },
    #[error("the requested scope is not within the issuer's scope")]
    ScopeNotWithinIssuer,
    #[error("region cell out of range: hue {hue}, exposure {exposure}")]
    InvalidRegion { hue: u8, exposure: u8 },
    #[error("probe coordinate is not finite or outside the supported range")]
    ProbeOutOfRange,
    #[error("region coverage is empty")]
    EmptyCoverage,
    #[error("region coverage share is negative or not finite")]
    InvalidShare,
    #[error("region coverage shares sum to {0}, not a positive number")]
    EmptyShares(f64),
    #[error("version {0} is not in the history")]
    UnknownVersion(VersionId),
    #[error("identifier is not valid: {0}")]
    InvalidId(String),
    #[error("reputation value {0} is not finite")]
    InvalidReputation(f64),
    #[error("encoding failed: {0}")]
    Encode(String),
}

pub type Result<T, E = TrustError> = core::result::Result<T, E>;
