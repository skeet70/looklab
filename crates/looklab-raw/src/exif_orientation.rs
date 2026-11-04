// SPDX-License-Identifier: AGPL-3.0-or-later
//! EXIF orientation from the main TIFF IFD, or a preview's `Exif\0\0` block for RAF.

/// Orientation tag (0x0112) from the main TIFF IFD, for TIFF-based files.
pub(crate) fn main_orientation(bytes: &[u8]) -> Option<u16> {
    parse_tiff_orientation(bytes)
}

/// Orientation from the first parseable `Exif\0\0` block, for non-TIFF containers.
pub(crate) fn embedded_exif_orientation(bytes: &[u8]) -> Option<u16> {
    const MARKER: &[u8; 6] = b"Exif\0\0";
    bytes
        .windows(MARKER.len())
        .enumerate()
        .filter(|(_, window)| *window == MARKER)
        .find_map(|(offset, _)| {
            let tiff = offset.checked_add(MARKER.len())?;
            parse_tiff_orientation(bytes.get(tiff..)?)
        })
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
        let entry = ifd.checked_add(2)?.checked_add(index.checked_mul(12)?)?;
        if u16_at(tiff, entry)? != 0x0112 {
            continue;
        }
        let field_type = u16_at(tiff, entry.checked_add(2)?)?;
        if u32_at(tiff, entry.checked_add(4)?)? != 1 {
            return None;
        }
        let value = entry.checked_add(8)?;
        return match field_type {
            3 => u16_at(tiff, value),
            4 => u32_at(tiff, value).and_then(|v| u16::try_from(v).ok()),
            _ => None,
        };
    }
    None
}

fn u16_le(bytes: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    let b = bytes.get(offset..end)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn u16_be(bytes: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    let b = bytes.get(offset..end)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

fn u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let b = bytes.get(offset..end)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn u32_be(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let b = bytes.get(offset..end)?;
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

    fn little_endian_orientation(value: u16) -> Vec<u8> {
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&value.to_le_bytes());
        tiff.extend_from_slice(&0u16.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff
    }

    #[test]
    fn reads_little_endian_orientation() {
        let tiff = little_endian_orientation(6);
        assert_eq!(main_orientation(&tiff), Some(6));
        assert_eq!(embedded_exif_orientation(&exif_block(&tiff)), Some(6));
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
        assert_eq!(main_orientation(&tiff), Some(8));
    }

    #[test]
    fn ignores_data_without_a_tiff_header() {
        assert_eq!(main_orientation(b"FUJIFILMCCD-RAW"), None);
        assert_eq!(embedded_exif_orientation(b"Exif\0\0not a tiff"), None);
        assert_eq!(embedded_exif_orientation(b"no exif marker"), None);
    }

    #[test]
    fn maximal_offsets_return_none() {
        let mut tiff = b"II*\0".to_vec();
        tiff.extend_from_slice(&0xffff_ffffu32.to_le_bytes());
        assert_eq!(main_orientation(&tiff), None);
        assert_eq!(embedded_exif_orientation(&exif_block(&tiff)), None);

        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff.extend_from_slice(&4u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&0xffff_ffffu32.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(main_orientation(&tiff), None);
    }
}
