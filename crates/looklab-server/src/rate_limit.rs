// SPDX-License-Identifier: AGPL-3.0-or-later
//! In-memory token buckets. Never keyed by an address.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Instant;

use sha2::Digest as _;

use crate::ids::RawCredentialId;

const MAX_BUCKETS: usize = 16_384;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Upload,
    StatusReissue,
    Withdraw,
    Admin,
    Endorse,
    Root,
    ModSession,
    Proposal,
    Confirmation,
    ProposalThread,
    FixtureUpload,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RateKey {
    Nonce,
    Registration,
    PublicRead,
    GlobalUpload,
    GlobalSigned,
    Assertion { action: Action, id: RawCredentialId },
}

impl RateKey {
    const fn quota(&self) -> Quota {
        match self {
            Self::Nonce => Quota {
                capacity: 300.0,
                refill_per_second: 5.0,
            },
            Self::Registration => Quota {
                capacity: 60.0,
                refill_per_second: 1.0,
            },
            Self::GlobalUpload => Quota {
                capacity: 60.0,
                refill_per_second: 2.0,
            },
            Self::GlobalSigned => Quota {
                capacity: 120.0,
                refill_per_second: 4.0,
            },
            Self::PublicRead => Quota {
                capacity: 120.0,
                refill_per_second: 8.0,
            },
            Self::Assertion { action, .. } => match action {
                Action::Upload => Quota {
                    capacity: 30.0,
                    refill_per_second: 0.2,
                },
                Action::StatusReissue
                | Action::Withdraw
                | Action::Admin
                | Action::Endorse
                | Action::Root
                | Action::ModSession => Quota {
                    capacity: 20.0,
                    refill_per_second: 0.1,
                },
                Action::Proposal => Quota {
                    capacity: 10.0,
                    refill_per_second: 0.02,
                },
                Action::Confirmation => Quota {
                    capacity: 30.0,
                    refill_per_second: 0.1,
                },
                Action::ProposalThread => Quota {
                    capacity: 60.0,
                    refill_per_second: 1.0,
                },
                Action::FixtureUpload => Quota {
                    capacity: 10.0,
                    refill_per_second: 0.05,
                },
            },
        }
    }
}

impl core::fmt::Display for RateKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let action = |action: Action| match action {
            Action::Upload => "upload",
            Action::StatusReissue => "status-reissue",
            Action::Withdraw => "withdraw",
            Action::Admin => "admin",
            Action::Endorse => "endorse",
            Action::Root => "root",
            Action::ModSession => "mod-session",
            Action::Proposal => "proposal",
            Action::Confirmation => "confirmation",
            Action::ProposalThread => "proposal-thread",
            Action::FixtureUpload => "fixture-upload",
        };
        match self {
            Self::Nonce => f.write_str("nonce"),
            Self::Registration => f.write_str("registration"),
            Self::PublicRead => f.write_str("public-read"),
            Self::GlobalUpload => f.write_str("upload"),
            Self::GlobalSigned => f.write_str("signed"),
            Self::Assertion { action: a, id } => {
                let digest = sha2::Sha256::digest(id.as_slice());
                write!(f, "{}:assertion:{}", action(*a), hex(&digest[..8]))
            }
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[derive(Debug, Clone, Copy)]
struct Quota {
    capacity: f64,
    refill_per_second: f64,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
    quota: Quota,
}

#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: Mutex<Buckets>,
}

#[derive(Debug, Default)]
struct Buckets {
    buckets: HashMap<String, Bucket>,
    order: VecDeque<String>,
}

impl RateLimiter {
    pub fn check(&self, key: &RateKey) -> bool {
        let quota = key.quota();
        let now = Instant::now();
        let Ok(mut state) = self.buckets.lock() else {
            return false;
        };
        let name = key.to_string();
        if !state.buckets.contains_key(&name) {
            state.evict_one();
            state.buckets.insert(
                name.clone(),
                Bucket {
                    tokens: quota.capacity,
                    last: now,
                    quota,
                },
            );
            state.order.push_back(name.clone());
        }
        let bucket = state
            .buckets
            .get_mut(&name)
            .expect("bucket inserted or present");
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens =
            (bucket.tokens + elapsed * bucket.quota.refill_per_second).min(bucket.quota.capacity);
        bucket.last = now;
        if bucket.tokens < 1.0 {
            return false;
        }
        bucket.tokens -= 1.0;
        true
    }
}

impl Buckets {
    fn evict_one(&mut self) {
        while self.buckets.len() >= MAX_BUCKETS {
            let Some(oldest) = self.order.pop_front() else {
                self.buckets.clear();
                return;
            };
            self.buckets.remove(&oldest);
        }
    }
}
