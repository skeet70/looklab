// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end preparation: decode, orient, align, mask, sample, shuffle, [docs/03 §2.1](../../../docs/03-data-model.md). One re-render group shares a single pixel set, shuffle and `in_rgb` column.

use looklab_color::linalg::Mat3;
use looklab_meta::{
    ParsedMetadata, RemovedField, RemovedFields, SanitizedMetadata, WhiteBalanceMultipliers, parse,
    sanitize,
};
use looklab_raw::{DecodedRaw, RawDecoder, RawlerDecoder};
use looklab_samples::{
    InputSpace, PrepParamsHash, PrepVersion, PriorTransformVersion, RerenderGroupId, SampleFlags,
    SampleHeader, SampleSet,
};
use rand_chacha::rand_core::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::align::{AlignParams, Alignment, align};
use crate::chart::ChartRegion;
use crate::error::PrepError;
use crate::geom::Point;
use crate::image::Image3;
use crate::jpeg::{DecodedJpeg, decode_jpeg};
use crate::mask::{MaskOptions, build_mask};
use crate::sample::{SampleParams, select_pixels};
use crate::seed::SamplingSeed;

/// Registered preparation version id, [docs/03 §2.2](../../../docs/03-data-model.md).
pub const PREP_VERSION: u32 = 1;

const DEFAULT_DOWNSAMPLE_MAX_PIXELS: u32 = 1_500_000;

/// RAW channels at or above this fraction of white are clipped.
const RAW_CLIP_LEVEL: f32 = 0.98;

/// Integration points per axis when averaging the JPEG over a sample's footprint.
const JPEG_FOOTPRINT_TAPS: u32 = 3;

/// Camera-to-XYZ for white-balanced linear camera RGB, with the CCT and the
/// version of the body input transform it came from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PriorTransform {
    xyz_from_camera: Mat3,
    cct_k: f64,
    version: PriorTransformVersion,
}

impl PriorTransform {
    pub fn new(xyz_from_camera: Mat3, cct_k: f64, version: PriorTransformVersion) -> Self {
        Self {
            xyz_from_camera,
            cct_k,
            version,
        }
    }

    /// CCT unknown (zero in the header).
    pub fn from_matrix(xyz_from_camera: Mat3, version: PriorTransformVersion) -> Self {
        Self::new(xyz_from_camera, 0.0, version)
    }

    /// Camera-to-XYZ at the as-shot neutral through the file's DNG profile, for white-balanced RGB ([DNG 1.6 chapter 6](https://www.adobe.com/go/dng_spec)).
    pub fn from_camera_matrices(
        matrices: &looklab_raw::ColorMatrices,
        as_shot_wb: [f64; 3],
        version: PriorTransformVersion,
    ) -> Result<Self, PrepError> {
        use looklab_color::dng::{CalibrationMatrix, DngColorSpec, calibration_temperature};

        if !as_shot_wb
            .iter()
            .all(|value| value.is_finite() && *value > 0.0)
        {
            return Err(PrepError::InvalidWbMultipliers);
        }
        let mut color = Vec::new();
        for camera in matrices.matrices() {
            if let Some(cct) = calibration_temperature(camera.illuminant.code())
                && let Some(entry) = CalibrationMatrix::new(cct, camera.xyz_to_camera)
            {
                color.push(entry);
            }
        }
        let mut forward = Vec::new();
        for camera in matrices.forward_matrices() {
            if let Some(cct) = calibration_temperature(camera.illuminant.code())
                && let Some(entry) = CalibrationMatrix::new(cct, camera.camera_to_xyz)
            {
                forward.push(entry);
            }
        }
        let spec = DngColorSpec::new(color, forward).map_err(|_| PrepError::PriorTransform)?;
        let neutral = [1.0 / as_shot_wb[0], 1.0, 1.0 / as_shot_wb[2]];
        let d65 = spec
            .from_neutral(neutral)
            .map_err(|_| PrepError::PriorTransform)?;
        let balance = Mat3::diag([
            1.0 / as_shot_wb[0],
            1.0 / as_shot_wb[1],
            1.0 / as_shot_wb[2],
        ]);
        Ok(Self::new(d65.matrix.mul(&balance), d65.cct, version))
    }

    pub fn matrix(&self) -> &Mat3 {
        &self.xyz_from_camera
    }

    pub fn cct_k(&self) -> f64 {
        self.cct_k
    }

    pub fn version(&self) -> PriorTransformVersion {
        self.version
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrepParams {
    pub align: AlignParams,
    pub sample: SampleParams,
    pub mask: MaskOptions,
    /// Area-average target of step 4.
    pub downsample_max_pixels: u32,
}

impl Default for PrepParams {
    fn default() -> Self {
        Self {
            align: AlignParams::default(),
            sample: SampleParams::default(),
            mask: MaskOptions::default(),
            downsample_max_pixels: DEFAULT_DOWNSAMPLE_MAX_PIXELS,
        }
    }
}

impl PrepParams {
    /// SHA-256 over the canonical postcard encoding.
    pub fn hash(&self) -> Result<PrepParamsHash, PrepError> {
        let encoded = postcard::to_allocvec(self)?;
        let digest = Sha256::digest(&encoded);
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(digest.as_slice());
        Ok(PrepParamsHash::new(bytes))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreparedImage {
    pub samples: SampleSet,
    pub sanitized: SanitizedMetadata,
    pub removed: Vec<RemovedField>,
}

/// A RAW after orientation, alignment and the fitted radial gain.
#[derive(Clone, Debug, PartialEq)]
pub struct AlignedRaw {
    image: Image3,
    alignment: Alignment,
}

impl AlignedRaw {
    /// White-balanced linear camera RGB.
    pub fn image(&self) -> &Image3 {
        &self.image
    }

    pub fn alignment(&self) -> &Alignment {
        &self.alignment
    }

    pub fn size(&self) -> (u32, u32) {
        (self.image.width, self.image.height)
    }
}

/// Decodes, orients, aligns and gain-corrects the RAW side of a pair.
pub fn prepare_aligned_raw(
    raw: &DecodedRaw,
    reference: &DecodedJpeg,
    params: &PrepParams,
) -> Result<AlignedRaw, PrepError> {
    let orientation = meta_orientation(raw.orientation);
    let raw_image = raw_linear_image(raw)?.apply_orientation(orientation);
    let alignment = align(&raw_image, reference, &params.align)?;
    Ok(AlignedRaw {
        image: apply_gain(&raw_image, &alignment),
        alignment,
    })
}

/// Prepares one RAW/JPEG pair from file bytes.
pub fn prepare_pair(
    raw_bytes: &[u8],
    jpeg_bytes: &[u8],
    prior: &PriorTransform,
    seed: SamplingSeed,
    params: &PrepParams,
) -> Result<PreparedImage, PrepError> {
    let mut prepared = prepare_group(raw_bytes, &[jpeg_bytes], prior, seed, params)?;
    prepared.pop().ok_or(PrepError::NoJpeg)
}

/// Prepares one RAW/JPEG pair, excluding `chart` from density sampling.
pub fn prepare_pair_with_chart(
    raw_bytes: &[u8],
    jpeg_bytes: &[u8],
    prior: &PriorTransform,
    seed: SamplingSeed,
    params: &PrepParams,
    chart: Option<&ChartRegion>,
) -> Result<PreparedImage, PrepError> {
    let mut prepared = prepare_group_chart(raw_bytes, &[jpeg_bytes], prior, seed, params, chart)?;
    prepared.pop().ok_or(PrepError::NoJpeg)
}

/// Prepares one RAW and every JPEG rendered from it.
pub fn prepare_group(
    raw_bytes: &[u8],
    jpegs: &[&[u8]],
    prior: &PriorTransform,
    seed: SamplingSeed,
    params: &PrepParams,
) -> Result<Vec<PreparedImage>, PrepError> {
    prepare_group_chart(raw_bytes, jpegs, prior, seed, params, None)
}

fn prepare_group_chart(
    raw_bytes: &[u8],
    jpegs: &[&[u8]],
    prior: &PriorTransform,
    seed: SamplingSeed,
    params: &PrepParams,
    chart: Option<&ChartRegion>,
) -> Result<Vec<PreparedImage>, PrepError> {
    if jpegs.is_empty() {
        return Err(PrepError::NoJpeg);
    }
    let metadata: ParsedMetadata = parse(raw_bytes)?;
    let decoded = RawlerDecoder.decode(raw_bytes)?;
    let (mut raw_sanitized, raw_removed) = sanitize(&metadata);
    raw_sanitized.set_as_shot_wb(white_balance(decoded.as_shot_wb));

    let mut decoded_jpegs = Vec::with_capacity(jpegs.len());
    let mut rendering = Vec::with_capacity(jpegs.len());
    for bytes in jpegs {
        let mut jpeg = decode_jpeg(bytes)?;
        let mut jpeg_metadata = None;
        if let Ok(parsed) = looklab_meta::parse_jpeg(bytes) {
            if let Some(orientation) = parsed.exif.orientation {
                jpeg = jpeg.apply_orientation(orientation);
            }
            jpeg_metadata = Some(sanitize(&parsed));
        }
        decoded_jpegs.push(jpeg);
        rendering.push(jpeg_metadata);
    }

    let members: Vec<&DecodedJpeg> = decoded_jpegs.iter().collect();
    let exposure_hint = exposure_hint_ev(&metadata);
    let sets = prepare_sets(
        &decoded,
        &members,
        prior,
        exposure_hint,
        seed,
        params,
        chart,
    )?;
    Ok(sets
        .into_iter()
        .zip(rendering)
        .map(|(samples, rendering)| {
            let sanitized = match &rendering {
                Some((jpeg_sanitized, _)) => merge_rendering(&raw_sanitized, jpeg_sanitized),
                None => raw_sanitized.clone(),
            };
            let removed =
                removed_union(&raw_removed, rendering.as_ref().map(|(_, removed)| removed));
            PreparedImage {
                samples,
                sanitized,
                removed,
            }
        })
        .collect())
}

pub fn prepare_pair_decoded(
    raw: &DecodedRaw,
    jpeg: &DecodedJpeg,
    prior: &PriorTransform,
    seed: SamplingSeed,
    params: &PrepParams,
) -> Result<SampleSet, PrepError> {
    let mut sets = prepare_decoded(raw, &[jpeg], prior, seed, params)?;
    sets.pop().ok_or(PrepError::NoJpeg)
}

/// Prepares one RAW and its decoded JPEGs, with no metadata.
pub fn prepare_decoded(
    raw: &DecodedRaw,
    jpegs: &[&DecodedJpeg],
    prior: &PriorTransform,
    seed: SamplingSeed,
    params: &PrepParams,
) -> Result<Vec<SampleSet>, PrepError> {
    prepare_decoded_chart(raw, jpegs, prior, seed, params, None)
}

/// Prepares one RAW and its decoded JPEGs, excluding `chart` from density
/// sampling.
pub fn prepare_decoded_with_chart(
    raw: &DecodedRaw,
    jpegs: &[&DecodedJpeg],
    prior: &PriorTransform,
    seed: SamplingSeed,
    params: &PrepParams,
    chart: Option<&ChartRegion>,
) -> Result<Vec<SampleSet>, PrepError> {
    prepare_decoded_chart(raw, jpegs, prior, seed, params, chart)
}

fn prepare_decoded_chart(
    raw: &DecodedRaw,
    jpegs: &[&DecodedJpeg],
    prior: &PriorTransform,
    seed: SamplingSeed,
    params: &PrepParams,
    chart: Option<&ChartRegion>,
) -> Result<Vec<SampleSet>, PrepError> {
    if jpegs.is_empty() {
        return Err(PrepError::NoJpeg);
    }
    prepare_sets(raw, jpegs, prior, 0.0, seed, params, chart)
}

/// RAW capture fields with the JPEG's rendering fields.
fn merge_rendering(raw: &SanitizedMetadata, jpeg: &SanitizedMetadata) -> SanitizedMetadata {
    let mut merged = raw.clone();
    merged.look = jpeg.look.clone();
    merged.software = jpeg.software.clone();
    merged.render_tag = jpeg.render_tag.clone();
    if merged.make.is_none() {
        merged.make = jpeg.make.clone();
    }
    if merged.model.is_none() {
        merged.model = jpeg.model.clone();
    }
    merged
}

fn removed_union(raw: &RemovedFields, jpeg: Option<&RemovedFields>) -> Vec<RemovedField> {
    let mut removed: Vec<RemovedField> = raw.iter().collect();
    if let Some(jpeg) = jpeg {
        for field in jpeg.iter() {
            if !removed.contains(&field) {
                removed.push(field);
            }
        }
    }
    removed
}

fn meta_orientation(orientation: looklab_raw::Orientation) -> looklab_meta::Orientation {
    match orientation {
        looklab_raw::Orientation::Normal => looklab_meta::Orientation::Normal,
        looklab_raw::Orientation::HorizontalFlip => looklab_meta::Orientation::FlipHorizontal,
        looklab_raw::Orientation::Rotate180 => looklab_meta::Orientation::Rotate180,
        looklab_raw::Orientation::VerticalFlip => looklab_meta::Orientation::FlipVertical,
        looklab_raw::Orientation::Transpose => looklab_meta::Orientation::Transpose,
        looklab_raw::Orientation::Rotate90 => looklab_meta::Orientation::Rotate90Clockwise,
        looklab_raw::Orientation::Transverse => looklab_meta::Orientation::Transverse,
        looklab_raw::Orientation::Rotate270 => looklab_meta::Orientation::Rotate270Clockwise,
    }
}

fn prepare_sets(
    raw: &DecodedRaw,
    jpegs: &[&DecodedJpeg],
    prior: &PriorTransform,
    exposure_hint_ev: f32,
    seed: SamplingSeed,
    params: &PrepParams,
    chart: Option<&ChartRegion>,
) -> Result<Vec<SampleSet>, PrepError> {
    params.sample.validate()?;
    for multiplier in raw.as_shot_wb {
        if !(multiplier.is_finite()
            && (f64::from(looklab_samples::MIN_WB_MULTIPLIER)
                ..=f64::from(looklab_samples::MAX_WB_MULTIPLIER))
                .contains(&multiplier))
        {
            return Err(PrepError::InvalidWbMultipliers);
        }
    }

    let orientation = meta_orientation(raw.orientation);
    let clip_marker = raw_clip_marker(raw)?.apply_orientation(orientation);
    let reference = jpegs[0];
    let aligned = prepare_aligned_raw(raw, reference, params)?;
    let downsampled = aligned.image.downsample_area(params.downsample_max_pixels);
    let clipped = clip_marker.downsample_area(params.downsample_max_pixels);
    debug_assert_eq!(clipped.data.len(), downsampled.data.len());
    let clipped_flags: Vec<bool> = clipped.data.iter().map(|px| px[0] > 0.0).collect();

    let mut mask = build_mask(&downsampled, &params.mask);
    mask.exclude_clipped(&clipped_flags);
    if let Some(region) = chart {
        mask.exclude_region(&chart_exclusion(
            region,
            &aligned.alignment,
            reference,
            &downsampled,
            aligned.image.width,
            aligned.image.height,
        )?);
    }

    let mut rng = seed.rng();
    let mut group_bytes = [0u8; 16];
    rng.fill_bytes(&mut group_bytes);
    let selection = select_pixels(
        &downsampled,
        &mask,
        prior.matrix(),
        &params.sample,
        &mut rng,
    )?;

    let in_rgb: Vec<[f32; 3]> = selection
        .indices()
        .iter()
        .map(|index| clamp_rgb(downsampled.data[*index]))
        .collect();
    let scale_x = f64::from(aligned.image.width) / f64::from(downsampled.width);
    let scale_y = f64::from(aligned.image.height) / f64::from(downsampled.height);

    let cct_hint_k = if prior.cct_k().is_finite() {
        prior.cct_k().clamp(0.0, 100_000.0) as f32
    } else {
        0.0
    };
    let header_base = SampleHeader {
        n: in_rgb.len() as u32,
        input_space: InputSpace::CameraRgbWbLinear,
        output_space: reference.space,
        wb_multipliers: wb_multipliers(raw.as_shot_wb),
        cct_hint_k,
        exposure_hint_ev: exposure_hint_ev.clamp(
            -looklab_samples::MAX_EXPOSURE_HINT_EV,
            looklab_samples::MAX_EXPOSURE_HINT_EV,
        ),
        rerender_group: (jpegs.len() > 1).then(|| RerenderGroupId::new(group_bytes)),
        prep_version: PrepVersion::new(PREP_VERSION),
        prior_transform_version: prior.version(),
        prep_params_hash: prep_params_hash(params, chart)?,
    };

    let mut sets = Vec::with_capacity(jpegs.len());
    for jpeg in jpegs {
        let mut out_rgb = Vec::with_capacity(in_rgb.len());
        let mut flags = Vec::with_capacity(in_rgb.len());
        for &index in selection.indices() {
            let x = (index % downsampled.width as usize) as f64;
            let y = (index / downsampled.width as usize) as f64;
            let x_raw =
                ((x + 0.5) * scale_x - 0.5).clamp(0.0, f64::from(aligned.image.width) - 1.0);
            let y_raw =
                ((y + 0.5) * scale_y - 0.5).clamp(0.0, f64::from(aligned.image.height) - 1.0);
            let (rgb, near_clip) = jpeg_footprint(
                jpeg,
                &aligned.alignment,
                (x_raw, y_raw),
                (scale_x, scale_y),
                aligned.image.width,
                aligned.image.height,
            )?;
            out_rgb.push(clamp_rgb(rgb));
            flags.push(if near_clip {
                SampleFlags::NEAR_CLIP_OUTPUT
            } else {
                SampleFlags::NONE
            });
        }
        let mut header = header_base.clone();
        header.output_space = jpeg.space;
        sets.push(SampleSet::new(
            header,
            in_rgb.clone(),
            out_rgb,
            selection.density_w().to_vec(),
            flags,
        )?);
    }
    Ok(sets)
}

/// SHA-256 over the parameters, plus the corners and JPEG size of the chart
/// region when one shaped the density sampling.
fn prep_params_hash(
    params: &PrepParams,
    chart: Option<&ChartRegion>,
) -> Result<PrepParamsHash, PrepError> {
    let Some(region) = chart else {
        return params.hash();
    };
    let mut encoded = postcard::to_allocvec(params)?;
    for point in region.corners() {
        encoded.extend_from_slice(&point.x.to_le_bytes());
        encoded.extend_from_slice(&point.y.to_le_bytes());
    }
    let (width, height) = region.jpeg_size();
    encoded.extend_from_slice(&width.to_le_bytes());
    encoded.extend_from_slice(&height.to_le_bytes());
    let digest = Sha256::digest(&encoded);
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(digest.as_slice());
    Ok(PrepParamsHash::new(bytes))
}

fn raw_linear_image(raw: &DecodedRaw) -> Result<Image3, PrepError> {
    let pixels = raw
        .width
        .checked_mul(raw.height)
        .ok_or(PrepError::InvalidParams {
            field: "raw_dimensions",
        })?;
    if raw.rgb.len() < pixels * 3 {
        return Err(PrepError::InvalidParams {
            field: "raw_rgb_len",
        });
    }
    let mut data = Vec::with_capacity(pixels);
    for index in 0..pixels {
        let base = index * 3;
        data.push([
            (f64::from(raw.rgb[base]) * raw.as_shot_wb[0]) as f32,
            (f64::from(raw.rgb[base + 1]) * raw.as_shot_wb[1]) as f32,
            (f64::from(raw.rgb[base + 2]) * raw.as_shot_wb[2]) as f32,
        ]);
    }
    Image3::new(raw.width as u32, raw.height as u32, data)
}

fn raw_clip_marker(raw: &DecodedRaw) -> Result<Image3, PrepError> {
    let pixels = raw
        .width
        .checked_mul(raw.height)
        .ok_or(PrepError::InvalidParams {
            field: "raw_dimensions",
        })?;
    if raw.rgb.len() < pixels * 3 {
        return Err(PrepError::InvalidParams {
            field: "raw_rgb_len",
        });
    }
    let mut data = Vec::with_capacity(pixels);
    for index in 0..pixels {
        let base = index * 3;
        let clipped = raw.rgb[base..base + 3]
            .iter()
            .any(|channel| *channel >= RAW_CLIP_LEVEL);
        data.push([if clipped { 1.0 } else { 0.0 }; 3]);
    }
    Image3::new(raw.width as u32, raw.height as u32, data)
}

/// Per-pixel membership of `region`, mapped through the alignment onto the
/// downsampled RAW grid.
fn chart_exclusion(
    region: &ChartRegion,
    alignment: &Alignment,
    reference: &DecodedJpeg,
    target: &Image3,
    raw_width: u32,
    raw_height: u32,
) -> Result<Vec<bool>, PrepError> {
    let (jpeg_width, jpeg_height) = (reference.image.width, reference.image.height);
    if region.jpeg_size() != (jpeg_width, jpeg_height) {
        return Err(PrepError::ChartRegionSize);
    }
    let scale_x = f64::from(raw_width) / f64::from(target.width);
    let scale_y = f64::from(raw_height) / f64::from(target.height);
    let mut inside = vec![false; target.data.len()];

    let mut min_x = f64::from(target.width);
    let mut min_y = f64::from(target.height);
    let mut max_x = 0.0f64;
    let mut max_y = 0.0f64;
    for corner in region.corners() {
        let (raw_x, raw_y) = alignment.jpeg_to_raw(
            corner.x,
            corner.y,
            jpeg_width,
            jpeg_height,
            raw_width,
            raw_height,
        );
        let x = (raw_x + 0.5) / scale_x - 0.5;
        let y = (raw_y + 0.5) / scale_y - 0.5;
        if !x.is_finite() || !y.is_finite() {
            return Err(PrepError::ChartRegionSize);
        }
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    let x0 = min_x.floor().clamp(0.0, f64::from(target.width - 1)) as u32;
    let y0 = min_y.floor().clamp(0.0, f64::from(target.height - 1)) as u32;
    let x1 = max_x.ceil().clamp(0.0, f64::from(target.width - 1)) as u32;
    let y1 = max_y.ceil().clamp(0.0, f64::from(target.height - 1)) as u32;
    for y in y0..=y1 {
        let raw_y = (f64::from(y) + 0.5) * scale_y - 0.5;
        for x in x0..=x1 {
            let raw_x = (f64::from(x) + 0.5) * scale_x - 0.5;
            let (jpeg_x, jpeg_y) =
                alignment.raw_to_jpeg(raw_x, raw_y, raw_width, raw_height, jpeg_width, jpeg_height);
            let index = y as usize * target.width as usize + x as usize;
            inside[index] = region.contains(Point::new(jpeg_x, jpeg_y));
        }
    }
    Ok(inside)
}

/// Applies the alignment's radial gain on normalized radius from the centre.
fn apply_gain(image: &Image3, alignment: &Alignment) -> Image3 {
    let width = image.width as usize;
    let height = image.height as usize;
    let center_x = (width as f64 - 1.0) * 0.5;
    let center_y = (height as f64 - 1.0) * 0.5;
    let data = image
        .data
        .iter()
        .enumerate()
        .map(|(index, rgb)| {
            let x = ((index % width) as f64 - center_x) / center_x;
            let y = ((index / width) as f64 - center_y) / center_y;
            let gain = alignment.gain_at((x * x + y * y).sqrt()) as f32;
            [rgb[0] * gain, rgb[1] * gain, rgb[2] * gain]
        })
        .collect();
    Image3 {
        width: image.width,
        height: image.height,
        data,
    }
}

/// Area-averages the JPEG over the footprint of one RAW sample.
fn jpeg_footprint(
    jpeg: &DecodedJpeg,
    alignment: &Alignment,
    center: (f64, f64),
    footprint: (f64, f64),
    raw_width: u32,
    raw_height: u32,
) -> Result<([f32; 3], bool), PrepError> {
    let taps = f64::from(JPEG_FOOTPRINT_TAPS);
    let mut sum = [0.0f64; 3];
    let mut count = 0.0f64;
    let mut near_clip = false;
    for ty in 0..JPEG_FOOTPRINT_TAPS {
        for tx in 0..JPEG_FOOTPRINT_TAPS {
            let fx = (f64::from(tx) + 0.5) / taps - 0.5;
            let fy = (f64::from(ty) + 0.5) / taps - 0.5;
            let x = center.0 + fx * footprint.0;
            let y = center.1 + fy * footprint.1;
            let (jx, jy) = alignment.raw_to_jpeg(
                x,
                y,
                raw_width,
                raw_height,
                jpeg.image.width,
                jpeg.image.height,
            );
            let Some(rgb) = jpeg.image.sample_bilinear(jx, jy) else {
                return Err(PrepError::SampleOutsideJpeg);
            };
            if !rgb.iter().all(|channel| channel.is_finite()) {
                return Err(PrepError::NonFiniteSample);
            }
            for (total, value) in sum.iter_mut().zip(rgb) {
                *total += f64::from(value);
            }
            count += 1.0;
            let pixel = nearest_pixel(jx, jy, jpeg.image.width, jpeg.image.height);
            near_clip |= jpeg
                .code_values_near_clip
                .get(pixel)
                .copied()
                .unwrap_or(false);
        }
    }
    Ok((
        [
            (sum[0] / count) as f32,
            (sum[1] / count) as f32,
            (sum[2] / count) as f32,
        ],
        near_clip,
    ))
}

fn clamp_rgb(rgb: [f32; 3]) -> [f32; 3] {
    rgb.map(|channel| channel.clamp(looklab_samples::MIN_RGB, looklab_samples::MAX_RGB))
}

fn wb_multipliers(wb: [f64; 3]) -> [f32; 3] {
    wb.map(|value| {
        (value as f32).clamp(
            looklab_samples::MIN_WB_MULTIPLIER,
            looklab_samples::MAX_WB_MULTIPLIER,
        )
    })
}

fn white_balance(wb: [f64; 3]) -> WhiteBalanceMultipliers {
    WhiteBalanceMultipliers {
        r: wb[0],
        g: wb[1],
        b: wb[2],
    }
}

fn nearest_pixel(x: f64, y: f64, width: u32, height: u32) -> usize {
    let px = x.round().clamp(0.0, width as f64 - 1.0) as usize;
    let py = y.round().clamp(0.0, height as f64 - 1.0) as usize;
    py * width as usize + px
}

/// EV at ISO 100 from the exposure equation, ISO 2720:1974.
fn exposure_hint_ev(metadata: &ParsedMetadata) -> f32 {
    let exif = &metadata.exif;
    let (Some(iso), Some(shutter), Some(aperture)) = (exif.iso, exif.exposure_time, exif.f_number)
    else {
        return 0.0;
    };
    let n = aperture.f_number();
    let t = shutter.seconds();
    let s = f64::from(iso.value());
    let ev = 2.0 * libm::log2(n) - libm::log2(t) - libm::log2(s / 100.0);
    if !ev.is_finite() {
        return 0.0;
    }
    ev.clamp(
        -f64::from(looklab_samples::MAX_EXPOSURE_HINT_EV),
        f64::from(looklab_samples::MAX_EXPOSURE_HINT_EV),
    ) as f32
}

#[cfg(test)]
mod tests {
    use looklab_meta::{FilmMode, FujiTag, Iso, LookSettings, ParsedTag};

    use super::{SanitizedMetadata, merge_rendering};

    fn sanitized(film: Option<FilmMode>, iso: Option<u32>) -> SanitizedMetadata {
        SanitizedMetadata {
            make: Some("FUJIFILM".to_owned()),
            model: Some("X-S10".to_owned()),
            firmware: Some("1.00".to_owned()),
            lens_model: None,
            focal_length: None,
            iso: iso.and_then(Iso::new),
            shutter: None,
            aperture: None,
            exposure_compensation: None,
            as_shot_wb: None,
            capture_month: None,
            software: None,
            render_tag: None,
            look: film.map(|value| LookSettings {
                film_mode: Some(ParsedTag {
                    tag: FujiTag::FilmMode,
                    raw: 0x0100,
                    value: Some(value),
                }),
                ..LookSettings::default()
            }),
        }
    }

    #[test]
    fn rendering_metadata_comes_from_the_jpeg() {
        let raw = sanitized(Some(FilmMode::Provia), Some(200));
        let jpeg = sanitized(Some(FilmMode::ClassicChrome), Some(800));
        let merged = merge_rendering(&raw, &jpeg);

        assert_eq!(merged.look, jpeg.look);
        assert_eq!(merged.iso, raw.iso);
        assert_eq!(merged.firmware, raw.firmware);
    }
}
