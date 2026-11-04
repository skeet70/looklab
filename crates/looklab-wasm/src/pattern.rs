// SPDX-License-Identifier: AGPL-3.0-or-later
//! Full-screen phone pattern pixels ([docs/07 §3.1](../../../docs/07-contribution.md)).

use looklab_prep::pattern::{PatternSeed, render};

use crate::error::FacadeError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatternImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// # Errors
/// Rejects a seed that is not 64 hexadecimal characters, and a size the layout
/// rejects.
pub fn render_pattern(seed_hex: &str, width: u32, height: u32) -> Result<PatternImage, FacadeError> {
    let seed = PatternSeed::parse_hex(seed_hex)
        .map_err(|error| FacadeError::Input(error.to_string()))?;
    let image = render(&seed, width, height)
        .map_err(|error| FacadeError::Input(error.to_string()))?;
    let mut rgba = Vec::with_capacity(image.data.len() * 4);
    for pixel in &image.data {
        for channel in pixel {
            rgba.push(to_byte(*channel));
        }
        rgba.push(255);
    }
    Ok(PatternImage {
        width: image.width,
        height: image.height,
        rgba,
    })
}

fn to_byte(value: f32) -> u8 {
    if !value.is_finite() {
        return 0;
    }
    let scaled = value.clamp(0.0, 1.0) * 255.0;
    scaled.round() as u8
}
