// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tristimulus and RGB value types.

use serde::{Deserialize, Serialize};

/// CIE 1931 2° tristimulus values, Y of the reference white = 1.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Xyz {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Xyy {
    pub x: f64,
    pub y: f64,
    pub big_y: f64,
}

/// Linear or encoded depending on context; the caller carries the colour space.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rgb {
    pub r: f64,
    pub g: f64,
    pub b: f64,
}

impl Xyz {
    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }
    pub const fn to_array(self) -> [f64; 3] {
        [self.x, self.y, self.z]
    }
    pub const fn from_array(v: [f64; 3]) -> Self {
        Self::new(v[0], v[1], v[2])
    }
    pub fn scale(self, k: f64) -> Self {
        Self::new(self.x * k, self.y * k, self.z * k)
    }
}

impl Rgb {
    pub const fn new(r: f64, g: f64, b: f64) -> Self {
        Self { r, g, b }
    }
    pub const fn to_array(self) -> [f64; 3] {
        [self.r, self.g, self.b]
    }
    pub const fn from_array(v: [f64; 3]) -> Self {
        Self::new(v[0], v[1], v[2])
    }
}

impl Xyy {
    pub const fn new(x: f64, y: f64, big_y: f64) -> Self {
        Self { x, y, big_y }
    }
    pub const fn to_array(self) -> [f64; 3] {
        [self.x, self.y, self.big_y]
    }
    pub const fn from_array(v: [f64; 3]) -> Self {
        Self::new(v[0], v[1], v[2])
    }
}
