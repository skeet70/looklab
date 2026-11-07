// SPDX-License-Identifier: AGPL-3.0-or-later
//! Blob v1 types: the sample header, its enums and newtypes, and the sample
//! columns.
//!
//! Wire form follows [docs/03 §2.2](../../../docs/03-data-model.md): postcard
//! header, zstd body of `in_rgb`, `out_rgb`, `density_w`, `flags`, all
//! little-endian.

use serde::Serialize;

use crate::error::{BlobKind, Error};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum InputSpace {
    CameraRgbWbLinear,
}

impl InputSpace {
    pub(crate) fn from_wire(value: u32) -> Result<Self, Error> {
        match value {
            0 => Ok(Self::CameraRgbWbLinear),
            other => Err(Error::UnknownInputSpace(other)),
        }
    }
}

/// The space the in-camera reference samples are expressed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum OutputSpace {
    Srgb,
    AdobeRgb,
}

impl OutputSpace {
    pub(crate) fn from_wire(value: u32) -> Result<Self, Error> {
        match value {
            0 => Ok(Self::Srgb),
            1 => Ok(Self::AdobeRgb),
            other => Err(Error::UnknownOutputSpace(other)),
        }
    }
}

/// Client-assigned id shared by every image rendered from one RAW. Members of a
/// group carry an identical `in_rgb` column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct RerenderGroupId([u8; 16]);

impl RerenderGroupId {
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// Registered preparation version id (docs/06 §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct PrepVersion(u32);

impl PrepVersion {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Body input-transform version used for density sampling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct PriorTransformVersion(u32);

impl PriorTransformVersion {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct PrepParamsHash([u8; 32]);

impl PrepParamsHash {
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Bits above bit 2 are undefined in version 1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SampleFlags(u8);

impl SampleFlags {
    pub const NONE: Self = Self(0);
    pub const CHART: Self = Self(1 << 0);
    pub const PATTERN: Self = Self(1 << 1);
    /// The in-camera reference is at or near clipping.
    pub const NEAR_CLIP_OUTPUT: Self = Self(1 << 2);
    pub const KNOWN_BITS: u8 = 0b0000_0111;

    /// Returns `None` when `bits` sets a bit that version 1 does not define.
    pub fn from_bits(bits: u8) -> Option<Self> {
        if bits & !Self::KNOWN_BITS != 0 {
            return None;
        }
        Some(Self(bits))
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl core::ops::BitOr for SampleFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SampleHeader {
    pub n: u32,
    pub input_space: InputSpace,
    pub output_space: OutputSpace,
    pub wb_multipliers: [f32; 3],
    pub cct_hint_k: f32,
    pub exposure_hint_ev: f32,
    pub rerender_group: Option<RerenderGroupId>,
    pub prep_version: PrepVersion,
    pub prior_transform_version: PriorTransformVersion,
    pub prep_params_hash: PrepParamsHash,
}

impl SampleHeader {
    pub fn validate(&self) -> Result<(), Error> {
        if self.n == 0 || self.n > crate::MAX_SAMPLES {
            return Err(Error::CountOutOfRange {
                kind: BlobKind::Samples,
                n: self.n,
                max: crate::MAX_SAMPLES,
            });
        }
        for wb in self.wb_multipliers {
            check_header_range(
                "wb_multipliers",
                wb,
                crate::MIN_WB_MULTIPLIER,
                crate::MAX_WB_MULTIPLIER,
            )?;
        }
        check_header_range(
            "cct_hint_k",
            self.cct_hint_k,
            crate::MIN_CCT_HINT_K,
            crate::MAX_CCT_HINT_K,
        )?;
        check_header_range(
            "exposure_hint_ev",
            self.exposure_hint_ev,
            -crate::MAX_EXPOSURE_HINT_EV,
            crate::MAX_EXPOSURE_HINT_EV,
        )?;
        Ok(())
    }
}

/// Names the field an error refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    InRgb,
    OutRgb,
    DensityW,
    Flags,
    InRgbMean,
    InRgbStd,
    OutRgbMean,
    OutRgbStd,
}

impl core::fmt::Display for Field {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::InRgb => "in_rgb",
            Self::OutRgb => "out_rgb",
            Self::DensityW => "density_w",
            Self::Flags => "flags",
            Self::InRgbMean => "in_rgb_mean",
            Self::InRgbStd => "in_rgb_std",
            Self::OutRgbMean => "out_rgb_mean",
            Self::OutRgbStd => "out_rgb_std",
        })
    }
}

pub(crate) fn check_header_range(
    field: &'static str,
    value: f32,
    min: f32,
    max: f32,
) -> Result<(), Error> {
    if !value.is_finite() {
        return Err(Error::NonFiniteHeader { field });
    }
    if value < min || value > max {
        return Err(Error::HeaderValueOutOfRange {
            field,
            value,
            min,
            max,
        });
    }
    Ok(())
}

/// Column or patch-table field value checks shared by the constructors and the
/// readers.
pub(crate) fn check_value(
    field: Field,
    index: usize,
    value: f32,
    min: f32,
    max: f32,
) -> Result<(), Error> {
    if !value.is_finite() {
        return Err(Error::NonFinite { field, index });
    }
    if value < min || value > max {
        return Err(Error::ValueOutOfRange {
            field,
            index,
            value,
            min,
            max,
        });
    }
    Ok(())
}

pub(crate) fn check_rgb(field: Field, index: usize, rgb: &[f32; 3]) -> Result<(), Error> {
    for value in rgb {
        check_value(field, index, *value, crate::MIN_RGB, crate::MAX_RGB)?;
    }
    Ok(())
}

/// Columns that passed every check the reader applies. [`SampleSet::new`] and
/// [`crate::decode`] are its only constructors.
#[derive(Clone, Debug, PartialEq)]
pub struct SampleSet {
    header: SampleHeader,
    in_rgb: Vec<[f32; 3]>,
    out_rgb: Vec<[f32; 3]>,
    density_w: Vec<f32>,
    flags: Vec<SampleFlags>,
}

impl SampleSet {
    pub fn new(
        header: SampleHeader,
        in_rgb: Vec<[f32; 3]>,
        out_rgb: Vec<[f32; 3]>,
        density_w: Vec<f32>,
        flags: Vec<SampleFlags>,
    ) -> Result<Self, Error> {
        header.validate()?;
        let n = header.n as usize;
        validate_columns(n, &in_rgb, &out_rgb, &density_w, &flags)?;
        Ok(Self {
            header,
            in_rgb,
            out_rgb,
            density_w,
            flags,
        })
    }

    pub fn header(&self) -> &SampleHeader {
        &self.header
    }

    pub fn n(&self) -> usize {
        self.in_rgb.len()
    }

    pub fn in_rgb(&self) -> &[[f32; 3]] {
        &self.in_rgb
    }

    pub fn out_rgb(&self) -> &[[f32; 3]] {
        &self.out_rgb
    }

    pub fn density_w(&self) -> &[f32] {
        &self.density_w
    }

    pub fn flags(&self) -> &[SampleFlags] {
        &self.flags
    }
}

fn validate_columns(
    n: usize,
    in_rgb: &[[f32; 3]],
    out_rgb: &[[f32; 3]],
    density_w: &[f32],
    flags: &[SampleFlags],
) -> Result<(), Error> {
    for (field, len) in [
        (Field::InRgb, in_rgb.len()),
        (Field::OutRgb, out_rgb.len()),
        (Field::DensityW, density_w.len()),
        (Field::Flags, flags.len()),
    ] {
        if len != n {
            return Err(Error::ColumnLengthMismatch { field, len, n });
        }
    }
    for (index, rgb) in in_rgb.iter().enumerate() {
        check_rgb(Field::InRgb, index, rgb)?;
    }
    for (index, rgb) in out_rgb.iter().enumerate() {
        check_rgb(Field::OutRgb, index, rgb)?;
    }
    for (index, w) in density_w.iter().enumerate() {
        if !w.is_finite() {
            return Err(Error::NonFinite {
                field: Field::DensityW,
                index,
            });
        }
        if *w <= 0.0 {
            return Err(Error::NotPositive {
                field: Field::DensityW,
                index,
            });
        }
        check_value(Field::DensityW, index, *w, 0.0, crate::MAX_DENSITY_WEIGHT)?;
    }
    for (index, flag) in flags.iter().enumerate() {
        if flag.bits() & !SampleFlags::KNOWN_BITS != 0 {
            return Err(Error::UnknownFlagBits {
                index,
                bits: flag.bits(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A coordinate field must fail to compile here before it can ship.
    #[test]
    fn a_sample_set_has_no_field_beyond_its_columns() {
        let set = SampleSet::new(
            SampleHeader {
                n: 1,
                input_space: InputSpace::CameraRgbWbLinear,
                output_space: OutputSpace::Srgb,
                wb_multipliers: [1.0; 3],
                cct_hint_k: 5500.0,
                exposure_hint_ev: 0.0,
                rerender_group: None,
                prep_version: PrepVersion::new(1),
                prior_transform_version: PriorTransformVersion::new(1),
                prep_params_hash: PrepParamsHash::new([0; 32]),
            },
            vec![[0.5; 3]],
            vec![[0.5; 3]],
            vec![1.0],
            vec![SampleFlags::NONE],
        )
        .expect("valid columns");

        let SampleSet {
            header,
            in_rgb,
            out_rgb,
            density_w,
            flags,
        } = set;
        assert_eq!(header.n, 1);
        assert_eq!(
            (in_rgb.len(), out_rgb.len(), density_w.len(), flags.len()),
            (1, 1, 1, 1)
        );
    }
}
