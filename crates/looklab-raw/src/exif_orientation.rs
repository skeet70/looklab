// SPDX-License-Identifier: AGPL-3.0-or-later
//! EXIF orientation from file bytes.
//!
//! rawler populates `RawImage::orientation` for DNG only; RAF stores it in the embedded
//! preview's EXIF block. DNG has priority when both are present.

/// EXIF orientation tag (0x0112) from the first parseable `Exif\0\0` block.
pub(crate) fn read_exif_orientation(bytes: &[u8]) -> Option<u16> {
    const MARKER: &[u8; 6] = b"Exif\0\0";
    bytes
        .windows(MARKER.len())
        .enumerate()
        .filter(|(_, window)| *window == MARKER)
        .find_map(|(offset, _)| parse_tiff_orientation(bytes.get(offset + MARKER.len()..)?))
}

type U16Reader = fn(&[u8], usize) -> Option<u16>;
type U32Reader = fn(&[u8], usize) -> Option<u32>;

fn parse_tiff_orientation(tiff: &[u8]) -> Option<u16> {
    let (u16_at, u32_at): (U16Reader, U32Reader) = match tiff.get(0..2)? {
        b"II" => (u16_le, u32_le),
        b"MM" => (u16_be, u32_be),
        _ => return None,
    };
    if u16_at(tiff, 2)? != 42 {
        return None;
    }
    let ifd = u32_at(tiff, 4)? as usize;
    let entries = u16_at(tiff, ifd)? as usize;
    for index in 0..entries {
        let entry = ifd.checked_add(2 + index * 12)?;
        if u16_at(tiff, entry)? != 0x0112 {
            continue;
        }
        let field_type = u16_at(tiff, entry + 2)?;
        if u32_at(tiff, entry + 4)? != 1 {
            return None;
        }
        return match field_type {
            3 => u16_at(tiff, entry + 8),
            4 => u32_at(tiff, entry + 8).and_then(|v| u16::try_from(v).ok()),
            _ => None,
        };
    }
    None
}

fn u16_le(bytes: &[u8], offset: usize) -> Option<u16> {
    let b = bytes.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn u16_be(bytes: &[u8], offset: usize) -> Option<u16> {
    let b = bytes.get(offset..offset + 2)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

fn u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    let b = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn u32_be(bytes: &[u8], offset: usize) -> Option<u32> {
    let b = bytes.get(offset..offset + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exif_block(tiff: &[u8]) -> Vec<u8> {
        let mut bytes = b"Exif\0\0".to_vec();
        bytes.extend_from_slice(tiff);
        bytes
    }

    #[test]
    fn reads_little_endian_orientation() {
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&6u16.to_le_bytes());
        tiff.extend_from_slice(&0u16.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(read_exif_orientation(&exif_block(&tiff)), Some(6));
    }

    #[test]
    fn reads_big_endian_orientation() {
        let mut tiff = b"MM\0*\0\0\0\x08".to_vec();
        tiff.extend_from_slice(&1u16.to_be_bytes());
        tiff.extend_from_slice(&0x0112u16.to_be_bytes());
        tiff.extend_from_slice(&3u16.to_be_bytes());
        tiff.extend_from_slice(&1u32.to_be_bytes());
        tiff.extend_from_slice(&8u16.to_be_bytes());
        tiff.extend_from_slice(&0u16.to_be_bytes());
        tiff.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(read_exif_orientation(&exif_block(&tiff)), Some(8));
    }

    #[test]
    fn ignores_data_without_a_tiff_header() {
        assert_eq!(read_exif_orientation(b"Exif\0\0not a tiff"), None);
        assert_eq!(read_exif_orientation(b"no exif marker"), None);
    }
}
