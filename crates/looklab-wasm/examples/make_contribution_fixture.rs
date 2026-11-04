// SPDX-License-Identifier: AGPL-3.0-or-later
//! Renders the committed X-S10 RAW into a synthetic JPEG for the browser flow.

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
        None => root.join("fixtures/fujifilm-x-s10.raf"),
    };
    let output = match args.next() {
        Some(path) => std::path::PathBuf::from(path),
        None => root.join("fixtures/generated/fujifilm-x-s10.jpg"),
    };
    let raw = std::fs::read(&raw_path).map_err(|error| error.to_string())?;
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
    let jpeg = insert_exif(&jpeg);

    let out = output;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(&out, &jpeg).map_err(|error| error.to_string())?;
    println!("{}×{} -> {} ({} bytes)", width, height, out.display(), jpeg.len());
    Ok(())
}

/// A minimal EXIF APP1 block carrying orientation 1.
fn insert_exif(jpeg: &[u8]) -> Vec<u8> {
    let tiff: [u8; 26] = [
        b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, IFD at 8
        0x01, 0x00, // one entry
        0x12, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // orientation
        0x00, 0x00, 0x00, 0x00, // next IFD
    ];
    let mut payload = b"Exif\0\0".to_vec();
    payload.extend_from_slice(&tiff);
    let length = u16::try_from(payload.len() + 2).unwrap_or(u16::MAX);
    let mut out = Vec::with_capacity(jpeg.len() + payload.len() + 4);
    out.extend_from_slice(&jpeg[..2]);
    out.extend_from_slice(&[0xff, 0xe1]);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&payload);
    out.extend_from_slice(&jpeg[2..]);
    out
}
