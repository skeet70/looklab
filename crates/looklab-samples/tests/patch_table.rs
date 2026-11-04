// SPDX-License-Identifier: AGPL-3.0-or-later
//! Patch-table blob round trip, determinism and rejections.

mod common;

use looklab_samples::{
    BlobKind, Error, Field, InputSpace, OutputSpace, PatchId, PatchRow, PatchTable,
    PatchTableHeader, PrepVersion, decode_patch_table, encode_patch_table,
};

use common::{compress, container};

fn header(n: u32) -> PatchTableHeader {
    PatchTableHeader {
        n,
        input_space: InputSpace::CameraRgbWbLinear,
        output_space: OutputSpace::Srgb,
        prep_version: PrepVersion::new(1),
    }
}

fn row(id: u16, scale: f32) -> PatchRow {
    PatchRow {
        id: PatchId::new(id),
        in_mean: [scale, 0.5, 0.25],
        in_std: [0.01, 0.02, 0.03],
        out_mean: [0.4, 0.5, 0.6],
        out_std: [0.02, 0.02, 0.02],
    }
}

fn table(rows: Vec<PatchRow>) -> PatchTable {
    let header = header(rows.len() as u32);
    PatchTable::new(header, rows).expect("fixture rows are valid")
}

fn header_bytes(n: u32) -> Vec<u8> {
    postcard::to_allocvec(&(n, 0u32, 0u32, 1u32)).expect("postcard encodes a fixed tuple")
}

fn row_bytes(rows: &[PatchRow]) -> Vec<u8> {
    let mut raw = Vec::new();
    for row in rows {
        raw.extend_from_slice(&row.id.get().to_le_bytes());
        for value in row
            .in_mean
            .iter()
            .chain(row.in_std.iter())
            .chain(row.out_mean.iter())
            .chain(row.out_std.iter())
        {
            raw.extend_from_slice(&value.to_le_bytes());
        }
    }
    raw
}

#[test]
fn patch_table_roundtrips() {
    let table = table(vec![row(2, 0.1), row(5, 0.2), row(9, 0.3)]);
    let bytes = encode_patch_table(&table).expect("valid table encodes");
    let decoded = decode_patch_table(&bytes).expect("encodes then decodes");
    assert_eq!(decoded, table);
}

#[test]
fn patch_table_encoding_is_deterministic() {
    let a = encode_patch_table(&table(vec![row(1, 0.1), row(2, 0.2)])).expect("encodes");
    let b = encode_patch_table(&table(vec![row(1, 0.1), row(2, 0.2)])).expect("encodes");
    assert_eq!(a, b);
}

#[test]
fn bad_patch_magic_is_rejected() {
    let mut bytes = encode_patch_table(&table(vec![row(1, 0.1)])).expect("encodes");
    bytes[0] = b'X';
    match decode_patch_table(&bytes) {
        Err(Error::BadMagic { expected, .. }) => assert_eq!(expected, *b"LLPTCH"),
        other => panic!("expected BadMagic, got {other:?}"),
    }
}

#[test]
fn unsorted_patch_ids_are_rejected() {
    let result = PatchTable::new(header(2), vec![row(4, 0.1), row(3, 0.2)]);
    assert!(matches!(
        result,
        Err(Error::PatchIdsNotAscending { id: 3, previous: 4 })
    ));
}

#[test]
fn duplicate_patch_ids_are_rejected() {
    let result = PatchTable::new(header(2), vec![row(3, 0.1), row(3, 0.2)]);
    assert!(matches!(
        result,
        Err(Error::PatchIdsNotAscending { id: 3, previous: 3 })
    ));
}

#[test]
fn a_row_count_that_disagrees_with_the_header_is_rejected() {
    let bytes = container(
        looklab_samples::PATCH_MAGIC,
        looklab_samples::PATCH_VERSION,
        &header_bytes(2),
        &compress(&row_bytes(&[row(1, 0.1)])),
    );
    assert!(matches!(
        decode_patch_table(&bytes),
        Err(Error::BodySizeMismatch { .. })
    ));
}

#[test]
fn zero_patch_rows_are_rejected() {
    let bytes = container(
        looklab_samples::PATCH_MAGIC,
        looklab_samples::PATCH_VERSION,
        &header_bytes(0),
        &compress(&[]),
    );
    assert!(matches!(
        decode_patch_table(&bytes),
        Err(Error::CountOutOfRange {
            kind: BlobKind::PatchRows,
            n: 0,
            ..
        })
    ));
}

#[test]
fn nan_patch_mean_is_rejected() {
    let mut rows = vec![row(1, 0.1)];
    rows[0].in_mean[1] = f32::NAN;
    let bytes = container(
        looklab_samples::PATCH_MAGIC,
        looklab_samples::PATCH_VERSION,
        &header_bytes(1),
        &compress(&row_bytes(&rows)),
    );
    assert!(matches!(
        decode_patch_table(&bytes),
        Err(Error::NonFinite {
            field: Field::InRgbMean,
            index: 0
        })
    ));
}

#[test]
fn negative_patch_spread_is_rejected() {
    let mut rows = vec![row(1, 0.1)];
    rows[0].out_std[2] = -0.5;
    let bytes = container(
        looklab_samples::PATCH_MAGIC,
        looklab_samples::PATCH_VERSION,
        &header_bytes(1),
        &compress(&row_bytes(&rows)),
    );
    assert!(matches!(
        decode_patch_table(&bytes),
        Err(Error::ValueOutOfRange {
            field: Field::OutRgbStd,
            index: 0,
            ..
        })
    ));
}

#[test]
fn trailing_bytes_after_a_patch_frame_are_rejected() {
    let mut bytes = encode_patch_table(&table(vec![row(1, 0.1)])).expect("encodes");
    bytes.extend_from_slice(b"tail");
    assert!(matches!(
        decode_patch_table(&bytes),
        Err(Error::TrailingBodyBytes)
    ));
}
