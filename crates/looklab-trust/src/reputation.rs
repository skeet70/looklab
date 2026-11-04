// SPDX-License-Identifier: AGPL-3.0-or-later
//! Reputation updates and decay ([docs/05 §4]).

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Result, TrustError};
use crate::params::TrustParams;

pub const REPUTATION_MIN: f64 = 0.2;

pub const REPUTATION_MAX: f64 = 1.5;

pub const REPUTATION_NEUTRAL: f64 = 0.5;

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Reputation(f64);

impl Reputation {
    /// Clamps `value` into the documented range.
    pub fn new(value: f64) -> Result<Self> {
        if !value.is_finite() {
            return Err(TrustError::InvalidReputation(value));
        }
        Ok(Self(value.clamp(REPUTATION_MIN, REPUTATION_MAX)))
    }

    /// A non-finite value becomes [`REPUTATION_NEUTRAL`].
    pub fn saturating(value: f64) -> Self {
        if value.is_finite() {
            Self(value.clamp(REPUTATION_MIN, REPUTATION_MAX))
        } else {
            Self(REPUTATION_NEUTRAL)
        }
    }

    pub const fn value(self) -> f64 {
        self.0
    }

    /// Decays over the elapsed days, then applies the release's reward and penalties.
    pub fn updated(self, params: &TrustParams, update: ReputationUpdate) -> Self {
        let neutral = params.reputation_neutral;
        let elapsed = update.days_elapsed;
        let decayed =
            if elapsed.is_finite() && elapsed > 0.0 && params.reputation_half_life_days > 0.0 {
                neutral
                    + (self.0 - neutral)
                        * libm::pow(0.5, elapsed / params.reputation_half_life_days)
            } else {
                self.0
            };
        let accepted = {
            if update.accepted_fraction.is_finite() {
                update.accepted_fraction.clamp(0.0, 1.0)
            } else {
                0.0
            }
        };
        let delta = params.reputation_eta * (accepted - 0.5)
            - params.penalty_deliberate * f64::from(update.deliberate_quarantines)
            - params.penalty_automatic * f64::from(update.automatic_quarantines)
            + params.chart_session_bonus * f64::from(update.passing_chart_sessions);
        Self((decayed + delta).clamp(params.reputation_min, params.reputation_max))
    }
}

impl Serialize for Reputation {
    fn serialize<S: Serializer>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error> {
        serializer.serialize_f64(self.0)
    }
}

impl<'de> Deserialize<'de> for Reputation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> core::result::Result<Self, D::Error> {
        let value = f64::deserialize(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

/// The terms of one reputation update after a release.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReputationUpdate {
    /// Fraction of the contributor's images accepted without down-weighting.
    pub accepted_fraction: f64,
    pub deliberate_quarantines: u32,
    pub automatic_quarantines: u32,
    pub passing_chart_sessions: u32,
    /// Days since the previous update. The caller owns the clock.
    pub days_elapsed: f64,
}
