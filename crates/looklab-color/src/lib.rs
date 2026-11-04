// SPDX-License-Identifier: AGPL-3.0-or-later
//! Color science: colour spaces, TRCs, chromatic adaptation, CIELAB, CIEDE2000, CAM16, CAM16-UCS, chart data.
//!
//! Ported from colour-science (BSD-3-Clause, Colour Developers); see `NOTICE`.
//! All transcendental math goes through `libm` so native and wasm32 outputs are bit-identical.

pub mod linalg;
pub mod types;

pub use linalg::Mat3;
pub use types::{Rgb, Xyz, Xyy};
