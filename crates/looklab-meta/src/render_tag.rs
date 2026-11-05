// SPDX-License-Identifier: AGPL-3.0-or-later
//! The firmware render tag of [docs/04 §8.4](../../../docs/04-matching-and-export.md).
//!
//! Firmware writes one ASCII token into EXIF `UserComment`; the sanitizer keeps
//! only these parsed fields ([docs/06 §1](../../../docs/06-privacy-and-payload.md)).

use core::fmt;

use serde::{Deserialize, Serialize};

pub const RENDER_TAG_PREFIX: &str = "looklab-card/1 ";
pub const MAX_FIRMWARE_ID_LEN: usize = 32;
pub const MAX_FIRMWARE_VERSION_LEN: usize = 24;
pub const MAX_LOOK_SLUG_LEN: usize = 63;
pub const MAX_LOOK_VERSION_LEN: usize = 31;
pub const CONTENT_ID_PREFIX_LEN: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RenderTagError {
    #[error("not a looklab-card/1 render tag")]
    WrongPrefix,
    #[error("render tag is missing its {0} field")]
    MissingField(&'static str),
    #[error("render tag has a field in an unexpected position")]
    UnexpectedField,
    #[error("render tag has bytes after cid")]
    TrailingData,
    #[error("fw is not `<firmware-id>/<firmware-version>`")]
    BadFirmwareIdentity,
    #[error("firmware id is not 1-32 lowercase letters, digits or `-`")]
    BadFirmwareId,
    #[error("firmware version is not 1-24 letters, digits, `.`, `+` or `-`")]
    BadFirmwareVersion,
    #[error("look is not `<slug>@<version>`")]
    BadLookIdentity,
    #[error("look slug is not 1-63 lowercase letters, digits, `.`, `_` or `-`")]
    BadLookSlug,
    #[error("look version is not 1-31 letters, digits, `.`, `+` or `-`")]
    BadLookVersion,
    #[error("cid is not 16 lowercase hex characters")]
    BadContentIdPrefix,
}

macro_rules! tag_string {
    ($(#[$meta:meta])* $name:ident, $max:expr, $allowed:expr, $error:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: &str) -> Result<Self, RenderTagError> {
                if value.is_empty() || value.len() > $max {
                    return Err(RenderTagError::$error);
                }
                if !value.bytes().all($allowed) {
                    return Err(RenderTagError::$error);
                }
                Ok(Self(value.to_owned()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = RenderTagError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(&value)?;
                Ok(Self(value))
            }
        }

        impl TryFrom<&str> for $name {
            type Error = RenderTagError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

tag_string!(
    /// `[a-z0-9-]{1,32}`.
    FirmwareId,
    MAX_FIRMWARE_ID_LEN,
    |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-',
    BadFirmwareId
);

tag_string!(
    /// `[0-9A-Za-z.+-]{1,24}`.
    FirmwareVersion,
    MAX_FIRMWARE_VERSION_LEN,
    |byte: u8| {
        byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'+' || byte == b'-'
    },
    BadFirmwareVersion
);

tag_string!(
    /// `[a-z0-9._-]{1,63}`.
    RenderLookSlug,
    MAX_LOOK_SLUG_LEN,
    |byte: u8| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || byte == b'.'
            || byte == b'_'
            || byte == b'-'
    },
    BadLookSlug
);

tag_string!(
    /// `[0-9A-Za-z.+-]{1,31}`.
    RenderLookVersion,
    MAX_LOOK_VERSION_LEN,
    |byte: u8| {
        byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'+' || byte == b'-'
    },
    BadLookVersion
);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RenderTag {
    pub firmware_id: FirmwareId,
    pub firmware_version: FirmwareVersion,
    pub look_slug: RenderLookSlug,
    pub look_version: RenderLookVersion,
    /// First 8 bytes of the card's content id, as written in the tag.
    pub cid_prefix: [u8; CONTENT_ID_PREFIX_LEN],
}

impl RenderTag {
    pub fn cid_prefix_hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(2 * CONTENT_ID_PREFIX_LEN);
        for byte in self.cid_prefix {
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        out
    }
}

/// Parses the exact token of docs/04 §8.4.
pub fn parse_render_tag(input: &str) -> Result<RenderTag, RenderTagError> {
    let body = input
        .strip_prefix(RENDER_TAG_PREFIX)
        .ok_or(RenderTagError::WrongPrefix)?;
    let mut parts = body.split(' ');
    let fw = parts.next().ok_or(RenderTagError::MissingField("fw"))?;
    let look = parts.next().ok_or(RenderTagError::MissingField("look"))?;
    let cid = parts.next().ok_or(RenderTagError::MissingField("cid"))?;
    if parts.next().is_some() {
        return Err(RenderTagError::TrailingData);
    }

    let fw = fw
        .strip_prefix("fw=")
        .ok_or(RenderTagError::UnexpectedField)?;
    let (firmware_id, firmware_version) = fw
        .split_once('/')
        .ok_or(RenderTagError::BadFirmwareIdentity)?;
    if firmware_version.contains('/') {
        return Err(RenderTagError::BadFirmwareIdentity);
    }

    let look = look
        .strip_prefix("look=")
        .ok_or(RenderTagError::UnexpectedField)?;
    let (look_slug, look_version) = look
        .split_once('@')
        .ok_or(RenderTagError::BadLookIdentity)?;
    if look_version.contains('@') {
        return Err(RenderTagError::BadLookIdentity);
    }

    let cid = cid
        .strip_prefix("cid=")
        .ok_or(RenderTagError::UnexpectedField)?;

    Ok(RenderTag {
        firmware_id: FirmwareId::new(firmware_id)?,
        firmware_version: FirmwareVersion::new(firmware_version)?,
        look_slug: RenderLookSlug::new(look_slug)?,
        look_version: RenderLookVersion::new(look_version)?,
        cid_prefix: parse_cid_prefix(cid)?,
    })
}

fn parse_cid_prefix(text: &str) -> Result<[u8; CONTENT_ID_PREFIX_LEN], RenderTagError> {
    let bytes = text.as_bytes();
    if bytes.len() != 2 * CONTENT_ID_PREFIX_LEN {
        return Err(RenderTagError::BadContentIdPrefix);
    }
    let mut prefix = [0u8; CONTENT_ID_PREFIX_LEN];
    let (pairs, _) = bytes.as_chunks::<2>();
    for (index, pair) in pairs.iter().enumerate() {
        let high = hex_digit(pair[0])?;
        let low = hex_digit(pair[1])?;
        prefix[index] = (high << 4) | low;
    }
    Ok(prefix)
}

fn hex_digit(byte: u8) -> Result<u8, RenderTagError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(RenderTagError::BadContentIdPrefix),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANONICAL: &str =
        "looklab-card/1 fw=acme-sim/2.0.1 look=provia@1.0.0 cid=0123456789abcdef";

    #[test]
    fn parses_the_canonical_token() {
        let tag = parse_render_tag(CANONICAL).expect("canonical tag");
        assert_eq!(tag.firmware_id.as_str(), "acme-sim");
        assert_eq!(tag.firmware_version.as_str(), "2.0.1");
        assert_eq!(tag.look_slug.as_str(), "provia");
        assert_eq!(tag.look_version.as_str(), "1.0.0");
        assert_eq!(tag.cid_prefix_hex(), "0123456789abcdef");
    }

    #[test]
    fn rejects_every_shape_the_grammar_does_not_allow() {
        for input in [
            "",
            "looklab-card/1",
            "looklab-card/2 fw=acme/1 look=provia@1 cid=0123456789abcdef",
            "looklab-card/1 fw=acme/1 look=provia@1",
            "looklab-card/1 fw=acme/1 cid=0123456789abcdef",
            "looklab-card/1 look=provia@1 cid=0123456789abcdef",
            "looklab-card/1 fw=acme/1 look=provia@1 cid=0123456789abcdef extra",
            "looklab-card/1 fw=acme look=provia@1 cid=0123456789abcdef",
            "looklab-card/1 fw=acme/1/2 look=provia@1 cid=0123456789abcdef",
            "looklab-card/1 fw=Acme/1 look=provia@1 cid=0123456789abcdef",
            "looklab-card/1 fw=acme/ look=provia@1 cid=0123456789abcdef",
            "looklab-card/1 fw=acme/1 look=provia cid=0123456789abcdef",
            "looklab-card/1 fw=acme/1 look=provia@1@2 cid=0123456789abcdef",
            "looklab-card/1 fw=acme/1 look=Provia@1 cid=0123456789abcdef",
            "looklab-card/1 fw=acme/1 look=provia@1 cid=0123456789ABCDEf",
            "looklab-card/1 fw=acme/1 looks=provia@1 cid=0123456789abcdef",
            "looklab-card 1 fw=acme/1 look=provia@1 cid=0123456789abcdef",
        ] {
            assert!(parse_render_tag(input).is_err(), "accepted {input:?}");
        }
    }

    #[test]
    fn rejects_values_past_their_limits() {
        let long_id = "a".repeat(MAX_FIRMWARE_ID_LEN + 1);
        let long_version = "1".repeat(MAX_FIRMWARE_VERSION_LEN + 1);
        let long_slug = "a".repeat(MAX_LOOK_SLUG_LEN + 1);
        let long_look_version = "1".repeat(MAX_LOOK_VERSION_LEN + 1);
        for input in [
            format!("looklab-card/1 fw={long_id}/1 look=provia@1 cid=0123456789abcdef"),
            format!("looklab-card/1 fw=acme/{long_version} look=provia@1 cid=0123456789abcdef"),
            format!("looklab-card/1 fw=acme/1 look={long_slug}@1 cid=0123456789abcdef"),
            format!(
                "looklab-card/1 fw=acme/1 look=provia@{long_look_version} cid=0123456789abcdef"
            ),
        ] {
            assert!(matches!(
                parse_render_tag(&input),
                Err(RenderTagError::BadFirmwareId)
                    | Err(RenderTagError::BadFirmwareVersion)
                    | Err(RenderTagError::BadLookSlug)
                    | Err(RenderTagError::BadLookVersion)
            ));
        }
    }
}
