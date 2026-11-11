// SPDX-License-Identifier: AGPL-3.0-or-later
//! How a target's published appearance enters a recipe score.

use looklab_color::ucs::Ucs;

pub use looklab_model::{OutputContext, OutputPin, RenderJacobian};
pub use looklab_schema::{AssessmentStatus, ScoringMetric};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TargetAppearance {
    PreClip,
    /// Rendered through the target's pinned reference output encoding and clip.
    Rendered(OutputPin),
}

impl TargetAppearance {
    pub const fn metric(&self) -> ScoringMetric {
        match self {
            Self::PreClip => ScoringMetric::LegacyPreClipV1,
            Self::Rendered(_) => ScoringMetric::RenderedPostClipV1,
        }
    }

    pub const fn pin(&self) -> Option<OutputPin> {
        match self {
            Self::PreClip => None,
            Self::Rendered(pin) => Some(*pin),
        }
    }

    pub fn render(&self, pre: Ucs) -> Ucs {
        match self {
            Self::PreClip => pre,
            Self::Rendered(pin) => (*pin).render(pre),
        }
    }
}
