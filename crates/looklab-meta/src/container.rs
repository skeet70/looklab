// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::error::MetaError;
use crate::exif::{self, ParsedMetadata};

const RAF_JPEG_OFFSET: usize = 0x54;
const RAF_JPEG_LENGTH: usize = 0x58;
const RAF_FIRMWARE_OFFSET: usize = 0x3c;

pub fn parse(bytes: &[u8]) -> Result<ParsedMetadata, MetaError> {
    if bytes.starts_with(&[0xff, 0xd8]) {
        parse_jpeg(bytes)
    } else if bytes.starts_with(b"FUJIFILMCCD-RAW ") {
        parse_raf(bytes)
    } else if bytes.starts_with(b"II") || bytes.starts_with(b"MM") {
        parse_tiff(bytes)
    } else {
        Err(MetaError::UnknownContainer)
    }
}

pub fn parse_jpeg(bytes: &[u8]) -> Result<ParsedMetadata, MetaError> {
    let payload = exif_payload(bytes).ok_or(MetaError::NoExif)?;
    let mut parsed = exif::parse(payload)?;
    parsed.firmware = parsed
        .exif
        .software
        .as_deref()
        .and_then(firmware_from_software);
    Ok(parsed)
}

pub fn parse_raf(bytes: &[u8]) -> Result<ParsedMetadata, MetaError> {
    if !bytes.starts_with(b"FUJIFILMCCD-RAW ") {
        return Err(MetaError::NotRaf);
    }
    let offset = bytes
        .get(RAF_JPEG_OFFSET..RAF_JPEG_LENGTH)
        .and_then(|raw| raw.try_into().ok())
        .map(u32::from_be_bytes)
        .map(|value| value as usize)
        .ok_or(MetaError::InvalidRaf)?;
    let length = bytes
        .get(RAF_JPEG_LENGTH..RAF_JPEG_LENGTH + 4)
        .and_then(|raw| raw.try_into().ok())
        .map(u32::from_be_bytes)
        .map(|value| value as usize)
        .ok_or(MetaError::InvalidRaf)?;
    let end = offset.checked_add(length).ok_or(MetaError::InvalidRaf)?;
    let jpeg = bytes.get(offset..end).ok_or(MetaError::InvalidRaf)?;
    let mut parsed = parse_jpeg(jpeg)?;
    if let Some(firmware) = raf_firmware(bytes) {
        parsed.firmware = Some(firmware);
    }
    Ok(parsed)
}

pub fn parse_tiff(bytes: &[u8]) -> Result<ParsedMetadata, MetaError> {
    exif::parse(bytes)
}

fn raf_firmware(bytes: &[u8]) -> Option<String> {
    let raw = bytes.get(RAF_FIRMWARE_OFFSET..RAF_FIRMWARE_OFFSET + 4)?;
    let text = core::str::from_utf8(raw).ok()?;
    let text = text.trim_matches(|c: char| c == '\0' || c == ' ');
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_graphic()) {
        return None;
    }
    Some(normalize_firmware(text))
}

fn normalize_firmware(text: &str) -> String {
    if text.len() == 4
        && text.bytes().all(|byte| byte.is_ascii_digit())
        && let Ok(major) = text[..2].parse::<u8>()
    {
        return format!("{major}.{}", &text[2..]);
    }
    text.to_owned()
}

fn firmware_from_software(software: &str) -> Option<String> {
    let mut tokens = software.split_whitespace();
    while let Some(token) = tokens.next() {
        let candidate = if token.eq_ignore_ascii_case("ver") {
            tokens.next()?
        } else if token
            .get(..3)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("ver"))
        {
            &token[3..]
        } else {
            continue;
        };
        let candidate = candidate.strip_prefix('.').unwrap_or(candidate);
        if is_firmware_version(candidate) {
            return Some(candidate.to_owned());
        }
    }
    None
}

fn is_firmware_version(text: &str) -> bool {
    let bytes = text.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 16
        && bytes[0].is_ascii_digit()
        && bytes[bytes.len() - 1].is_ascii_digit()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b'.')
}

fn exif_payload(jpeg: &[u8]) -> Option<&[u8]> {
    if !jpeg.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut position = 2usize;
    while position
        .checked_add(1)
        .is_some_and(|next| next < jpeg.len())
    {
        if jpeg[position] != 0xff {
            return None;
        }
        let mut marker_position = position.checked_add(1)?;
        while marker_position < jpeg.len() && jpeg[marker_position] == 0xff {
            marker_position = marker_position.checked_add(1)?;
        }
        let marker = *jpeg.get(marker_position)?;
        if marker == 0xd8 || marker == 0xd9 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            position = marker_position.checked_add(1)?;
            continue;
        }
        if marker == 0xda {
            return None;
        }
        let length_start = marker_position.checked_add(1)?;
        let length_end = marker_position.checked_add(3)?;
        let length =
            u16::from_be_bytes(jpeg.get(length_start..length_end)?.try_into().ok()?) as usize;
        if length < 2 {
            return None;
        }
        let end = marker_position.checked_add(1)?.checked_add(length)?;
        if end > jpeg.len() {
            return None;
        }
        if marker == 0xe1
            && let Some(payload) = jpeg.get(marker_position.checked_add(3)?..end)
            && let Some(exif) = payload.strip_prefix(b"Exif\0\0")
        {
            return Some(exif);
        }
        position = end;
    }
    None
}
