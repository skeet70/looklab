// SPDX-License-Identifier: AGPL-3.0-or-later

/// Jobs retry a variant unless [`WorkerError::permanent`] holds.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
    #[error("database: {0}")]
    Open(#[from] looklab_server::DbError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("yaml: {0}")]
    Yaml(#[from] serde_norway::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("task: {0}")]
    Task(#[from] tokio::task::JoinError),
    #[error("fit: {0}")]
    Fit(#[from] looklab_fit::FitError),
    #[error("body: {0}")]
    Body(#[from] looklab_body::BodyError),
    #[error("samples: {0}")]
    Samples(#[from] looklab_samples::Error),
    #[error("model: {0}")]
    Model(#[from] looklab_model::ModelError),
    #[error("realization: {0}")]
    Realization(#[from] looklab_model::realization::RealizationError),
    #[error("trust: {0}")]
    Trust(#[from] looklab_trust::TrustError),
    #[error("publish: {0}")]
    Publish(#[from] looklab_publish::PublishError),
    #[error("card: {0}")]
    Card(#[from] looklab_export::card::CardError),
    #[error("lut: {0}")]
    Lut(#[from] looklab_export::LutLookError),
    #[error("manifest: {0}")]
    Admin(#[from] looklab_server::admin::AdminError),
    #[error("data: {0}")]
    Data(String),
    #[error(
        "release {version} predates the relative exposure gauge; a moderator-approved audited re-anchor and refit is required"
    )]
    MigrationRequired { version: u32 },
    #[error("job: {0}")]
    Job(String),
}

impl WorkerError {
    /// True when retrying cannot change the outcome.
    pub fn permanent(&self) -> bool {
        match self {
            Self::Data(_) | Self::Job(_) | Self::MigrationRequired { .. } | Self::Lut(_) => true,
            Self::Admin(error) => !matches!(error, looklab_server::admin::AdminError::Database(_)),
            _ => false,
        }
    }
}

pub type Result<T, E = WorkerError> = core::result::Result<T, E>;
