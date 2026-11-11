// SPDX-License-Identifier: AGPL-3.0-or-later
//! Offline operator commands: state initialisation, the first admin grant and
//! diagnostic retrieval.

use std::io::Write;

use looklab_trust::{Actor, Grant};
use sqlx::SqlitePool;

use crate::admin::{self, GrantRecord};
use crate::client_errors::{self, BuildId, ErrorType, Frame, Page, ReportKind, StoredReport};
use crate::config::{Config, ConfigError, ServerSecret};
use crate::db::{self, DbError};
use crate::ids::CredentialId;
use crate::proposals::rfc3339;

#[derive(Debug, thiserror::Error)]
pub enum OperatorError {
    #[error("database: {0}")]
    Db(#[from] DbError),
    #[error("database: {0}")]
    Query(#[from] sqlx::Error),
    #[error("configuration: {0}")]
    Config(#[from] ConfigError),
    #[error("filesystem: {0}")]
    Io(#[from] std::io::Error),
    #[error("admin: {0}")]
    Admin(#[from] admin::AdminError),
    #[error("`{0}` is not an internal credential id")]
    Credential(String),
    #[error("`{0}` is not a display handle")]
    DisplayHandle(String),
    #[error("no active credential has display handle `{0}`")]
    NoSuchCredential(String),
    #[error("several active credentials have display handle `{0}`")]
    AmbiguousHandle(String),
    #[error("credential `{0}` is not active")]
    Inactive(String),
    #[error("stored credential status `{0}` is unknown")]
    UnknownStatus(String),
    #[error("output: {0}")]
    Output(#[from] serde_json::Error),
}

/// A display name accepted by passkey registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayHandle(String);

impl DisplayHandle {
    pub fn parse(value: &str) -> Option<Self> {
        let valid = !value.is_empty()
            && value.chars().count() <= 64
            && !value.chars().any(char::is_control);
        valid.then(|| Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for DisplayHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The one credential a bootstrap grant names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminSubject {
    Credential(CredentialId),
    DisplayHandle(DisplayHandle),
}

impl AdminSubject {
    pub fn credential(value: &str) -> Result<Self, OperatorError> {
        CredentialId::try_from(value)
            .map(Self::Credential)
            .map_err(|_| OperatorError::Credential(value.to_owned()))
    }

    pub fn display_handle(value: &str) -> Result<Self, OperatorError> {
        DisplayHandle::parse(value)
            .map(Self::DisplayHandle)
            .ok_or_else(|| OperatorError::DisplayHandle(value.to_owned()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialStatus {
    Active,
    Revoked,
}

impl CredentialStatus {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "revoked" => Some(Self::Revoked),
            _ => None,
        }
    }
}

/// Creates missing state directories, the server secret and the migrated database.
pub async fn init(config: &Config) -> Result<(), OperatorError> {
    for dir in [
        Some(config.blob_dir.as_path()),
        Some(config.bodies_dir.as_path()),
        Some(config.snapshot_dir.as_path()),
        config.site.data_dir.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        std::fs::create_dir_all(dir)?;
    }
    ServerSecret::load_or_create(&config.server_secret_path)?;
    let pool = db::open(&config.db_path).await?;
    pool.close().await;
    Ok(())
}

/// Grants `admin` to one credential as the offline CLI actor and records the audit entry.
pub async fn bootstrap_admin(
    config: &Config,
    subject: AdminSubject,
    reason: &str,
) -> Result<GrantRecord, OperatorError> {
    let pool = db::open(&config.db_path).await?;
    let subject = resolve(&pool, subject).await?;
    let record =
        admin::grant_credential(&pool, Actor::Cli, &subject, Grant::Admin, None, reason).await?;
    pool.close().await;
    Ok(record)
}

async fn resolve(pool: &SqlitePool, subject: AdminSubject) -> Result<CredentialId, OperatorError> {
    match subject {
        AdminSubject::Credential(subject) => {
            ensure_active(pool, &subject).await?;
            Ok(subject)
        }
        AdminSubject::DisplayHandle(handle) => {
            let rows: Vec<(String, String)> = sqlx::query_as(
                "SELECT id, status FROM credentials WHERE display_handle = ?
                 ORDER BY created_at, id",
            )
            .bind(handle.as_str())
            .fetch_all(pool)
            .await?;
            let mut active = Vec::new();
            for (id, status) in rows {
                let status = CredentialStatus::parse(&status)
                    .ok_or_else(|| OperatorError::UnknownStatus(status.clone()))?;
                if status == CredentialStatus::Active {
                    active.push(id);
                }
            }
            match active.as_slice() {
                [] => Err(OperatorError::NoSuchCredential(handle.to_string())),
                [only] => CredentialId::try_from(only.clone())
                    .map_err(|_| OperatorError::Credential(only.clone())),
                _ => Err(OperatorError::AmbiguousHandle(handle.to_string())),
            }
        }
    }
}

async fn ensure_active(pool: &SqlitePool, subject: &CredentialId) -> Result<(), OperatorError> {
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM credentials WHERE id = ?")
        .bind(subject.as_str())
        .fetch_optional(pool)
        .await?;
    let Some(status) = status else {
        return Err(OperatorError::NoSuchCredential(subject.to_string()));
    };
    match CredentialStatus::parse(&status)
        .ok_or_else(|| OperatorError::UnknownStatus(status.clone()))?
    {
        CredentialStatus::Active => Ok(()),
        CredentialStatus::Revoked => Err(OperatorError::Inactive(subject.to_string())),
    }
}

/// Reads diagnostic reports through a read-only connection.
///
/// # Errors
/// The database is missing, unreadable or holds a malformed row.
pub async fn client_error_reports(
    config: &Config,
    limit: usize,
    release: Option<&BuildId>,
) -> Result<Vec<StoredReport>, OperatorError> {
    let pool = db::open_read_only(&config.db_path).await?;
    let reports = client_errors::recent(&pool, limit, release).await;
    pool.close().await;
    reports.map_err(Into::into)
}

#[derive(serde::Serialize)]
struct ClientErrorOutput<'a> {
    received_at: i64,
    timestamp: String,
    release: &'a BuildId,
    page: Page,
    kind: ReportKind,
    error_type: ErrorType,
    frames: &'a [Frame],
}

/// Writes reports newest first as text blocks or a JSON array.
///
/// # Errors
/// Writing the stream fails.
pub fn write_client_error_reports(
    mut out: impl Write,
    reports: &[StoredReport],
    json: bool,
) -> Result<(), OperatorError> {
    if json {
        let rows: Vec<ClientErrorOutput<'_>> = reports
            .iter()
            .map(|report| ClientErrorOutput {
                received_at: report.received_at,
                timestamp: rfc3339(report.received_at),
                release: &report.release,
                page: report.page,
                kind: report.kind,
                error_type: report.error_type,
                frames: &report.frames,
            })
            .collect();
        serde_json::to_writer_pretty(&mut out, &rows)?;
        writeln!(out)?;
    } else {
        for report in reports {
            writeln!(
                out,
                "{} release={} page={} kind={} type={}",
                rfc3339(report.received_at),
                report.release,
                report.page,
                report.kind,
                report.error_type
            )?;
            for frame in &report.frames {
                writeln!(out, "  {frame}")?;
            }
        }
    }
    Ok(())
}
