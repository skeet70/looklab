// SPDX-License-Identifier: AGPL-3.0-or-later
//! A 2D chromaticity histogram of prepared samples, for the live coverage map.

use serde::Serialize;

use crate::error::FacadeError;

pub const COVERAGE_BINS: u32 = 32;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CoverageMap {
    pub bins: u32,
    pub counts: Vec<u32>,
    pub max: u32,
    pub samples: u64,
    pub images: usize,
}

/// Counts prepared samples into `r/(r+g+b)`, `g/(r+g+b)` bins.
///
/// # Errors
/// Rejects a sample blob the bounded reader rejects.
pub fn coverage_map(blobs: &[Vec<u8>]) -> Result<CoverageMap, FacadeError> {
    let bins = COVERAGE_BINS as usize;
    let mut counts = vec![0_u32; bins * bins];
    let mut samples = 0_u64;
    for blob in blobs {
        let set = looklab_samples::decode(blob).map_err(|error| FacadeError::Reply(error.to_string()))?;
        for rgb in set.out_rgb() {
            let sum = rgb[0] + rgb[1] + rgb[2];
            if !(sum > 0.0) || !sum.is_finite() {
                continue;
            }
            let x = (rgb[0] / sum).clamp(0.0, 1.0);
            let y = (rgb[1] / sum).clamp(0.0, 1.0);
            let bx = ((x * bins as f32) as usize).min(bins - 1);
            let by = ((y * bins as f32) as usize).min(bins - 1);
            counts[by * bins + bx] += 1;
            samples += 1;
        }
    }
    let max = counts.iter().copied().max().unwrap_or(0);
    Ok(CoverageMap {
        bins: COVERAGE_BINS,
        counts,
        max,
        samples,
        images: blobs.len(),
    })
}

/// Downsampled output RGB values for the payload inspector canvases.
///
/// # Errors
/// Rejects a sample blob the bounded reader rejects.
pub fn sample_points(blobs: &[Vec<u8>], limit: usize) -> Result<Vec<[f32; 3]>, FacadeError> {
    let sets = blobs
        .iter()
        .map(|blob| {
            looklab_samples::decode(blob).map_err(|error| FacadeError::Reply(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let total: usize = sets.iter().map(looklab_samples::SampleSet::n).sum();
    let stride = total.div_ceil(limit.max(1)).max(1);
    let mut out = Vec::with_capacity(total.min(limit.max(1)));
    let mut index = 0;
    for set in &sets {
        for rgb in set.out_rgb() {
            if index % stride == 0 && out.len() < limit {
                out.push(*rgb);
            }
            index += 1;
        }
    }
    Ok(out)
}
