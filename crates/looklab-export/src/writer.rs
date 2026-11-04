// SPDX-License-Identifier: AGPL-3.0-or-later
//! Byte-level artifact writers and their readers: `.cube`, HaldCLUT PNG, `native_json`.

use looklab_schema::{Format, Interpolation};
use std::io::Cursor;

use crate::artifact::{Lut3d, MAX_CURVE1D_SIZE, MAX_LUT_SIZE, Shaper1d};
use crate::bake::BakeOutput;
use crate::error::{ExportError, Result};
use crate::native;

/// A parsed 3D `.cube` file.
#[derive(Clone, Debug, PartialEq)]
pub struct CubeFile {
    pub lut: Lut3d,
    pub domain: [f64; 2],
}

/// A parsed 1D `.cube` shaper.
#[derive(Clone, Debug, PartialEq)]
pub struct Cube1dFile {
    pub shaper: Shaper1d,
    pub domain: [f64; 2],
}

/// A 3D `.cube` in Resolve/Adobe layout: `LUT_3D_SIZE`, one `DOMAIN_MIN`/`DOMAIN_MAX`
/// for all channels, then one `r g b` line per node with red varying fastest.
pub fn write_cube(lut: &Lut3d, title: &str, domain: [f64; 2]) -> String {
    let mut out = String::new();
    out.push_str("# looklab-export: 3D LUT, red fastest\n");
    match &lut.shaper {
        Some(shaper) => out.push_str(&format!(
            "# shaper: separate 1D cube of {} points, applied per channel first\n",
            shaper.size
        )),
        None => out.push_str("# shaper: none\n"),
    }
    out.push_str(&format!("TITLE \"{}\"\n", escape_title(title)));
    out.push_str(&format!("LUT_3D_SIZE {}\n", lut.size));
    out.push_str(&domain_lines(domain));
    for b in 0..lut.size as usize {
        for g in 0..lut.size as usize {
            for r in 0..lut.size as usize {
                let value = lut.data[lut.index(r, g, b)];
                out.push_str(&format!(
                    "{} {} {}\n",
                    format_f32(value[0]),
                    format_f32(value[1]),
                    format_f32(value[2])
                ));
            }
        }
    }
    out
}

/// The shaper reads the insertion code over `range`; the grid it feeds reads the
/// shaper's output, so its domain is unit.
pub fn cube_files(lut: &Lut3d, title: &str, range: [f64; 2]) -> (String, Option<String>) {
    let grid_domain = if lut.shaper.is_some() {
        [0.0, 1.0]
    } else {
        range
    };
    let grid = write_cube(lut, title, grid_domain);
    let shaper = lut
        .shaper
        .as_ref()
        .map(|shaper| write_cube_1d(shaper, title, range));
    (grid, shaper)
}

/// A 1D `.cube` shaper: `LUT_1D_SIZE`, the same domain lines, then the red channel
/// per step.
pub fn write_cube_1d(shaper: &Shaper1d, title: &str, domain: [f64; 2]) -> String {
    let mut out = String::new();
    out.push_str("# looklab-export: 1D shaper, insertion code to 3D LUT coordinate\n");
    out.push_str(&format!("TITLE \"{}\"\n", escape_title(title)));
    out.push_str(&format!("LUT_1D_SIZE {}\n", shaper.size));
    out.push_str(&domain_lines(domain));
    for value in &shaper.values {
        let text = format_f32(*value);
        out.push_str(&format!("{text} {text} {text}\n"));
    }
    out
}

fn domain_lines(domain: [f64; 2]) -> String {
    format!(
        "DOMAIN_MIN {min} {min} {min}\nDOMAIN_MAX {max} {max} {max}\n",
        min = format_f64(domain[0]),
        max = format_f64(domain[1])
    )
}

fn escape_title(title: &str) -> String {
    title.replace('"', "'")
}

/// Shortest decimal that round-trips an f32.
fn format_f32(value: f32) -> String {
    format!("{value}")
}

fn format_f64(value: f64) -> String {
    format!("{value}")
}

/// # Errors
/// [`ExportError::Cube`] for a missing or duplicated size, a per-channel domain, or
/// the wrong node count.
pub fn read_cube(text: &str) -> Result<CubeFile> {
    let parsed = parse_cube_text(text)?;
    let size = parsed
        .size_3d
        .ok_or_else(|| ExportError::Cube("missing LUT_3D_SIZE".to_string()))?;
    if parsed.size_1d.is_some() {
        return Err(ExportError::Cube("expected a 3D LUT".to_string()));
    }
    if !(2..=MAX_LUT_SIZE).contains(&size) {
        return Err(ExportError::Cube("LUT_3D_SIZE out of range".to_string()));
    }
    let expected = (size as u64).pow(3);
    if parsed.values.len() as u64 != expected {
        return Err(ExportError::Cube(format!(
            "expected {expected} nodes, found {}",
            parsed.values.len()
        )));
    }
    Ok(CubeFile {
        lut: Lut3d {
            size,
            data: parsed.values,
            shaper: None,
        },
        domain: parsed.domain,
    })
}

/// # Errors
/// [`ExportError::Cube`] when the file is not a 1D LUT or its channels differ.
pub fn read_cube_1d(text: &str) -> Result<Cube1dFile> {
    let parsed = parse_cube_text(text)?;
    let size = parsed
        .size_1d
        .ok_or_else(|| ExportError::Cube("missing LUT_1D_SIZE".to_string()))?;
    if parsed.size_3d.is_some() {
        return Err(ExportError::Cube("expected a 1D LUT".to_string()));
    }
    if !(2..=MAX_CURVE1D_SIZE).contains(&size) {
        return Err(ExportError::Cube("LUT_1D_SIZE out of range".to_string()));
    }
    if parsed.values.len() != size as usize {
        return Err(ExportError::Cube(format!(
            "expected {size} steps, found {}",
            parsed.values.len()
        )));
    }
    let mut values = Vec::with_capacity(size as usize);
    for value in &parsed.values {
        if value[0].to_bits() != value[1].to_bits() || value[0].to_bits() != value[2].to_bits() {
            return Err(ExportError::Cube("shaper channels differ".to_string()));
        }
        values.push(value[0]);
    }
    Ok(Cube1dFile {
        shaper: Shaper1d { size, values },
        domain: parsed.domain,
    })
}

struct ParsedCube {
    size_3d: Option<u32>,
    size_1d: Option<u32>,
    domain: [f64; 2],
    values: Vec<[f32; 3]>,
}

fn parse_cube_text(text: &str) -> Result<ParsedCube> {
    let mut parsed = ParsedCube {
        size_3d: None,
        size_1d: None,
        domain: [0.0, 1.0],
        values: Vec::new(),
    };
    let mut domain_min: Option<[f64; 3]> = None;
    let mut domain_max: Option<[f64; 3]> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let head = fields.next().unwrap_or_default();
        match head {
            "TITLE" => {}
            "LUT_3D_SIZE" => parsed.size_3d = Some(parse_u32(fields.next())?),
            "LUT_1D_SIZE" => parsed.size_1d = Some(parse_u32(fields.next())?),
            "DOMAIN_MIN" => domain_min = Some(parse_triple(fields)?),
            "DOMAIN_MAX" => domain_max = Some(parse_triple(fields)?),
            _ => {
                let r = parse_f32(Some(head))?;
                let g = parse_f32(fields.next())?;
                let b = parse_f32(fields.next())?;
                parsed.values.push([r, g, b]);
            }
        }
    }
    if let (Some(min), Some(max)) = (domain_min, domain_max) {
        if min[0] != min[1] || min[0] != min[2] || max[0] != max[1] || max[0] != max[2] {
            return Err(ExportError::Cube(
                "per-channel domains are not supported".to_string(),
            ));
        }
        parsed.domain = [min[0], max[0]];
    }
    Ok(parsed)
}

fn parse_u32(field: Option<&str>) -> Result<u32> {
    field
        .ok_or_else(|| ExportError::Cube("missing size value".to_string()))?
        .parse()
        .map_err(|_| ExportError::Cube("invalid size".to_string()))
}

fn parse_f32(field: Option<&str>) -> Result<f32> {
    field
        .ok_or_else(|| ExportError::Cube("missing channel value".to_string()))?
        .parse()
        .map_err(|_| ExportError::Cube("invalid channel value".to_string()))
}

fn parse_triple<'a>(mut fields: impl Iterator<Item = &'a str>) -> Result<[f64; 3]> {
    let mut out = [0.0_f64; 3];
    for value in &mut out {
        *value = fields
            .next()
            .ok_or_else(|| ExportError::Cube("missing domain value".to_string()))?
            .parse()
            .map_err(|_| ExportError::Cube("invalid domain value".to_string()))?;
    }
    Ok(out)
}

/// The smallest Hald level whose cube is at least `size` per axis.
pub fn hald_level(size: u32) -> u32 {
    let mut level = 2;
    while level * level < size.max(2) {
        level += 1;
    }
    level
}

/// A 16-bit RGB PNG HaldCLUT: level `L` holds an `(L²)³` cube on an `L³ × L³` image,
/// red fastest in raster order.
///
/// # Errors
/// [`ExportError::Png`] when the PNG encoder fails.
pub fn write_haldclut(lut: &Lut3d) -> Result<Vec<u8>> {
    let level = hald_level(lut.size);
    let cube = level * level;
    let side = level * cube;
    let mut image = vec![0_u8; (side as usize) * (side as usize) * 6];
    let scale = (cube - 1) as f64;
    for b in 0..cube as usize {
        for g in 0..cube as usize {
            for r in 0..cube as usize {
                let code = [r as f64 / scale, g as f64 / scale, b as f64 / scale];
                let value = lut.eval(code, Interpolation::Trilinear);
                let pixel = r + cube as usize * (g + cube as usize * b);
                for (channel, sample) in value.iter().enumerate() {
                    let quantized = (sample.clamp(0.0, 1.0) * 65_535.0).round() as u16;
                    let offset = pixel * 6 + channel * 2;
                    image[offset..offset + 2].copy_from_slice(&quantized.to_be_bytes());
                }
            }
        }
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, side, side);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Sixteen);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder
            .write_header()
            .map_err(|error| ExportError::Png(error.to_string()))?;
        writer
            .write_image_data(&image)
            .map_err(|error| ExportError::Png(error.to_string()))?;
        writer
            .finish()
            .map_err(|error| ExportError::Png(error.to_string()))?;
    }
    Ok(out)
}

/// # Errors
/// [`ExportError::Png`] for a malformed or unsupported PNG: not square, side not a
/// cube, not 16-bit RGB/RGBA.
pub fn read_haldclut(bytes: &[u8]) -> Result<Lut3d> {
    let decoder = png::Decoder::new(Cursor::new(bytes));
    let mut reader = decoder
        .read_info()
        .map_err(|error| ExportError::Png(error.to_string()))?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| ExportError::Png("image too large".to_string()))?;
    let mut buffer = vec![0_u8; size];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|error| ExportError::Png(error.to_string()))?;
    if info.width != info.height {
        return Err(ExportError::Png("a HaldCLUT is square".to_string()));
    }
    if info.bit_depth != png::BitDepth::Sixteen {
        return Err(ExportError::Png("a HaldCLUT is 16-bit".to_string()));
    }
    let level =
        cube_root(info.width).ok_or_else(|| ExportError::Png("side is not a cube".to_string()))?;
    let cube = level * level;
    let channels = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        _ => return Err(ExportError::Png("a HaldCLUT is RGB".to_string())),
    };
    let mut data = Vec::with_capacity((cube as usize).pow(3));
    let stride = channels * 2;
    for pixel in 0..(cube as usize).pow(3) {
        let offset = pixel * stride;
        let mut value = [0.0_f32; 3];
        for (channel, sample) in value.iter_mut().enumerate() {
            let index = offset + channel * 2;
            let word = u16::from_be_bytes([buffer[index], buffer[index + 1]]);
            *sample = word as f32 / 65_535.0;
        }
        data.push(value);
    }
    Ok(Lut3d {
        size: cube,
        data,
        shaper: None,
    })
}

fn cube_root(value: u32) -> Option<u32> {
    let mut level = 2;
    while level * level * level < value {
        level += 1;
    }
    (level * level * level == value).then_some(level)
}

/// Bytes the writer produces for `output` in `format`; the largest single file when
/// the bake has one artifact per illuminant.
///
/// # Errors
/// [`ExportError::Unsupported`] when the artifact kind has no writer for `format`.
pub fn written_bytes(output: &BakeOutput, format: Format) -> Result<u64> {
    let title = output.profile_id.to_string();
    let domain = output.insertion.range;
    match format {
        Format::NativeJson => Ok(native::write_native_json(output)?.len() as u64),
        Format::Cube => {
            let mut largest = 0;
            for entry in &output.artifacts {
                let lut = entry
                    .artifact
                    .as_lut()
                    .ok_or(ExportError::Unsupported("cube for a non-LUT artifact"))?;
                let (grid, shaper) = cube_files(lut, &title, domain);
                let bytes = grid.len() + shaper.map_or(0, |text| text.len());
                largest = largest.max(bytes as u64);
            }
            Ok(largest)
        }
        Format::Haldclut => {
            let mut largest = 0;
            for entry in &output.artifacts {
                let lut = entry
                    .artifact
                    .as_lut()
                    .ok_or(ExportError::Unsupported("HaldCLUT for a non-LUT artifact"))?;
                largest = largest.max(write_haldclut(lut)?.len() as u64);
            }
            Ok(largest)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use looklab_schema::Precision;

    fn sample() -> Lut3d {
        Lut3d::from_fn(5, None, Precision::F32, |at| {
            [at[0], at[1] * 0.5, 1.0 - at[2]]
        })
    }

    #[test]
    fn cube_round_trips_exactly() {
        let lut = sample();
        let text = write_cube(&lut, "t", [-12.0, 6.0]);
        let parsed = read_cube(&text).expect("cube");
        assert_eq!(parsed.lut.data, lut.data);
        assert_eq!(parsed.domain, [-12.0, 6.0]);
    }

    #[test]
    fn shaper_cube_round_trips_exactly() {
        let shaper = Shaper1d::from_fn(7, |code| 1.0 - code);
        let text = write_cube_1d(&shaper, "t", [0.0, 1.0]);
        let parsed = read_cube_1d(&text).expect("shaper");
        assert_eq!(parsed.shaper, shaper);
    }

    #[test]
    fn hald_round_trips_within_16_bit() {
        let lut = sample().resample(64, Precision::F32);
        let png = write_haldclut(&lut).expect("png");
        let back = read_haldclut(&png).expect("hald");
        assert_eq!(back.size, 64);
        for (a, b) in lut.data.iter().zip(back.data.iter()) {
            for channel in 0..3 {
                assert!((a[channel] - b[channel]).abs() <= 0.5 / 65_535.0 + 1.0e-7);
            }
        }
    }

    #[test]
    fn haldclut_bakes_the_shaper_into_the_grid() {
        let shaper = Shaper1d::from_fn(1024, |code| code * code);
        let lut = Lut3d::from_fn(64, Some(shaper), Precision::F32, |at| at);
        let png = write_haldclut(&lut).expect("png");
        let back = read_haldclut(&png).expect("hald");
        assert_eq!(back.size, 64);
        assert!(back.shaper.is_none());
        for code in [0.1, 0.25, 0.5, 0.75, 0.9] {
            let at = [code, code, code];
            let expected = lut.eval(at, Interpolation::Trilinear);
            let actual = back.eval(at, Interpolation::Trilinear);
            for channel in 0..3 {
                assert!(
                    (expected[channel] - actual[channel]).abs() < 2.0e-3,
                    "{code}: {expected:?} vs {actual:?}"
                );
            }
        }
        let faded = back.eval([0.5, 0.5, 0.5], Interpolation::Trilinear);
        assert!((faded[0] - 0.25).abs() < 2.0e-3, "{faded:?}");
    }

    #[test]
    fn cube_pair_declares_a_unit_grid_after_the_shaper() {
        let shaper = Shaper1d::from_fn(9, |code| code * code);
        let lut = Lut3d::from_fn(5, Some(shaper), Precision::F32, |at| at);
        let (grid_text, shaper_text) = cube_files(&lut, "t", [0.0, 8.0]);
        let grid = read_cube(&grid_text).expect("grid");
        let shaper = read_cube_1d(&shaper_text.expect("shaper")).expect("shaper");
        assert_eq!(grid.domain, [0.0, 1.0]);
        assert_eq!(shaper.domain, [0.0, 8.0]);
        for code in [0.1, 0.4, 0.75] {
            let at = shaper.shaper.eval(code / 8.0);
            let through = grid.lut.eval([at, at, at], Interpolation::Trilinear);
            let direct = lut.eval(
                [code / 8.0, code / 8.0, code / 8.0],
                Interpolation::Trilinear,
            );
            for channel in 0..3 {
                assert!((through[channel] - direct[channel]).abs() < 1.0e-5);
            }
        }
    }

    #[test]
    fn hald_resamples_a_non_square_cube() {
        let png = write_haldclut(&sample()).expect("png");
        let back = read_haldclut(&png).expect("hald");
        assert_eq!(back.size, 9);
    }
}
