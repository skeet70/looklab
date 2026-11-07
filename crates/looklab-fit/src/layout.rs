// SPDX-License-Identifier: AGPL-3.0-or-later
//! Fit parameter layout and the constrained-to-unconstrained coordinate maps.

use core::ops::Range;
use looklab_model::basis::{RESIDUAL_COEFS, S_COEFS, TINT_COEFS, TONE_COEFS};
use looklab_model::{ClipSettings, ColorLayer, GrlComponent, Look, Residual, Tint, Tone};

pub const GRL_COEFS: usize = looklab_model::look::GRL_COEFS;
const VOLUME_COEFS: usize = looklab_model::basis::VOLUME_COEFS;

/// Three nuisances per image: exposure, `log w_R`, `log w_B`.
pub const NUISANCE_PER_IMAGE: usize = 3;

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub tone_c0: usize,
    pub tone_beta: Range<usize>,
    pub tint_a: Range<usize>,
    pub tint_b: Range<usize>,
    pub g: Range<usize>,
    pub r: Range<usize>,
    pub lambda: Range<usize>,
    pub residual: Range<usize>,
    pub images: Range<usize>,
    pub modes: Range<usize>,
    pub clip: usize,
    pub sigma: Range<usize>,
    pub response: Range<usize>,
    pub total: usize,
    /// Scale on the identity prior curvature; set from `FitOptions::identity_scale`.
    pub identity_scale: f64,
}

impl Layout {
    #[cfg(test)]
    pub fn new(n_images: usize, n_modes: usize, n_contributors: usize) -> Self {
        Self::with_response(n_images, n_modes, n_contributors, 0)
    }

    pub fn with_response(
        n_images: usize,
        n_modes: usize,
        n_contributors: usize,
        n_response: usize,
    ) -> Self {
        let tone_c0 = 0;
        let tone_beta = 1..1 + TONE_COEFS;
        let tint_a = tone_beta.end..tone_beta.end + TINT_COEFS;
        let tint_b = tint_a.end..tint_a.end + TINT_COEFS;
        let g = tint_b.end..tint_b.end + GRL_COEFS;
        let r = g.end..g.end + GRL_COEFS;
        let lambda = r.end..r.end + GRL_COEFS;
        let residual = lambda.end..lambda.end + RESIDUAL_COEFS;
        let images = residual.end..residual.end + n_images * NUISANCE_PER_IMAGE;
        let modes = images.end..images.end + n_modes;
        let clip = modes.end;
        let sigma = clip + 1..clip + 1 + n_contributors;
        let response = sigma.end..sigma.end + n_response;
        let total = response.end;
        Self {
            tone_c0,
            tone_beta,
            tint_a,
            tint_b,
            g,
            r,
            lambda,
            residual,
            images,
            modes,
            clip,
            sigma,
            response,
            total,
            identity_scale: 1.0,
        }
    }

    pub fn image(&self, image: usize) -> Range<usize> {
        let start = self.images.start + image * NUISANCE_PER_IMAGE;
        start..start + NUISANCE_PER_IMAGE
    }

    pub fn sigma_of(&self, contributor: usize) -> usize {
        self.sigma.start + contributor
    }

    pub fn mode_of(&self, index: usize) -> usize {
        self.modes.start + index
    }

    pub fn look(&self, theta: &[f64]) -> Look {
        let mut beta = [0.0_f64; TONE_COEFS];
        for (k, value) in beta.iter_mut().enumerate() {
            *value = softplus(theta[self.tone_beta.start + k]);
        }
        // softplus is finite and non-negative, so `Tone::spline` cannot reject these.
        let tone = Tone::spline(theta[self.tone_c0], beta).unwrap_or(Tone::Colorimetric);
        let component = |range: Range<usize>| GrlComponent {
            plane: theta[range.start..range.start + S_COEFS].to_vec(),
            volume: theta[range.start + S_COEFS..range.start + S_COEFS + VOLUME_COEFS].to_vec(),
            core: theta[range.start + S_COEFS + VOLUME_COEFS..range.end].to_vec(),
        };
        Look {
            tone,
            tint: Tint {
                a: theta[self.tint_a.clone()].to_vec(),
                b: theta[self.tint_b.clone()].to_vec(),
            },
            color: ColorLayer {
                g: component(self.g.clone()),
                r: component(self.r.clone()),
                lambda: component(self.lambda.clone()),
            },
            residual: Residual {
                values: theta[self.residual.clone()].to_vec(),
            },
            clip: ClipSettings::default_v1(),
        }
    }
}

/// `β = softplus(η)` keeps the tone spline monotone for any `η`.
pub fn softplus(eta: f64) -> f64 {
    if eta > 30.0 {
        eta + libm::exp(-eta)
    } else {
        libm::log1p(libm::exp(eta))
    }
}

/// Inverse of [`softplus`], floored at `β = 1e-6`.
pub fn softplus_inverse(beta: f64) -> f64 {
    let beta = beta.max(1e-6);
    if beta > 30.0 {
        beta
    } else {
        libm::log(libm::expm1(beta))
    }
}

/// `dβ/dη`, the chain factor for the tone columns.
pub fn softplus_slope(eta: f64) -> f64 {
    1.0 / (1.0 + libm::exp(-eta))
}

pub fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + libm::exp(-x))
}

/// Clip `k` from its unconstrained coordinate, over `[0.005, 0.1]`.
pub fn clip_from_zeta(zeta: f64) -> f64 {
    0.005 + 0.095 * sigmoid(zeta)
}

pub fn zeta_from_clip(k: f64) -> f64 {
    let p = ((k - 0.005) / 0.095).clamp(1e-6, 1.0 - 1e-6);
    libm::log(p / (1.0 - p))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Tone,
    Tint,
    G,
    R,
    Lambda,
    Residual,
    Nuisance,
    ResponseTone,
    ResponseTint,
    ResponseChroma,
    ResponseHue,
    ResponseLightness,
    ResponseWb,
}

#[derive(Clone, Debug)]
pub struct Params {
    pub layout: Layout,
    pub theta: Vec<f64>,
}

impl Params {
    pub fn initial(layout: Layout, contributor_count: usize) -> Self {
        let mut theta = vec![0.0_f64; layout.total];
        if let Ok(Tone::Spline { c0, beta }) = Tone::colorimetric(&looklab_model::basis::TONE_KNOTS)
        {
            theta[layout.tone_c0] = c0;
            for (k, value) in beta.iter().enumerate() {
                theta[layout.tone_beta.start + k] = softplus_inverse(*value);
            }
        }
        if let Some(slot) = theta.get_mut(layout.clip) {
            *slot = zeta_from_clip(0.03);
        }
        for contributor in 0..contributor_count {
            let slot = layout.sigma_of(contributor);
            theta[slot] = libm::log(1.5);
        }
        Self { layout, theta }
    }

    pub fn look(&self) -> Look {
        self.layout.look(&self.theta)
    }

    pub fn clip_k(&self) -> f64 {
        clip_from_zeta(self.theta[self.layout.clip])
    }

    pub fn clip_slope(&self) -> f64 {
        let zeta = self.theta[self.layout.clip];
        0.095 * sigmoid(zeta) * (1.0 - sigmoid(zeta))
    }

    pub fn image(&self, index: usize) -> [f64; NUISANCE_PER_IMAGE] {
        let range = self.layout.image(index);
        [
            self.theta[range.start],
            self.theta[range.start + 1],
            self.theta[range.start + 2],
        ]
    }

    pub fn mode_offset(&self, index: usize) -> f64 {
        self.theta[self.layout.mode_of(index)]
    }

    pub fn sigma(&self, contributor: usize) -> f64 {
        libm::exp(self.theta[self.layout.sigma_of(contributor)])
    }
}
