// SPDX-License-Identifier: AGPL-3.0-or-later

use core::fmt;

use looklab_model::ProbeGroup;
use serde::{Deserialize, Serialize};

/// Probe-group multipliers for the user's emphasis (docs/04 §3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Emphasis {
    #[default]
    Balanced,
    Skin,
    Landscape,
    Product,
}

impl Emphasis {
    pub const ALL: [Self; 4] = [Self::Balanced, Self::Skin, Self::Landscape, Self::Product];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Balanced => "balanced",
            Self::Skin => "skin",
            Self::Landscape => "landscape",
            Self::Product => "product",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|emphasis| emphasis.as_str() == value)
    }

    pub fn group_multiplier(self, group: ProbeGroup) -> f64 {
        use ProbeGroup::*;
        match (self, group) {
            (Self::Balanced, _) => 1.0,
            (Self::Skin, Skin) => 3.0,
            (Self::Skin, MemoryColors) => 1.2,
            (Self::Skin, HueGrid) => 0.8,
            (Self::Landscape, MemoryColors) => 3.0,
            (Self::Landscape, HueGrid) => 1.25,
            (Self::Landscape, GrayRamp) => 0.8,
            (Self::Landscape, Skin) => 0.5,
            (Self::Product, ChartPatches) => 2.0,
            (Self::Product, GrayRamp) => 1.2,
            (Self::Product, Skin) => 0.8,
            (Self::Skin, _) | (Self::Landscape, _) | (Self::Product, _) => 1.0,
        }
    }
}

impl fmt::Display for Emphasis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
