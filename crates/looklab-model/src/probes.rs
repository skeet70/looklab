// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_color::chart::colorchecker24_d65;
use looklab_color::xyy::{Illuminant, xyy_to_xyz};
use looklab_color::{Xyy, Xyz};
use serde::{Deserialize, Serialize};

use crate::error::{ModelError, Result};
use crate::file::ProbeSetVersion;

const POINTER_JSON: &str = include_str!("../../../data/probes/pointer_gamut.json");

pub const PROBES_V1_YAML: &str = include_str!("../../../data/probes/v1.yaml");

/// Probe count of v1; fixed by the generator and checked by the file format.
pub const V1_PROBE_COUNT: usize = 1057;

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

    let pointer = pointer_gamut();
    for hue_step in 0..24 {
        let hue = 15.0 * hue_step as f64;
        let boundary = pointer.boundary_xy(hue);
        let boundary_xyz = xyy_to_xyz(Xyy::new(boundary.0, boundary.1, 0.18));
        let boundary_ucs = crate::deriv::reference_cam16().ucs(boundary_xyz);
        let boundary_m = (boundary_ucs.a * boundary_ucs.a + boundary_ucs.b * boundary_ucs.b).sqrt();
        let boundary_h = libm::atan2(boundary_ucs.b, boundary_ucs.a);
        let white_xy = Illuminant::D65.xy();
        for fraction in [0.2, 0.4, 0.6, 0.8] {
            let initial = (
                white_xy.0 + fraction * (boundary.0 - white_xy.0),
                white_xy.1 + fraction * (boundary.1 - white_xy.1),
            );
            let chromaticity = solve_chromaticity(boundary_h, fraction * boundary_m, initial);
            let base = xyy_to_xyz(Xyy::new(chromaticity.0, chromaticity.1, 0.18));
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
            "Hue grid: 24 hues, 20/40/60/80% of the Pointer gamut boundary CAM16-UCS M' at Y = 0.18 (boundary hue, D65 adapted).".to_string(),
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

static POINTER: std::sync::LazyLock<PointerGamut> = std::sync::LazyLock::new(PointerGamut::load);

fn pointer_gamut() -> &'static PointerGamut {
    &POINTER
}

/// `xy` of the Pointer gamut boundary at `hue`, Bradford-adapted to D65.
pub fn pointer_boundary_xy(hue_degrees: f64) -> (f64, f64) {
    pointer_gamut().boundary_xy(hue_degrees)
}

fn ucs_at_018(x: f64, y: f64) -> looklab_color::ucs::Ucs {
    crate::deriv::reference_cam16().ucs(xyy_to_xyz(Xyy::new(x, y, 0.18)))
}

fn solve_chromaticity(target_hue: f64, target_chroma: f64, initial: (f64, f64)) -> (f64, f64) {
    let mut x = initial.0;
    let mut y = initial.1;
    let step = 1e-6;
    let pi = core::f64::consts::PI;
    for _ in 0..60 {
        let value = ucs_at_018(x, y);
        let hue = libm::atan2(value.b, value.a);
        let chroma = (value.a * value.a + value.b * value.b).sqrt();
        let mut d_hue = hue - target_hue;
        while d_hue > pi {
            d_hue -= 2.0 * pi;
        }
        while d_hue < -pi {
            d_hue += 2.0 * pi;
        }
        let d_chroma = chroma - target_chroma;
        if d_hue.abs() < 1e-10 && d_chroma.abs() < 1e-9 {
            break;
        }
        let along_x = ucs_at_018(x + step, y);
        let along_y = ucs_at_018(x, y + step);
        let mut d_hue_dx = libm::atan2(along_x.b, along_x.a) - hue;
        while d_hue_dx > pi {
            d_hue_dx -= 2.0 * pi;
        }
        while d_hue_dx < -pi {
            d_hue_dx += 2.0 * pi;
        }
        let mut d_hue_dy = libm::atan2(along_y.b, along_y.a) - hue;
        while d_hue_dy > pi {
            d_hue_dy -= 2.0 * pi;
        }
        while d_hue_dy < -pi {
            d_hue_dy += 2.0 * pi;
        }
        let d_hue_dx = d_hue_dx / step;
        let d_hue_dy = d_hue_dy / step;
        let d_chroma_dx = ((along_x.a * along_x.a + along_x.b * along_x.b).sqrt() - chroma) / step;
        let d_chroma_dy = ((along_y.a * along_y.a + along_y.b * along_y.b).sqrt() - chroma) / step;
        let determinant = d_hue_dx * d_chroma_dy - d_hue_dy * d_chroma_dx;
        if !determinant.is_finite() || determinant.abs() < 1e-12 {
            break;
        }
        x += (-d_hue * d_chroma_dy + d_hue_dy * d_chroma) / determinant;
        y += (d_hue * d_chroma_dx - d_chroma * d_hue_dx) / determinant;
        if !(x.is_finite() && y.is_finite()) || !(x > 0.0 && y > 0.0 && x + y < 1.0) {
            return initial;
        }
    }
    (x, y)
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
