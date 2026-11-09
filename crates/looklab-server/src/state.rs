// SPDX-License-Identifier: AGPL-3.0-or-later
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sqlx::SqlitePool;
use tokio::sync::{RwLock, Semaphore};
use webauthn_rs::prelude::{Passkey, Url, WebauthnBuilder};
use webauthn_rs_core::{WebauthnCore, proto::RegistrationState};

use crate::blob::{BlobError, BlobStore, FsBlobStore};
use crate::body_schema::{Bodies, BodiesError};
use crate::config::{Config, ConfigError, ServerSecret};
use crate::db::{DbError, now_unix, open};
use crate::ids::{CredentialId, RandomError, RegistrationHandle, StatusToken, TokenHash};
use crate::manuals::{Manuals, ManualsError};
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
    #[error(transparent)]
    Manuals(#[from] ManualsError),
    #[error("publication request: {0}")]
    Publication(#[from] sqlx::Error),
    #[error("WebAuthn configuration: {0}")]
    Webauthn(String),
}

#[derive(Debug)]
pub struct PendingRegistration {
    created: Instant,
    promoting: bool,
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

const MAX_BATCH_ATTEMPTS: u8 = 3;
const MAX_BATCH_ENTRIES: usize = 4096;

fn upload_slots(max_body_bytes: usize) -> usize {
    let per_upload = 2 * max_body_bytes.max(1) as u64;
    usize::try_from((UPLOAD_MEMORY_BUDGET / per_upload).clamp(1, 16)).unwrap_or(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromotionError {
    Stale,
    Busy,
}

pub struct PendingPromotion {
    pending: Arc<Mutex<HashMap<RegistrationHandle, PendingRegistration>>>,
    handle: RegistrationHandle,
    passkey: Passkey,
    display_handle: Option<String>,
    committing: bool,
    finished: bool,
}

impl PendingPromotion {
    pub fn passkey(&self) -> &Passkey {
        &self.passkey
    }

    pub fn display_handle(&self) -> Option<&str> {
        self.display_handle.as_deref()
    }

    pub fn finish(mut self) {
        finish_promotion(&self.pending, &self.handle);
        self.finished = true;
    }

    /// Removes the pending entry even when the commit outcome is unknown.
    pub async fn commit(
        mut self,
        tx: sqlx::Transaction<'static, sqlx::Sqlite>,
    ) -> Result<(), sqlx::Error> {
        self.committing = true;
        let outcome = tx.commit().await;
        self.finish();
        outcome
    }
}

impl Drop for PendingPromotion {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if self.committing {
            finish_promotion(&self.pending, &self.handle);
        } else {
            cancel_promotion(&self.pending, &self.handle);
        }
    }
}

fn begin_promotion(
    pending: &Arc<Mutex<HashMap<RegistrationHandle, PendingRegistration>>>,
    original: &Passkey,
    verified: &Passkey,
) -> Result<PendingPromotion, PromotionError> {
    let Ok(mut entries) = pending.lock() else {
        return Err(PromotionError::Busy);
    };
    let original = serde_json::to_string(original).map_err(|_| PromotionError::Stale)?;
    let credential_id = verified.cred_id().as_slice();
    for (handle, entry) in entries.iter_mut() {
        if let PendingStage::Credential {
            passkey: stored,
            display_handle,
        } = &mut entry.stage
            && stored.cred_id().as_slice() == credential_id
        {
            if entry.promoting {
                return Err(PromotionError::Busy);
            }
            if serde_json::to_string(&**stored).ok().as_deref() != Some(original.as_str()) {
                return Err(PromotionError::Stale);
            }
            **stored = verified.clone();
            entry.promoting = true;
            return Ok(PendingPromotion {
                pending: Arc::clone(pending),
                handle: handle.clone(),
                passkey: verified.clone(),
                display_handle: display_handle.clone(),
                committing: false,
                finished: false,
            });
        }
    }
    Err(PromotionError::Stale)
}

fn purge_pending(pending: &mut HashMap<RegistrationHandle, PendingRegistration>, ttl: Duration) {
    pending.retain(|_, entry| entry.promoting || entry.created.elapsed() < ttl);
}

fn finish_promotion(
    pending: &Mutex<HashMap<RegistrationHandle, PendingRegistration>>,
    handle: &RegistrationHandle,
) {
    let Ok(mut entries) = pending.lock() else {
        return;
    };
    if entries.get(handle).is_some_and(|entry| entry.promoting) {
        entries.remove(handle);
    }
}

fn cancel_promotion(
    pending: &Mutex<HashMap<RegistrationHandle, PendingRegistration>>,
    handle: &RegistrationHandle,
) {
    let Ok(mut entries) = pending.lock() else {
        return;
    };
    if let Some(entry) = entries.get_mut(handle)
        && entry.promoting
    {
        entry.promoting = false;
    }
}

type BatchKey = ([u8; 32], [u8; 32]);

#[derive(Debug, Default)]
pub struct BatchAdmission {
    entries: Mutex<HashMap<BatchKey, BatchEntry>>,
}

#[derive(Debug)]
struct BatchEntry {
    in_flight: bool,
    failures: u8,
    expires_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchAdmissionError {
    InFlight,
    Exhausted,
    Capacity,
}

pub struct BatchAttempt<'a> {
    admission: &'a BatchAdmission,
    key: ([u8; 32], [u8; 32]),
    finished: bool,
}

impl BatchAdmission {
    /// One in-flight attempt per pending batch, and at most
    /// [`MAX_BATCH_ATTEMPTS`] failed attempts before a new send is required.
    pub fn try_begin(
        &self,
        nonce: &[u8; 32],
        batch: &[u8; 32],
        expires_at: i64,
    ) -> Result<BatchAttempt<'_>, BatchAdmissionError> {
        let key = (*nonce, *batch);
        let Ok(mut entries) = self.entries.lock() else {
            return Err(BatchAdmissionError::InFlight);
        };
        let now = now_unix();
        if entries.len() >= MAX_BATCH_ENTRIES {
            entries.retain(|_, entry| entry.in_flight || entry.expires_at >= now);
        }
        if !entries.contains_key(&key) && entries.len() >= MAX_BATCH_ENTRIES {
            return Err(BatchAdmissionError::Capacity);
        }
        let entry = entries.entry(key).or_insert(BatchEntry {
            in_flight: false,
            failures: 0,
            expires_at,
        });
        entry.expires_at = entry.expires_at.max(expires_at);
        if entry.in_flight {
            return Err(BatchAdmissionError::InFlight);
        }
        if entry.failures >= MAX_BATCH_ATTEMPTS {
            return Err(BatchAdmissionError::Exhausted);
        }
        entry.in_flight = true;
        Ok(BatchAttempt {
            admission: self,
            key,
            finished: false,
        })
    }

    fn record_failure(&self, key: ([u8; 32], [u8; 32])) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        if let Some(entry) = entries.get_mut(&key) {
            entry.failures = entry.failures.saturating_add(1);
            entry.in_flight = false;
        }
    }
}

impl BatchAttempt<'_> {
    pub fn succeed(mut self) {
        if let Ok(mut entries) = self.admission.entries.lock() {
            entries.remove(&self.key);
        }
        self.finished = true;
    }
}

impl Drop for BatchAttempt<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.admission.record_failure(self.key);
        }
    }
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
    pub bodies: RwLock<Bodies>,
    pub manuals: Arc<Manuals>,
    pub rate_limiter: RateLimiter,
    pub trust: Arc<dyn TrustPolicy>,
    pub upload_slots: Arc<Semaphore>,
    pub mod_sessions: ModSessions,
    pub batch_admission: BatchAdmission,
    pending: Arc<Mutex<HashMap<RegistrationHandle, PendingRegistration>>>,
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
        let bodies = RwLock::new(Bodies::load(&config.bodies_dir, &config.snapshot_dir)?);
        let manuals = Arc::new(Manuals::load(&config.manuals_dir)?);
        {
            let mut conn = db.acquire().await?;
            crate::store::request_publication(&mut conn).await?;
        }
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
            manuals,
            rate_limiter: RateLimiter::default(),
            trust,
            upload_slots: Arc::new(Semaphore::new(slots)),
            mod_sessions: ModSessions::default(),
            batch_admission: BatchAdmission::default(),
            pending: Arc::new(Mutex::new(HashMap::new())),
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
                promoting: false,
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
                promoting: false,
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

    /// Advances the pending passkey `original` was verified against and
    /// reserves it until the returned token finishes or drops.
    pub fn begin_promotion(
        &self,
        original: &Passkey,
        verified: &Passkey,
    ) -> Result<PendingPromotion, PromotionError> {
        begin_promotion(&self.pending, original, verified)
    }

    pub fn remove_pending_credential(&self, credential_id: &[u8]) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        pending.retain(|_, entry| match &entry.stage {
            PendingStage::Credential { passkey, .. } => {
                entry.promoting || passkey.cred_id().as_slice() != credential_id
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
        purge_pending(pending, self.config.registration_ttl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use webauthn_rs_core::proto::{
        AttestationFormat, COSEAlgorithm, COSEEC2Key, COSEKey, COSEKeyType, Credential, ECDSACurve,
        ParsedAttestation, RegisteredExtensions, UserVerificationPolicy,
    };

    fn passkey(counter: u32, backup_state: bool) -> Passkey {
        Credential {
            cred_id: vec![1, 2, 3, 4],
            cred: COSEKey {
                type_: COSEAlgorithm::ES256,
                key: COSEKeyType::EC_EC2(COSEEC2Key {
                    curve: ECDSACurve::SECP256R1,
                    x: vec![0; 32],
                    y: vec![0; 32],
                }),
            },
            counter,
            transports: None,
            user_verified: true,
            backup_eligible: false,
            backup_state,
            registration_policy: UserVerificationPolicy::Required,
            extensions: RegisteredExtensions::none(),
            attestation: ParsedAttestation::default(),
            attestation_format: AttestationFormat::None,
        }
        .into()
    }

    fn pending(passkey: Passkey) -> Arc<Mutex<HashMap<RegistrationHandle, PendingRegistration>>> {
        let handle = RegistrationHandle::generate().expect("handle");
        Arc::new(Mutex::new(HashMap::from([(
            handle,
            PendingRegistration {
                created: Instant::now(),
                promoting: false,
                stage: PendingStage::Credential {
                    passkey: Box::new(passkey),
                    display_handle: None,
                },
            },
        )])))
    }

    fn key(index: usize) -> ([u8; 32], [u8; 32]) {
        let index = u64::try_from(index).expect("index");
        let mut nonce = [0u8; 32];
        nonce[..8].copy_from_slice(&index.to_be_bytes());
        let mut batch = [0u8; 32];
        batch[8..16].copy_from_slice(&index.to_be_bytes());
        (nonce, batch)
    }

    #[test]
    fn an_exhausted_batch_survives_admission_churn() {
        let admission = BatchAdmission::default();
        let expires = now_unix() + 3600;
        let exhausted = key(0);
        for _ in 0..MAX_BATCH_ATTEMPTS {
            drop(
                admission
                    .try_begin(&exhausted.0, &exhausted.1, expires)
                    .expect("admission"),
            );
        }
        assert_eq!(
            admission
                .try_begin(&exhausted.0, &exhausted.1, expires)
                .err(),
            Some(BatchAdmissionError::Exhausted)
        );

        let mut index = 1;
        loop {
            let (nonce, batch) = key(index);
            match admission.try_begin(&nonce, &batch, expires) {
                Ok(attempt) => drop(attempt),
                Err(BatchAdmissionError::Capacity) => break,
                Err(error) => panic!("unexpected admission error: {error:?}"),
            }
            index += 1;
        }

        assert_eq!(
            admission
                .try_begin(&exhausted.0, &exhausted.1, expires)
                .err(),
            Some(BatchAdmissionError::Exhausted)
        );
        let (nonce, batch) = key(index + 1);
        assert_eq!(
            admission.try_begin(&nonce, &batch, expires).err(),
            Some(BatchAdmissionError::Capacity)
        );
    }

    #[test]
    fn a_pending_promotion_rejects_concurrent_mutation() {
        let original = passkey(1, false);
        let pending = pending(original.clone());
        let promotion =
            begin_promotion(&pending, &original, &passkey(2, false)).expect("promotion");
        assert_eq!(
            begin_promotion(&pending, &original, &passkey(3, false)).err(),
            Some(PromotionError::Busy)
        );
        drop(promotion);
        assert!(begin_promotion(&pending, &passkey(2, false), &passkey(3, false)).is_ok());
    }

    #[test]
    fn a_stale_pending_promotion_is_rejected() {
        let original = passkey(1, false);
        let pending = pending(original.clone());
        let promotion =
            begin_promotion(&pending, &original, &passkey(2, false)).expect("promotion");
        drop(promotion);
        assert_eq!(
            begin_promotion(&pending, &original, &passkey(3, false)).err(),
            Some(PromotionError::Stale)
        );
    }

    #[test]
    fn a_finished_pending_promotion_is_removed() {
        let original = passkey(1, false);
        let pending = pending(original.clone());
        begin_promotion(&pending, &original, &passkey(2, false))
            .expect("promotion")
            .finish();
        assert_eq!(
            begin_promotion(&pending, &passkey(2, false), &passkey(3, false)).err(),
            Some(PromotionError::Stale)
        );
    }

    #[test]
    fn a_dropped_committing_promotion_removes_the_shadow() {
        let original = passkey(1, false);
        let pending = pending(original.clone());
        let mut promotion =
            begin_promotion(&pending, &original, &passkey(2, false)).expect("promotion");
        promotion.committing = true;
        drop(promotion);
        assert!(pending.lock().expect("pending").is_empty());
    }

    #[test]
    fn a_zero_counter_pending_promotion_is_applied() {
        let original = passkey(0, false);
        let pending = pending(original.clone());
        assert!(begin_promotion(&pending, &original, &passkey(0, true)).is_ok());
    }

    #[test]
    fn a_promoting_entry_survives_expiry_until_the_promotion_ends() {
        let original = passkey(1, false);
        let pending = pending(original.clone());
        let ttl = Duration::from_secs(60);
        {
            let mut entries = pending.lock().expect("pending");
            for entry in entries.values_mut() {
                entry.created = Instant::now() - ttl - Duration::from_secs(1);
            }
        }
        let promotion =
            begin_promotion(&pending, &original, &passkey(2, false)).expect("promotion");
        {
            let mut entries = pending.lock().expect("pending");
            purge_pending(&mut entries, ttl);
            assert_eq!(entries.len(), 1);
        }
        drop(promotion);
        {
            let mut entries = pending.lock().expect("pending");
            purge_pending(&mut entries, ttl);
            assert!(entries.is_empty());
        }
    }
}
