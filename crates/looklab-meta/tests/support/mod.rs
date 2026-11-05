// SPDX-License-Identifier: AGPL-3.0-or-later
#![allow(dead_code)]

use serde_json::Value as Json;

#[derive(Clone, Copy, Debug)]
pub enum Endian {
    Little,
    Big,
}

#[derive(Clone, Debug)]
pub enum Value {
    Ascii(String),
    Short(u16),
    Shorts(Vec<u16>),
    Long(u32),
    Rational(u32, u32),
    SLong(i32),
    SRational(i32, i32),
    Undefined(Vec<u8>),
}

impl Value {
    fn typ(&self) -> u16 {
        match self {
            Self::Ascii(_) => 2,
            Self::Short(_) => 3,
            Self::Shorts(_) => 3,
            Self::Long(_) => 4,
            Self::Rational(_, _) => 5,
            Self::SLong(_) => 9,
            Self::SRational(_, _) => 10,
            Self::Undefined(_) => 7,
        }
    }

    fn count(&self) -> u32 {
        match self {
            Self::Ascii(text) => text.len() as u32 + 1,
            Self::Short(_) | Self::Long(_) | Self::SLong(_) => 1,
            Self::Shorts(values) => values.len() as u32,
            Self::Rational(_, _) | Self::SRational(_, _) => 1,
            Self::Undefined(bytes) => bytes.len() as u32,
        }
    }

    fn bytes(&self, endian: Endian) -> Vec<u8> {
        let u16_bytes = |value: u16| match endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        let u32_bytes = |value: u32| match endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        let i32_bytes = |value: i32| match endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        match self {
            Self::Ascii(text) => {
                let mut bytes = text.as_bytes().to_vec();
                bytes.push(0);
                bytes
            }
            Self::Short(value) => u16_bytes(*value).to_vec(),
            Self::Shorts(values) => values.iter().flat_map(|value| u16_bytes(*value)).collect(),
            Self::Long(value) => u32_bytes(*value).to_vec(),
            Self::Rational(numerator, denominator) => {
                [u32_bytes(*numerator), u32_bytes(*denominator)].concat()
            }
            Self::SLong(value) => i32_bytes(*value).to_vec(),
            Self::SRational(numerator, denominator) => {
                [i32_bytes(*numerator), i32_bytes(*denominator)].concat()
            }
            Self::Undefined(bytes) => bytes.clone(),
        }
    }
}

pub fn build_tiff(
    ifd0: &[(u16, Value)],
    exif: &[(u16, Value)],
    gps: Option<&[(u16, Value)]>,
) -> Vec<u8> {
    build_tiff_with(Endian::Little, ifd0, exif, gps)
}

pub fn build_tiff_be(
    ifd0: &[(u16, Value)],
    exif: &[(u16, Value)],
    gps: Option<&[(u16, Value)]>,
) -> Vec<u8> {
    build_tiff_with(Endian::Big, ifd0, exif, gps)
}

fn build_tiff_with(
    endian: Endian,
    ifd0: &[(u16, Value)],
    exif: &[(u16, Value)],
    gps: Option<&[(u16, Value)]>,
) -> Vec<u8> {
    let mut ifd0 = ifd0.to_vec();
    let has_exif = !exif.is_empty();
    let has_gps = gps.is_some_and(|entries| !entries.is_empty());
    if has_exif {
        ifd0.push((0x8769, Value::Long(0)));
    }
    if has_gps {
        ifd0.push((0x8825, Value::Long(0)));
    }

    let ifd0_offset = 8usize;
    let mut cursor = ifd0_offset + ifd_size(&ifd0);
    let ifd0_offsets = allocate(&ifd0, &mut cursor, endian);
    let exif_layout = has_exif.then(|| {
        let offset = align2(cursor);
        cursor = offset + ifd_size(exif);
        let offsets = allocate(exif, &mut cursor, endian);
        (offset, offsets)
    });
    let gps_layout = has_gps.then(|| {
        let offset = align2(cursor);
        cursor = offset + ifd_size(gps.unwrap());
        let offsets = allocate(gps.unwrap(), &mut cursor, endian);
        (offset, offsets)
    });

    if let Some((offset, _)) = &exif_layout
        && let Some(entry) = ifd0.iter_mut().find(|(tag, _)| *tag == 0x8769)
    {
        entry.1 = Value::Long(*offset as u32);
    }
    if let Some((offset, _)) = &gps_layout
        && let Some(entry) = ifd0.iter_mut().find(|(tag, _)| *tag == 0x8825)
    {
        entry.1 = Value::Long(*offset as u32);
    }

    let mut out = vec![0u8; cursor];
    match endian {
        Endian::Little => out[..2].copy_from_slice(b"II"),
        Endian::Big => out[..2].copy_from_slice(b"MM"),
    }
    put_u16(&mut out, 2, 42, endian);
    put_u32(&mut out, 4, ifd0_offset as u32, endian);
    write_ifd(&mut out, ifd0_offset, &ifd0, &ifd0_offsets, endian);
    if let Some((offset, offsets)) = &exif_layout {
        write_ifd(&mut out, *offset, exif, offsets, endian);
    }
    if let Some((offset, offsets)) = &gps_layout {
        write_ifd(&mut out, *offset, gps.unwrap(), offsets, endian);
    }
    out
}

fn ifd_size(entries: &[(u16, Value)]) -> usize {
    2 + entries.len() * 12 + 4
}

fn align2(value: usize) -> usize {
    (value + 1) & !1
}

fn allocate(entries: &[(u16, Value)], cursor: &mut usize, endian: Endian) -> Vec<Option<u32>> {
    entries
        .iter()
        .map(|(_, value)| {
            let bytes = value.bytes(endian);
            if bytes.len() > 4 {
                *cursor = align2(*cursor);
                let offset = *cursor as u32;
                *cursor += bytes.len();
                Some(offset)
            } else {
                None
            }
        })
        .collect()
}

fn write_ifd(
    out: &mut [u8],
    offset: usize,
    entries: &[(u16, Value)],
    data_offsets: &[Option<u32>],
    endian: Endian,
) {
    put_u16(out, offset, entries.len() as u16, endian);
    let mut position = offset + 2;
    for ((tag, value), data_offset) in entries.iter().zip(data_offsets) {
        put_u16(out, position, *tag, endian);
        put_u16(out, position + 2, value.typ(), endian);
        put_u32(out, position + 4, value.count(), endian);
        match data_offset {
            Some(data_offset) => {
                put_u32(out, position + 8, *data_offset, endian);
                let bytes = value.bytes(endian);
                out[*data_offset as usize..*data_offset as usize + bytes.len()]
                    .copy_from_slice(&bytes);
            }
            None => {
                let bytes = value.bytes(endian);
                out[position + 8..position + 12].fill(0);
                out[position + 8..position + 8 + bytes.len()].copy_from_slice(&bytes);
            }
        }
        position += 12;
    }
    put_u32(out, position, 0, endian);
}

fn put_u16(out: &mut [u8], at: usize, value: u16, endian: Endian) {
    let bytes = match endian {
        Endian::Little => value.to_le_bytes(),
        Endian::Big => value.to_be_bytes(),
    };
    out[at..at + 2].copy_from_slice(&bytes);
}

fn put_u32(out: &mut [u8], at: usize, value: u32, endian: Endian) {
    let bytes = match endian {
        Endian::Little => value.to_le_bytes(),
        Endian::Big => value.to_be_bytes(),
    };
    out[at..at + 4].copy_from_slice(&bytes);
}

#[derive(Clone, Debug)]
pub enum FujiValue {
    Ascii(String),
    Short(u16),
    SLong(i32),
    SLongPair(i32, i32),
    Undefined(Vec<u8>),
}

impl FujiValue {
    fn typ(&self) -> u16 {
        match self {
            Self::Ascii(_) => 2,
            Self::Short(_) => 3,
            Self::SLong(_) | Self::SLongPair(_, _) => 9,
            Self::Undefined(_) => 7,
        }
    }

    fn count(&self) -> u32 {
        match self {
            Self::Ascii(text) => text.len() as u32 + 1,
            Self::Short(_) | Self::SLong(_) => 1,
            Self::SLongPair(_, _) => 2,
            Self::Undefined(bytes) => bytes.len() as u32,
        }
    }

    fn bytes(&self) -> Vec<u8> {
        match self {
            Self::Ascii(text) => {
                let mut bytes = text.as_bytes().to_vec();
                bytes.push(0);
                bytes
            }
            Self::Short(value) => value.to_le_bytes().to_vec(),
            Self::SLong(value) => value.to_le_bytes().to_vec(),
            Self::SLongPair(first, second) => [first.to_le_bytes(), second.to_le_bytes()].concat(),
            Self::Undefined(bytes) => bytes.clone(),
        }
    }
}

pub fn build_makernote(entries: &[(u16, FujiValue)]) -> Vec<u8> {
    let ifd_offset = 12usize;
    let mut cursor = ifd_offset + 2 + entries.len() * 12 + 4;
    let mut data_offsets = Vec::with_capacity(entries.len());
    for (_, value) in entries {
        let bytes = value.bytes();
        if bytes.len() > 4 {
            cursor = align2(cursor);
            data_offsets.push(Some(cursor as u32));
            cursor += bytes.len();
        } else {
            data_offsets.push(None);
        }
    }

    let mut out = vec![0u8; cursor];
    out[..8].copy_from_slice(b"FUJIFILM");
    out[8..12].copy_from_slice(&(ifd_offset as u32).to_le_bytes());
    out[ifd_offset..ifd_offset + 2].copy_from_slice(&(entries.len() as u16).to_le_bytes());
    let mut position = ifd_offset + 2;
    for ((tag, value), data_offset) in entries.iter().zip(&data_offsets) {
        out[position..position + 2].copy_from_slice(&tag.to_le_bytes());
        out[position + 2..position + 4].copy_from_slice(&value.typ().to_le_bytes());
        out[position + 4..position + 8].copy_from_slice(&value.count().to_le_bytes());
        match data_offset {
            Some(data_offset) => {
                out[position + 8..position + 12].copy_from_slice(&data_offset.to_le_bytes());
                let bytes = value.bytes();
                out[*data_offset as usize..*data_offset as usize + bytes.len()]
                    .copy_from_slice(&bytes);
            }
            None => {
                let bytes = value.bytes();
                out[position + 8..position + 12].fill(0);
                out[position + 8..position + 8 + bytes.len()].copy_from_slice(&bytes);
            }
        }
        position += 12;
    }
    out[position..position + 4].fill(0);
    out
}

pub fn fuji_makernote() -> Vec<u8> {
    build_makernote(&[
        (0x0000, FujiValue::Undefined(b"0101".to_vec())),
        (
            0x0010,
            FujiValue::Ascii("FPX20017035 592D31313034060427796060".into()),
        ),
        (0x1001, FujiValue::Short(0x84)),
        (0x1002, FujiValue::Short(0x200)),
        (0x1003, FujiValue::Short(0x100)),
        (0x100a, FujiValue::SLongPair(-40, 20)),
        (0x100e, FujiValue::Short(0x200)),
        (0x100f, FujiValue::SLong(2000)),
        (0x1040, FujiValue::SLong(-16)),
        (0x1041, FujiValue::SLong(32)),
        (0x1047, FujiValue::SLong(32)),
        (0x1048, FujiValue::SLong(64)),
        (0x104c, FujiValue::Short(16)),
        (0x104e, FujiValue::SLong(32)),
        (0x1101, FujiValue::Short(77)),
        (0x1400, FujiValue::Short(3)),
        (0x1401, FujiValue::Short(0x700)),
        (0x1402, FujiValue::Short(0x201)),
        (0x1403, FujiValue::Short(400)),
        (0x1436, FujiValue::Short(1)),
        (0x1438, FujiValue::Short(1234 | 0x8000)),
        (0x9999, FujiValue::Short(1)),
    ])
}

pub fn full_exif_tiff() -> Vec<u8> {
    build_tiff(
        &[
            (0x010e, Value::Ascii("Test image".into())),
            (0x010f, Value::Ascii("FUJIFILM".into())),
            (0x0110, Value::Ascii("X-T5".into())),
            (0x0112, Value::Short(6)),
            (0x0131, Value::Ascii("Digital Camera X-T5 Ver1.01".into())),
            (0x013b, Value::Ascii("Test Artist".into())),
            (0x8298, Value::Ascii("Test Copyright".into())),
        ],
        &[
            (0x829a, Value::Rational(1, 250)),
            (0x829d, Value::Rational(28, 10)),
            (0x8827, Value::Short(400)),
            (0x9003, Value::Ascii("2024:05:17 10:11:12".into())),
            (0x9204, Value::SRational(-1, 3)),
            (0x920a, Value::Rational(35, 1)),
            (0x927c, Value::Undefined(fuji_makernote())),
            (0x9286, Value::Undefined(b"comment".to_vec())),
            (0x9291, Value::Ascii("123".into())),
            (
                0xa420,
                Value::Ascii("0123456789abcdef0123456789abcdef".into()),
            ),
            (0xa430, Value::Ascii("Owner".into())),
            (0xa431, Value::Ascii("SERIAL123".into())),
            (0xa434, Value::Ascii("XF35mmF1.4 R".into())),
            (0xa435, Value::Ascii("LENS-SERIAL".into())),
        ],
        Some(&[(0x0001, Value::Ascii("N".into()))]),
    )
}

pub fn full_exif_jpeg() -> Vec<u8> {
    build_jpeg(&full_exif_tiff())
}

pub fn build_jpeg(tiff: &[u8]) -> Vec<u8> {
    let length = 2 + 6 + tiff.len();
    assert!(length <= u16::MAX as usize);
    let mut out = vec![0xff, 0xd8, 0xff, 0xe1];
    out.extend_from_slice(&(length as u16).to_be_bytes());
    out.extend_from_slice(b"Exif\0\0");
    out.extend_from_slice(tiff);
    out.extend_from_slice(&[0xff, 0xd9]);
    out
}

pub fn build_raf(jpeg: &[u8]) -> Vec<u8> {
    build_raf_with_firmware(jpeg, b"0101")
}

pub fn build_raf_with_firmware(jpeg: &[u8], firmware: &[u8; 4]) -> Vec<u8> {
    let header_length = 0x94usize;
    let mut out = vec![0u8; header_length];
    out[..16].copy_from_slice(b"FUJIFILMCCD-RAW ");
    out[16..20].copy_from_slice(b"0201");
    out[0x1c..0x1c + 6].copy_from_slice(b"X-T5\0\0");
    out[0x3c..0x40].copy_from_slice(firmware);
    out[0x54..0x58].copy_from_slice(&(header_length as u32).to_be_bytes());
    out[0x58..0x5c].copy_from_slice(&(jpeg.len() as u32).to_be_bytes());
    out.extend_from_slice(jpeg);
    out
}

pub const TOP_LEVEL_KEYS: &[&str] = &[
    "make",
    "model",
    "firmware",
    "lens_model",
    "focal_length",
    "iso",
    "shutter",
    "aperture",
    "exposure_compensation",
    "as_shot_wb",
    "capture_month",
    "software",
    "render_tag",
    "look",
];

pub const NESTED_KEYS: &[&str] = &[
    "r",
    "g",
    "b",
    "tag",
    "raw",
    "value",
    "red",
    "blue",
    "film_mode",
    "dynamic_range",
    "dynamic_range_setting",
    "development_dynamic_range",
    "saturation",
    "white_balance",
    "white_balance_fine_tune",
    "highlight_tone",
    "shadow_tone",
    "sharpness",
    "noise_reduction",
    "clarity",
    "grain_roughness",
    "grain_size",
    "color_chrome_effect",
    "color_chrome_fx_blue",
    "firmware_id",
    "firmware_version",
    "look_slug",
    "look_version",
    "cid_prefix",
];

pub fn assert_whitelisted_keys(json: &str) {
    let value: Json = serde_json::from_str(json).expect("sanitized metadata is JSON");
    let mut keys = Vec::new();
    collect_keys(&value, &mut keys);
    for key in keys {
        assert!(
            TOP_LEVEL_KEYS.contains(&key.as_str()) || NESTED_KEYS.contains(&key.as_str()),
            "sanitized metadata leaked key {key:?}"
        );
    }
}

fn collect_keys(value: &Json, keys: &mut Vec<String>) {
    match value {
        Json::Object(map) => {
            for (key, value) in map {
                keys.push(key.clone());
                collect_keys(value, keys);
            }
        }
        Json::Array(values) => {
            for value in values {
                collect_keys(value, keys);
            }
        }
        Json::Null | Json::Bool(_) | Json::Number(_) | Json::String(_) => {}
    }
}
