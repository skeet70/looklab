// SPDX-License-Identifier: AGPL-3.0-or-later
//! In-memory token buckets. Never keyed by an address.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use sha2::Digest as _;

use crate::ids::RawCredentialId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Upload,
    StatusReissue,
    Withdraw,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RateKey {
    Nonce,
    GlobalUpload,
    Assertion { action: Action, id: RawCredentialId },
}

impl core::fmt::Display for RateKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let action = |action: Action| match action {
            Action::Upload => "upload",
            Action::StatusReissue => "status-reissue",
            Action::Withdraw => "withdraw",
        };
        match self {
            Self::Nonce => f.write_str("nonce"),
            Self::GlobalUpload => f.write_str("upload"),
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
pub struct Quota {
    pub capacity: f64,
    pub refill_per_second: f64,
}

pub const NONCE_QUOTA: Quota = Quota {
    capacity: 60.0,
    refill_per_second: 1.0,
};
pub const GLOBAL_UPLOAD_QUOTA: Quota = Quota {
    capacity: 600.0,
    refill_per_second: 10.0,
};
pub const CREDENTIAL_UPLOAD_QUOTA: Quota = Quota {
    capacity: 30.0,
    refill_per_second: 0.2,
};
pub const CREDENTIAL_SIGNED_QUOTA: Quota = Quota {
    capacity: 20.0,
    refill_per_second: 0.1,
};

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    pub fn check(&self, key: &RateKey, quota: Quota) -> bool {
        let now = Instant::now();
        let Ok(mut buckets) = self.buckets.lock() else {
            return false;
        };
        if buckets.len() > 4096 {
            buckets.retain(|_, bucket| {
                bucket.tokens + bucket.last.elapsed().as_secs_f64() * quota.refill_per_second
                    < quota.capacity
            });
        }
        let bucket = buckets.entry(key.to_string()).or_insert_with(|| Bucket {
            tokens: quota.capacity,
            last: now,
        });
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * quota.refill_per_second).min(quota.capacity);
        bucket.last = now;
        if bucket.tokens < 1.0 {
            return false;
        }
        bucket.tokens -= 1.0;
        true
    }
}
