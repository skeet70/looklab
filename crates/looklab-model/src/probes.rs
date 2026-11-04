// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_color::chart::colorchecker24_d65;
use looklab_color::xyy::{Illuminant, xyy_to_xyz};
use looklab_color::{Xyy, Xyz};
use serde::{Deserialize, Serialize};

use crate::error::{ModelError, Result};
use crate::file::ProbeSetVersion;

const POINTER_JSON: &str = include_str!("../../../data/probes/pointer_gamut.json");

pub const PROBES_V1_YAML: &str = include_str!("../../../data/probes/v1.yaml");

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeGroup {
    GrayRamp,
    ChartPatches,
    HueGrid,
    Skin,
    MemoryColors,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProbeId(String);

impl ProbeId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Probe {
    pub id: ProbeId,
    pub group: ProbeGroup,
    pub weight: f64,
    pub xyz: Xyz,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProbeSet {
    pub version: ProbeSetVersion,
    pub notes: Vec<String>,
    pub probes: Vec<Probe>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProbesFormat {
    #[serde(rename = "looklab-probes/1")]
    V1,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeSetFile {
    pub format: ProbesFormat,
    pub version: ProbeSetVersion,
    pub description: String,
    pub notes: Vec<String>,
    pub probes: Vec<Probe>,
}

impl ProbeSet {
    pub fn to_file(&self) -> ProbeSetFile {
        ProbeSetFile {
            format: ProbesFormat::V1,
            version: self.version,
            description: "looklab probe set v1 (docs/02 §6)".to_string(),
            notes: self.notes.clone(),
            probes: self.probes.clone(),
        }
    }

    pub fn to_yaml(&self) -> Result<String> {
        Ok(serde_norway::to_string(&self.to_file())?)
    }
}

pub fn v1() -> ProbeSet {
    let white = Illuminant::D65.xyz();
    let mut probes = Vec::new();

    for step in 0..=60 {
        let s = -10.0 + 0.25 * step as f64;
        let y = 0.18 * libm::pow(2.0, s);
        probes.push(Probe {
            id: ProbeId::new(format!("gray-s{s:+.2}")),
            group: ProbeGroup::GrayRamp,
            weight: 1.0,
            xyz: white.scale(y),
        });
    }

    let patches = colorchecker24_d65();
    for patch in patches {
        for s in [-2.0, -1.0, 0.0, 1.0, 2.0] {
            probes.push(Probe {
                id: ProbeId::new(format!("chart-{}-s{s:+.0}", slug(&patch.name))),
                group: ProbeGroup::ChartPatches,
                weight: 1.0,
                xyz: patch.xyz.scale(libm::pow(2.0, s)),
            });
        }
    }

    let pointer = PointerGamut::load();
    let white_xy = Illuminant::D65.xy();
    for hue_step in 0..24 {
        let hue = 15.0 * hue_step as f64;
        let boundary = pointer.boundary_xy(hue);
        for fraction in [0.2, 0.4, 0.6, 0.8] {
            let x = white_xy.0 + fraction * (boundary.0 - white_xy.0);
            let y = white_xy.1 + fraction * (boundary.1 - white_xy.1);
            let base = xyy_to_xyz(Xyy::new(x, y, 0.18));
            for s in -5..=3 {
                probes.push(Probe {
                    id: ProbeId::new(format!(
                        "hue-{hue:03.0}-{percent:02}-s{s:+03}",
                        percent = (fraction * 100.0) as u32
                    )),
                    group: ProbeGroup::HueGrid,
                    weight: 0.5,
                    xyz: base.scale(libm::pow(2.0, s as f64)),
                });
            }
        }
    }

    for name in ["dark skin", "light skin"] {
        let patch = find_patch(patches, name);
        for s in [-1.0, 0.0, 1.0] {
            probes.push(Probe {
                id: ProbeId::new(format!("skin-{}-s{s:+.0}", slug(name))),
                group: ProbeGroup::Skin,
                weight: 2.0,
                xyz: patch.xyz.scale(libm::pow(2.0, s)),
            });
        }
    }

    for name in ["blue sky", "foliage"] {
        let patch = find_patch(patches, name);
        for s in [-1.0, 0.0, 1.0] {
            probes.push(Probe {
                id: ProbeId::new(format!("memory-{}-s{s:+.0}", slug(name))),
                group: ProbeGroup::MemoryColors,
                weight: 1.5,
                xyz: patch.xyz.scale(libm::pow(2.0, s)),
            });
        }
    }

    ProbeSet {
        version: ProbeSetVersion::V1,
        notes: vec![
            "SG 140 is absent from the committed chart data; the chart group is ColorChecker 24 only.".to_string(),
            "Chart and skin patches are scaled by 2^s from the D65-adapted ColorChecker 24 values.".to_string(),
            "Hue grid: 24 hues, 20/40/60/80% of the Pointer gamut boundary at L* = 49.5, D65 adapted.".to_string(),
            "Skin: ColorChecker patches 1 and 2 only (docs/10 Q5).".to_string(),
            "Memory colors: sky = ColorChecker patch 3 (blue sky), foliage = patch 4.".to_string(),
        ],
        probes,
    }
}

fn find_patch<'a>(
    patches: &'a [looklab_color::chart::ChartPatch],
    name: &str,
) -> &'a looklab_color::chart::ChartPatch {
    patches
        .iter()
        .find(|patch| patch.name == name)
        .expect("ColorChecker patch name is fixed data")
}

fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut previous_dash = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            out.push(character.to_ascii_lowercase());
            previous_dash = false;
        } else if !previous_dash && !out.is_empty() {
            out.push('-');
            previous_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

#[derive(Deserialize)]
struct PointerFile {
    boundary_d65: Vec<BoundaryPoint>,
}

#[derive(Deserialize)]
struct BoundaryPoint {
    xy: [f64; 2],
}

struct PointerGamut {
    points: Vec<BoundaryPoint>,
}

impl PointerGamut {
    fn load() -> Self {
        let file: PointerFile = serde_json::from_str(POINTER_JSON)
            .expect("pointer_gamut.json is generated by tools/golden/gen_pointer.py");
        Self {
            points: file.boundary_d65,
        }
    }

    fn boundary_xy(&self, hue: f64) -> (f64, f64) {
        let step = 360.0 / self.points.len() as f64;
        let lower = (hue / step).floor();
        let fraction = hue / step - lower;
        let lower_index = (lower as usize) % self.points.len();
        let upper_index = (lower_index + 1) % self.points.len();
        let a = self.points[lower_index].xy;
        let b = self.points[upper_index].xy;
        (
            a[0] + fraction * (b[0] - a[0]),
            a[1] + fraction * (b[1] - a[1]),
        )
    }
}

pub fn v1_yaml() -> Result<String> {
    v1().to_yaml()
}

pub fn validate_yaml(yaml: &str) -> Result<ProbeSetFile> {
    serde_norway::from_str(yaml).map_err(ModelError::from)
}
