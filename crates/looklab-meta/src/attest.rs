// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::exif::ParsedMetadata;
use crate::types::{CaptureTime, ImageNumber};

pub fn has_makernotes(metadata: &ParsedMetadata) -> bool {
    metadata.exif.has_maker_note || metadata.fuji.is_some()
}

pub fn software_matches_firmware(software: Option<&str>, firmware: Option<&str>) -> bool {
    let (Some(software), Some(firmware)) = (software, firmware) else {
        return false;
    };
    if software.trim() == firmware.trim() {
        return true;
    }
    let Some(firmware_version) = parse_version(firmware) else {
        return false;
    };
    software
        .split_whitespace()
        .any(|token| parse_version(token) == Some(firmware_version))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingData {
    pub capture_time: Option<CaptureTime>,
    pub image_number: Option<ImageNumber>,
}

pub fn pairing_data(metadata: &ParsedMetadata) -> PairingData {
    PairingData {
        capture_time: metadata.exif.capture_time.clone(),
        image_number: metadata.fuji.as_ref().and_then(|fuji| fuji.image_count),
    }
}

fn parse_version(text: &str) -> Option<(u32, u32)> {
    let text = text.trim().trim_matches('\0').trim();
    let text = text
        .strip_prefix("Ver")
        .or_else(|| text.strip_prefix("ver"))
        .unwrap_or(text);
    let text = text
        .strip_prefix('V')
        .or_else(|| text.strip_prefix('v'))
        .unwrap_or(text)
        .trim();
    if let Some((major, minor)) = text.split_once('.') {
        return Some((component(major)?, component(minor)?));
    }
    if text.len() == 4 && text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some((component(&text[..2])?, component(&text[2..])?));
    }
    if !text.is_empty() && text.len() <= 3 && text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some((text.parse().ok()?, 0));
    }
    None
}

fn component(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}
