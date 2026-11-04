// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end preparation: decode, orient, align, mask, sample, shuffle, [docs/03 §2.1](../../../docs/03-data-model.md). One re-render group shares a single pixel set, shuffle and `in_rgb` column.

use looklab_color::linalg::Mat3;
use looklab_meta::{
    ParsedMetadata, RemovedFields, SanitizedMetadata, WhiteBalanceMultipliers, parse, sanitize,
};
use looklab_raw::{DecodedRaw, RawDecoder, RawlerDecoder};
use looklab_samples::{
    InputSpace, PrepParamsHash, PrepVersion, PriorTransformVersion, RerenderGroupId, SampleFlags,
    SampleHeader, SampleSet,
};
use rand_chacha::rand_core::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::align::{AlignParams, align};
use crate::error::PrepError;
use crate::image::Image3;
use crate::jpeg::{DecodedJpeg, decode_jpeg};
use crate::mask::{MaskOptions, build_mask};
use crate::sample::{SampleParams, select_pixels};
use crate::seed::SamplingSeed;

/// Registered preparation version id, [docs/03 §2.2](../../../docs/03-data-model.md).
pub const PREP_VERSION: u32 = 1;

const DEFAULT_DOWNSAMPLE_MAX_PIXELS: u32 = 1_500_000;

/// Camera-to-XYZ at the image's CCT, with the input-transform version used.
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

    /// Camera-to-XYZ at the as-shot neutral through the file's DNG profile,
    /// [DNG 1.6 chapter 6](https://www.adobe.com/go/dng_spec).
    pub fn from_camera_matrices(
        matrices: &looklab_raw::ColorMatrices,
        as_shot_wb: [f64; 3],
        version: PriorTransformVersion,
    ) -> Result<Self, PrepError> {
        use looklab_color::dng::{CalibrationMatrix, DngColorSpec, calibration_temperature};

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
        Ok(Self::new(d65.matrix, d65.cct, version))
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
    pub removed: RemovedFields,
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

/// Prepares one RAW and every JPEG rendered from it.
pub fn prepare_group(
    raw_bytes: &[u8],
    jpegs: &[&[u8]],
    prior: &PriorTransform,
    seed: SamplingSeed,
    params: &PrepParams,
) -> Result<Vec<PreparedImage>, PrepError> {
    if jpegs.is_empty() {
        return Err(PrepError::NoJpeg);
    }
    let metadata: ParsedMetadata = parse(raw_bytes)?;
    let decoded = RawlerDecoder.decode(raw_bytes)?;
    let (mut sanitized, removed) = sanitize(&metadata);
    sanitized.set_as_shot_wb(WhiteBalanceMultipliers {
        r: decoded.as_shot_wb[0],
        g: decoded.as_shot_wb[1],
        b: decoded.as_shot_wb[2],
    });
    let mut decoded_jpegs = Vec::with_capacity(jpegs.len());
    for bytes in jpegs {
        let mut jpeg = decode_jpeg(bytes)?;
        if let Ok(jpeg_metadata) = looklab_meta::parse_jpeg(bytes)
            && let Some(orientation) = jpeg_metadata.exif.orientation
        {
            jpeg = jpeg.apply_orientation(orientation);
        }
        decoded_jpegs.push(jpeg);
    }
    let members: Vec<&DecodedJpeg> = decoded_jpegs.iter().collect();
    let exposure_hint = exposure_hint_ev(&metadata);
    let sets = prepare_sets(&decoded, &members, prior, exposure_hint, seed, params)?;
    Ok(sets
        .into_iter()
        .map(|samples| PreparedImage {
            samples,
            sanitized: sanitized.clone(),
            removed: removed.clone(),
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
    if jpegs.is_empty() {
        return Err(PrepError::NoJpeg);
    }
    prepare_sets(raw, jpegs, prior, 0.0, seed, params)
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

    let raw_image = raw_linear_image(raw)?.apply_orientation(meta_orientation(raw.orientation));
    let reference = jpegs[0];
    let alignment = align(&raw_image, reference, &params.align)?;
    let gain_corrected = apply_gain(&raw_image, &alignment);
    let downsampled = gain_corrected.downsample_area(params.downsample_max_pixels);
    let mask = build_mask(&downsampled, &params.mask);

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
    let scale_x = raw_image.width as f64 / downsampled.width as f64;
    let scale_y = raw_image.height as f64 / downsampled.height as f64;

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
        prep_params_hash: params.hash()?,
    };

    let mut sets = Vec::with_capacity(jpegs.len());
    for jpeg in jpegs {
        let mut out_rgb = Vec::with_capacity(in_rgb.len());
        let mut flags = Vec::with_capacity(in_rgb.len());
        for &index in selection.indices() {
            let x = (index % downsampled.width as usize) as f64;
            let y = (index / downsampled.width as usize) as f64;
            let x_raw = ((x + 0.5) * scale_x - 0.5).clamp(0.0, raw_image.width as f64 - 1.0);
            let y_raw = ((y + 0.5) * scale_y - 0.5).clamp(0.0, raw_image.height as f64 - 1.0);
            let (jx, jy) = alignment.raw_to_jpeg(
                x_raw,
                y_raw,
                raw_image.width,
                raw_image.height,
                jpeg.image.width,
                jpeg.image.height,
            );
            let Some(rgb) = jpeg.image.sample_bilinear(jx, jy) else {
                return Err(PrepError::SampleOutsideJpeg);
            };
            if !rgb.iter().all(|channel| channel.is_finite()) {
                return Err(PrepError::NonFiniteSample);
            }
            out_rgb.push(clamp_rgb(rgb));
            let pixel = nearest_pixel(jx, jy, jpeg.image.width, jpeg.image.height);
            if jpeg
                .code_values_near_clip
                .get(pixel)
                .copied()
                .unwrap_or(false)
            {
                flags.push(SampleFlags::NEAR_CLIP_OUTPUT);
            } else {
                flags.push(SampleFlags::NONE);
            }
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
    Ok(Image3 {
        width: raw.width as u32,
        height: raw.height as u32,
        data,
    })
}

/// Applies the alignment's radial gain on normalized radius from the centre.
fn apply_gain(image: &Image3, alignment: &crate::align::Alignment) -> Image3 {
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
