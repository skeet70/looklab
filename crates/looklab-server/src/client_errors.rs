// SPDX-License-Identifier: AGPL-3.0-or-later
//! Untrusted browser diagnostic reports: strict wire schema, bounded storage and
//! operator-only retrieval.

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

pub const MAX_REPORT_BYTES: usize = 16 * 1024;
pub const MAX_FRAMES: usize = 12;
pub const RETENTION_DAYS: i64 = 14;
pub const MAX_STORED_REPORTS: i64 = 10_000;
pub const MAX_READ_LIMIT: usize = 1_000;

const REPORT_VERSION: u8 = 1;
const SECONDS_PER_DAY: i64 = 86_400;

const WORKER_ASSETS: [&str; 3] = [
    "workers/prep-worker.js",
    "workers/match-worker.js",
    "workers/scene-worker.js",
];
const GENERATED_WASM_ASSETS: [&str; 3] = [
    "generated/wasm/looklab_prep_module.wasm",
    "generated/wasm/looklab_scene_module.wasm",
    "generated/wasm/looklab_match.wasm",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportKind {
    ConsoleError,
    Exception,
    UnhandledRejection,
    ResourceError,
}

impl ReportKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConsoleError => "console_error",
            Self::Exception => "exception",
            Self::UnhandledRejection => "unhandled_rejection",
            Self::ResourceError => "resource_error",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "console_error" => Some(Self::ConsoleError),
            "exception" => Some(Self::Exception),
            "unhandled_rejection" => Some(Self::UnhandledRejection),
            "resource_error" => Some(Self::ResourceError),
            _ => None,
        }
    }
}

impl core::fmt::Display for ReportKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ErrorType {
    Error,
    TypeError,
    RangeError,
    ReferenceError,
    SyntaxError,
    #[serde(rename = "URIError")]
    UriError,
    EvalError,
    RuntimeError,
    CompileError,
    LinkError,
    Unknown,
}

impl ErrorType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "Error",
            Self::TypeError => "TypeError",
            Self::RangeError => "RangeError",
            Self::ReferenceError => "ReferenceError",
            Self::SyntaxError => "SyntaxError",
            Self::UriError => "URIError",
            Self::EvalError => "EvalError",
            Self::RuntimeError => "RuntimeError",
            Self::CompileError => "CompileError",
            Self::LinkError => "LinkError",
            Self::Unknown => "Unknown",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "Error" => Some(Self::Error),
            "TypeError" => Some(Self::TypeError),
            "RangeError" => Some(Self::RangeError),
            "ReferenceError" => Some(Self::ReferenceError),
            "SyntaxError" => Some(Self::SyntaxError),
            "URIError" => Some(Self::UriError),
            "EvalError" => Some(Self::EvalError),
            "RuntimeError" => Some(Self::RuntimeError),
            "CompileError" => Some(Self::CompileError),
            "LinkError" => Some(Self::LinkError),
            "Unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

impl core::fmt::Display for ErrorType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Page {
    Home,
    Looks,
    Look,
    Compare,
    Cameras,
    Camera,
    Match,
    Contribute,
    Me,
    Status,
    Community,
    Moderation,
    About,
    Other,
}

impl Page {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Looks => "looks",
            Self::Look => "look",
            Self::Compare => "compare",
            Self::Cameras => "cameras",
            Self::Camera => "camera",
            Self::Match => "match",
            Self::Contribute => "contribute",
            Self::Me => "me",
            Self::Status => "status",
            Self::Community => "community",
            Self::Moderation => "moderation",
            Self::About => "about",
            Self::Other => "other",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "home" => Some(Self::Home),
            "looks" => Some(Self::Looks),
            "look" => Some(Self::Look),
            "compare" => Some(Self::Compare),
            "cameras" => Some(Self::Cameras),
            "camera" => Some(Self::Camera),
            "match" => Some(Self::Match),
            "contribute" => Some(Self::Contribute),
            "me" => Some(Self::Me),
            "status" => Some(Self::Status),
            "community" => Some(Self::Community),
            "moderation" => Some(Self::Moderation),
            "about" => Some(Self::About),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

impl core::fmt::Display for Page {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a build id is 64 lowercase hexadecimal characters")]
pub struct InvalidBuildId;

/// A build pipeline's SHA-256 identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct BuildId(String);

impl BuildId {
    /// # Errors
    /// The value is not 64 lowercase hexadecimal characters.
    pub fn parse(value: &str) -> Result<Self, InvalidBuildId> {
        let valid = value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidBuildId)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for BuildId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for BuildId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("an asset path is not an allowed browser script or wasm module")]
pub struct InvalidAssetPath;

/// A static asset path a browser stack frame may name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct AssetPath(String);

impl AssetPath {
    /// # Errors
    /// The value is not one of the allowed site, worker or generated wasm paths.
    pub fn parse(value: &str) -> Result<Self, InvalidAssetPath> {
        let allowed = value == "errors.js"
            || WORKER_ASSETS.contains(&value)
            || GENERATED_WASM_ASSETS.contains(&value)
            || value.strip_prefix("pkg/").is_some_and(is_pkg_asset);
        if allowed {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidAssetPath)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_pkg_asset(name: &str) -> bool {
    let Some(stem) = name
        .strip_suffix(".js")
        .or_else(|| name.strip_suffix(".wasm"))
    else {
        return false;
    };
    !stem.is_empty()
        && stem
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

impl core::fmt::Display for AssetPath {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AssetPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    pub asset: AssetPath,
    pub line: u32,
    pub column: u32,
}

impl core::fmt::Display for Frame {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}:{}:{}", self.asset, self.line, self.column)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReport {
    version: u8,
    kind: ReportKind,
    error_type: ErrorType,
    page: Page,
    release: BuildId,
    frames: Vec<Frame>,
}

/// A validated wire report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientErrorReport {
    kind: ReportKind,
    error_type: ErrorType,
    page: Page,
    release: BuildId,
    frames: Vec<Frame>,
}

impl ClientErrorReport {
    /// # Errors
    /// The bytes are not a strict report with a supported version and bounded frames.
    pub fn parse(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

impl<'de> Deserialize<'de> for ClientErrorReport {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawReport::deserialize(deserializer)?;
        if raw.version != REPORT_VERSION {
            return Err(de::Error::custom("unsupported report version"));
        }
        if raw.frames.len() > MAX_FRAMES {
            return Err(de::Error::custom("too many frames"));
        }
        Ok(Self {
            kind: raw.kind,
            error_type: raw.error_type,
            page: raw.page,
            release: raw.release,
            frames: raw.frames,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredReport {
    pub received_at: i64,
    pub release: BuildId,
    pub kind: ReportKind,
    pub error_type: ErrorType,
    pub page: Page,
    pub frames: Vec<Frame>,
}

#[derive(Debug, thiserror::Error)]
enum StoredReportError {
    #[error("stored release is not a build id")]
    Release,
    #[error("stored kind is unknown")]
    Kind,
    #[error("stored error type is unknown")]
    ErrorType,
    #[error("stored page is unknown")]
    Page,
    #[error("stored frames are invalid: {0}")]
    Frames(#[from] serde_json::Error),
}

impl StoredReport {
    fn from_row(
        row: (i64, String, String, String, String, String),
    ) -> Result<Self, StoredReportError> {
        let (received_at, release, kind, error_type, page, frames) = row;
        Ok(Self {
            received_at,
            release: BuildId::parse(&release).map_err(|_| StoredReportError::Release)?,
            kind: ReportKind::parse(&kind).ok_or(StoredReportError::Kind)?,
            error_type: ErrorType::parse(&error_type).ok_or(StoredReportError::ErrorType)?,
            page: Page::parse(&page).ok_or(StoredReportError::Page)?,
            frames: serde_json::from_str(&frames)?,
        })
    }
}

/// Prunes expired and surplus rows in the same transaction as the insert.
///
/// # Errors
/// The insert, prune or commit fails.
pub async fn insert(
    db: &SqlitePool,
    report: &ClientErrorReport,
    received_at: i64,
) -> Result<(), sqlx::Error> {
    let frames = serde_json::to_string(&report.frames)
        .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
    let mut tx = db.begin().await?;
    sqlx::query(
        "INSERT INTO client_errors (received_at, release, kind, error_type, page, frames)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(received_at)
    .bind(report.release.as_str())
    .bind(report.kind.as_str())
    .bind(report.error_type.as_str())
    .bind(report.page.as_str())
    .bind(frames)
    .execute(&mut *tx)
    .await?;
    let cutoff = received_at.saturating_sub(RETENTION_DAYS * SECONDS_PER_DAY);
    sqlx::query("DELETE FROM client_errors WHERE received_at < ?")
        .bind(cutoff)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "DELETE FROM client_errors WHERE id NOT IN
         (SELECT id FROM client_errors ORDER BY received_at DESC, id DESC LIMIT ?)",
    )
    .bind(MAX_STORED_REPORTS)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// Newest first, excluding reports older than the retention window.
///
/// # Errors
/// The database read fails or a stored row is unreadable.
pub async fn recent(
    db: &SqlitePool,
    limit: usize,
    release: Option<&BuildId>,
) -> Result<Vec<StoredReport>, sqlx::Error> {
    let limit = i64::try_from(limit.clamp(1, MAX_READ_LIMIT)).unwrap_or(i64::MAX);
    let cutoff = crate::db::now_unix().saturating_sub(RETENTION_DAYS * SECONDS_PER_DAY);
    let rows: Vec<(i64, String, String, String, String, String)> = match release {
        Some(release) => {
            sqlx::query_as(
                "SELECT received_at, release, kind, error_type, page, frames
                 FROM client_errors WHERE received_at >= ? AND release = ?
                 ORDER BY received_at DESC, id DESC LIMIT ?",
            )
            .bind(cutoff)
            .bind(release.as_str())
            .bind(limit)
            .fetch_all(db)
            .await?
        }
        None => {
            sqlx::query_as(
                "SELECT received_at, release, kind, error_type, page, frames
                 FROM client_errors WHERE received_at >= ?
                 ORDER BY received_at DESC, id DESC LIMIT ?",
            )
            .bind(cutoff)
            .bind(limit)
            .fetch_all(db)
            .await?
        }
    };
    rows.into_iter()
        .map(|row| {
            StoredReport::from_row(row).map_err(|error| sqlx::Error::Decode(Box::new(error)))
        })
        .collect()
}
