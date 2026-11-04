// SPDX-License-Identifier: AGPL-3.0-or-later
//! Zstandard framing via `ruzstd`: pure Rust, builds for
//! `wasm32-unknown-unknown` on stable, and encodes deterministically.
//!
//! The C `zstd` crate does not build for that target here, and the browser
//! has to encode as well as decode.

use std::io::Read;

use ruzstd::decoding::StreamingDecoder;
use ruzstd::encoding::{CompressionLevel, FrameCompressor};

use crate::error::Error;

/// Zstandard frame magic, RFC 8878 §3.1.1.
const FRAME_MAGIC: u32 = 0xFD2F_B528;

pub(crate) fn compress(raw: &[u8]) -> Vec<u8> {
    // `Fastest` is the highest level ruzstd implements; `Default` panics.
    let mut compressor = FrameCompressor::new(CompressionLevel::Fastest);
    compressor.set_source(raw);
    let mut out = Vec::new();
    compressor.set_drain(&mut out);
    compressor.compress();
    out
}

struct FrameInfo {
    window_size: u64,
    content_size: Option<u64>,
    single_segment: bool,
}

/// RFC 8878 §3.1.1 frame header fields that bound decoder memory.
fn frame_info(frame: &[u8]) -> Result<FrameInfo, Error> {
    let magic = frame
        .get(..4)
        .and_then(|slice| <[u8; 4]>::try_from(slice).ok())
        .ok_or(Error::InvalidFrameHeader)?;
    if u32::from_le_bytes(magic) != FRAME_MAGIC {
        return Err(Error::InvalidFrameHeader);
    }
    let descriptor = *frame.get(4).ok_or(Error::InvalidFrameHeader)?;
    if descriptor & 0b0000_1000 != 0 {
        return Err(Error::InvalidFrameHeader);
    }
    let fcs_flag = descriptor >> 6;
    let single_segment = descriptor & 0b0010_0000 != 0;
    let dict_len = [0usize, 1, 2, 4][usize::from(descriptor & 0b0000_0011)];
    let mut pos = 5;

    let mut window_size = 0u64;
    if !single_segment {
        let descriptor_byte = *frame.get(pos).ok_or(Error::InvalidFrameHeader)?;
        pos += 1;
        let exponent = u64::from(descriptor_byte >> 3);
        let mantissa = u64::from(descriptor_byte & 0b111);
        let base = 1u64 << (10 + exponent);
        window_size = base + (base / 8) * mantissa;
    }
    pos += dict_len;

    let fcs_len = match (fcs_flag, single_segment) {
        (0, true) => 1,
        (0, false) => 0,
        (1, _) => 2,
        (2, _) => 4,
        _ => 8,
    };
    let content_size = if fcs_len == 0 {
        None
    } else {
        let field = frame
            .get(pos..pos + fcs_len)
            .ok_or(Error::InvalidFrameHeader)?;
        let mut buf = [0u8; 8];
        buf[..fcs_len].copy_from_slice(field);
        let value = match fcs_len {
            1 => u64::from(buf[0]),
            2 => u64::from(u16::from_le_bytes([buf[0], buf[1]])) + 256,
            4 => u64::from(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])),
            _ => u64::from_le_bytes(buf),
        };
        Some(value)
    };
    if single_segment {
        window_size = content_size.unwrap_or(0);
    }
    Ok(FrameInfo {
        window_size,
        content_size,
        single_segment,
    })
}

/// Decodes one frame into exactly `expected` bytes, collecting at most
/// `expected + DECOMPRESS_HEADROOM` and rejecting an oversized window before
/// decoding starts.
pub(crate) fn decompress(frame: &[u8], expected: usize) -> Result<Vec<u8>, Error> {
    let info = frame_info(frame)?;
    if let Some(declared) = info.content_size
        && declared != expected as u64
    {
        return Err(Error::FrameContentSizeMismatch {
            declared,
            expected: expected as u64,
        });
    }
    if !info.single_segment && info.window_size > crate::MAX_WINDOW_BYTES {
        return Err(Error::WindowTooLarge {
            window: info.window_size,
            max: crate::MAX_WINDOW_BYTES,
        });
    }
    let cap = expected.saturating_add(crate::DECOMPRESS_HEADROOM);
    let window_limit = crate::MAX_WINDOW_BYTES.max(expected as u64);
    let mut decoder = StreamingDecoder::new_with_max_window_size(frame, window_limit)
        .map_err(|_| Error::InvalidFrameHeader)?;
    let mut out = Vec::new();
    {
        let mut limited = (&mut decoder).take(cap as u64);
        limited
            .read_to_end(&mut out)
            .map_err(|_| Error::InvalidBody)?;
    }
    let (_, frame_decoder) = decoder.into_parts();
    if out.len() != expected {
        return Err(Error::BodySizeMismatch {
            actual: out.len(),
            expected,
        });
    }
    if frame_decoder.bytes_read_from_source() != frame.len() as u64 {
        return Err(Error::TrailingBodyBytes);
    }
    Ok(out)
}
