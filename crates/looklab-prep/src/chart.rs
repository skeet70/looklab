// SPDX-License-Identifier: AGPL-3.0-or-later
//! Assisted colour-chart extraction, [docs/07 §3.3](../../../docs/07-contribution.md)
//! rung 5.
//!
//! The user clicks the four corners of the chart in the JPEG. [`refine_corners`]
//! snaps them to the chart's edges, [`extract`] samples the central half of each
//! patch in both images, and the returned [`ChartRegion`] goes back into
//! preparation so the chart does not enter density sampling.

use std::sync::LazyLock;

use looklab_color::Xyz;
use looklab_samples::{InputSpace, PatchId, PatchRow, PatchTable, PatchTableHeader, PrepVersion};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::align::Alignment;
use crate::geom::{
    GeometryError, Homography, Line, Point, Quad, UNIT_SQUARE, fit_line, quad_area, quad_contains,
    quad_is_convex,
};
use crate::image::Image3;
use crate::jpeg::DecodedJpeg;
use crate::patch::PatchSampler;
use crate::pipeline::PREP_VERSION;

const LAYOUTS_JSON: &str = include_str!("../../../data/charts/layouts.json");
const LAYOUTS_VERSION: u32 = 1;
const LUMA: [f64; 3] = [0.2126, 0.7152, 0.0722];

/// Identifier of a chart layout in `data/charts/layouts.json`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChartLayoutId(String);

impl ChartLayoutId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for ChartLayoutId {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Reference patch values a layout's patch ids index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChartReference {
    ColorChecker24,
}

impl ChartReference {
    fn patches(self) -> &'static [looklab_color::chart::ChartPatch] {
        match self {
            Self::ColorChecker24 => looklab_color::chart::colorchecker24(),
        }
    }
}

/// Grid geometry and reference values of one chart.
#[derive(Clone, Debug, PartialEq)]
pub struct ChartLayout {
    id: ChartLayoutId,
    name: String,
    reference: ChartReference,
    rows: u32,
    cols: u32,
    margin: (f64, f64),
    gap: f64,
}

impl ChartLayout {
    pub fn id(&self) -> &ChartLayoutId {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn rows(&self) -> u32 {
        self.rows
    }

    pub fn cols(&self) -> u32 {
        self.cols
    }

    pub fn patch_count(&self) -> u32 {
        self.rows * self.cols
    }

    /// Reference tristimulus values of patch `id`, under the chart illuminant
    /// recorded beside them in `data/charts`.
    pub fn reference_xyz(&self, id: PatchId) -> Option<Xyz> {
        self.reference
            .patches()
            .get(usize::from(id.get()))
            .map(|patch| patch.xyz)
    }

    /// Patch centre and half extents in normalized coordinates of the clicked
    /// outline, or `None` for an id past the grid.
    fn patch_spec(&self, index: u32) -> Option<(Point, Point)> {
        if index >= self.patch_count() {
            return None;
        }
        let col = f64::from(index % self.cols);
        let row = f64::from(index / self.cols);
        let (margin_x, margin_y) = self.margin;
        let cell_width = (1.0 - 2.0 * margin_x) / f64::from(self.cols);
        let cell_height = (1.0 - 2.0 * margin_y) / f64::from(self.rows);
        let centre = Point::new(
            margin_x + (col + 0.5) * cell_width,
            margin_y + (row + 0.5) * cell_height,
        );
        let half = Point::new(
            cell_width * (1.0 - self.gap) * 0.5,
            cell_height * (1.0 - self.gap) * 0.5,
        );
        Some((centre, half))
    }

    /// Normalized corners of patch `index` within the clicked outline, in
    /// top-left, top-right, bottom-right, bottom-left order.
    pub fn patch_quad(&self, index: u32) -> Option<Quad> {
        let (centre, half) = self.patch_spec(index)?;
        Some([
            Point::new(centre.x - half.x, centre.y - half.y),
            Point::new(centre.x + half.x, centre.y - half.y),
            Point::new(centre.x + half.x, centre.y + half.y),
            Point::new(centre.x - half.x, centre.y + half.y),
        ])
    }
}

#[derive(Deserialize)]
struct LayoutsFile {
    version: u32,
    layouts: Vec<LayoutEntry>,
}

#[derive(Deserialize)]
struct LayoutEntry {
    id: String,
    name: String,
    reference: String,
    rows: u32,
    cols: u32,
    margin: [f64; 2],
    gap: f64,
}

static LAYOUTS: LazyLock<Vec<ChartLayout>> = LazyLock::new(load_layouts);

/// Every chart layout compiled into the crate.
pub fn layouts() -> &'static [ChartLayout] {
    LAYOUTS.as_slice()
}

pub fn layout(id: &ChartLayoutId) -> Option<&'static ChartLayout> {
    layouts().iter().find(|layout| layout.id == *id)
}

/// The 24-patch ColorChecker Classic layout.
pub fn colorchecker_classic() -> &'static ChartLayout {
    layouts()
        .iter()
        .find(|layout| layout.id.as_str() == "colorchecker-classic")
        .expect("the embedded layouts contain the ColorChecker Classic")
}

fn load_layouts() -> Vec<ChartLayout> {
    let file: LayoutsFile =
        serde_json::from_str(LAYOUTS_JSON).expect("embedded chart layouts are well formed");
    assert_eq!(
        file.version, LAYOUTS_VERSION,
        "embedded chart layouts have an unknown version"
    );
    file.layouts
        .into_iter()
        .map(|entry| {
            let reference = match entry.reference.as_str() {
                "colorchecker24_2014" => ChartReference::ColorChecker24,
                other => panic!(
                    "chart layout `{}` names unknown reference `{other}`",
                    entry.id
                ),
            };
            assert!(entry.rows > 0 && entry.cols > 0, "chart grid is empty");
            assert_eq!(
                entry.rows * entry.cols,
                reference.patches().len() as u32,
                "chart layout `{}` does not cover its reference patches",
                entry.id
            );
            assert!(
                entry
                    .margin
                    .iter()
                    .all(|margin| (0.0..0.45).contains(margin)),
                "chart layout `{}` has a margin outside 0.0..0.45",
                entry.id
            );
            assert!(
                (0.0..0.8).contains(&entry.gap),
                "chart layout `{}` has a gap outside 0.0..0.8",
                entry.id
            );
            ChartLayout {
                id: ChartLayoutId::new(entry.id),
                name: entry.name,
                reference,
                rows: entry.rows,
                cols: entry.cols,
                margin: (entry.margin[0], entry.margin[1]),
                gap: entry.gap,
            }
        })
        .collect()
}

/// Parameters of the corner fit and patch sampling.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChartParams {
    /// Half-width of the perpendicular edge search at each corner, in JPEG pixels.
    pub corner_search_px: f64,
    /// Search steps per side of that window.
    pub corner_search_steps: u32,
    /// Sample points along each chart edge.
    pub edge_samples: u32,
    /// Fraction of a patch's linear extent averaged about its centre.
    pub inner_fraction: f64,
    /// Sample points per axis within that region.
    pub patch_taps: u32,
    /// A patch is rejected when a channel's standard deviation over its mean
    /// exceeds this.
    pub max_relative_std: f64,
}

impl Default for ChartParams {
    fn default() -> Self {
        Self {
            corner_search_px: 8.0,
            corner_search_steps: 16,
            edge_samples: 64,
            inner_fraction: 0.5,
            patch_taps: 9,
            max_relative_std: 0.05,
        }
    }
}

impl ChartParams {
    fn validate(&self) -> Result<(), ChartError> {
        if !(self.corner_search_px.is_finite() && self.corner_search_px > 0.0) {
            return Err(ChartError::InvalidParams {
                field: "corner_search_px",
            });
        }
        if self.corner_search_steps < 2 {
            return Err(ChartError::InvalidParams {
                field: "corner_search_steps",
            });
        }
        if self.edge_samples < 4 {
            return Err(ChartError::InvalidParams {
                field: "edge_samples",
            });
        }
        if !(0.0 < self.inner_fraction && self.inner_fraction <= 1.0) {
            return Err(ChartError::InvalidParams {
                field: "inner_fraction",
            });
        }
        if self.patch_taps < 2 {
            return Err(ChartError::InvalidParams {
                field: "patch_taps",
            });
        }
        if !(self.max_relative_std.is_finite() && self.max_relative_std > 0.0) {
            return Err(ChartError::InvalidParams {
                field: "max_relative_std",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ChartError {
    #[error("invalid chart parameter `{field}`")]
    InvalidParams { field: &'static str },
    #[error(transparent)]
    Geometry(#[from] GeometryError),
    #[error("the chart corners do not enclose an area")]
    DegenerateCorners,
    #[error("a chart corner lies outside the JPEG")]
    CornerOutside,
    #[error("the layout has more patches than a patch table can index")]
    TooManyPatches,
    #[error("no chart patch passed the glare and defect checks")]
    NoPatches,
    #[error("the patch table is invalid: {0}")]
    Table(#[from] looklab_samples::Error),
}

/// The chart's outline in JPEG pixel coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChartRegion {
    corners: Quad,
    jpeg_size: (u32, u32),
}

impl ChartRegion {
    pub fn new(corners: Quad, jpeg_size: (u32, u32)) -> Result<Self, ChartError> {
        if jpeg_size.0 == 0 || jpeg_size.1 == 0 {
            return Err(ChartError::DegenerateCorners);
        }
        if !quad_is_convex(&corners) || quad_area(&corners).abs() < 1.0 {
            return Err(ChartError::DegenerateCorners);
        }
        Ok(Self { corners, jpeg_size })
    }

    pub fn corners(&self) -> &Quad {
        &self.corners
    }

    pub fn jpeg_size(&self) -> (u32, u32) {
        self.jpeg_size
    }

    pub fn contains(&self, point: Point) -> bool {
        quad_contains(&self.corners, point)
    }
}

/// One chart shot's patch table and the geometry it was measured with.
#[derive(Clone, Debug, PartialEq)]
pub struct ChartExtraction {
    pub layout: ChartLayoutId,
    pub corners: Quad,
    pub region: ChartRegion,
    pub table: PatchTable,
    pub rejected: Vec<PatchId>,
}

/// Snaps the four clicked corners to the chart's outer edges.
pub fn refine_corners(
    jpeg: &DecodedJpeg,
    corners: &Quad,
    params: &ChartParams,
) -> Result<Quad, ChartError> {
    params.validate()?;
    let width = f64::from(jpeg.image.width);
    let height = f64::from(jpeg.image.height);
    for point in corners {
        if !point.is_finite() {
            return Err(ChartError::DegenerateCorners);
        }
        if point.x < -1.0 || point.y < -1.0 || point.x > width || point.y > height {
            return Err(ChartError::CornerOutside);
        }
    }

    let mut edges = [None; 4];
    for (index, edge) in edges.iter_mut().enumerate() {
        *edge = refine_edge(
            &jpeg.image,
            corners[index],
            corners[(index + 1) % 4],
            params,
        );
    }
    let [top, right, bottom, left] = edges;
    let (top, right, bottom, left) = (
        top.ok_or(ChartError::DegenerateCorners)?,
        right.ok_or(ChartError::DegenerateCorners)?,
        bottom.ok_or(ChartError::DegenerateCorners)?,
        left.ok_or(ChartError::DegenerateCorners)?,
    );
    let refined: Quad = [
        top.intersection(&left)
            .ok_or(ChartError::DegenerateCorners)?,
        top.intersection(&right)
            .ok_or(ChartError::DegenerateCorners)?,
        bottom
            .intersection(&right)
            .ok_or(ChartError::DegenerateCorners)?,
        bottom
            .intersection(&left)
            .ok_or(ChartError::DegenerateCorners)?,
    ];
    if !quad_is_convex(&refined) {
        return Err(ChartError::DegenerateCorners);
    }
    Ok(refined)
}

/// Searches a perpendicular window about one edge for the strongest luminance
/// step, then fits a line through the subpixel maxima.
fn refine_edge(image: &Image3, a: Point, b: Point, params: &ChartParams) -> Option<Line> {
    let length = a.distance(b);
    if length <= 4.0 {
        return fit_line(&[a, b]);
    }
    let direction = Point::new((b.x - a.x) / length, (b.y - a.y) / length);
    let normal = Point::new(-direction.y, direction.x);
    let steps = params.corner_search_steps;
    let step = params.corner_search_px / f64::from(steps);
    let samples = params.edge_samples;

    let mut points = Vec::with_capacity(samples as usize);
    let mut strengths = Vec::with_capacity(samples as usize);
    for index in 0..samples {
        let t = (f64::from(index) + 0.5) / f64::from(samples);
        let margin = 0.75 * params.corner_search_px / length;
        if t < margin || t > 1.0 - margin {
            continue;
        }
        let base = a.lerp(b, t);
        let mut profile = Vec::with_capacity(usize::try_from(2 * steps + 1).ok()?);
        for k in -(i64::from(steps))..=i64::from(steps) {
            let offset = k as f64 * step;
            let magnitude = edge_gradient(image, base, normal, offset)
                .unwrap_or(0.0)
                .abs();
            profile.push(magnitude);
        }
        let floor = median(&profile).unwrap_or(0.0);
        let mut weight_sum = 0.0;
        let mut weighted = 0.0;
        let mut strength = 0.0f64;
        for (index, magnitude) in profile.iter().enumerate() {
            let weight = (magnitude - floor).max(0.0);
            weight_sum += weight;
            weighted += weight * index as f64;
            strength = strength.max(weight);
        }
        if weight_sum <= 0.0 {
            continue;
        }
        let offset = (weighted / weight_sum - f64::from(steps)) * step;
        points.push(Point::new(
            base.x + normal.x * offset,
            base.y + normal.y * offset,
        ));
        strengths.push(strength);
    }
    if points.len() < 3 {
        return None;
    }

    let max_strength = strengths.iter().copied().fold(0.0f64, f64::max);
    let (points, _): (Vec<Point>, Vec<f64>) = points
        .into_iter()
        .zip(strengths)
        .filter(|(_, strength)| *strength >= 0.35 * max_strength)
        .unzip();
    if points.len() < 3 {
        return None;
    }
    let line = fit_line(&points)?;
    let distances: Vec<f64> = points
        .iter()
        .map(|point| line.distance_to(*point))
        .collect();
    let median = median(&distances)?;
    let tolerance = (3.0 * median).max(1.0);
    let kept: Vec<Point> = points
        .iter()
        .zip(&distances)
        .filter(|(_, distance)| **distance <= tolerance)
        .map(|(point, _)| *point)
        .collect();
    fit_line(&kept)
}

/// Luminance derivative across the edge at perpendicular offset `s`.
fn edge_gradient(image: &Image3, base: Point, normal: Point, s: f64) -> Option<f64> {
    let ahead = Point::new(base.x + normal.x * (s + 0.5), base.y + normal.y * (s + 0.5));
    let behind = Point::new(base.x + normal.x * (s - 0.5), base.y + normal.y * (s - 0.5));
    Some(luma(image, ahead)? - luma(image, behind)?)
}

fn luma(image: &Image3, point: Point) -> Option<f64> {
    let rgb = image.sample_bilinear(point.x, point.y)?;
    Some(f64::from(rgb[0]) * LUMA[0] + f64::from(rgb[1]) * LUMA[1] + f64::from(rgb[2]) * LUMA[2])
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    Some(sorted[sorted.len() / 2])
}

/// Measures every patch from one aligned pair.
///
/// `raw` is white-balanced linear camera RGB after orientation and the fitted
/// radial gain, the same image the sample path uses.
pub fn extract(
    layout: &ChartLayout,
    corners: &Quad,
    jpeg: &DecodedJpeg,
    raw: &Image3,
    alignment: &Alignment,
    params: &ChartParams,
) -> Result<ChartExtraction, ChartError> {
    params.validate()?;
    if layout.patch_count() > u32::from(u16::MAX) {
        return Err(ChartError::TooManyPatches);
    }
    let refined = refine_corners(jpeg, corners, params)?;
    let region = ChartRegion::new(refined, (jpeg.image.width, jpeg.image.height))?;
    let to_image = Homography::from_quad(&UNIT_SQUARE, &refined)?;
    let sampler = PatchSampler {
        jpeg,
        raw,
        alignment,
        inner_fraction: params.inner_fraction,
        taps: params.patch_taps,
        max_relative_std: params.max_relative_std,
    };

    let mut rows = Vec::with_capacity(layout.patch_count() as usize);
    let mut rejected = Vec::new();
    for index in 0..layout.patch_count() {
        let Some(id) = u16::try_from(index).ok().map(PatchId::new) else {
            return Err(ChartError::TooManyPatches);
        };
        let Some((centre, half)) = layout.patch_spec(index) else {
            return Err(ChartError::TooManyPatches);
        };
        match sampler.sample(&to_image, centre, half) {
            Some(sample) => rows.push(PatchRow {
                id,
                in_mean: sample.in_mean,
                in_std: sample.in_std,
                out_mean: sample.out_mean,
                out_std: sample.out_std,
            }),
            None => rejected.push(id),
        }
    }
    if rows.is_empty() {
        return Err(ChartError::NoPatches);
    }
    let header = PatchTableHeader {
        n: rows.len() as u32,
        input_space: InputSpace::CameraRgbWbLinear,
        output_space: jpeg.space,
        prep_version: PrepVersion::new(PREP_VERSION),
    };
    let table = PatchTable::new(header, rows)?;
    Ok(ChartExtraction {
        layout: layout.id.clone(),
        corners: refined,
        region,
        table,
        rejected,
    })
}

#[cfg(test)]
mod tests {
    use super::{colorchecker_classic, layout, layouts, load_layouts};

    #[test]
    fn embedded_layouts_are_well_formed() {
        let layouts = layouts();
        assert!(layouts.len() >= 2);
        for entry in layouts {
            assert!(entry.rows() * entry.cols() == entry.patch_count());
            assert!(entry.patch_quad(0).is_some());
            assert!(entry.patch_quad(entry.patch_count()).is_none());
        }
        assert_eq!(colorchecker_classic().patch_count(), 24);
    }

    #[test]
    fn reference_values_follow_the_patch_ids() {
        let chart = colorchecker_classic();
        let first = chart
            .reference_xyz(looklab_samples::PatchId::new(0))
            .expect("patch 0 exists");
        let last = chart
            .reference_xyz(looklab_samples::PatchId::new(23))
            .expect("patch 23 exists");
        assert!(first.y > 0.05 && first.y < 0.15);
        assert!(last.y > 0.01 && last.y < 0.05);
    }

    #[test]
    fn lookup_by_id_finds_every_layout() {
        for entry in layouts() {
            assert!(layout(entry.id()).is_some());
        }
        let ids: Vec<_> = load_layouts().into_iter().map(|entry| entry.id).collect();
        assert_eq!(ids.len(), layouts().len());
    }
}
