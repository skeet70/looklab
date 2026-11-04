// SPDX-License-Identifier: AGPL-3.0-or-later

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::MetaError;

const MAX_SERIAL_ASCII: usize = 64;

/// A body serial that reaches callers only as an upload field or as HMAC input.
/// Implements none of [`Clone`], [`core::fmt::Debug`], [`core::fmt::Display`] or [`serde::Serialize`].
///
/// ```compile_fail
/// let serial = looklab_meta::RawSerial::new(b"1234567").unwrap();
/// let _ = format!("{serial:?}");
/// ```
///
/// ```compile_fail
/// let serial = looklab_meta::RawSerial::new(b"1234567").unwrap();
/// let _ = serde_json::to_string(&serial);
/// ```
pub struct RawSerial {
    ascii: String,
}

impl RawSerial {
    pub fn new(ascii: &[u8]) -> Option<Self> {
        let text = core::str::from_utf8(ascii).ok()?;
        let text = text.trim_matches(|c: char| c == '\0' || c.is_whitespace());
        if !valid_serial(text.as_bytes()) {
            return None;
        }
        Some(Self {
            ascii: text.to_owned(),
        })
    }

    pub fn from_envelope_ascii(ascii: &[u8]) -> Result<Self, MetaError> {
        if !valid_serial(ascii) {
            return Err(MetaError::InvalidSerial);
        }
        let text = core::str::from_utf8(ascii).map_err(|_| MetaError::InvalidSerial)?;
        Ok(Self {
            ascii: text.to_owned(),
        })
    }

    /// Appends the canonical upload-envelope serial field: present tag, length, ASCII bytes.
    pub fn write_envelope_field(&self, out: &mut Vec<u8>) {
        debug_assert!(self.ascii.len() <= MAX_SERIAL_ASCII);
        out.push(1);
        out.push(self.ascii.len() as u8);
        out.extend_from_slice(self.ascii.as_bytes());
    }

    /// HMAC-SHA256 over the big-endian `u32` length and bytes of make, model, then serial.
    pub fn hmac(&self, key: &SerialHmacKey, make: &str, model: &str) -> SerialHmac {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&key.0).expect("HMAC-SHA256 accepts keys of any length");
        for field in [make.as_bytes(), model.as_bytes(), self.ascii.as_bytes()] {
            mac.update(&(field.len() as u32).to_be_bytes());
            mac.update(field);
        }
        let tag = mac.finalize().into_bytes();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&tag);
        SerialHmac(bytes)
    }
}

fn valid_serial(ascii: &[u8]) -> bool {
    !ascii.is_empty()
        && ascii.len() <= MAX_SERIAL_ASCII
        && ascii
            .iter()
            .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
}

pub struct SerialHmacKey([u8; 32]);

impl SerialHmacKey {
    pub const fn new(key: [u8; 32]) -> Self {
        Self(key)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SerialHmac([u8; 32]);

impl SerialHmac {
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}
