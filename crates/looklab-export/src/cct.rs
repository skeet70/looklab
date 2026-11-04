// SPDX-License-Identifier: AGPL-3.0-or-later
//! Correlated colour temperatures as typed values.

use serde::{Deserialize, Serialize};

/// A correlated colour temperature in kelvin, strictly positive and finite.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CctK(f64);

impl CctK {
    pub const D65: Self = Self(6500.0);
    pub const DAYLIGHT: Self = Self(5500.0);
    pub const TUNGSTEN: Self = Self(3200.0);
    /// DNG's calibration illuminant for the first colour matrix.
    pub const STD_A: Self = Self(2856.0);

    pub fn new(kelvin: f64) -> Option<Self> {
        (kelvin.is_finite() && kelvin > 0.0).then_some(Self(kelvin))
    }

    pub fn kelvin(self) -> f64 {
        self.0
    }

    /// Weight of the D65 artifact in the firmware's 1/T interpolation between
    /// StdA and D65, clamped outside the pair.
    pub fn dual_weight(self) -> f64 {
        let inverse = 1.0 / self.0;
        let std_a = 1.0 / Self::STD_A.0;
        let d65 = 1.0 / Self::D65.0;
        ((inverse - std_a) / (d65 - std_a)).clamp(0.0, 1.0)
    }
}

impl core::fmt::Display for CctK {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "{}K", self.0)
    }
}

/// The three scene CCTs a single-illuminant bake is reported at.
pub const BAKE_CCTS: [CctK; 3] = [CctK::D65, CctK::DAYLIGHT, CctK::TUNGSTEN];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dual_weights_are_one_at_d65_and_zero_at_std_a() {
        assert_eq!(CctK::D65.dual_weight(), 1.0);
        assert_eq!(CctK::STD_A.dual_weight(), 0.0);
        assert_eq!(CctK::new(2000.0).expect("cct").dual_weight(), 0.0);
        let daylight = CctK::DAYLIGHT.dual_weight();
        let tungsten = CctK::TUNGSTEN.dual_weight();
        assert!(0.0 < tungsten && tungsten < daylight && daylight < 1.0);
    }

    #[test]
    fn rejects_non_positive_temperatures() {
        assert!(CctK::new(0.0).is_none());
        assert!(CctK::new(f64::NAN).is_none());
        assert_eq!(CctK::new(5000.0).map(CctK::kelvin), Some(5000.0));
    }
}
