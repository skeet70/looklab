// SPDX-License-Identifier: AGPL-3.0-or-later
//! Synthetic bodies, Looks and per-image trust facts for the red-team scenarios.
//!
//! A population can be rendered through any Look, so honest contributors and an
//! attacker draw scenes from the same generator.

use looklab_color::Xyz;
use looklab_fit::FitImage;
use looklab_fit::synth::{self, LookSelector, SynthConfig};
use looklab_meta::SanitizedMetadata;
use looklab_model::{ClipFunction, Look, LookInput, Tone, basis::HUE_COEFS, basis::S_COEFS};

use crate::error::{Result, TrustError};
use crate::evidence::EvidenceGrid;
use crate::ids::ArtifactHash;
use crate::region::{Region, RegionCoverage};
use crate::weight::CaptureTuple;

/// A rendered image with its scene colours and generator exposure.
pub struct GeneratedImage {
    pub fit: FitImage,
    /// Noise-free scene colours, used for coverage and evidence.
    pub points: Vec<Xyz>,
    pub true_exposure: f64,
}

pub fn render(
    seed: u64,
    count: usize,
    samples: usize,
    noise_ucs: f64,
    truth: &Look,
    look: &Look,
) -> Vec<GeneratedImage> {
    let config = SynthConfig {
        seed,
        images: count,
        samples_per_image: samples,
        noise_ucs,
        ..SynthConfig::default()
    };
    let dataset = synth::dataset_with(&config, truth, &LookSelector::Single(look));
    dataset
        .images
        .into_iter()
        .zip(dataset.points)
        .zip(dataset.nuisances)
        .map(|((fit, points), nuisance)| GeneratedImage {
            fit,
            points,
            true_exposure: nuisance.exposure,
        })
        .collect()
}

/// A variant of `truth` whose lightness shifts by `delta` `J'` at `hue` and the
/// given 1-based chroma knots (1 near neutral, 3 at the gamut boundary).
pub fn hue_band_look(truth: &Look, hue: usize, chroma: &[usize], delta: f64) -> Look {
    let mut look = truth.clone();
    for s in 0..S_COEFS {
        for &c in chroma {
            let index = (hue % HUE_COEFS * S_COEFS + s) * 3 + (c.clamp(1, 3) - 1);
            look.color.lambda.volume[index] += delta;
        }
    }
    look
}

pub fn artifact_hash(look: &Look) -> ArtifactHash {
    let mut state = [
        0xcbf2_9ce4_8422_2325_u64,
        0x9e37_79b9_7f4a_7c15,
        0x6a09_e667_f3bc_c908,
        0xbb67_ae85_84ca_a73b,
    ];
    let mut write = |bits: u64| {
        for (index, value) in state.iter_mut().enumerate() {
            let mix = bits
                .wrapping_mul(0x100_0000_01b3)
                .rotate_left(17 + index as u32);
            *value = (*value ^ mix).wrapping_mul(0xff51_afd7_ed55_8ccd);
        }
    };
    match look.tone {
        Tone::Colorimetric => write(0),
        Tone::Spline { c0, beta } => {
            write(c0.to_bits());
            for value in beta {
                write(value.to_bits());
            }
        }
    }
    for value in look.tint.a.iter().chain(&look.tint.b) {
        write(value.to_bits());
    }
    for component in [&look.color.g, &look.color.r, &look.color.lambda] {
        for value in component.plane.iter().chain(&component.volume) {
            write(value.to_bits());
        }
    }
    for value in &look.residual.values {
        write(value.to_bits());
    }
    write(match look.clip.function {
        ClipFunction::TanhSoftV1 => 1,
    });
    write(look.clip.k.to_bits());
    let mut bytes = [0u8; 32];
    for (index, value) in state.iter().enumerate() {
        bytes[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes());
    }
    ArtifactHash::new(bytes)
}

/// A sample's Look input coordinates, the axes coverage and evidence are binned on.
#[derive(Clone, Copy, Debug)]
pub struct SampleAxes {
    pub s: f64,
    pub hue: f64,
    pub chroma: f64,
}

pub fn axes(points: &[Xyz]) -> Vec<SampleAxes> {
    points
        .iter()
        .map(|xyz| {
            let input = LookInput::new(*xyz);
            SampleAxes {
                s: input.s,
                hue: input.h_in,
                chroma: input.c_in,
            }
        })
        .collect()
}

/// Share of an image's samples per `(hue, exposure)` region.
pub fn region_coverage(axes: &[SampleAxes]) -> Result<RegionCoverage> {
    if axes.is_empty() {
        return Err(TrustError::EmptyCoverage);
    }
    let mut counts: std::collections::BTreeMap<Region, f64> = std::collections::BTreeMap::new();
    for axes in axes {
        let region = Region::from_probe(axes.s, axes.hue)?;
        *counts.entry(region).or_insert(0.0) += 1.0;
    }
    RegionCoverage::from_shares(counts)
}

/// Sample counts over `(s, h, √C)`, the grid [`crate::coverage_gain`] compares.
pub fn evidence_grid(axes: &[SampleAxes]) -> Result<EvidenceGrid> {
    let mut grid = EvidenceGrid::default();
    for axes in axes {
        grid.add(axes.s, axes.hue, axes.chroma, 1.0)?;
    }
    Ok(grid)
}

/// Equal tags give equal capture tuples.
pub fn capture_tuple(tag: &str) -> Result<CaptureTuple> {
    let metadata = SanitizedMetadata {
        make: None,
        model: None,
        firmware: None,
        lens_model: None,
        focal_length: None,
        iso: None,
        shutter: None,
        aperture: None,
        exposure_compensation: None,
        as_shot_wb: None,
        capture_month: None,
        software: Some(tag.to_owned()),
        look: None,
    };
    CaptureTuple::from_metadata(&metadata)
}
