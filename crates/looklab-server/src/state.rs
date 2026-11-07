// SPDX-License-Identifier: AGPL-3.0-or-later
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sqlx::SqlitePool;
use tokio::sync::Semaphore;
use webauthn_rs::prelude::{Passkey, Url, WebauthnBuilder};
use webauthn_rs_core::{WebauthnCore, proto::RegistrationState};

use crate::blob::{BlobError, BlobStore, FsBlobStore};
use crate::body_schema::{Bodies, BodiesError};
use crate::config::{Config, ConfigError, ServerSecret};
use crate::db::{DbError, open};
use crate::ids::{CredentialId, RandomError, RegistrationHandle, StatusToken, TokenHash};
use crate::rate_limit::RateLimiter;
use crate::trust::{DefaultTrustPolicy, TrustPolicy};

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Blob(#[from] BlobError),
    #[error(transparent)]
    Bodies(#[from] BodiesError),
    #[error("WebAuthn configuration: {0}")]
    Webauthn(String),
}

#[derive(Debug)]
pub struct PendingRegistration {
    created: Instant,
    stage: PendingStage,
}

#[derive(Debug)]
enum PendingStage {
    Challenge {
        registration: Box<RegistrationState>,
        display_handle: Option<String>,
    },
    Credential {
        passkey: Box<Passkey>,
        display_handle: Option<String>,
    },
}

const MAX_PENDING_REGISTRATIONS: usize = 1024;
const UPLOAD_MEMORY_BUDGET: u64 = 1 << 30;

fn upload_slots(max_body_bytes: usize) -> usize {
    let per_upload = 2 * max_body_bytes.max(1) as u64;
    usize::try_from((UPLOAD_MEMORY_BUDGET / per_upload).clamp(1, 16)).unwrap_or(1)
}

pub const MOD_SESSION_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_MOD_SESSIONS: usize = 1024;

#[derive(Debug)]
struct ModSessionEntry {
    credential: CredentialId,
    expires: Instant,
}

/// Moderator sessions opened by a passkey assertion.
#[derive(Debug, Default)]
pub struct ModSessions {
    sessions: Mutex<HashMap<TokenHash, ModSessionEntry>>,
}

impl ModSessions {
    pub fn open(&self, credential: CredentialId) -> Result<StatusToken, RandomError> {
        let token = StatusToken::generate()?;
        let Ok(mut sessions) = self.sessions.lock() else {
            return Err(RandomError);
        };
        let now = Instant::now();
        sessions.retain(|_, entry| entry.expires > now);
        while sessions.len() >= MAX_MOD_SESSIONS {
            let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, entry)| entry.expires)
                .map(|(hash, _)| *hash)
            else {
                break;
            };
            sessions.remove(&oldest);
        }
        sessions.insert(
            token.hash(),
            ModSessionEntry {
                credential,
                expires: now + MOD_SESSION_TTL,
            },
        );
        Ok(token)
    }

    pub fn authenticate(&self, token: &StatusToken) -> Option<CredentialId> {
        let Ok(mut sessions) = self.sessions.lock() else {
            return None;
        };
        let now = Instant::now();
        sessions.retain(|_, entry| entry.expires > now);
        sessions
            .get(&token.hash())
            .map(|entry| entry.credential.clone())
    }

    pub fn close(&self, token: &StatusToken) {
        let Ok(mut sessions) = self.sessions.lock() else {
            return;
        };
        sessions.remove(&token.hash());
    }

    pub fn close_credential(&self, credential: &CredentialId) {
        let Ok(mut sessions) = self.sessions.lock() else {
            return;
        };
        sessions.retain(|_, entry| &entry.credential != credential);
    }
}

pub struct AppState {
    pub config: Config,
    pub db: SqlitePool,
    pub blobs: Arc<dyn BlobStore>,
    pub webauthn: WebauthnCore,
    pub secret: ServerSecret,
    pub bodies: Bodies,
    pub rate_limiter: RateLimiter,
    pub trust: Arc<dyn TrustPolicy>,
    pub upload_slots: Arc<Semaphore>,
    pub mod_sessions: ModSessions,
    pending: Mutex<HashMap<RegistrationHandle, PendingRegistration>>,
}

impl core::fmt::Debug for AppState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AppState")
            .field("db", &self.db)
            .field("bodies", &self.bodies)
            .field("webauthn", &self.webauthn)
            .finish_non_exhaustive()
    }
}

impl AppState {
    pub async fn new(mut config: Config) -> Result<Self, StateError> {
        config.normalize_origin()?;
        config.validate()?;
        let db = open(&config.db_path).await?;
        let blobs: Arc<dyn BlobStore> = Arc::new(FsBlobStore::new(&config.blob_dir)?);
        let bodies = Bodies::load(&config.bodies_dir, &config.snapshot_dir)?;
        let secret = ServerSecret::load_or_create(&config.server_secret_path)?;
        let origin =
            Url::parse(&config.origin).map_err(|error| StateError::Webauthn(error.to_string()))?;
        WebauthnBuilder::new(&config.rp_id, &origin)
            .map_err(|error| StateError::Webauthn(error.to_string()))?;
        let webauthn = WebauthnCore::new_unsafe_experts_only(
            "looklab",
            &config.rp_id,
            vec![origin],
            Duration::from_secs(300),
            Some(false),
            Some(false),
        );
        let trust = Arc::new(DefaultTrustPolicy::new(config.pow));
        let slots = upload_slots(config.limits.max_body_bytes);
        Ok(Self {
            config,
            db,
            blobs,
            webauthn,
            secret,
            bodies,
            rate_limiter: RateLimiter::default(),
            trust,
            upload_slots: Arc::new(Semaphore::new(slots)),
            mod_sessions: ModSessions::default(),
            pending: Mutex::new(HashMap::new()),
        })
    }

    pub fn insert_challenge(
        &self,
        handle: RegistrationHandle,
        registration: RegistrationState,
        display_handle: Option<String>,
    ) -> bool {
        let Ok(mut pending) = self.pending.lock() else {
            return false;
        };
        self.purge_locked(&mut pending);
        if pending.len() >= MAX_PENDING_REGISTRATIONS {
            return false;
        }
        pending.insert(
            handle,
            PendingRegistration {
                created: Instant::now(),
                stage: PendingStage::Challenge {
                    registration: Box::new(registration),
                    display_handle,
                },
            },
        );
        true
    }

    pub fn take_challenge(
        &self,
        handle: &RegistrationHandle,
    ) -> Option<(RegistrationState, Option<String>)> {
        let Ok(mut pending) = self.pending.lock() else {
            return None;
        };
        self.purge_locked(&mut pending);
        match pending.remove(handle) {
            Some(PendingRegistration {
                stage:
                    PendingStage::Challenge {
                        registration,
                        display_handle,
                    },
                ..
            }) => Some((*registration, display_handle)),
            other => {
                if let Some(other) = other {
                    pending.insert(handle.clone(), other);
                }
                None
            }
        }
    }

    pub fn insert_credential(
        &self,
        handle: RegistrationHandle,
        passkey: Passkey,
        display_handle: Option<String>,
    ) -> bool {
        let Ok(mut pending) = self.pending.lock() else {
            return false;
        };
        self.purge_locked(&mut pending);
        let duplicate = pending.values().any(|entry| match &entry.stage {
            PendingStage::Credential {
                passkey: existing, ..
            } => existing.cred_id().as_slice() == passkey.cred_id().as_slice(),
            PendingStage::Challenge { .. } => false,
        });
        if duplicate {
            return false;
        }
        pending.insert(
            handle,
            PendingRegistration {
                created: Instant::now(),
                stage: PendingStage::Credential {
                    passkey: Box::new(passkey),
                    display_handle,
                },
            },
        );
        true
    }

    pub fn pending_credential(&self, credential_id: &[u8]) -> Option<(Passkey, Option<String>)> {
        let Ok(mut pending) = self.pending.lock() else {
            return None;
        };
        self.purge_locked(&mut pending);
        pending.values().find_map(|entry| match &entry.stage {
            PendingStage::Credential {
                passkey,
                display_handle,
            } if passkey.cred_id().as_slice() == credential_id => {
                Some((passkey.as_ref().clone(), display_handle.clone()))
            }
            _ => None,
        })
    }

    pub fn remove_pending_credential(&self, credential_id: &[u8]) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        pending.retain(|_, entry| match &entry.stage {
            PendingStage::Credential { passkey, .. } => {
                passkey.cred_id().as_slice() != credential_id
            }
            PendingStage::Challenge { .. } => true,
        });
    }

    pub fn pending_len(&self) -> usize {
        self.pending
            .lock()
            .map(|pending| pending.len())
            .unwrap_or(0)
    }

    fn purge_locked(&self, pending: &mut HashMap<RegistrationHandle, PendingRegistration>) {
        let ttl = self.config.registration_ttl;
        pending.retain(|_, entry| entry.created.elapsed() < ttl);
    }
}
