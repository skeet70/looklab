// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::error::MetaError;
use crate::exif::{self, ParsedMetadata};

const RAF_JPEG_OFFSET: usize = 0x54;
const RAF_JPEG_LENGTH: usize = 0x58;

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
    exif::parse(payload)
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
    parse_jpeg(jpeg)
}

pub fn parse_tiff(bytes: &[u8]) -> Result<ParsedMetadata, MetaError> {
    exif::parse(bytes)
}

fn exif_payload(jpeg: &[u8]) -> Option<&[u8]> {
    if !jpeg.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut position = 2;
    while position + 1 < jpeg.len() {
        if jpeg[position] != 0xff {
            return None;
        }
        let mut marker_position = position + 1;
        while marker_position < jpeg.len() && jpeg[marker_position] == 0xff {
            marker_position += 1;
        }
        let marker = *jpeg.get(marker_position)?;
        if marker == 0xd8 || marker == 0xd9 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            position = marker_position + 1;
            continue;
        }
        if marker == 0xda {
            return None;
        }
        let length = u16::from_be_bytes(
            jpeg.get(marker_position + 1..marker_position + 3)?
                .try_into()
                .ok()?,
        ) as usize;
        if length < 2 {
            return None;
        }
        let end = marker_position.checked_add(1)?.checked_add(length)?;
        if end > jpeg.len() {
            return None;
        }
        if marker == 0xe1
            && let Some(payload) = jpeg.get(marker_position + 3..end)
            && let Some(exif) = payload.strip_prefix(b"Exif\0\0")
        {
            return Some(exif);
        }
        position = end;
    }
    None
}
