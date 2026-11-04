// SPDX-License-Identifier: AGPL-3.0-or-later
//! DNG tags that rawler's `RawImage` does not expose.

use crate::matrices::{ForwardMatrix, Illuminant};
use looklab_color::Mat3;
use rawler::formats::tiff::IFD;
use rawler::tags::DngTag;

pub(crate) struct DngInfo {
    pub(crate) is_dng: bool,
    pub(crate) forward_matrices: Vec<ForwardMatrix>,
}

const NOT_DNG: DngInfo = DngInfo {
    is_dng: false,
    forward_matrices: Vec::new(),
};

pub(crate) fn inspect(bytes: &[u8]) -> DngInfo {
    if !(bytes.starts_with(b"II") || bytes.starts_with(b"MM")) {
        return NOT_DNG;
    }
    let mut cursor = std::io::Cursor::new(bytes);
    let Ok(ifd) = IFD::new_root(&mut cursor, 0) else {
        return NOT_DNG;
    };
    if ifd.get_entry(DngTag::DNGVersion).is_none() {
        return NOT_DNG;
    }
    DngInfo {
        is_dng: true,
        forward_matrices: forward_matrices(&ifd),
    }
}

/// rawler keeps these tags out of `RawImage`; its public TIFF parser reads them without a fork.
fn forward_matrices(ifd: &IFD) -> Vec<ForwardMatrix> {
    let mut matrices = Vec::new();
    for (matrix_tag, illuminant_tag) in [
        (DngTag::ForwardMatrix1, DngTag::CalibrationIlluminant1),
        (DngTag::ForwardMatrix2, DngTag::CalibrationIlluminant2),
    ] {
        let (Some(matrix), Some(illuminant)) = (
            ifd.get_entry_recursive(matrix_tag),
            ifd.get_entry_recursive(illuminant_tag),
        ) else {
            continue;
        };
        if matrix.count() < 9 {
            continue;
        }
        let Some(illuminant) = Illuminant::from_code(illuminant.force_u16(0)) else {
            continue;
        };
        let cell = |index: usize| f64::from(matrix.force_f32(index));
        matrices.push(ForwardMatrix {
            illuminant,
            camera_to_xyz: Mat3([
                [cell(0), cell(1), cell(2)],
                [cell(3), cell(4), cell(5)],
                [cell(6), cell(7), cell(8)],
            ]),
        });
    }
    matrices
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiff(entries: &[(u16, u16, u32, [u8; 4])]) -> Vec<u8> {
        let mut bytes = b"II*\0\x08\0\0\0".to_vec();
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (tag, field_type, count, value) in entries {
            bytes.extend_from_slice(&tag.to_le_bytes());
            bytes.extend_from_slice(&field_type.to_le_bytes());
            bytes.extend_from_slice(&count.to_le_bytes());
            bytes.extend_from_slice(value);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes
    }

    #[test]
    fn detects_the_dng_version_tag() {
        let dng = tiff(&[(50706, 1, 4, [1, 4, 0, 0])]);
        assert!(inspect(&dng).is_dng);
        assert!(!inspect(&tiff(&[])).is_dng);
    }

    #[test]
    fn non_tiff_input_is_not_dng() {
        assert!(!inspect(b"FUJIFILMCCD-RAW").is_dng);
    }
}
