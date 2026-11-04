// SPDX-License-Identifier: AGPL-3.0-or-later

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
        if text.is_empty() || !text.is_ascii() {
            return None;
        }
        Some(Self {
            ascii: text.to_owned(),
        })
    }

    /// `make`, `model` and the serial separated by NUL bytes.
    pub fn hmac_message(&self, make: &str, model: &str) -> Vec<u8> {
        let mut message = Vec::with_capacity(make.len() + model.len() + self.ascii.len() + 2);
        message.extend_from_slice(make.as_bytes());
        message.push(0);
        message.extend_from_slice(model.as_bytes());
        message.push(0);
        message.extend_from_slice(self.ascii.as_bytes());
        message
    }

    pub fn envelope_ascii(&self) -> &[u8] {
        self.ascii.as_bytes()
    }
}
