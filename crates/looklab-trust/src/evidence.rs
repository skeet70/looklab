// SPDX-License-Identifier: AGPL-3.0-or-later
//! Image evidence from leave-one-out sample coverage ([docs/05 §5]).

use crate::error::{Result, TrustError};
use crate::params::TrustParams;
use crate::region::{EXPOSURE_MAX, EXPOSURE_MIN};

/// Upper bound of the square-root-chroma axis in the coverage grid.
pub const MAX_SQRT_CHROMA: f64 = 10.0;

/// Binned sample counts over `(s, h, √C)`.
#[derive(Clone, Debug, PartialEq)]
pub struct EvidenceGrid {
    s_bins: usize,
    hue_bins: usize,
    chroma_bins: usize,
    counts: Vec<f64>,
}

impl Default for EvidenceGrid {
    fn default() -> Self {
        const S_BINS: usize = 36;
        const HUE_BINS: usize = 24;
        const CHROMA_BINS: usize = 10;
        Self {
            s_bins: S_BINS,
            hue_bins: HUE_BINS,
            chroma_bins: CHROMA_BINS,
            counts: vec![0.0; S_BINS * HUE_BINS * CHROMA_BINS],
        }
    }
}

impl EvidenceGrid {
    pub fn new(s_bins: usize, hue_bins: usize, chroma_bins: usize) -> Result<Self> {
        if s_bins == 0 || hue_bins == 0 || chroma_bins == 0 {
            return Err(TrustError::EmptyCoverage);
        }
        Ok(Self {
            s_bins,
            hue_bins,
            chroma_bins,
            counts: vec![0.0; s_bins * hue_bins * chroma_bins],
        })
    }

    pub fn add(&mut self, s: f64, hue: f64, sqrt_chroma: f64, weight: f64) -> Result<()> {
        if !(s.is_finite() && hue.is_finite() && sqrt_chroma.is_finite() && weight.is_finite()) {
            return Err(TrustError::ProbeOutOfRange);
        }
        if weight < 0.0 {
            return Err(TrustError::InvalidShare);
        }
        let s_index = self.cell(s, EXPOSURE_MIN, EXPOSURE_MAX, self.s_bins);
        let hue_index = self.cell(hue, 0.0, 360.0, self.hue_bins);
        let chroma_index = self.cell(sqrt_chroma, 0.0, MAX_SQRT_CHROMA, self.chroma_bins);
        let index = (s_index * self.hue_bins + hue_index) * self.chroma_bins + chroma_index;
        self.counts[index] += weight;
        Ok(())
    }

    pub fn add_samples(
        &mut self,
        samples: impl IntoIterator<Item = (f64, f64, f64)>,
    ) -> Result<()> {
        for (s, hue, sqrt_chroma) in samples {
            self.add(s, hue, sqrt_chroma, 1.0)?;
        }
        Ok(())
    }

    pub fn total(&self) -> f64 {
        self.counts.iter().sum()
    }

    /// Counts added as whole weights are exact, so a union minus one image reproduces
    /// that image's leave-one-out grid.
    pub fn subtract(&mut self, other: &EvidenceGrid) -> Result<()> {
        if self.bins() != other.bins() {
            return Err(TrustError::InvalidShare);
        }
        if self
            .counts
            .iter()
            .zip(&other.counts)
            .any(|(count, remove)| *count < *remove)
        {
            return Err(TrustError::InvalidShare);
        }
        for (count, remove) in self.counts.iter_mut().zip(&other.counts) {
            *count -= remove;
        }
        Ok(())
    }

    pub fn bins(&self) -> (usize, usize, usize) {
        (self.s_bins, self.hue_bins, self.chroma_bins)
    }

    fn cell(&self, value: f64, low: f64, high: f64, bins: usize) -> usize {
        let position = ((value - low) / (high - low) * bins as f64).floor();
        if position <= 0.0 {
            0
        } else if position >= bins as f64 - 0.5 {
            bins - 1
        } else {
            position as usize
        }
    }
}

/// Total-variation distance between an image's sample distribution and the rest of the
/// pool's, both over the same grid. `0` means the image lies where the pool already has
/// data, `1` means disjoint support.
pub fn coverage_gain(rest: &EvidenceGrid, image: &EvidenceGrid) -> Result<f64> {
    if rest.bins() != image.bins() {
        return Err(TrustError::InvalidShare);
    }
    let image_total = image.total();
    if image_total <= 0.0 {
        return Ok(0.0);
    }
    let rest_total = rest.total();
    if rest_total <= 0.0 {
        return Ok(1.0);
    }
    let overlap: f64 = rest
        .counts
        .iter()
        .zip(&image.counts)
        .map(|(rest_count, image_count)| {
            let rest_share = rest_count / rest_total;
            let image_share = image_count / image_total;
            if rest_share < image_share {
                rest_share
            } else {
                image_share
            }
        })
        .sum();
    Ok((1.0 - overlap).clamp(0.0, 1.0))
}

/// `evidence = coverage_gain^0.5 × (1 − clip_fraction)`, clamped to the configured range.
pub fn evidence(coverage_gain: f64, clip_fraction: f64, params: &TrustParams) -> f64 {
    let gain = coverage_gain.clamp(0.0, 1.0);
    let clip = {
        if clip_fraction.is_finite() {
            clip_fraction.clamp(0.0, 1.0)
        } else {
            0.0
        }
    };
    (gain.sqrt() * (1.0 - clip)).clamp(params.evidence_min, params.evidence_max)
}
