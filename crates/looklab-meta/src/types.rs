// SPDX-License-Identifier: AGPL-3.0-or-later
use core::fmt;
use serde::{Serialize, Serializer};

/// Focal length in millimetres.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct FocalLength(f64);

impl FocalLength {
    pub fn new(millimetres: f64) -> Option<Self> {
        (millimetres.is_finite() && millimetres > 0.0).then_some(Self(millimetres))
    }

    pub const fn millimetres(self) -> f64 {
        self.0
    }
}

/// Photographic sensitivity (ISO arithmetic scale).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct Iso(u32);

impl Iso {
    pub const fn new(value: u32) -> Option<Self> {
        if value > 0 { Some(Self(value)) } else { None }
    }

    pub const fn value(self) -> u32 {
        self.0
    }
}

/// Exposure time in seconds.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ExposureTime(f64);

impl ExposureTime {
    pub fn new(seconds: f64) -> Option<Self> {
        (seconds.is_finite() && seconds > 0.0).then_some(Self(seconds))
    }

    pub const fn seconds(self) -> f64 {
        self.0
    }
}

/// Lens aperture as an f-number.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Aperture(f64);

impl Aperture {
    pub fn new(f_number: f64) -> Option<Self> {
        (f_number.is_finite() && f_number > 0.0).then_some(Self(f_number))
    }

    pub const fn f_number(self) -> f64 {
        self.0
    }
}

/// Exposure compensation in exposure value stops.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ExposureCompensation(f64);

impl ExposureCompensation {
    pub fn new(ev: f64) -> Option<Self> {
        ev.is_finite().then_some(Self(ev))
    }

    pub const fn ev(self) -> f64 {
        self.0
    }
}

/// Camera-neutral multipliers that turn the as-shot white into R = G = B.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct WhiteBalanceMultipliers {
    pub r: f64,
    pub g: f64,
    pub b: f64,
}

/// A capture month, serialized as `YYYY-MM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct YearMonth {
    year: u16,
    month: u8,
}

impl YearMonth {
    pub const fn new(year: u16, month: u8) -> Option<Self> {
        if year >= 1 && year <= 9999 && month >= 1 && month <= 12 {
            Some(Self { year, month })
        } else {
            None
        }
    }

    pub const fn year(self) -> u16 {
        self.year
    }

    pub const fn month(self) -> u8 {
        self.month
    }
}

impl Serialize for YearMonth {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{:04}-{:02}", self.year, self.month))
    }
}

impl fmt::Display for YearMonth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}", self.year, self.month)
    }
}

/// Sub-second part of DateTimeOriginal, as ASCII digits.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SubSec(String);

impl SubSec {
    pub fn new(digits: &str) -> Option<Self> {
        let digits = digits.trim();
        if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(Self(digits.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// DateTimeOriginal with SubSecTimeOriginal when the body writes one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CaptureTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub subsec: Option<SubSec>,
}

impl CaptureTime {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
        subsec: Option<SubSec>,
    ) -> Option<Self> {
        let date_ok =
            (1..=9999).contains(&year) && (1..=12).contains(&month) && (1..=31).contains(&day);
        let time_ok = hour <= 23 && minute <= 59 && second <= 60;
        if date_ok && time_ok {
            Some(Self {
                year,
                month,
                day,
                hour,
                minute,
                second,
                subsec,
            })
        } else {
            None
        }
    }
}

/// EXIF orientation, values 1-8 from the TIFF specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Orientation {
    Normal,
    FlipHorizontal,
    Rotate180,
    FlipVertical,
    Transpose,
    Rotate90Clockwise,
    Transverse,
    Rotate270Clockwise,
}

impl Orientation {
    pub const fn from_raw(value: u16) -> Option<Self> {
        Some(match value {
            1 => Self::Normal,
            2 => Self::FlipHorizontal,
            3 => Self::Rotate180,
            4 => Self::FlipVertical,
            5 => Self::Transpose,
            6 => Self::Rotate90Clockwise,
            7 => Self::Transverse,
            8 => Self::Rotate270Clockwise,
            _ => return None,
        })
    }
}

/// Camera shot counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ImageNumber(u16);

impl ImageNumber {
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u16 {
        self.0
    }
}
