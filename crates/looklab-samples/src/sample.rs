// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sample blob v1 encoder and bounded decoder.

use serde::Deserialize;

use crate::error::Error;
use crate::types::{
    InputSpace, OutputSpace, PrepParamsHash, PrepVersion, PriorTransformVersion, RerenderGroupId,
    SampleFlags, SampleHeader, SampleSet,
};
use crate::{blob, zstd_frame};

pub const MAGIC: [u8; 6] = *b"LLSAMP";
pub const VERSION: u16 = 1;

/// The same set always yields the same bytes: fixed encoder level, one
/// thread, no dictionary.
pub fn encode(set: &SampleSet) -> Result<Vec<u8>, Error> {
    let header = postcard::to_allocvec(set.header()).map_err(Error::HeaderCodec)?;
    let mut raw = Vec::with_capacity(set.n() * crate::ROW_BYTES);
    for rgb in set.in_rgb() {
        push_rgb(&mut raw, rgb);
    }
    for rgb in set.out_rgb() {
        push_rgb(&mut raw, rgb);
    }
    for w in set.density_w() {
        raw.extend_from_slice(&w.to_le_bytes());
    }
    for flag in set.flags() {
        raw.push(flag.bits());
    }
    blob::assemble(MAGIC, VERSION, &header, &zstd_frame::compress(&raw))
}

pub fn decode(bytes: &[u8]) -> Result<SampleSet, Error> {
    let parts = blob::split(bytes, MAGIC, VERSION)?;
    let (wire, rest) =
        postcard::take_from_bytes::<SampleHeaderWire>(parts.header).map_err(Error::HeaderCodec)?;
    if !rest.is_empty() {
        return Err(Error::TrailingHeaderBytes);
    }
    let header = SampleHeader::try_from(wire)?;
    header.validate()?;
    let raw = zstd_frame::decompress(parts.body, header.n as usize * crate::ROW_BYTES)?;
    let (in_rgb, out_rgb, density_w, flags) = read_columns(&raw, header.n as usize)?;
    SampleSet::new(header, in_rgb, out_rgb, density_w, flags)
}

fn push_rgb(raw: &mut Vec<u8>, rgb: &[f32; 3]) {
    for value in rgb {
        raw.extend_from_slice(&value.to_le_bytes());
    }
}

/// Mirror of [`crate::SampleSet`]'s header with enum fields widened to `u32`,
/// which is how postcard encodes a variant index.
#[derive(Deserialize)]
struct SampleHeaderWire {
    n: u32,
    input_space: u32,
    output_space: u32,
    wb_multipliers: [f32; 3],
    cct_hint_k: f32,
    exposure_hint_ev: f32,
    rerender_group: Option<[u8; 16]>,
    prep_version: u32,
    prior_transform_version: u32,
    prep_params_hash: [u8; 32],
}

impl TryFrom<SampleHeaderWire> for SampleHeader {
    type Error = Error;

    fn try_from(wire: SampleHeaderWire) -> Result<Self, Error> {
        Ok(Self {
            n: wire.n,
            input_space: InputSpace::from_wire(wire.input_space)?,
            output_space: OutputSpace::from_wire(wire.output_space)?,
            wb_multipliers: wire.wb_multipliers,
            cct_hint_k: wire.cct_hint_k,
            exposure_hint_ev: wire.exposure_hint_ev,
            rerender_group: wire.rerender_group.map(RerenderGroupId::new),
            prep_version: PrepVersion::new(wire.prep_version),
            prior_transform_version: PriorTransformVersion::new(wire.prior_transform_version),
            prep_params_hash: PrepParamsHash::new(wire.prep_params_hash),
        })
    }
}

type Columns = (Vec<[f32; 3]>, Vec<[f32; 3]>, Vec<f32>, Vec<SampleFlags>);

fn read_columns(raw: &[u8], n: usize) -> Result<Columns, Error> {
    let mut rows = RowReader::new(raw);
    let mut in_rgb = Vec::with_capacity(n);
    for _ in 0..n {
        let r = rows.f32()?;
        let g = rows.f32()?;
        let b = rows.f32()?;
        in_rgb.push([r, g, b]);
    }
    let mut out_rgb = Vec::with_capacity(n);
    for _ in 0..n {
        let r = rows.f32()?;
        let g = rows.f32()?;
        let b = rows.f32()?;
        out_rgb.push([r, g, b]);
    }
    let mut density_w = Vec::with_capacity(n);
    for _ in 0..n {
        density_w.push(rows.f32()?);
    }
    let mut flags = Vec::with_capacity(n);
    for index in 0..n {
        let bits = rows.u8()?;
        let flag = SampleFlags::from_bits(bits).ok_or(Error::UnknownFlagBits { index, bits })?;
        flags.push(flag);
    }
    Ok((in_rgb, out_rgb, density_w, flags))
}

struct RowReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> RowReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn f32(&mut self) -> Result<f32, Error> {
        let slice = self
            .bytes
            .get(self.pos..self.pos + 4)
            .ok_or(Error::InvalidBody)?;
        self.pos += 4;
        Ok(f32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
    }

    fn u8(&mut self) -> Result<u8, Error> {
        let byte = *self.bytes.get(self.pos).ok_or(Error::InvalidBody)?;
        self.pos += 1;
        Ok(byte)
    }
}
