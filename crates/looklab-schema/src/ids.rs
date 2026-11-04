// SPDX-License-Identifier: AGPL-3.0-or-later
//! Identifier newtypes for the YAML data files and the upload envelope.

use core::fmt;

use serde::{Deserialize, Serialize};

pub const MAX_SLUG_LEN: usize = 64;
pub const MAX_BODY_ID_LEN: usize = 128;

/// A rejected identifier.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("identifier is empty")]
    Empty,
    #[error("identifier contains whitespace")]
    Whitespace,
    #[error("identifier of {len} bytes exceeds the {max}-byte limit")]
    TooLong { len: usize, max: usize },
    #[error("byte {byte:#04x} at index {index} is not allowed in an identifier")]
    InvalidCharacter { index: usize, byte: u8 },
    #[error("body id must be `make/model`, with exactly one `/`")]
    BodyIdShape,
}

/// Lowercase ASCII letters, digits, `-` and `_`, at most [`MAX_SLUG_LEN`].
pub fn validate_slug(value: &str) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::Empty);
    }
    if value.len() > MAX_SLUG_LEN {
        return Err(IdError::TooLong {
            len: value.len(),
            max: MAX_SLUG_LEN,
        });
    }
    for (index, byte) in value.bytes().enumerate() {
        check_slug_byte(index, byte)?;
    }
    Ok(())
}

/// Two [`validate_slug`] segments joined by exactly one `/`.
pub fn validate_body_id(value: &str) -> Result<(), IdError> {
    if value.len() > MAX_BODY_ID_LEN {
        return Err(IdError::TooLong {
            len: value.len(),
            max: MAX_BODY_ID_LEN,
        });
    }
    let mut parts = value.split('/');
    let (Some(make), Some(model), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(IdError::BodyIdShape);
    };
    if make.is_empty() || model.is_empty() {
        return Err(IdError::Empty);
    }
    validate_slug(make)?;
    validate_slug(model)
}

fn check_slug_byte(index: usize, byte: u8) -> Result<(), IdError> {
    if byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_' {
        Ok(())
    } else {
        Err(IdError::InvalidCharacter { index, byte })
    }
}

fn validate_generic(value: &str) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::Empty);
    }
    if value.chars().any(char::is_whitespace) {
        return Err(IdError::Whitespace);
    }
    Ok(())
}

pub const MAX_INTERFACE_ID_LEN: usize = 128;

fn validate_interface_id(value: &str) -> Result<(), IdError> {
    validate_generic(value)?;
    if value.len() > MAX_INTERFACE_ID_LEN {
        return Err(IdError::TooLong {
            len: value.len(),
            max: MAX_INTERFACE_ID_LEN,
        });
    }
    Ok(())
}

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        id_type!($(#[$meta])* $name, validate_generic);
    };
    ($(#[$meta:meta])* $name:ident, $check:path) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: &str) -> Result<Self, IdError> {
                $check(value)?;
                Ok(Self(value.to_owned()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                $check(&value)?;
                Ok(Self(value))
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdError;

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

id_type!(
    /// `make/model`, for example `fujifilm/x-t5`.
    BodyId,
    validate_body_id
);

impl BodyId {
    pub fn make(&self) -> &str {
        self.0.split('/').next().unwrap_or(&self.0)
    }

    pub fn model(&self) -> &str {
        self.0.split('/').nth(1).unwrap_or(&self.0)
    }
}

id_type!(LookId);
id_type!(ProposalRef);
id_type!(
    /// Interface profile id, for example `example-firmware/sim-v1`.
    InterfaceId,
    validate_interface_id
);
id_type!(ControlKey, validate_slug);
id_type!(SimKey, validate_slug);
id_type!(ExposureModeKey);
id_type!(StageName);
id_type!(EnumValue);

impl EnumValue {
    pub(crate) fn from_unchecked(value: String) -> Self {
        Self(value)
    }
}

id_type!(MakernoteTag);
