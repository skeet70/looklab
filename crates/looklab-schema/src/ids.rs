// SPDX-License-Identifier: AGPL-3.0-or-later
//! Identifier newtypes for the YAML data files.

use core::fmt;

use serde::Deserialize;

/// An empty or whitespace-bearing identifier.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("identifier is empty")]
    Empty,
    #[error("identifier contains whitespace")]
    Whitespace,
}

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
        #[serde(try_from = "String")]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                if value.is_empty() {
                    return Err(IdError::Empty);
                }
                if value.chars().any(char::is_whitespace) {
                    return Err(IdError::Whitespace);
                }
                Ok(Self(value))
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::try_from(value.to_owned())
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
    BodyId
);
id_type!(LookId);
id_type!(
    /// Interface profile id, for example `example-firmware/sim-v1`.
    InterfaceId
);
id_type!(ControlKey);
id_type!(SimKey);
id_type!(ExposureModeKey);
id_type!(StageName);
id_type!(EnumValue);

impl EnumValue {
    pub(crate) fn from_unchecked(value: String) -> Self {
        Self(value)
    }
}

id_type!(MakernoteTag);
