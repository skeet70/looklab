// SPDX-License-Identifier: AGPL-3.0-or-later
//! Renders the X-S10 RAW fixture into a synthetic JPEG for the browser flow.

use std::path::Path;

use looklab_raw::{Orientation, RawDecoder, RawlerDecoder};

const MAX_WIDTH: usize = 2048;

fn main() -> Result<(), String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "workspace root".to_owned())?;
    let mut args = std::env::args().skip(1);
    let raw_path = match args.next() {
        Some(path) => std::path::PathBuf::from(path),
        None => root.join("fixtures/cache/fujifilm-x-s10.raf"),
    };
    let output = match args.next() {
        Some(path) => std::path::PathBuf::from(path),
        None => root.join("fixtures/generated/fujifilm-x-s10.jpg"),
    };
    let raw = std::fs::read(&raw_path).map_err(|error| {
        format!(
            "read {}: {error}; run tools/fetch-fixtures.sh",
            raw_path.display()
        )
    })?;
    let decoded = RawlerDecoder
        .decode(&raw)
        .map_err(|error| error.to_string())?;
    if decoded.orientation != Orientation::Normal {
        return Err(format!(
            "the fixture has orientation {:?}; the synthetic JPEG assumes Normal",
            decoded.orientation
        ));
    }

    let step = decoded.width.div_ceil(MAX_WIDTH).max(1);
    let width = decoded.width / step;
    let height = decoded.height / step;
    let mut pixels = Vec::with_capacity(width * height * 3);
    for y in 0..height {
        for x in 0..width {
            let mut sum = [0.0_f64; 3];
            for dy in 0..step {
                for dx in 0..step {
                    let index = (y * step + dy) * decoded.width + (x * step + dx);
                    for channel in 0..3 {
                        sum[channel] += f64::from(decoded.rgb[index * 3 + channel]);
                    }
                }
            }
            let count = (step * step) as f64;
            for channel in 0..3 {
                let linear = (sum[channel] / count * decoded.as_shot_wb[channel]).clamp(0.0, 1.0);
                let encoded = libm::pow(linear, 1.0 / 2.2);
                pixels.push((encoded * 255.0).round().clamp(0.0, 255.0) as u8);
            }
        }
    }

    let mut jpeg = Vec::new();
    jpeg_encoder::Encoder::new(&mut jpeg, 92)
        .encode(
            &pixels,
            u16::try_from(width).map_err(|_| "width".to_owned())?,
            u16::try_from(height).map_err(|_| "height".to_owned())?,
            jpeg_encoder::ColorType::Rgb,
        )
        .map_err(|error| error.to_string())?;
    let jpeg = splice_exif(&raf_jpeg(&raw), &jpeg);

    let out = output;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(&out, &jpeg).map_err(|error| error.to_string())?;
    println!(
        "{}×{} -> {} ({} bytes)",
        width,
        height,
        out.display(),
        jpeg.len()
    );
    Ok(())
}

fn raf_jpeg(raw: &[u8]) -> Vec<u8> {
    let offset = u32::from_be_bytes(raw[0x54..0x58].try_into().expect("RAF JPEG offset")) as usize;
    let length = u32::from_be_bytes(raw[0x58..0x5c].try_into().expect("RAF JPEG length")) as usize;
    raw[offset..offset + length].to_vec()
}

fn exif_segment(jpeg: &[u8]) -> Option<&[u8]> {
    let mut index = 2;
    while index + 4 <= jpeg.len() {
        if jpeg[index] != 0xff {
            return None;
        }
        let marker = jpeg[index + 1];
        if marker == 0xda || marker == 0xd9 {
            return None;
        }
        let length = usize::from(u16::from_be_bytes([jpeg[index + 2], jpeg[index + 3]]));
        if marker == 0xe1 && jpeg.get(index + 4..index + 10)? == b"Exif\0\0" {
            return Some(&jpeg[index..index + 2 + length]);
        }
        index += 2 + length;
    }
    None
}

/// The RAW's EXIF block, makernotes included.
fn splice_exif(original: &[u8], render: &[u8]) -> Vec<u8> {
    let Some(segment) = exif_segment(original) else {
        return render.to_vec();
    };
    let mut out = Vec::with_capacity(render.len() + segment.len());
    out.extend_from_slice(&render[..2]);
    out.extend_from_slice(segment);
    out.extend_from_slice(&render[2..]);
    out
}
