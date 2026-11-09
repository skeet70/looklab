// SPDX-License-Identifier: AGPL-3.0-or-later
//! JPEG decoding to linear RGB and colour-space detection.

use looklab_color::RgbSpace;
use looklab_samples::OutputSpace;
use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

use looklab_meta::Orientation;

use crate::error::PrepError;
use crate::image::{Image3, LinearPixels, orientation_size, orientation_source};

/// The decoded JPEG in its own primaries, still in stored orientation.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedJpeg {
    pub image: Image3,
    pub space: OutputSpace,
    /// No EXIF, ICC or Adobe marker identified the space; sRGB was assumed.
    pub space_assumed: bool,
    /// Per pixel: any channel code value is at least 250 or at most 2.
    pub code_values_near_clip: Vec<bool>,
}

impl LinearPixels for DecodedJpeg {
    fn width(&self) -> u32 {
        self.image.width
    }

    fn height(&self) -> u32 {
        self.image.height
    }

    fn pixel(&self, x: u32, y: u32) -> Option<[f32; 3]> {
        Image3::pixel(&self.image, x, y)
    }

    fn is_empty(&self) -> bool {
        self.image.data.is_empty()
    }

    fn sample_bilinear(&self, x: f64, y: f64) -> Option<[f32; 3]> {
        Image3::sample_bilinear(&self.image, x, y)
    }

    fn downsample_area(&self, max_pixels: u32) -> Image3 {
        Image3::downsample_area(&self.image, max_pixels)
    }
}

/// A JPEG that can be sampled and tested for near-clipped code values.
pub(crate) trait JpegPixels: LinearPixels {
    fn space(&self) -> OutputSpace;
    fn near_clip(&self, x: u32, y: u32) -> bool;
}

impl JpegPixels for DecodedJpeg {
    fn space(&self) -> OutputSpace {
        self.space
    }

    fn near_clip(&self, x: u32, y: u32) -> bool {
        if x >= self.image.width || y >= self.image.height {
            return false;
        }
        let index = usize::try_from(y).expect("row fits usize")
            * usize::try_from(self.image.width).expect("width fits usize")
            + usize::try_from(x).expect("column fits usize");
        self.code_values_near_clip
            .get(index)
            .copied()
            .unwrap_or(false)
    }
}

/// Decoded RGB8 codes with a per-space linear table.
pub(crate) struct JpegPlane {
    width: u32,
    height: u32,
    oriented_width: u32,
    oriented_height: u32,
    data: Vec<u8>,
    lut: [f32; 256],
    orientation: Orientation,
    space: OutputSpace,
    space_assumed: bool,
}

impl JpegPlane {
    pub(crate) fn orient(mut self, orientation: Orientation) -> Self {
        let (width, height) = orientation_size(self.width, self.height, orientation);
        self.oriented_width = width;
        self.oriented_height = height;
        self.orientation = orientation;
        self
    }

    fn source(&self, x: u32, y: u32) -> Option<(u32, u32)> {
        if x >= self.oriented_width || y >= self.oriented_height {
            return None;
        }
        Some(orientation_source(
            x,
            y,
            self.width - 1,
            self.height - 1,
            self.orientation,
        ))
    }
}

// The stored plane keeps its un-oriented size; the trait reports the displayed size.
#[allow(clippy::misnamed_getters)]
impl LinearPixels for JpegPlane {
    fn width(&self) -> u32 {
        self.oriented_width
    }

    fn height(&self) -> u32 {
        self.oriented_height
    }

    fn pixel(&self, x: u32, y: u32) -> Option<[f32; 3]> {
        let (source_x, source_y) = self.source(x, y)?;
        let base = (usize::try_from(source_y).ok()? * usize::try_from(self.width).ok()?
            + usize::try_from(source_x).ok()?)
            * 3;
        let codes = self.data.get(base..base + 3)?;
        Some([
            self.lut[usize::from(codes[0])],
            self.lut[usize::from(codes[1])],
            self.lut[usize::from(codes[2])],
        ])
    }

    fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

impl JpegPixels for JpegPlane {
    fn space(&self) -> OutputSpace {
        self.space
    }

    fn near_clip(&self, x: u32, y: u32) -> bool {
        let Some((source_x, source_y)) = self.source(x, y) else {
            return false;
        };
        let base = (usize::try_from(source_y).expect("row fits usize")
            * usize::try_from(self.width).expect("width fits usize")
            + usize::try_from(source_x).expect("column fits usize"))
            * 3;
        self.data
            .get(base..base + 3)
            .is_some_and(|codes| codes.iter().any(|&value| value >= 250 || value <= 2))
    }
}

fn rgb_space(space: OutputSpace) -> RgbSpace {
    match space {
        OutputSpace::Srgb => RgbSpace::Srgb,
        OutputSpace::AdobeRgb => RgbSpace::AdobeRgb,
    }
}

fn linear_lut(space: OutputSpace) -> [f32; 256] {
    let rgb_space = rgb_space(space);
    std::array::from_fn(|code| rgb_space.decode(f64::from(code as u8) / 255.0) as f32)
}

pub(crate) fn decode_jpeg_plane(bytes: &[u8]) -> Result<JpegPlane, PrepError> {
    let (space, space_assumed) = detect_space(bytes)?;
    let options = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::RGB)
        .set_use_unsafe(false);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    let pixels = decoder
        .decode()
        .map_err(|error| PrepError::DecodeJpeg(error.to_string()))?;
    let info = decoder
        .info()
        .ok_or(PrepError::JpegStructure("missing frame header"))?;
    let width = u32::from(info.width);
    let height = u32::from(info.height);
    if width == 0 || height == 0 {
        return Err(PrepError::ImageEmpty);
    }
    let count = usize::try_from(width)
        .ok()
        .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
        .ok_or(PrepError::ImageTooLarge)?;
    if pixels.len() != count * 3 {
        return Err(PrepError::JpegStructure("decoded sample count mismatch"));
    }

    Ok(JpegPlane {
        width,
        height,
        oriented_width: width,
        oriented_height: height,
        data: pixels,
        lut: linear_lut(space),
        orientation: Orientation::Normal,
        space,
        space_assumed,
    })
}

pub fn decode_jpeg(bytes: &[u8]) -> Result<DecodedJpeg, PrepError> {
    let plane = decode_jpeg_plane(bytes)?;
    let count = usize::try_from(plane.width)
        .ok()
        .and_then(|w| {
            usize::try_from(plane.height)
                .ok()
                .and_then(|h| w.checked_mul(h))
        })
        .ok_or(PrepError::ImageTooLarge)?;
    let mut data = Vec::with_capacity(count);
    let mut code_values_near_clip = Vec::with_capacity(count);
    for codes in plane.data.as_chunks::<3>().0 {
        code_values_near_clip.push(codes.iter().any(|&value| value >= 250 || value <= 2));
        data.push([
            plane.lut[usize::from(codes[0])],
            plane.lut[usize::from(codes[1])],
            plane.lut[usize::from(codes[2])],
        ]);
    }

    Ok(DecodedJpeg {
        image: Image3::new(plane.width, plane.height, data)?,
        space: plane.space,
        space_assumed: plane.space_assumed,
        code_values_near_clip,
    })
}

/// Collects EXIF and ICC colour-space hints from the JPEG segment list.
/// An explicit ICC profile that cannot be identified as sRGB or Adobe RGB is an error.
fn detect_space(bytes: &[u8]) -> Result<(OutputSpace, bool), PrepError> {
    let mut exif_colorspace = None;
    let mut icc = IccChunks::default();
    let mut index = 2_usize;
    while index + 4 <= bytes.len() {
        if bytes[index] != 0xFF {
            break;
        }
        let mut marker = bytes[index + 1];
        let mut next = index + 2;
        while marker == 0xFF && next < bytes.len() {
            marker = bytes[next];
            next += 1;
        }
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            index = next;
            continue;
        }
        if marker == 0xD9 || marker == 0xDA || next + 2 > bytes.len() {
            break;
        }
        let length = usize::from(u16::from_be_bytes([bytes[next], bytes[next + 1]]));
        if length < 2 || next + length > bytes.len() {
            break;
        }
        let payload = &bytes[next + 2..next + length];
        match marker {
            0xE1 => {
                if let Some(tiff) = payload.strip_prefix(b"Exif\0\0") {
                    exif_colorspace = exif_colorspace.or_else(|| exif_color_space(tiff));
                }
            }
            0xE2 => {
                if let Some(chunk) = payload.strip_prefix(b"ICC_PROFILE\0") {
                    icc.push(chunk)?;
                }
            }
            _ => {}
        }
        index = next + length;
    }

    if let Some(space) = icc.space()? {
        Ok((space, false))
    } else if let Some(value) = exif_colorspace {
        Ok(match value {
            1 => (OutputSpace::Srgb, false),
            2 => (OutputSpace::AdobeRgb, false),
            _ => (OutputSpace::Srgb, true),
        })
    } else {
        Ok((OutputSpace::Srgb, true))
    }
}

#[derive(Default)]
struct IccChunks<'a> {
    total: Option<u8>,
    chunks: Vec<Option<&'a [u8]>>,
}

impl<'a> IccChunks<'a> {
    /// `chunk` is the APP2 payload after the `ICC_PROFILE\0` signature.
    fn push(&mut self, chunk: &'a [u8]) -> Result<(), PrepError> {
        let Some((&sequence, rest)) = chunk.split_first() else {
            return Err(PrepError::UnsupportedColorProfile);
        };
        let Some((&total, data)) = rest.split_first() else {
            return Err(PrepError::UnsupportedColorProfile);
        };
        if sequence == 0 || total == 0 || sequence > total || data.is_empty() {
            return Err(PrepError::UnsupportedColorProfile);
        }
        if self.total.is_some_and(|known| known != total) {
            return Err(PrepError::UnsupportedColorProfile);
        }
        self.total = Some(total);
        let slots = usize::from(total);
        if self.chunks.len() < slots {
            self.chunks.resize(slots, None);
        }
        if self.chunks[usize::from(sequence - 1)]
            .replace(data)
            .is_some()
        {
            return Err(PrepError::UnsupportedColorProfile);
        }
        Ok(())
    }

    fn space(&self) -> Result<Option<OutputSpace>, PrepError> {
        if self.total.is_none() {
            return Ok(None);
        }
        if self.chunks.iter().any(Option::is_none) {
            return Err(PrepError::UnsupportedColorProfile);
        }
        let capacity = self
            .chunks
            .iter()
            .filter_map(|chunk| *chunk)
            .map(|data| data.len())
            .sum();
        let mut profile = Vec::with_capacity(capacity);
        for chunk in &self.chunks {
            if let Some(data) = *chunk {
                profile.extend_from_slice(data);
            }
        }
        identify_space(&profile)
            .map(Some)
            .ok_or(PrepError::UnsupportedColorProfile)
    }
}

fn identify_space(profile: &[u8]) -> Option<OutputSpace> {
    if profile.len() < 132 {
        return None;
    }
    if profile.get(36..40)? != b"acsp".as_slice() {
        return None;
    }
    if profile.get(16..20)? != b"RGB ".as_slice() {
        return None;
    }
    let declared = usize::try_from(read_u32(profile, 0)?).ok()?;
    if declared != profile.len() {
        return None;
    }
    let tags = usize::try_from(read_u32(profile, 128)?).ok()?;
    let table_end = 132_usize.checked_add(tags.checked_mul(12)?)?;
    if table_end > declared {
        return None;
    }
    let mut description = None;
    for index in 0..tags {
        let entry = 132 + index * 12;
        if profile.get(entry..entry + 4)? != b"desc".as_slice() {
            continue;
        }
        if description.is_some() {
            return None;
        }
        let offset = usize::try_from(read_u32(profile, entry + 4)?).ok()?;
        let size = usize::try_from(read_u32(profile, entry + 8)?).ok()?;
        description = Some(profile.get(offset..offset.checked_add(size)?)?);
    }
    description_space(description?)
}

fn description_space(tag: &[u8]) -> Option<OutputSpace> {
    match tag.get(..4)? {
        b"desc" => text_description_space(tag),
        b"mluc" => multi_localized_space(tag),
        _ => None,
    }
}

fn text_description_space(tag: &[u8]) -> Option<OutputSpace> {
    let ascii_count = usize::try_from(read_u32(tag, 8)?).ok()?;
    let ascii = tag.get(12..12usize.checked_add(ascii_count)?)?;
    let unicode_count_at = 12usize.checked_add(ascii_count)?.checked_add(4)?;
    let unicode_count = usize::try_from(read_u32(tag, unicode_count_at)?).ok()?;
    let unicode_start = unicode_count_at.checked_add(4)?;
    let unicode_end = unicode_start.checked_add(unicode_count.checked_mul(2)?)?;
    let unicode = tag.get(unicode_start..unicode_end)?;

    let mut space = None;
    observe_description(&mut space, ascii_recognized(ascii))?;
    observe_description(&mut space, utf16_recognized(unicode))?;
    space
}

fn multi_localized_space(tag: &[u8]) -> Option<OutputSpace> {
    let count = usize::try_from(read_u32(tag, 8)?).ok()?;
    let record_size = usize::try_from(read_u32(tag, 12)?).ok()?;
    if record_size < 12 {
        return None;
    }
    let records_end = 16usize.checked_add(count.checked_mul(record_size)?)?;
    if records_end > tag.len() {
        return None;
    }
    let mut space = None;
    for index in 0..count {
        let record = 16 + index * record_size;
        let record = tag.get(record..record.checked_add(12)?)?;
        let length = usize::try_from(read_u32(record, 4)?).ok()?;
        let offset = usize::try_from(read_u32(record, 8)?).ok()?;
        let bytes = tag.get(offset..offset.checked_add(length)?)?;
        observe_description(&mut space, utf16_recognized(bytes))?;
    }
    space
}

enum Recognized {
    Empty,
    Space(OutputSpace),
    Unknown,
}

fn observe_description(space: &mut Option<OutputSpace>, description: Recognized) -> Option<()> {
    match description {
        Recognized::Empty => Some(()),
        Recognized::Unknown => None,
        Recognized::Space(candidate) => {
            if space.is_some_and(|known| known != candidate) {
                return None;
            }
            *space = Some(candidate);
            Some(())
        }
    }
}

fn ascii_recognized(bytes: &[u8]) -> Recognized {
    let text = trim_ascii(bytes);
    if text.is_empty() {
        Recognized::Empty
    } else {
        name_space_bytes(text)
    }
}

fn name_space_bytes(bytes: &[u8]) -> Recognized {
    if (starts_with_bytes(bytes, b"Adobe RGB") && is_name_separator_byte(bytes.get(9).copied()))
        || (starts_with_bytes(bytes, b"AdobeRGB") && is_name_separator_byte(bytes.get(8).copied()))
    {
        Recognized::Space(OutputSpace::AdobeRgb)
    } else if starts_with_bytes(bytes, b"sRGB") && is_name_separator_byte(bytes.get(4).copied()) {
        Recognized::Space(OutputSpace::Srgb)
    } else {
        Recognized::Unknown
    }
}

fn starts_with_bytes(bytes: &[u8], name: &[u8]) -> bool {
    bytes.len() >= name.len() && bytes[..name.len()].eq_ignore_ascii_case(name)
}

fn is_name_separator_byte(byte: Option<u8>) -> bool {
    byte.is_none_or(|byte| byte == 0 || byte == b'(' || byte.is_ascii_whitespace())
}

const NAME_SCAN_UNITS: usize = 16;

fn utf16_recognized(bytes: &[u8]) -> Recognized {
    let (pairs, remainder) = bytes.as_chunks::<2>();
    if !remainder.is_empty() {
        return Recognized::Unknown;
    }
    let mut start = 0;
    while start < pairs.len() && is_ignorable_unit(u16::from_be_bytes(pairs[start])) {
        start += 1;
        if start >= NAME_SCAN_UNITS && start < pairs.len() {
            return Recognized::Unknown;
        }
    }
    if start == pairs.len() {
        return Recognized::Empty;
    }
    let end = (start + NAME_SCAN_UNITS).min(pairs.len());
    let mut units = [0_u16; NAME_SCAN_UNITS];
    for (unit, pair) in units.iter_mut().zip(&pairs[start..end]) {
        *unit = u16::from_be_bytes(*pair);
    }
    name_space_units(&units[..end - start])
}

fn name_space_units(units: &[u16]) -> Recognized {
    if (starts_with_units(units, b"Adobe RGB") && is_name_separator_unit(units.get(9).copied()))
        || (starts_with_units(units, b"AdobeRGB") && is_name_separator_unit(units.get(8).copied()))
    {
        Recognized::Space(OutputSpace::AdobeRgb)
    } else if starts_with_units(units, b"sRGB") && is_name_separator_unit(units.get(4).copied()) {
        Recognized::Space(OutputSpace::Srgb)
    } else {
        Recognized::Unknown
    }
}

fn starts_with_units(units: &[u16], name: &[u8]) -> bool {
    units.len() >= name.len()
        && units[..name.len()].iter().zip(name).all(|(&unit, &byte)| {
            unit <= u16::from(u8::MAX) && (unit as u8).eq_ignore_ascii_case(&byte)
        })
}

fn is_name_separator_unit(unit: Option<u16>) -> bool {
    unit.is_none_or(|unit| unit <= u16::from(u8::MAX) && is_name_separator_byte(Some(unit as u8)))
}

fn is_ignorable_unit(unit: u16) -> bool {
    unit == 0
        || unit == 0xFEFF
        || (unit <= u16::from(u8::MAX) && (unit as u8).is_ascii_whitespace())
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| *byte != 0 && !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| *byte != 0 && !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &bytes[start..end]
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let value = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}

fn exif_color_space(tiff: &[u8]) -> Option<u16> {
    if tiff.len() < 8 {
        return None;
    }
    let little_endian = match &tiff[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let read_u32 = |offset: usize| -> Option<u32> {
        let value = tiff.get(offset..offset + 4)?;
        Some(if little_endian {
            u32::from_le_bytes([value[0], value[1], value[2], value[3]])
        } else {
            u32::from_be_bytes([value[0], value[1], value[2], value[3]])
        })
    };
    let ifd0 = usize::try_from(read_u32(4)?).ok()?;
    let mut exif_ifd = None;
    for (tag, entry_value) in ifd_entries(tiff, ifd0, little_endian)? {
        if tag == 0xA001 {
            return u16::try_from(entry_value).ok();
        }
        if tag == 0x8769 {
            exif_ifd = Some(entry_value);
        }
    }
    let exif_ifd = usize::try_from(exif_ifd?).ok()?;
    for (tag, entry_value) in ifd_entries(tiff, exif_ifd, little_endian)? {
        if tag == 0xA001 {
            return u16::try_from(entry_value).ok();
        }
    }
    None
}

fn ifd_entries(tiff: &[u8], offset: usize, little_endian: bool) -> Option<Vec<(u16, u32)>> {
    let count_bytes = tiff.get(offset..offset + 2)?;
    let count = if little_endian {
        u16::from_le_bytes([count_bytes[0], count_bytes[1]])
    } else {
        u16::from_be_bytes([count_bytes[0], count_bytes[1]])
    };
    let mut entries = Vec::with_capacity(usize::from(count));
    for index in 0..usize::from(count) {
        let start = offset + 2 + index * 12;
        let entry = tiff.get(start..start + 12)?;
        let tag = if little_endian {
            u16::from_le_bytes([entry[0], entry[1]])
        } else {
            u16::from_be_bytes([entry[0], entry[1]])
        };
        let kind = if little_endian {
            u16::from_le_bytes([entry[2], entry[3]])
        } else {
            u16::from_be_bytes([entry[2], entry[3]])
        };
        if kind == 3 {
            let value = if little_endian {
                u16::from_le_bytes([entry[8], entry[9]])
            } else {
                u16::from_be_bytes([entry[8], entry[9]])
            };
            entries.push((tag, u32::from(value)));
        } else if kind == 4 {
            let value = if little_endian {
                u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]])
            } else {
                u32::from_be_bytes([entry[8], entry[9], entry[10], entry[11]])
            };
            entries.push((tag, value));
        }
    }
    Some(entries)
}
