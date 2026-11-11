// SPDX-License-Identifier: AGPL-3.0-or-later
//! Closed scoring identity shared by matching, evidence and reporting.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::qualification::QualificationValueError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoringMetric {
    /// Camera prediction and target rendered through their output encoding and clip.
    RenderedPostClipV1,
    LegacyPreClipV1,
}

impl ScoringMetric {
    pub const RENDERED_IDENTITY: &str = "looklab-scoring-metric/rendered-post-clip/1";
    pub const LEGACY_IDENTITY: &str = "looklab-scoring-metric/legacy-pre-clip/1";

    pub const fn identity(self) -> &'static str {
        match self {
            Self::RenderedPostClipV1 => Self::RENDERED_IDENTITY,
            Self::LegacyPreClipV1 => Self::LEGACY_IDENTITY,
        }
    }

    /// # Errors
    /// A token that is not a known scoring-metric identity.
    pub fn parse(value: &str) -> Result<Self, QualificationValueError> {
        match value {
            Self::RENDERED_IDENTITY => Ok(Self::RenderedPostClipV1),
            Self::LEGACY_IDENTITY => Ok(Self::LegacyPreClipV1),
            other => Err(QualificationValueError::Invalid {
                field: "ScoringMetric",
                value: other.to_owned(),
            }),
        }
    }

    pub const fn is_rendered(self) -> bool {
        matches!(self, Self::RenderedPostClipV1)
    }

    pub const fn is_legacy(self) -> bool {
        matches!(self, Self::LegacyPreClipV1)
    }
}

impl fmt::Display for ScoringMetric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.identity())
    }
}

/// Whether a captured-error statistic supports a significance claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssessmentStatus {
    /// The joint conditional-mean covariance is present and usable.
    Assessed,
    /// Some declared settings or contributor cells are assessed.
    Partial,
    /// Descriptive error only; support and joint uncertainty are unassessed.
    Unassessed,
}
