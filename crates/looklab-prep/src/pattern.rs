// SPDX-License-Identifier: AGPL-3.0-or-later
//! Phone-pattern session generator and detector, [docs/07 §3.1](../../../docs/07-contribution.md)
//! rung 3.
//!
//! [`render`] draws the pattern a phone displays for a server-issued
//! [`PatternSeed`], and [`detect`] recovers the display's patch values from a
//! photograph of it. Both use the same layout in a fixed 9:16 content box.

use std::sync::LazyLock;

use looklab_color::RgbSpace;
use looklab_samples::{InputSpace, PatchId, PatchRow, PatchTable, PatchTableHeader, PrepVersion};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{RngCore, SeedableRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::align::Alignment;
use crate::error::PrepError;
use crate::geom::{GeometryError, Homography, Point, Quad, quad_area, quad_is_convex};
use crate::image::Image3;
use crate::jpeg::DecodedJpeg;
use crate::patch::PatchSampler;
use crate::pipeline::PREP_VERSION;

pub const PATCH_COLUMNS: u32 = 12;
pub const PATCH_ROWS: u32 = 8;
pub const PATCH_COUNT: usize = (PATCH_COLUMNS * PATCH_ROWS) as usize;

/// Digest bits in the session strip; the seed authenticates the session.
const DIGEST_BITS: usize = 16;
const DIGEST_BYTES: usize = DIGEST_BITS / 8;
/// One start marker, the digest bits, one stop marker.
const STRIP_CELLS: usize = DIGEST_BITS + 2;

const CONTENT_ASPECT: f64 = 9.0 / 16.0;
const FIDUCIAL_MARGIN: f64 = 0.05;
const FIDUCIAL_SIZE: f64 = 0.09;
const FIDUCIAL_HOLE: f64 = 0.25;
const STRIP_TOP_GAP: f64 = 0.03;
const STRIP_HEIGHT: f64 = 0.05;
const STRIP_LEFT: f64 = 0.20;
const STRIP_RIGHT: f64 = 0.80;
const FIELD_GAP: f64 = 0.02;
const PATCH_GAP: f64 = 0.12;
const MIN_CONTENT_WIDTH: u32 = 320;

const LUMA: [f64; 3] = [0.2126, 0.7152, 0.0722];

/// The server-issued session seed, 32 bytes, as 64 hexadecimal characters in
/// the pattern URL.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PatternSeed([u8; 32]);

impl PatternSeed {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn parse_hex(text: &str) -> Result<Self, PatternError> {
        let bytes = text.as_bytes();
        if bytes.len() != 64 {
            return Err(PatternError::SeedFormat);
        }
        let mut seed = [0u8; 32];
        for (index, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
            let high = hex_digit(pair[0]).ok_or(PatternError::SeedFormat)?;
            let low = hex_digit(pair[1]).ok_or(PatternError::SeedFormat)?;
            seed[index] = (high << 4) | low;
        }
        Ok(Self(seed))
    }

    pub fn to_hex(self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            out.push(hex_char(byte >> 4));
            out.push(hex_char(byte & 0x0f));
        }
        out
    }

    pub const fn bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The value the session strip carries.
    pub fn digest(self) -> PatternDigest {
        let key = derive(b"looklab/pattern-digest/1", &self);
        let mut bytes = [0u8; DIGEST_BYTES];
        bytes.copy_from_slice(&key[..DIGEST_BYTES]);
        PatternDigest(bytes)
    }
}

impl core::fmt::Debug for PatternSeed {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("PatternSeed(")?;
        formatter.write_str(&self.to_hex())?;
        formatter.write_str(")")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PatternDigest([u8; DIGEST_BYTES]);

impl PatternDigest {
    pub const fn from_bytes(bytes: [u8; DIGEST_BYTES]) -> Self {
        Self(bytes)
    }

    pub const fn bytes(self) -> [u8; DIGEST_BYTES] {
        self.0
    }

    pub fn to_hex(self) -> String {
        let mut out = String::with_capacity(DIGEST_BYTES * 2);
        for byte in self.0 {
            out.push(hex_char(byte >> 4));
            out.push(hex_char(byte & 0x0f));
        }
        out
    }
}

impl core::fmt::Display for PatternDigest {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn hex_char(value: u8) -> char {
    char::from(if value < 10 {
        b'0' + value
    } else {
        b'a' + value - 10
    })
}

fn derive(domain: &[u8], seed: &PatternSeed) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(seed.bytes());
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn center(&self) -> Point {
        Point::new(self.x + self.width * 0.5, self.y + self.height * 0.5)
    }

    /// Shrinks each side by `fraction` of the rectangle's own width or height.
    pub fn inset(&self, fraction: f64) -> Self {
        let dx = self.width * fraction;
        let dy = self.height * fraction;
        Self::new(
            self.x + dx,
            self.y + dy,
            self.width - 2.0 * dx,
            self.height - 2.0 * dy,
        )
    }
}

struct LayoutRects {
    fiducials: [Rect; 4],
    strip: [Rect; STRIP_CELLS],
    patches: [Rect; PATCH_COUNT],
}

/// The pattern in a content box `width × height` whose aspect ratio is 9:16.
fn layout_rects(width: f64, height: f64) -> LayoutRects {
    let margin = FIDUCIAL_MARGIN * width;
    let size = FIDUCIAL_SIZE * width;
    let fiducials = [
        Rect::new(margin, margin, size, size),
        Rect::new(width - margin - size, margin, size, size),
        Rect::new(width - margin - size, height - margin - size, size, size),
        Rect::new(margin, height - margin - size, size, size),
    ];

    let strip_y = margin + size + STRIP_TOP_GAP * width;
    let strip_height = STRIP_HEIGHT * width;
    let strip_x = STRIP_LEFT * width;
    let strip_width = (STRIP_RIGHT - STRIP_LEFT) * width;
    let cell_width = strip_width / STRIP_CELLS as f64;
    let strip = core::array::from_fn(|index| {
        Rect::new(
            strip_x + index as f64 * cell_width,
            strip_y,
            cell_width,
            strip_height,
        )
    });

    let field_gap = FIELD_GAP * width;
    let field_x = margin + size + field_gap;
    let field_y = strip_y + strip_height + field_gap;
    let field_width = (width - margin - size - field_gap - field_x).max(1.0);
    let field_height = (height - margin - size - field_gap - field_y).max(1.0);
    let cell_w = field_width / f64::from(PATCH_COLUMNS);
    let cell_h = field_height / f64::from(PATCH_ROWS);
    let patches = core::array::from_fn(|index| {
        let column = (index % PATCH_COLUMNS as usize) as f64;
        let row = (index / PATCH_COLUMNS as usize) as f64;
        Rect::new(
            field_x + column * cell_w,
            field_y + row * cell_h,
            cell_w,
            cell_h,
        )
    });

    LayoutRects {
        fiducials,
        strip,
        patches,
    }
}

struct Template {
    fiducial_centers: [Point; 4],
    strip: [Rect; STRIP_CELLS],
    patches: [Rect; PATCH_COUNT],
}

impl Template {
    // Rectangles as fractions of the content box.
    fn new() -> Self {
        let rects = layout_rects(9.0, 16.0);
        let normalize = |rect: &Rect| {
            Rect::new(
                rect.x / 9.0,
                rect.y / 16.0,
                rect.width / 9.0,
                rect.height / 16.0,
            )
        };
        Self {
            fiducial_centers: rects.fiducials.map(|rect| {
                let rect = normalize(&rect);
                rect.center()
            }),
            strip: rects.strip.map(|rect| normalize(&rect)),
            patches: rects.patches.map(|rect| normalize(&rect)),
        }
    }
}

static TEMPLATE: LazyLock<Template> = LazyLock::new(Template::new);

/// The rendered pattern: a 9:16 content box centred in the viewport, black
/// elsewhere.
#[derive(Clone, Debug, PartialEq)]
pub struct PatternLayout {
    pub width: u32,
    pub height: u32,
    pub content: Rect,
    fiducials: [Rect; 4],
    strip: [Rect; STRIP_CELLS],
    patches: [Rect; PATCH_COUNT],
}

impl PatternLayout {
    pub fn new(width: u32, height: u32) -> Result<Self, PatternError> {
        let content_width = (f64::from(width)).min(f64::from(height) * CONTENT_ASPECT);
        if width == 0 || height == 0 || content_width < f64::from(MIN_CONTENT_WIDTH) {
            return Err(PatternError::TooSmall {
                min_width: MIN_CONTENT_WIDTH,
                min_height: (f64::from(MIN_CONTENT_WIDTH) / CONTENT_ASPECT).ceil() as u32,
            });
        }
        let content_height = content_width / CONTENT_ASPECT;
        let content = Rect::new(
            (f64::from(width) - content_width) * 0.5,
            (f64::from(height) - content_height) * 0.5,
            content_width,
            content_height,
        );
        let rects = layout_rects(content_width, content_height);
        let shift = |rect: &Rect| {
            Rect::new(
                rect.x + content.x,
                rect.y + content.y,
                rect.width,
                rect.height,
            )
        };
        Ok(Self {
            width,
            height,
            content,
            fiducials: rects.fiducials.map(|rect| shift(&rect)),
            strip: rects.strip.map(|rect| shift(&rect)),
            patches: rects.patches.map(|rect| shift(&rect)),
        })
    }

    pub fn fiducials(&self) -> &[Rect; 4] {
        &self.fiducials
    }

    pub fn strip(&self) -> &[Rect; STRIP_CELLS] {
        &self.strip
    }

    pub fn patches(&self) -> &[Rect; PATCH_COUNT] {
        &self.patches
    }
}

/// Renders the pattern in linear sRGB.
///
/// A page showing it encodes each channel with the sRGB transfer function.
pub fn render(seed: &PatternSeed, width: u32, height: u32) -> Result<Image3, PatternError> {
    let layout = PatternLayout::new(width, height)?;
    let colors = patch_colors();
    let order = patch_order(seed);
    let mut data = vec![[0.0f32; 3]; (width as usize) * (height as usize)];

    for (cell, color) in order.iter().enumerate() {
        fill_rect(
            &mut data,
            width,
            height,
            &layout.patches[cell].inset(PATCH_GAP),
            colors[*color],
        );
    }
    for fiducial in &layout.fiducials {
        fill_rect(&mut data, width, height, fiducial, [1.0; 3]);
        fill_rect(
            &mut data,
            width,
            height,
            &fiducial.inset(FIDUCIAL_HOLE),
            [0.0; 3],
        );
    }
    let bits = digest_bits(seed.digest());
    for (index, cell) in layout.strip.iter().enumerate() {
        let on = if index == 0 || index == STRIP_CELLS - 1 {
            true
        } else {
            bits[index - 1]
        };
        fill_rect(
            &mut data,
            width,
            height,
            cell,
            if on { [1.0; 3] } else { [0.0; 3] },
        );
    }

    Ok(Image3::new(width, height, data)?)
}

fn fill_rect(data: &mut [[f32; 3]], width: u32, height: u32, rect: &Rect, color: [f64; 3]) {
    let x0 = rect.x.floor().clamp(0.0, f64::from(width)) as u32;
    let y0 = rect.y.floor().clamp(0.0, f64::from(height)) as u32;
    let x1 = (rect.x + rect.width).ceil().clamp(0.0, f64::from(width)) as u32;
    let y1 = (rect.y + rect.height).ceil().clamp(0.0, f64::from(height)) as u32;
    for y in y0..y1 {
        let row = y as usize * width as usize;
        for x in x0..x1 {
            data[row + x as usize] = [color[0] as f32, color[1] as f32, color[2] as f32];
        }
    }
}

fn digest_bits(digest: PatternDigest) -> [bool; DIGEST_BITS] {
    let bytes = digest.bytes();
    let mut bits = [false; DIGEST_BITS];
    for (index, bit) in bits.iter_mut().enumerate() {
        *bit = bytes[index / 8] & (1 << (7 - index % 8)) != 0;
    }
    bits
}

/// The 96 display colors: eight neutrals and 22 hues at four levels, in linear
/// sRGB. A patch id is the index in this list, not the cell it lands in.
fn patch_colors() -> [[f64; 3]; PATCH_COUNT] {
    const NEUTRALS: [f64; 8] = [0.0, 0.02, 0.05, 0.10, 0.20, 0.35, 0.60, 1.00];
    const LEVELS: [f64; 4] = [1.0, 0.72, 0.45, 0.25];
    const HUES: usize = 22;

    let mut colors = [[0.0f64; 3]; PATCH_COUNT];
    for (slot, encoded) in NEUTRALS.iter().enumerate() {
        let linear = RgbSpace::Srgb.decode(*encoded);
        colors[slot] = [linear; 3];
    }
    for hue in 0..HUES {
        let encoded = hue_rgb(360.0 * hue as f64 / HUES as f64);
        for (level, value) in LEVELS.iter().enumerate() {
            let index = NEUTRALS.len() + hue * LEVELS.len() + level;
            colors[index] =
                encoded.map(|channel| RgbSpace::Srgb.decode(channel * value).clamp(0.0, 1.0));
        }
    }
    colors
}

/// Fully saturated hue at `degrees` in encoded sRGB.
fn hue_rgb(degrees: f64) -> [f64; 3] {
    let scaled = degrees / 60.0;
    let sector = scaled.floor();
    let fraction = scaled - sector;
    let sector = (sector as usize) % 6;
    match sector {
        0 => [1.0, fraction, 0.0],
        1 => [1.0 - fraction, 1.0, 0.0],
        2 => [0.0, 1.0, fraction],
        3 => [0.0, 1.0 - fraction, 1.0],
        4 => [fraction, 0.0, 1.0],
        _ => [1.0, 0.0, 1.0 - fraction],
    }
}

/// The color index that sits in each grid cell, a Fisher–Yates permutation of
/// the patch list keyed by the seed.
fn patch_order(seed: &PatternSeed) -> [usize; PATCH_COUNT] {
    let mut order: [usize; PATCH_COUNT] = core::array::from_fn(|index| index);
    let mut rng = ChaCha20Rng::from_seed(derive(b"looklab/pattern-order/1", seed));
    for index in (1..PATCH_COUNT).rev() {
        let swap = bounded(&mut rng, (index + 1) as u32) as usize;
        order.swap(index, swap);
    }
    order
}

/// Uniform in `0..bound`.
fn bounded(rng: &mut ChaCha20Rng, bound: u32) -> u32 {
    let zone = u32::MAX - (u32::MAX % bound);
    loop {
        let value = rng.next_u32();
        if value < zone {
            return value % bound;
        }
    }
}

/// Parameters of fiducial search, strip decoding and patch sampling.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PatternParams {
    /// Width the luminance image is reduced to before blob search.
    pub working_width: u32,
    /// Fraction of the 99th percentile luminance a bright pixel must reach.
    pub bright_threshold: f64,
    /// Fiducial blob area as a fraction of the working image.
    pub min_fiducial_area: f64,
    pub max_fiducial_area: f64,
    /// A fiducial's centre is this much darker than its mean.
    pub fiducial_hole_ratio: f64,
    /// Fraction of a patch's linear extent averaged about its centre.
    pub inner_fraction: f64,
    /// Sample points per axis within that region.
    pub patch_taps: u32,
    /// A patch is rejected when a channel's standard deviation over its mean
    /// exceeds this.
    pub max_relative_std: f64,
}

impl Default for PatternParams {
    fn default() -> Self {
        Self {
            working_width: 480,
            bright_threshold: 0.5,
            min_fiducial_area: 0.0002,
            max_fiducial_area: 0.02,
            fiducial_hole_ratio: 0.5,
            inner_fraction: 0.5,
            patch_taps: 7,
            max_relative_std: 0.12,
        }
    }
}

impl PatternParams {
    fn validate(&self) -> Result<(), PatternError> {
        if self.working_width < 64 {
            return Err(PatternError::InvalidParams {
                field: "working_width",
            });
        }
        if !(0.0 < self.bright_threshold && self.bright_threshold < 1.0) {
            return Err(PatternError::InvalidParams {
                field: "bright_threshold",
            });
        }
        if !(self.min_fiducial_area > 0.0
            && self.min_fiducial_area < self.max_fiducial_area
            && self.max_fiducial_area < 1.0)
        {
            return Err(PatternError::InvalidParams {
                field: "fiducial_area",
            });
        }
        if !(0.0 < self.fiducial_hole_ratio && self.fiducial_hole_ratio < 1.0) {
            return Err(PatternError::InvalidParams {
                field: "fiducial_hole_ratio",
            });
        }
        if !(0.0 < self.inner_fraction && self.inner_fraction <= 1.0) {
            return Err(PatternError::InvalidParams {
                field: "inner_fraction",
            });
        }
        if self.patch_taps < 2 {
            return Err(PatternError::InvalidParams {
                field: "patch_taps",
            });
        }
        if !(self.max_relative_std.is_finite() && self.max_relative_std > 0.0) {
            return Err(PatternError::InvalidParams {
                field: "max_relative_std",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum PatternError {
    #[error("invalid pattern parameter `{field}`")]
    InvalidParams { field: &'static str },
    #[error("the pattern seed is not 64 hexadecimal characters")]
    SeedFormat,
    #[error("the viewport is too small for the pattern: at least {min_width}×{min_height}")]
    TooSmall { min_width: u32, min_height: u32 },
    #[error(transparent)]
    Geometry(#[from] GeometryError),
    #[error("four corner fiducials were not found")]
    FiducialsNotFound,
    #[error("the fiducials do not form a pattern frame")]
    FiducialGeometry,
    #[error("the session strip does not decode")]
    StripUndecodable,
    #[error("the session digest is {found}, the expected digest is {expected}")]
    SessionMismatch {
        found: PatternDigest,
        expected: PatternDigest,
    },
    #[error("no pattern patch passed the glare and defect checks")]
    NoPatches,
    #[error("the patch table is invalid: {0}")]
    Table(#[from] looklab_samples::Error),
    #[error(transparent)]
    Image(#[from] PrepError),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PatternDetection {
    pub seed_digest: PatternDigest,
    pub homography: Homography,
    pub patches: PatchTable,
}

/// Finds the fiducials, decodes the session strip and samples every patch.
///
/// `raw` is white-balanced linear camera RGB after orientation and the fitted
/// radial gain, the same image the sample path uses.
pub fn detect(
    seed: &PatternSeed,
    jpeg: &DecodedJpeg,
    raw: &Image3,
    alignment: &Alignment,
    params: &PatternParams,
) -> Result<PatternDetection, PatternError> {
    params.validate()?;
    let gray = Gray::new(&jpeg.image, params.working_width);
    let white = gray.bright_level();
    if !white.is_finite() || white <= 0.0 {
        return Err(PatternError::FiducialsNotFound);
    }
    let centers = gray.fiducials(white, params)?;
    let template_centers = TEMPLATE.fiducial_centers;

    let fiducial_quad: Quad = [
        gray.to_full(centers[0]),
        gray.to_full(centers[1]),
        gray.to_full(centers[2]),
        gray.to_full(centers[3]),
    ];
    let homography = Homography::from_quad(&template_centers, &fiducial_quad)?;
    let mapped = homography
        .map(Point::new(0.5, 0.5))
        .ok_or(PatternError::FiducialGeometry)?;
    let width = f64::from(jpeg.image.width);
    let height = f64::from(jpeg.image.height);
    if mapped.x < 0.0
        || mapped.y < 0.0
        || mapped.x > width
        || mapped.y > height
        || quad_area(&fiducial_quad).abs() < 0.02 * width * height
        || !quad_is_convex(&fiducial_quad)
    {
        return Err(PatternError::FiducialGeometry);
    }

    let digest = gray
        .decode_strip(&homography, white)
        .ok_or(PatternError::StripUndecodable)?;
    let expected = seed.digest();
    if digest != expected {
        return Err(PatternError::SessionMismatch {
            found: digest,
            expected,
        });
    }

    let sampler = PatchSampler {
        jpeg,
        raw,
        alignment,
        inner_fraction: params.inner_fraction,
        taps: params.patch_taps,
        max_relative_std: params.max_relative_std,
    };
    let order = patch_order(seed);
    let mut rows = Vec::with_capacity(PATCH_COUNT);
    for (cell, color_index) in order.iter().enumerate() {
        let rect = TEMPLATE.patches[cell];
        let half = Point::new(rect.width * 0.5, rect.height * 0.5);
        let Some(sample) = sampler.sample(&homography, rect.center(), half) else {
            continue;
        };
        rows.push(PatchRow {
            id: PatchId::new(*color_index as u16),
            in_mean: sample.in_mean,
            in_std: sample.in_std,
            out_mean: sample.out_mean,
            out_std: sample.out_std,
        });
    }
    if rows.is_empty() {
        return Err(PatternError::NoPatches);
    }
    rows.sort_by_key(|row| row.id.get());
    let header = PatchTableHeader {
        n: rows.len() as u32,
        input_space: InputSpace::CameraRgbWbLinear,
        output_space: jpeg.space,
        prep_version: PrepVersion::new(PREP_VERSION),
    };
    let patches = PatchTable::new(header, rows)?;
    Ok(PatternDetection {
        seed_digest: digest,
        homography,
        patches,
    })
}

#[derive(Clone)]
struct Gray {
    width: usize,
    height: usize,
    factor: f64,
    data: Vec<f64>,
}

impl Gray {
    fn new(image: &Image3, max_width: u32) -> Self {
        let factor = (f64::from(image.width) / f64::from(max_width.max(1)))
            .ceil()
            .max(1.0);
        let width = ((f64::from(image.width) / factor).ceil() as usize).max(1);
        let height = ((f64::from(image.height) / factor).ceil() as usize).max(1);
        let mut data = vec![0.0f64; width * height];
        for y in 0..height {
            let y0 = ((y as f64 * factor).floor() as usize).min(image.height as usize - 1);
            let y1 = (((y as f64 + 1.0) * factor).floor() as usize)
                .min(image.height as usize)
                .max(y0 + 1);
            for x in 0..width {
                let x0 = ((x as f64 * factor).floor() as usize).min(image.width as usize - 1);
                let x1 = (((x as f64 + 1.0) * factor).floor() as usize)
                    .min(image.width as usize)
                    .max(x0 + 1);
                let mut sum = 0.0;
                let mut count = 0.0;
                for source_y in y0..y1 {
                    let row = source_y * image.width as usize;
                    for source_x in x0..x1 {
                        let rgb = image.data[row + source_x];
                        let value = f64::from(rgb[0]) * LUMA[0]
                            + f64::from(rgb[1]) * LUMA[1]
                            + f64::from(rgb[2]) * LUMA[2];
                        if value.is_finite() {
                            sum += value;
                            count += 1.0;
                        }
                    }
                }
                data[y * width + x] = if count > 0.0 { sum / count } else { 0.0 };
            }
        }
        Self {
            width,
            height,
            factor,
            data,
        }
    }

    fn to_full(&self, point: Point) -> Point {
        let offset = (self.factor - 1.0) * 0.5;
        Point::new(
            point.x * self.factor + offset,
            point.y * self.factor + offset,
        )
    }

    fn to_working(&self, point: Point) -> Point {
        let offset = (self.factor - 1.0) * 0.5;
        Point::new(
            (point.x - offset) / self.factor,
            (point.y - offset) / self.factor,
        )
    }

    fn sample(&self, point: Point) -> Option<f64> {
        if !point.is_finite() {
            return None;
        }
        let max_x = (self.width - 1) as f64;
        let max_y = (self.height - 1) as f64;
        if point.x < 0.0 || point.y < 0.0 || point.x > max_x || point.y > max_y {
            return None;
        }
        let x0 = point.x.floor() as usize;
        let y0 = point.y.floor() as usize;
        let x1 = (x0 + 1).min(self.width - 1);
        let y1 = (y0 + 1).min(self.height - 1);
        let fx = point.x - x0 as f64;
        let fy = point.y - y0 as f64;
        let top =
            self.data[y0 * self.width + x0] * (1.0 - fx) + self.data[y0 * self.width + x1] * fx;
        let bottom =
            self.data[y1 * self.width + x0] * (1.0 - fx) + self.data[y1 * self.width + x1] * fx;
        Some(top * (1.0 - fy) + bottom * fy)
    }

    /// The 99th percentile luminance, which is the display's white.
    fn bright_level(&self) -> f64 {
        const BINS: usize = 512;
        let max = self
            .data
            .iter()
            .copied()
            .filter(|value| value.is_finite())
            .fold(0.0f64, f64::max);
        if !max.is_finite() || max <= 0.0 {
            return 0.0;
        }
        let mut histogram = vec![0u32; BINS];
        for value in &self.data {
            if value.is_finite() && *value > 0.0 {
                let bin = ((value / max) * (BINS - 1) as f64).round() as usize;
                histogram[bin.min(BINS - 1)] += 1;
            }
        }
        let total: u32 = histogram.iter().sum();
        let target = ((total as f64 * 0.99) as u32).clamp(1, total);
        let mut running = 0u32;
        for (index, count) in histogram.iter().enumerate() {
            running += *count;
            if running >= target {
                return (index as f64 + 0.5) / (BINS - 1) as f64 * max;
            }
        }
        max
    }

    fn fiducials(&self, white: f64, params: &PatternParams) -> Result<[Point; 4], PatternError> {
        let threshold = params.bright_threshold * white;
        let bright: Vec<bool> = self.data.iter().map(|value| *value >= threshold).collect();
        let total = (self.width * self.height) as f64;
        let min_area = ((params.min_fiducial_area * total) as usize).max(8);
        let blobs = self.components(&bright, min_area);
        let candidates: Vec<&Blob> = blobs
            .iter()
            .filter(|blob| {
                let fraction = blob.area as f64 / total;
                let box_width = (blob.x1 - blob.x0 + 1) as f64;
                let box_height = (blob.y1 - blob.y0 + 1) as f64;
                let aspect = box_width / box_height;
                fraction >= params.min_fiducial_area
                    && fraction <= params.max_fiducial_area
                    && (0.5..=2.0).contains(&aspect)
                    && blob.mean >= 0.4 * white
                    && blob.centre_mean < params.fiducial_hole_ratio * blob.mean
            })
            .collect();
        if candidates.len() < 4 {
            return Err(PatternError::FiducialsNotFound);
        }
        let corners = [
            Point::new(0.0, 0.0),
            Point::new(self.width as f64, 0.0),
            Point::new(self.width as f64, self.height as f64),
            Point::new(0.0, self.height as f64),
        ];
        let mut used = vec![false; candidates.len()];
        let mut chosen: [Point; 4] = [Point::default(); 4];
        for (slot, corner) in corners.iter().enumerate() {
            let mut best: Option<(f64, usize)> = None;
            for (index, blob) in candidates.iter().enumerate() {
                if used[index] {
                    continue;
                }
                let distance = blob.centroid.distance(*corner);
                if best.is_none_or(|(closest, _)| distance < closest) {
                    best = Some((distance, index));
                }
            }
            let Some((_, index)) = best else {
                return Err(PatternError::FiducialsNotFound);
            };
            used[index] = true;
            chosen[slot] = candidates[index].centroid;
        }
        Ok(chosen)
    }

    fn components(&self, bright: &[bool], min_area: usize) -> Vec<Blob> {
        let mut visited = vec![false; bright.len()];
        let mut blobs = Vec::new();
        let mut stack: Vec<usize> = Vec::new();
        for start in 0..bright.len() {
            if !bright[start] || visited[start] {
                continue;
            }
            stack.clear();
            stack.push(start);
            visited[start] = true;
            let mut blob = BlobBuilder::new();
            while let Some(index) = stack.pop() {
                let x = index % self.width;
                let y = index / self.width;
                blob.push(x, y, self.data[index]);
                let neighbours = [
                    (x > 0).then(|| index - 1),
                    (x + 1 < self.width).then(|| index + 1),
                    (y > 0).then(|| index - self.width),
                    (y + 1 < self.height).then(|| index + self.width),
                ];
                for neighbour in neighbours.into_iter().flatten() {
                    if bright[neighbour] && !visited[neighbour] {
                        visited[neighbour] = true;
                        stack.push(neighbour);
                    }
                }
            }
            if blob.area >= min_area {
                blobs.push(blob.finish(self));
            }
        }
        blobs
    }

    fn decode_strip(&self, homography: &Homography, white: f64) -> Option<PatternDigest> {
        let threshold = 0.4 * white;
        let mut cells = [0.0f64; STRIP_CELLS];
        for (index, cell) in TEMPLATE.strip.iter().enumerate() {
            let mut sum = 0.0;
            let mut count = 0.0;
            for (sx, sy) in inner_offsets(3) {
                let point = Point::new(cell.x + cell.width * sx, cell.y + cell.height * sy);
                let mapped = homography.map(point)?;
                let working = self.to_working(mapped);
                if let Some(value) = self.sample(working) {
                    sum += value;
                    count += 1.0;
                }
            }
            if count == 0.0 {
                return None;
            }
            cells[index] = sum / count;
        }
        if cells[0] < threshold || cells[STRIP_CELLS - 1] < threshold {
            return None;
        }
        let mut bytes = [0u8; DIGEST_BYTES];
        for index in 0..DIGEST_BITS {
            if cells[index + 1] > threshold {
                bytes[index / 8] |= 1 << (7 - index % 8);
            }
        }
        Some(PatternDigest::from_bytes(bytes))
    }
}

struct Blob {
    area: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    centroid: Point,
    mean: f64,
    centre_mean: f64,
}

struct BlobBuilder {
    area: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    sum_x: f64,
    sum_y: f64,
    sum: f64,
}

impl BlobBuilder {
    fn new() -> Self {
        Self {
            area: 0,
            x0: usize::MAX,
            y0: usize::MAX,
            x1: 0,
            y1: 0,
            sum_x: 0.0,
            sum_y: 0.0,
            sum: 0.0,
        }
    }

    fn push(&mut self, x: usize, y: usize, value: f64) {
        self.area += 1;
        self.x0 = self.x0.min(x);
        self.y0 = self.y0.min(y);
        self.x1 = self.x1.max(x);
        self.y1 = self.y1.max(y);
        self.sum_x += x as f64 + 0.5;
        self.sum_y += y as f64 + 0.5;
        self.sum += value;
    }

    fn finish(self, gray: &Gray) -> Blob {
        let area = self.area as f64;
        let centre_x0 = self.x0 + (self.x1 - self.x0) * 3 / 10;
        let centre_x1 = self.x0 + (self.x1 - self.x0) * 7 / 10;
        let centre_y0 = self.y0 + (self.y1 - self.y0) * 3 / 10;
        let centre_y1 = self.y0 + (self.y1 - self.y0) * 7 / 10;
        let mut sum = 0.0;
        let mut count = 0.0;
        for y in centre_y0..=centre_y1 {
            for x in centre_x0..=centre_x1 {
                sum += gray.data[y * gray.width + x];
                count += 1.0;
            }
        }
        Blob {
            area: self.area,
            x0: self.x0,
            y0: self.y0,
            x1: self.x1,
            y1: self.y1,
            centroid: Point::new(self.sum_x / area, self.sum_y / area),
            mean: self.sum / area,
            centre_mean: if count > 0.0 { sum / count } else { 0.0 },
        }
    }
}

fn inner_offsets(taps: u32) -> Vec<(f64, f64)> {
    let mut offsets = Vec::with_capacity((taps * taps) as usize);
    for iy in 0..taps {
        for ix in 0..taps {
            offsets.push((
                (f64::from(ix) + 0.5) / f64::from(taps),
                (f64::from(iy) + 0.5) / f64::from(taps),
            ));
        }
    }
    offsets
}

#[cfg(test)]
mod tests {
    use super::{
        PATCH_COUNT, PatternDigest, PatternLayout, PatternSeed, STRIP_CELLS, patch_colors,
        patch_order, render,
    };

    fn seed(fill: u8) -> PatternSeed {
        PatternSeed::from_bytes([fill; 32])
    }

    #[test]
    fn render_is_deterministic_and_seed_dependent() {
        let first = render(&seed(0x11), 360, 640).expect("renders");
        let repeat = render(&seed(0x11), 360, 640).expect("renders");
        let other = render(&seed(0x22), 360, 640).expect("renders");
        assert_eq!(first, repeat);
        assert_ne!(first, other);
    }

    #[test]
    fn the_seed_hex_round_trips() {
        let original = seed(0xa5);
        let text = original.to_hex();
        assert_eq!(text.len(), 64);
        assert_eq!(PatternSeed::parse_hex(&text).expect("parses"), original);
        assert!(PatternSeed::parse_hex("not hexadecimal").is_err());
        assert!(PatternSeed::parse_hex(&text[1..]).is_err());
    }

    #[test]
    fn the_digest_covers_sixteen_bits() {
        let digest = seed(0x01).digest();
        assert_eq!(digest.bytes().len(), 2);
        assert_eq!(digest.to_hex().len(), 4);
        assert_ne!(digest, seed(0x02).digest());
        assert_eq!(PatternDigest::from_bytes(digest.bytes()), digest);
    }

    #[test]
    fn the_patch_order_is_a_permutation() {
        let order = patch_order(&seed(0x03));
        let mut seen = [false; PATCH_COUNT];
        for index in order {
            assert!(!seen[index]);
            seen[index] = true;
        }
        assert!(seen.iter().all(|value| *value));
        assert_eq!(order, patch_order(&seed(0x03)));
        assert_ne!(order, patch_order(&seed(0x04)));
    }

    #[test]
    fn the_layout_covers_the_content_box() {
        let layout = PatternLayout::new(1080, 2400).expect("layout");
        assert_eq!(layout.patches().len(), PATCH_COUNT);
        assert_eq!(layout.strip().len(), STRIP_CELLS);
        let content = layout.content;
        assert!((content.width / content.height - 9.0 / 16.0).abs() < 1e-9);
        for patch in layout.patches() {
            assert!(patch.x >= content.x);
            assert!(patch.y >= content.y);
            assert!(patch.x + patch.width <= content.x + content.width + 1e-6);
            assert!(patch.y + patch.height <= content.y + content.height + 1e-6);
        }
        assert!(PatternLayout::new(100, 100).is_err());
    }

    #[test]
    fn patch_colors_span_the_display_gamut() {
        let colors = patch_colors();
        assert_eq!(colors.len(), PATCH_COUNT);
        let max = colors.iter().flatten().copied().fold(0.0f64, f64::max);
        let min = colors.iter().flatten().copied().fold(1.0f64, f64::min);
        assert!((max - 1.0).abs() < 1e-9);
        assert!(min < 0.01);
        let saturated = colors
            .iter()
            .filter(|color| {
                let max = color.iter().copied().fold(0.0f64, f64::max);
                let min = color.iter().copied().fold(1.0f64, f64::min);
                max >= 0.99 && min <= 0.01
            })
            .count();
        assert!(saturated >= 20);
    }
}
