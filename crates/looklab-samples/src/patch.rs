// SPDX-License-Identifier: AGPL-3.0-or-later
//! Chart and pattern patch-table blobs (docs/03 §2.2, last line): one row per
//! patch, carrying the mean and spread of the input and reference output
//! colours.

use serde::{Deserialize, Serialize};

use crate::error::{BlobKind, Error};
use crate::types::{Field, InputSpace, OutputSpace, PrepVersion, check_rgb, check_value};
use crate::{blob, zstd_frame};

pub const MAGIC: [u8; 6] = *b"LLPTCH";
pub const VERSION: u16 = 1;
pub const ROW_BYTES: usize = 2 + 4 * 3 * 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PatchId(u16);

impl PatchId {
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PatchRow {
    pub id: PatchId,
    pub in_mean: [f32; 3],
    pub in_std: [f32; 3],
    pub out_mean: [f32; 3],
    pub out_std: [f32; 3],
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PatchTableHeader {
    pub n: u32,
    pub input_space: InputSpace,
    pub output_space: OutputSpace,
    pub prep_version: PrepVersion,
}

impl PatchTableHeader {
    pub fn validate(&self) -> Result<(), Error> {
        if self.n == 0 || self.n > crate::MAX_PATCHES {
            return Err(Error::CountOutOfRange {
                kind: BlobKind::PatchRows,
                n: self.n,
                max: crate::MAX_PATCHES,
            });
        }
        Ok(())
    }
}

/// Validated patch rows. [`PatchTable::new`] and [`decode_patch_table`] are its
/// only constructors.
#[derive(Clone, Debug, PartialEq)]
pub struct PatchTable {
    header: PatchTableHeader,
    rows: Vec<PatchRow>,
}

impl PatchTable {
    pub fn new(header: PatchTableHeader, rows: Vec<PatchRow>) -> Result<Self, Error> {
        header.validate()?;
        if rows.len() != header.n as usize {
            return Err(Error::ColumnLengthMismatch {
                field: Field::InRgbMean,
                len: rows.len(),
                n: header.n as usize,
            });
        }
        validate_rows(&rows)?;
        Ok(Self { header, rows })
    }

    pub fn header(&self) -> &PatchTableHeader {
        &self.header
    }

    pub fn rows(&self) -> &[PatchRow] {
        &self.rows
    }
}

fn validate_rows(rows: &[PatchRow]) -> Result<(), Error> {
    let mut previous: Option<u16> = None;
    for (index, row) in rows.iter().enumerate() {
        if let Some(prev) = previous
            && row.id.get() <= prev
        {
            return Err(Error::PatchIdsNotAscending {
                id: row.id.get(),
                previous: prev,
            });
        }
        previous = Some(row.id.get());
        check_rgb(Field::InRgbMean, index, &row.in_mean)?;
        check_rgb(Field::OutRgbMean, index, &row.out_mean)?;
        check_std(Field::InRgbStd, index, &row.in_std)?;
        check_std(Field::OutRgbStd, index, &row.out_std)?;
    }
    Ok(())
}

fn check_std(field: Field, index: usize, std: &[f32; 3]) -> Result<(), Error> {
    for value in std {
        check_value(field, index, *value, 0.0, crate::MAX_RGB)?;
    }
    Ok(())
}

pub fn encode_patch_table(table: &PatchTable) -> Result<Vec<u8>, Error> {
    let header = postcard::to_allocvec(table.header()).map_err(Error::HeaderCodec)?;
    let mut raw = Vec::with_capacity(table.rows().len() * ROW_BYTES);
    for row in table.rows() {
        raw.extend_from_slice(&row.id.get().to_le_bytes());
        for value in row.in_mean.iter().chain(row.in_std.iter()) {
            raw.extend_from_slice(&value.to_le_bytes());
        }
        for value in row.out_mean.iter().chain(row.out_std.iter()) {
            raw.extend_from_slice(&value.to_le_bytes());
        }
    }
    blob::assemble(MAGIC, VERSION, &header, &zstd_frame::compress(&raw))
}

pub fn decode_patch_table(bytes: &[u8]) -> Result<PatchTable, Error> {
    let parts = blob::split(bytes, MAGIC, VERSION)?;
    let (wire, rest) = postcard::take_from_bytes::<PatchTableHeaderWire>(parts.header)
        .map_err(Error::HeaderCodec)?;
    if !rest.is_empty() {
        return Err(Error::TrailingHeaderBytes);
    }
    let header = PatchTableHeader::try_from(wire)?;
    header.validate()?;
    let raw = zstd_frame::decompress(parts.body, header.n as usize * ROW_BYTES)?;
    let rows = read_rows(&raw, header.n as usize)?;
    PatchTable::new(header, rows)
}

/// Mirror of [`PatchTableHeader`] with enum fields widened to `u32`, which is
/// how postcard encodes a variant index.
#[derive(Deserialize)]
struct PatchTableHeaderWire {
    n: u32,
    input_space: u32,
    output_space: u32,
    prep_version: u32,
}

impl TryFrom<PatchTableHeaderWire> for PatchTableHeader {
    type Error = Error;

    fn try_from(wire: PatchTableHeaderWire) -> Result<Self, Error> {
        Ok(Self {
            n: wire.n,
            input_space: InputSpace::from_wire(wire.input_space)?,
            output_space: OutputSpace::from_wire(wire.output_space)?,
            prep_version: PrepVersion::new(wire.prep_version),
        })
    }
}

fn read_rows(raw: &[u8], n: usize) -> Result<Vec<PatchRow>, Error> {
    let mut rows = RowReader::new(raw);
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let id = PatchId::new(rows.u16()?);
        let in_mean = rows.rgb()?;
        let in_std = rows.rgb()?;
        let out_mean = rows.rgb()?;
        let out_std = rows.rgb()?;
        out.push(PatchRow {
            id,
            in_mean,
            in_std,
            out_mean,
            out_std,
        });
    }
    Ok(out)
}

struct RowReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> RowReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn u16(&mut self) -> Result<u16, Error> {
        let slice = self
            .bytes
            .get(self.pos..self.pos + 2)
            .ok_or(Error::InvalidBody)?;
        self.pos += 2;
        Ok(u16::from_le_bytes([slice[0], slice[1]]))
    }

    fn f32(&mut self) -> Result<f32, Error> {
        let slice = self
            .bytes
            .get(self.pos..self.pos + 4)
            .ok_or(Error::InvalidBody)?;
        self.pos += 4;
        Ok(f32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
    }

    fn rgb(&mut self) -> Result<[f32; 3], Error> {
        Ok([self.f32()?, self.f32()?, self.f32()?])
    }
}
