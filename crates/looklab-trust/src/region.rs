// SPDX-License-Identifier: AGPL-3.0-or-later
//! The 12 hue × 6 exposure cell grid over supported probes ([docs/05 §6]).

use core::fmt;
use core::str::FromStr;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Result, TrustError};

pub const HUE_CELLS: u8 = 12;

pub const EXPOSURE_CELLS: u8 = 6;

/// Probe exposure range in stops, matching the Look input clamp.
pub const EXPOSURE_MIN: f64 = -12.0;

pub const EXPOSURE_MAX: f64 = 6.0;

const EXPOSURE_STEP: f64 = (EXPOSURE_MAX - EXPOSURE_MIN) / EXPOSURE_CELLS as f64;

const HUE_STEP_DEGREES: f64 = 360.0 / HUE_CELLS as f64;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Region {
    hue: u8,
    exposure: u8,
}

impl Region {
    pub fn new(hue: u8, exposure: u8) -> Result<Self> {
        if hue >= HUE_CELLS || exposure >= EXPOSURE_CELLS {
            return Err(TrustError::InvalidRegion { hue, exposure });
        }
        Ok(Self { hue, exposure })
    }

    /// Cell containing a probe at exposure stop `s` and input hue `h` in degrees.
    pub fn from_probe(s: f64, h: f64) -> Result<Self> {
        if !s.is_finite() || !h.is_finite() || !(EXPOSURE_MIN..=EXPOSURE_MAX).contains(&s) {
            return Err(TrustError::ProbeOutOfRange);
        }
        let hue_degrees = h - 360.0 * (h / 360.0).floor();
        let hue = (hue_degrees / HUE_STEP_DEGREES).floor();
        let exposure = ((s - EXPOSURE_MIN) / EXPOSURE_STEP).floor();
        Self::new(
            clamp_cell(hue, HUE_CELLS),
            clamp_cell(exposure, EXPOSURE_CELLS),
        )
    }

    pub const fn hue(self) -> u8 {
        self.hue
    }

    pub const fn exposure(self) -> u8 {
        self.exposure
    }
}

fn clamp_cell(value: f64, cells: u8) -> u8 {
    if value < 0.0 {
        0
    } else if value >= cells as f64 - 0.5 {
        cells - 1
    } else {
        value as u8
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "h{}/e{}", self.hue, self.exposure)
    }
}

impl fmt::Debug for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for Region {
    type Err = TrustError;

    fn from_str(text: &str) -> Result<Self> {
        let (hue, exposure) = text
            .split_once('/')
            .ok_or_else(|| TrustError::InvalidId(text.to_owned()))?;
        let hue = hue
            .strip_prefix('h')
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| TrustError::InvalidId(text.to_owned()))?;
        let exposure = exposure
            .strip_prefix('e')
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| TrustError::InvalidId(text.to_owned()))?;
        Self::new(hue, exposure)
    }
}

impl Serialize for Region {
    fn serialize<S: Serializer>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Region {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> core::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(D::Error::custom)
    }
}

/// An image's share of its samples per region. Shares sum to 1.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RegionCoverage(std::collections::BTreeMap<Region, f64>);

impl RegionCoverage {
    /// Splits one image's samples evenly over the regions it covers.
    pub fn uniform(regions: impl IntoIterator<Item = Region>) -> Result<Self> {
        let regions: Vec<Region> = regions.into_iter().collect();
        if regions.is_empty() {
            return Err(TrustError::EmptyCoverage);
        }
        let share = 1.0 / regions.len() as f64;
        Self::from_shares(regions.into_iter().map(|region| (region, share)))
    }

    pub fn from_shares(shares: impl IntoIterator<Item = (Region, f64)>) -> Result<Self> {
        let mut map = std::collections::BTreeMap::new();
        let mut total = 0.0;
        for (region, share) in shares {
            if !share.is_finite() || share < 0.0 {
                return Err(TrustError::InvalidShare);
            }
            *map.entry(region).or_insert(0.0) += share;
            total += share;
        }
        if map.is_empty() {
            return Err(TrustError::EmptyCoverage);
        }
        if total <= 0.0 {
            return Err(TrustError::EmptyShares(total));
        }
        Ok(Self(
            map.into_iter()
                .map(|(region, share)| (region, share / total))
                .collect(),
        ))
    }

    pub fn share(&self, region: Region) -> f64 {
        self.0.get(&region).copied().unwrap_or(0.0)
    }

    pub fn iter(&self) -> impl Iterator<Item = (Region, f64)> + '_ {
        self.0.iter().map(|(&region, &share)| (region, share))
    }

    pub fn regions(&self) -> impl Iterator<Item = Region> + '_ {
        self.0.keys().copied()
    }
}
