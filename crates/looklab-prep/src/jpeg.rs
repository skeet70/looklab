// SPDX-License-Identifier: AGPL-3.0-or-later
//! JPEG decoding to linear RGB and colour-space detection.

use looklab_color::RgbSpace;
use looklab_samples::OutputSpace;
use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

use crate::error::PrepError;
use crate::image::Image3;

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

impl DecodedJpeg {
    pub fn apply_orientation(&self, orientation: looklab_meta::Orientation) -> DecodedJpeg {
        let (out_width, out_height) =
            crate::image::orientation_size(self.image.width, self.image.height, orientation);
        let last_x = self.image.width - 1;
        let last_y = self.image.height - 1;
        let mut flags = vec![false; usize::try_from(out_width * out_height).expect("fits usize")];
        for y in 0..out_height {
            for x in 0..out_width {
                let (source_x, source_y) =
                    crate::image::orientation_source(x, y, last_x, last_y, orientation);
                let source = usize::try_from(source_y).expect("row fits usize")
                    * usize::try_from(self.image.width).expect("width fits usize")
                    + usize::try_from(source_x).expect("column fits usize");
                flags[usize::try_from(y).expect("row fits usize")
                    * usize::try_from(out_width).expect("width fits usize")
                    + usize::try_from(x).expect("column fits usize")] = self
                    .code_values_near_clip
                    .get(source)
                    .copied()
                    .expect("flags match the image");
            }
        }
        DecodedJpeg {
            image: self.image.apply_orientation(orientation),
            space: self.space,
            space_assumed: self.space_assumed,
            code_values_near_clip: flags,
        }
    }
}

pub fn decode_jpeg(bytes: &[u8]) -> Result<DecodedJpeg, PrepError> {
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

    let (space, space_assumed) = detect_space(bytes);
    let rgb_space = match space {
        OutputSpace::Srgb => RgbSpace::Srgb,
        OutputSpace::AdobeRgb => RgbSpace::AdobeRgb,
    };
    let (chunks, remainder) = pixels.as_chunks::<3>();
    if !remainder.is_empty() {
        return Err(PrepError::JpegStructure("decoded sample count mismatch"));
    }
    let mut data = Vec::with_capacity(count);
    let mut code_values_near_clip = Vec::with_capacity(count);
    for codes in chunks {
        code_values_near_clip.push(codes.iter().any(|&value| value >= 250 || value <= 2));
        let mut linear = [0.0_f32; 3];
        for channel in 0..3 {
            let encoded = f64::from(codes[channel]) / 255.0;
            let encoded = encoded.clamp(0.0, 1.0);
            linear[channel] = rgb_space.decode(encoded) as f32;
        }
        data.push(linear);
    }

    Ok(DecodedJpeg {
        image: Image3::new(width, height, data)?,
        space,
        space_assumed,
        code_values_near_clip,
    })
}

/// Collects EXIF and ICC colour-space hints from the JPEG segment list.
fn detect_space(bytes: &[u8]) -> (OutputSpace, bool) {
    let mut exif_colorspace = None;
    let mut icc = Vec::new();
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
                if let Some(chunk) = payload.strip_prefix(b"ICC_PROFILE\0")
                    && let Some(profile) = chunk.get(2..)
                {
                    icc.extend_from_slice(profile);
                }
            }
            _ => {}
        }
        index = next + length;
    }

    let icc_adobe = contains(&icc, b"Adobe RGB");
    let icc_srgb = contains(&icc, b"sRGB") || contains(&icc, b"sRGB IEC61966");
    if icc_adobe {
        (OutputSpace::AdobeRgb, false)
    } else if icc_srgb {
        (OutputSpace::Srgb, false)
    } else if let Some(value) = exif_colorspace {
        match value {
            1 => (OutputSpace::Srgb, false),
            2 => (OutputSpace::AdobeRgb, false),
            _ => (OutputSpace::Srgb, true),
        }
    } else {
        (OutputSpace::Srgb, true)
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
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
