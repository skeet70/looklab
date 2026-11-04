// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::error::MetaError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Endian {
    Little,
    Big,
}

impl Endian {
    pub(crate) fn u16(self, bytes: &[u8]) -> Option<u16> {
        let bytes: [u8; 2] = bytes.get(..2)?.try_into().ok()?;
        Some(match self {
            Self::Little => u16::from_le_bytes(bytes),
            Self::Big => u16::from_be_bytes(bytes),
        })
    }

    pub(crate) fn u32(self, bytes: &[u8]) -> Option<u32> {
        let bytes: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
        Some(match self {
            Self::Little => u32::from_le_bytes(bytes),
            Self::Big => u32::from_be_bytes(bytes),
        })
    }

    pub(crate) fn i32(self, bytes: &[u8]) -> Option<i32> {
        let bytes: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
        Some(match self {
            Self::Little => i32::from_le_bytes(bytes),
            Self::Big => i32::from_be_bytes(bytes),
        })
    }
}

fn type_size(typ: u16) -> Option<usize> {
    Some(match typ {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 => 8,
        _ => return None,
    })
}

pub(crate) struct Tiff<'a> {
    bytes: &'a [u8],
    endian: Endian,
    ifd0_offset: u32,
}

impl<'a> Tiff<'a> {
    pub(crate) fn parse(bytes: &'a [u8]) -> Result<Self, MetaError> {
        let endian = match bytes.get(..2) {
            Some(b"II") => Endian::Little,
            Some(b"MM") => Endian::Big,
            Some(_) => return Err(MetaError::NotTiff),
            None => return Err(MetaError::Truncated),
        };
        if endian.u16(bytes.get(2..).ok_or(MetaError::Truncated)?) != Some(42) {
            return Err(MetaError::InvalidTiff);
        }
        let ifd0_offset = endian
            .u32(bytes.get(4..).ok_or(MetaError::Truncated)?)
            .ok_or(MetaError::Truncated)?;
        if (ifd0_offset as usize) < 8 || ifd0_offset as usize >= bytes.len() {
            return Err(MetaError::InvalidTiff);
        }
        Ok(Self {
            bytes,
            endian,
            ifd0_offset,
        })
    }

    pub(crate) fn ifd0(&self) -> Result<Ifd<'a>, MetaError> {
        self.read_ifd(self.ifd0_offset as usize)
    }

    pub(crate) fn read_ifd(&self, offset: usize) -> Result<Ifd<'a>, MetaError> {
        let count_end = offset.checked_add(2).ok_or(MetaError::InvalidTiff)?;
        let count = self
            .endian
            .u16(
                self.bytes
                    .get(offset..count_end)
                    .ok_or(MetaError::Truncated)?,
            )
            .ok_or(MetaError::Truncated)? as usize;
        if count > 1024 {
            return Err(MetaError::InvalidTiff);
        }
        let mut entries = Vec::with_capacity(count);
        let mut position = count_end;
        for _ in 0..count {
            let entry_end = position.checked_add(12).ok_or(MetaError::InvalidTiff)?;
            let raw = self
                .bytes
                .get(position..entry_end)
                .ok_or(MetaError::Truncated)?;
            position = entry_end;
            let tag = self.endian.u16(raw).ok_or(MetaError::Truncated)?;
            let typ = self.endian.u16(&raw[2..]).ok_or(MetaError::Truncated)?;
            let count = self.endian.u32(&raw[4..]).ok_or(MetaError::Truncated)?;
            let Some(size) = type_size(typ) else { continue };
            let Some(total) = (count as usize).checked_mul(size) else {
                continue;
            };
            let data = if total <= 4 {
                EntryData::Inline([raw[8], raw[9], raw[10], raw[11]])
            } else {
                let Some(start) = self.endian.u32(&raw[8..]).map(|v| v as usize) else {
                    continue;
                };
                let Some(end) = start.checked_add(total) else {
                    continue;
                };
                let Some(slice) = self.bytes.get(start..end) else {
                    continue;
                };
                EntryData::At(slice)
            };
            entries.push(Entry {
                tag,
                typ,
                endian: self.endian,
                data,
            });
        }
        Ok(Ifd { entries })
    }
}

pub(crate) enum EntryData<'a> {
    Inline([u8; 4]),
    At(&'a [u8]),
}

pub(crate) struct Entry<'a> {
    pub(crate) tag: u16,
    typ: u16,
    endian: Endian,
    data: EntryData<'a>,
}

impl<'a> Entry<'a> {
    fn bytes(&self) -> &[u8] {
        match &self.data {
            EntryData::Inline(bytes) => bytes,
            EntryData::At(bytes) => bytes,
        }
    }

    pub(crate) fn unsigned(&self) -> Option<u32> {
        match self.typ {
            3 => self.endian.u16(self.bytes()).map(u32::from),
            4 => self.endian.u32(self.bytes()),
            _ => None,
        }
    }

    pub(crate) fn u16_value(&self) -> Option<u16> {
        if self.typ != 3 {
            return None;
        }
        self.endian.u16(self.bytes())
    }

    pub(crate) fn rational(&self) -> Option<f64> {
        if self.typ != 5 {
            return None;
        }
        let numerator = self.endian.u32(self.bytes())?;
        let denominator = self.endian.u32(self.bytes().get(4..)?)?;
        if denominator == 0 {
            return None;
        }
        Some(f64::from(numerator) / f64::from(denominator))
    }

    pub(crate) fn signed_rational(&self) -> Option<f64> {
        if self.typ != 10 {
            return None;
        }
        let numerator = self.endian.i32(self.bytes())?;
        let denominator = self.endian.i32(self.bytes().get(4..)?)?;
        if denominator == 0 {
            return None;
        }
        Some(f64::from(numerator) / f64::from(denominator))
    }

    pub(crate) fn ascii(&self) -> Option<&str> {
        if self.typ != 2 {
            return None;
        }
        let bytes = self.bytes();
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let text = core::str::from_utf8(&bytes[..end]).ok()?;
        let text = text.trim_end_matches(['\0', ' ']);
        (!text.is_empty()).then_some(text)
    }

    pub(crate) fn undefined(&self) -> Option<&[u8]> {
        if self.typ != 7 {
            return None;
        }
        Some(self.bytes())
    }
}

pub(crate) struct Ifd<'a> {
    entries: Vec<Entry<'a>>,
}

impl<'a> Ifd<'a> {
    pub(crate) fn find(&self, tag: u16) -> Option<&Entry<'a>> {
        self.entries.iter().find(|entry| entry.tag == tag)
    }

    pub(crate) fn has(&self, tag: u16) -> bool {
        self.find(tag).is_some()
    }
}
