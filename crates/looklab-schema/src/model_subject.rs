// SPDX-License-Identifier: AGPL-3.0-or-later

use core::num::NonZeroU32;

use serde::{Deserialize, Serialize};

use crate::{BodyId, LookId, SimKey};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ControlEpochId(u32);

impl ControlEpochId {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

macro_rules! positive_version {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(NonZeroU32);

        impl $name {
            pub const fn new(value: u32) -> Option<Self> {
                match NonZeroU32::new(value) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }

            pub const fn get(self) -> u32 {
                self.0.get()
            }

            pub const fn checked_next(self) -> Option<Self> {
                match self.get().checked_add(1) {
                    Some(value) => Self::new(value),
                    None => None,
                }
            }
        }
    };
}

positive_version!(TargetVersion);
positive_version!(RealizationVersion);
positive_version!(BodyStateVersion);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BodyRealizationId {
    pub body: BodyId,
    pub epoch: ControlEpochId,
    pub native_sim: SimKey,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FamilyTargetId {
    pub family: LookId,
    pub version: TargetVersion,
}

/// A published `look@version`, including a LUT-defined target that is not a [`ModelSubject`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PublishedTargetId {
    pub look: LookId,
    pub version: TargetVersion,
}

impl From<&FamilyTargetId> for PublishedTargetId {
    fn from(target: &FamilyTargetId) -> Self {
        Self {
            look: target.family.clone(),
            version: target.version,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BodyRealizationVersionId {
    pub realization: BodyRealizationId,
    pub version: RealizationVersion,
    pub target: FamilyTargetId,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelSubject {
    SharedFamily { family: LookId },
    BodyRealization { realization: BodyRealizationId },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelVersionId {
    SharedFamily {
        target: FamilyTargetId,
    },
    BodyRealization {
        realization: BodyRealizationVersionId,
    },
}
