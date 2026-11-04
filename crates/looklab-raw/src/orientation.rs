// SPDX-License-Identifier: AGPL-3.0-or-later
//! EXIF orientation, which rawler reports for DNG only.

/// EXIF orientation (1–8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Orientation {
    #[default]
    Normal,
    HorizontalFlip,
    Rotate180,
    VerticalFlip,
    Transpose,
    Rotate90,
    Transverse,
    Rotate270,
}

impl Orientation {
    pub const fn from_exif(value: u16) -> Option<Self> {
        Some(match value {
            1 => Self::Normal,
            2 => Self::HorizontalFlip,
            3 => Self::Rotate180,
            4 => Self::VerticalFlip,
            5 => Self::Transpose,
            6 => Self::Rotate90,
            7 => Self::Transverse,
            8 => Self::Rotate270,
            _ => return None,
        })
    }

    pub const fn exif(self) -> u8 {
        match self {
            Self::Normal => 1,
            Self::HorizontalFlip => 2,
            Self::Rotate180 => 3,
            Self::VerticalFlip => 4,
            Self::Transpose => 5,
            Self::Rotate90 => 6,
            Self::Transverse => 7,
            Self::Rotate270 => 8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exif_round_trips() {
        for value in 1..=8 {
            let orientation = Orientation::from_exif(value);
            assert!(orientation.is_some());
            assert_eq!(orientation.map(Orientation::exif), u8::try_from(value).ok());
        }
        assert_eq!(Orientation::from_exif(0), None);
        assert_eq!(Orientation::from_exif(9), None);
    }
}
