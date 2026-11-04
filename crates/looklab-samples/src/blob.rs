// SPDX-License-Identifier: AGPL-3.0-or-later
//! Container framing shared by the sample blob and the patch-table blob.

use crate::error::Error;

const PREFIX_LEN: usize = 6 + 2 + 4;

pub(crate) struct Parts<'a> {
    pub(crate) header: &'a [u8],
    pub(crate) body: &'a [u8],
}

pub(crate) fn split(bytes: &[u8], magic: [u8; 6], version: u16) -> Result<Parts<'_>, Error> {
    let found: [u8; 6] = bytes
        .get(..6)
        .and_then(|slice| slice.try_into().ok())
        .ok_or(Error::TooShort { len: bytes.len() })?;
    if found != magic {
        return Err(Error::BadMagic {
            found,
            expected: magic,
        });
    }
    let version_bytes = bytes
        .get(6..8)
        .ok_or(Error::TooShort { len: bytes.len() })?;
    let found_version = u16::from_le_bytes([version_bytes[0], version_bytes[1]]);
    if found_version != version {
        return Err(Error::UnsupportedVersion(found_version));
    }
    let len_bytes = bytes
        .get(8..12)
        .ok_or(Error::TooShort { len: bytes.len() })?;
    let header_len = u32::from_le_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]);
    if header_len > crate::MAX_HEADER_LEN {
        return Err(Error::HeaderTooLong {
            len: header_len,
            max: crate::MAX_HEADER_LEN,
        });
    }
    let header_len = header_len as usize;
    let header =
        bytes
            .get(PREFIX_LEN..PREFIX_LEN + header_len)
            .ok_or(Error::HeaderBeyondInput {
                len: header_len,
                available: bytes.len().saturating_sub(PREFIX_LEN),
            })?;
    Ok(Parts {
        header,
        body: &bytes[PREFIX_LEN + header_len..],
    })
}

pub(crate) fn assemble(
    magic: [u8; 6],
    version: u16,
    header: &[u8],
    body: &[u8],
) -> Result<Vec<u8>, Error> {
    let header_len = u32::try_from(header.len()).map_err(|_| Error::HeaderTooLong {
        len: u32::MAX,
        max: crate::MAX_HEADER_LEN,
    })?;
    if header_len > crate::MAX_HEADER_LEN {
        return Err(Error::HeaderTooLong {
            len: header_len,
            max: crate::MAX_HEADER_LEN,
        });
    }
    let mut out = Vec::with_capacity(PREFIX_LEN + header.len() + body.len());
    out.extend_from_slice(&magic);
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&header_len.to_le_bytes());
    out.extend_from_slice(header);
    out.extend_from_slice(body);
    Ok(out)
}
