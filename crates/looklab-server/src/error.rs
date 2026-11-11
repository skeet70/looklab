// SPDX-License-Identifier: AGPL-3.0-or-later
//! Typed API errors. Responses carry a code and nothing derived from request data.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    PayloadTooLarge,
    LengthRequired,
    RequestTimeout,
    SimulationTimeout,
    Busy,
    EnvelopeVersionUnsupported,
    MalformedEnvelope,
    NonceUnknown,
    NonceExpired,
    NonceReplayed,
    SignatureInvalid,
    PowInvalid,
    SendManifestInvalid,
    SendReplayed,
    BatchReplayed,
    SendCredentialMismatch,
    SendExpired,
    BodyUnknown,
    SchemaInvalid,
    RenderClassMismatch,
    SnapshotUnknown,
    SnapshotTooOld,
    CredentialUnknown,
    CredentialConflict,
    CredentialBlocked,
    RegistrationUnknown,
    TokenUnknown,
    ContributionUnknown,
    Forbidden,
    NotFound,
    Conflict,
    RateLimited,
    UnsupportedMediaType,
    Internal,
}

impl ErrorCode {
    pub const fn status(self) -> StatusCode {
        match self {
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::LengthRequired => StatusCode::LENGTH_REQUIRED,
            Self::RequestTimeout => StatusCode::REQUEST_TIMEOUT,
            Self::SimulationTimeout => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Busy => StatusCode::SERVICE_UNAVAILABLE,
            Self::EnvelopeVersionUnsupported => StatusCode::UPGRADE_REQUIRED,
            Self::MalformedEnvelope => StatusCode::BAD_REQUEST,
            Self::NonceUnknown | Self::NonceExpired => StatusCode::BAD_REQUEST,
            Self::NonceReplayed => StatusCode::CONFLICT,
            Self::SendManifestInvalid => StatusCode::BAD_REQUEST,
            Self::SendReplayed | Self::BatchReplayed => StatusCode::CONFLICT,
            Self::SendCredentialMismatch => StatusCode::UNAUTHORIZED,
            Self::SendExpired => StatusCode::BAD_REQUEST,
            Self::SignatureInvalid => StatusCode::UNAUTHORIZED,
            Self::PowInvalid => StatusCode::BAD_REQUEST,
            Self::BodyUnknown
            | Self::SchemaInvalid
            | Self::RenderClassMismatch
            | Self::SnapshotUnknown
            | Self::SnapshotTooOld => StatusCode::BAD_REQUEST,
            Self::CredentialUnknown => StatusCode::UNAUTHORIZED,
            Self::CredentialConflict => StatusCode::CONFLICT,
            Self::CredentialBlocked => StatusCode::FORBIDDEN,
            Self::RegistrationUnknown => StatusCode::NOT_FOUND,
            Self::TokenUnknown => StatusCode::NOT_FOUND,
            Self::ContributionUnknown => StatusCode::NOT_FOUND,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Conflict => StatusCode::CONFLICT,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::UnsupportedMediaType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PayloadTooLarge => "PAYLOAD_TOO_LARGE",
            Self::LengthRequired => "LENGTH_REQUIRED",
            Self::RequestTimeout => "REQUEST_TIMEOUT",
            Self::SimulationTimeout => "SIMULATION_TIMEOUT",
            Self::Busy => "BUSY",
            Self::EnvelopeVersionUnsupported => "ENVELOPE_VERSION_UNSUPPORTED",
            Self::MalformedEnvelope => "MALFORMED_ENVELOPE",
            Self::NonceUnknown => "NONCE_UNKNOWN",
            Self::NonceExpired => "NONCE_EXPIRED",
            Self::NonceReplayed => "NONCE_REPLAYED",
            Self::SendManifestInvalid => "SEND_MANIFEST_INVALID",
            Self::SendReplayed => "SEND_REPLAYED",
            Self::BatchReplayed => "BATCH_REPLAYED",
            Self::SendCredentialMismatch => "SEND_CREDENTIAL_MISMATCH",
            Self::SendExpired => "SEND_EXPIRED",
            Self::SignatureInvalid => "SIGNATURE_INVALID",
            Self::PowInvalid => "POW_INVALID",
            Self::BodyUnknown => "BODY_UNKNOWN",
            Self::SchemaInvalid => "SCHEMA_INVALID",
            Self::RenderClassMismatch => "RENDER_CLASS_MISMATCH",
            Self::SnapshotUnknown => "SNAPSHOT_UNKNOWN",
            Self::SnapshotTooOld => "SNAPSHOT_TOO_OLD",
            Self::CredentialUnknown => "CREDENTIAL_UNKNOWN",
            Self::CredentialConflict => "CREDENTIAL_CONFLICT",
            Self::CredentialBlocked => "CREDENTIAL_BLOCKED",
            Self::RegistrationUnknown => "REGISTRATION_UNKNOWN",
            Self::TokenUnknown => "TOKEN_UNKNOWN",
            Self::ContributionUnknown => "CONTRIBUTION_UNKNOWN",
            Self::Forbidden => "FORBIDDEN",
            Self::NotFound => "NOT_FOUND",
            Self::Conflict => "CONFLICT",
            Self::RateLimited => "RATE_LIMITED",
            Self::UnsupportedMediaType => "UNSUPPORTED_MEDIA_TYPE",
            Self::Internal => "INTERNAL",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ApiError {
    code: ErrorCode,
    action: Option<&'static str>,
}

impl ApiError {
    pub const fn new(code: ErrorCode) -> Self {
        Self { code, action: None }
    }

    pub const fn with_action(code: ErrorCode, action: &'static str) -> Self {
        Self {
            code,
            action: Some(action),
        }
    }
}

impl From<ErrorCode> for ApiError {
    fn from(code: ErrorCode) -> Self {
        Self::new(code)
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(_error: sqlx::Error) -> Self {
        tracing::error!(kind = "database", "request failed");
        Self::new(ErrorCode::Internal)
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<&'static str>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.code == ErrorCode::Internal {
            tracing::error!(code = self.code.as_str(), "request failed");
        } else {
            tracing::debug!(code = self.code.as_str(), "request rejected");
        }
        (
            self.code.status(),
            Json(ErrorBody {
                error: self.code.as_str(),
                action: self.action,
            }),
        )
            .into_response()
    }
}

impl From<crate::admin::AdminError> for ApiError {
    fn from(error: crate::admin::AdminError) -> Self {
        use crate::admin::AdminError as E;
        use looklab_trust::TrustError as T;
        let code = match error {
            E::Database(_) | E::Corrupt | E::Model(_) => ErrorCode::Internal,
            E::UnknownCredential
            | E::UnknownContribution
            | E::UnknownRootAdmission
            | E::UnknownVersion
            | E::UnknownProposal
            | E::UnknownLook
            | E::NoActiveRecord(_) => ErrorCode::NotFound,
            E::NotAuthorized => ErrorCode::Forbidden,
            E::Trust(T::NotAuthorized { .. } | T::ScopeNotWithinIssuer) => ErrorCode::Forbidden,
            E::Trust(T::UnknownCredential(_)) => ErrorCode::NotFound,
            E::Trust(T::CannotEndorseSeeder(_)) => ErrorCode::Conflict,
            E::Trust(_) => ErrorCode::MalformedEnvelope,
            E::ProposalDecided
            | E::Conflict
            | E::ManifestExists
            | E::LookState(_)
            | E::VersionNotPublished
            | E::RootRevoked(_)
            | E::TooManyRejections
            | E::ContributionWithdrawn
            | E::ContributionNotInLookEvidence => ErrorCode::Conflict,
            E::InvalidReason
            | E::InvalidEvidence
            | E::EmptyScope
            | E::ScopeTooLarge
            | E::IneligibleContribution
            | E::EmptyManifest
            | E::InvalidArtifact(_)
            | E::InvalidProposal(_)
            | E::CyclicBaseDependency => ErrorCode::MalformedEnvelope,
        };
        Self::new(code)
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
