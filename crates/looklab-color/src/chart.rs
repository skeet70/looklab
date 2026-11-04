// SPDX-License-Identifier: AGPL-3.0-or-later
//! ColorChecker 24 reference values.
//!
//! Data from colour-science `CCS_COLOURCHECKERS["ColorChecker24 - After
//! November 2014"]` (BSD-3-Clause, Colour Developers; see `NOTICE`), which
//! records the BabelColor/X-Rite reference values under D50. The JSON file is
//! written by `tools/golden/gen_basics.py`.

use std::sync::LazyLock;

use crate::cat::{Cat, adapt};
use crate::types::{Xyy, Xyz};
use crate::xyy::{Illuminant, xy_to_xyz, xyy_to_xyz};
use serde::{Deserialize, Serialize};

const CHART_JSON: &str = include_str!("../../../data/charts/colorchecker24_2014.json");

/// One ColorChecker patch; the name is the colour-science dataset key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChartPatch {
    pub name: String,
    pub xyz: Xyz,
}

#[derive(Deserialize)]
struct ChartFile {
    illuminant_xy: [f64; 2],
    patches: Vec<RawPatch>,
}

#[derive(Deserialize)]
struct RawPatch {
    name: String,
    xyy: [f64; 3],
}

struct Chart {
    illuminant: Xyz,
    patches: Vec<ChartPatch>,
}

static CHART: LazyLock<Chart> = LazyLock::new(load_chart);
static D65_PATCHES: LazyLock<Vec<ChartPatch>> = LazyLock::new(load_d65);

fn load_chart() -> Chart {
    let raw: ChartFile =
        serde_json::from_str(CHART_JSON).expect("embedded ColorChecker data is well formed");
    let illuminant = xy_to_xyz((raw.illuminant_xy[0], raw.illuminant_xy[1]));
    let patches = raw
        .patches
        .into_iter()
        .map(|patch| ChartPatch {
            name: patch.name,
            xyz: xyy_to_xyz(Xyy {
                x: patch.xyy[0],
                y: patch.xyy[1],
                big_y: patch.xyy[2],
            }),
        })
        .collect();
    Chart {
        illuminant,
        patches,
    }
}

fn load_d65() -> Vec<ChartPatch> {
    let source = CHART.illuminant;
    let destination = Illuminant::D65.xyz();
    CHART
        .patches
        .iter()
        .map(|patch| ChartPatch {
            name: patch.name.clone(),
            xyz: adapt(patch.xyz, Cat::Bradford, source, destination),
        })
        .collect()
}

/// The 24 patches under the chart illuminant (D50).
pub fn colorchecker24() -> &'static [ChartPatch] {
    &CHART.patches
}

/// Tristimulus values of the chart illuminant, Y = 1.
pub fn colorchecker24_illuminant() -> Xyz {
    CHART.illuminant
}

/// The 24 patches adapted to D65 with Bradford.
pub fn colorchecker24_d65() -> &'static [ChartPatch] {
    D65_PATCHES.as_slice()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xyy::xyz_to_xy;

    #[test]
    fn chart_has_the_twenty_four_named_patches() {
        let patches = colorchecker24();
        assert_eq!(patches.len(), 24);
        assert_eq!(patches[0].name, "dark skin");
        assert_eq!(patches[23].name, "black 2 (1.5 D)");
    }

    #[test]
    fn d65_adaptation_preserves_patch_names_and_moves_the_white_patch() {
        let d65 = colorchecker24_d65();
        assert_eq!(d65.len(), 24);
        assert_eq!(d65[18].name, "white 9.5 (.05 D)");
        let (x, y) = xyz_to_xy(d65[18].xyz);
        let (white_x, white_y) = Illuminant::D65.xy();
        assert!((x - white_x).abs() < 1e-2);
        assert!((y - white_y).abs() < 1e-2);

        let adapted_illuminant = adapt(
            colorchecker24_illuminant(),
            Cat::Bradford,
            colorchecker24_illuminant(),
            Illuminant::D65.xyz(),
        );
        let d65_white = Illuminant::D65.xyz();
        assert!((adapted_illuminant.x - d65_white.x).abs() < 1e-12);
        assert!((adapted_illuminant.y - d65_white.y).abs() < 1e-12);
        assert!((adapted_illuminant.z - d65_white.z).abs() < 1e-12);
    }
}
