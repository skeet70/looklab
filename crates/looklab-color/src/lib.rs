// SPDX-License-Identifier: AGPL-3.0-or-later
//! Color science: colour spaces, TRCs, chromatic adaptation, CIELAB, CIEDE2000, CAM16, CAM16-UCS, chart data.
//!
//! Ported from colour-science (BSD-3-Clause, Colour Developers); see `NOTICE`.
//! All transcendental math goes through `libm` so native and wasm32 outputs are bit-identical.

pub mod cam16;
pub mod cat;
pub mod chart;
pub mod ciede2000;
pub mod dng;
pub mod lab;
pub mod linalg;
pub mod spaces;
pub mod trc;
pub mod types;
pub mod ucs;
pub mod xyy;

pub use cat::Cat;
pub use lab::{Lab, Lchab};
pub use linalg::Mat3;
pub use spaces::RgbSpace;
pub use types::{Rgb, Xyy, Xyz};
pub use xyy::Illuminant;
