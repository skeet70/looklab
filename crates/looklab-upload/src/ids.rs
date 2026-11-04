// SPDX-License-Identifier: AGPL-3.0-or-later
//! Identifier and setting types shared by the upload, trust and verification
//! paths. Every type parses at the envelope boundary into a value that cannot
//! hold an invalid identifier.

use core::fmt;

use serde::{Deserialize, Serialize};

pub use looklab_schema::{
    BodyId, ControlKey, IdError, MAX_BODY_ID_LEN, MAX_SLUG_LEN, SimKey, validate_slug,
};

pub const MAX_FIRMWARE_LEN: usize = MAX_SLUG_LEN;

macro_rules! upload_id_type {
    ($(#[$meta:meta])* $name:ident, $max:expr, $check:expr) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: &str) -> Result<Self, IdError> {
                if value.is_empty() {
                    return Err(IdError::Empty);
                }
                if value.len() > $max {
                    return Err(IdError::TooLong {
                        len: value.len(),
                        max: $max,
                    });
                }
                for (index, byte) in value.bytes().enumerate() {
                    if !$check(byte) {
                        return Err(IdError::InvalidCharacter { index, byte });
                    }
                }
                Ok(Self(value.to_owned()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(&value)?;
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

fn is_slug_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
}

fn is_printable_byte(byte: u8) -> bool {
    byte.is_ascii_graphic() || byte == b' '
}

upload_id_type!(
    /// Look slug, for example `classic-chrome`.
    LookSlug,
    MAX_SLUG_LEN,
    is_slug_byte
);

upload_id_type!(
    /// The firmware version string as the body reports it, for example `2.10`.
    FirmwareVersionString,
    MAX_FIRMWARE_LEN,
    is_printable_byte
);

/// The contribution kind is claimed by the client; nothing verifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ContributionKind {
    Library,
    Rerender,
    Pattern,
    Gradient,
    Chart,
}

/// What the contributor asked the fit to do with the batch.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Intent {
    Native,
    Recipe { target_look: LookSlug },
}

impl Intent {
    pub fn target_look(&self) -> Option<&LookSlug> {
        match self {
            Self::Native => None,
            Self::Recipe { target_look } => Some(target_look),
        }
    }
}

/// A control's value expressed as an integer step of the schema's step size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StepIndex(i64);

impl StepIndex {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ControlEntry {
    pub key: ControlKey,
    pub step: StepIndex,
}

/// Rejected settings lists.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
    #[error("control key {key} appears twice")]
    DuplicateKey { key: String },
    #[error("control settings must be sorted by key")]
    NotSorted,
}

/// Control settings in canonical order: sorted by key, one entry per key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "Vec<ControlEntry>", into = "Vec<ControlEntry>")]
pub struct ControlSettings(Vec<ControlEntry>);

impl ControlSettings {
    pub fn new(mut entries: Vec<ControlEntry>) -> Result<Self, SettingsError> {
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        reject_duplicates(&entries)?;
        Ok(Self(entries))
    }

    pub fn entries(&self) -> &[ControlEntry] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

fn reject_duplicates(entries: &[ControlEntry]) -> Result<(), SettingsError> {
    for pair in entries.windows(2) {
        if pair[0].key == pair[1].key {
            return Err(SettingsError::DuplicateKey {
                key: pair[0].key.to_string(),
            });
        }
    }
    Ok(())
}

impl TryFrom<Vec<ControlEntry>> for ControlSettings {
    type Error = SettingsError;

    fn try_from(entries: Vec<ControlEntry>) -> Result<Self, Self::Error> {
        if !entries.windows(2).all(|pair| pair[0].key < pair[1].key) {
            return Err(SettingsError::NotSorted);
        }
        Ok(Self(entries))
    }
}

impl From<ControlSettings> for Vec<ControlEntry> {
    fn from(value: ControlSettings) -> Self {
        value.0
    }
}
