// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::error::MetaError;
use crate::fuji::{self, FujiMakernote};
use crate::render_tag::{RenderTag, parse_render_tag};
use crate::serial::RawSerial;
use crate::tiff::{Entry, Tiff};
use crate::types::{
    Aperture, CaptureTime, ExposureCompensation, ExposureTime, FocalLength, Iso, Orientation,
    SubSec,
};

const TAG_IMAGE_DESCRIPTION: u16 = 0x010e;
const TAG_MAKE: u16 = 0x010f;
const TAG_MODEL: u16 = 0x0110;
const TAG_ORIENTATION: u16 = 0x0112;
const TAG_SOFTWARE: u16 = 0x0131;
const TAG_ARTIST: u16 = 0x013b;
const TAG_COPYRIGHT: u16 = 0x8298;
const TAG_EXPOSURE_TIME: u16 = 0x829a;
const TAG_F_NUMBER: u16 = 0x829d;
const TAG_EXIF_IFD: u16 = 0x8769;
const TAG_GPS_IFD: u16 = 0x8825;
const TAG_ISO: u16 = 0x8827;
const TAG_DATETIME_ORIGINAL: u16 = 0x9003;
const TAG_EXPOSURE_BIAS: u16 = 0x9204;
const TAG_FOCAL_LENGTH: u16 = 0x920a;
const TAG_MAKER_NOTE: u16 = 0x927c;
const TAG_USER_COMMENT: u16 = 0x9286;
const TAG_SUBSEC_ORIGINAL: u16 = 0x9291;
const TAG_IMAGE_UNIQUE_ID: u16 = 0xa420;
const TAG_OWNER_NAME: u16 = 0xa430;
const TAG_BODY_SERIAL: u16 = 0xa431;
const TAG_LENS_MODEL: u16 = 0xa434;
const TAG_LENS_SERIAL: u16 = 0xa435;

pub struct ParsedMetadata {
    pub exif: ExifData,
    pub fuji: Option<FujiMakernote>,
    /// Camera firmware: RAF header `FirmwareVersion` (0x3c, four digits normalized to `x.yy`),
    /// else the `Ver` token of the EXIF `Software` tag (`Ver1.00` -> `1.00`).
    pub firmware: Option<String>,
    pub render_tag: Option<RenderTag>,
}

impl ParsedMetadata {
    /// EXIF `BodySerialNumber`, falling back to Fujifilm's `InternalSerialNumber`.
    pub fn body_serial(&self) -> Option<&RawSerial> {
        self.exif.body_serial.as_ref().or_else(|| {
            self.fuji
                .as_ref()
                .and_then(|fuji| fuji.internal_serial.as_ref())
        })
    }
}

pub struct ExifData {
    pub make: Option<String>,
    pub model: Option<String>,
    pub software: Option<String>,
    pub lens_model: Option<String>,
    pub orientation: Option<Orientation>,
    pub focal_length: Option<FocalLength>,
    pub iso: Option<Iso>,
    pub exposure_time: Option<ExposureTime>,
    pub f_number: Option<Aperture>,
    pub exposure_compensation: Option<ExposureCompensation>,
    pub capture_time: Option<CaptureTime>,
    pub body_serial: Option<RawSerial>,
    pub image_unique_id: Option<ImageUniqueId>,
    pub has_maker_note: bool,
    pub gps_present: bool,
    pub image_description_present: bool,
    pub artist_present: bool,
    pub copyright_present: bool,
    pub user_comment_present: bool,
    pub owner_name_present: bool,
    pub lens_serial_present: bool,
}

pub struct ImageUniqueId(String);

impl ImageUniqueId {
    fn new(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() || !text.is_ascii() {
            return None;
        }
        Some(Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub(crate) fn parse(bytes: &[u8]) -> Result<ParsedMetadata, MetaError> {
    let tiff = Tiff::parse(bytes)?;
    let ifd0 = tiff.ifd0()?;
    let exif_ifd = ifd0
        .find(TAG_EXIF_IFD)
        .and_then(Entry::unsigned)
        .and_then(|offset| tiff.read_ifd(offset as usize).ok());

    let get = |tag: u16| -> Option<&Entry> {
        exif_ifd
            .as_ref()
            .and_then(|ifd| ifd.find(tag))
            .or_else(|| ifd0.find(tag))
    };

    let maker_note = get(TAG_MAKER_NOTE);
    let has_maker_note = maker_note.is_some();
    let fuji = maker_note.and_then(Entry::undefined).and_then(fuji::parse);

    let capture_time = get(TAG_DATETIME_ORIGINAL)
        .and_then(Entry::ascii)
        .and_then(|datetime| {
            parse_capture_time(datetime, get(TAG_SUBSEC_ORIGINAL).and_then(Entry::ascii))
        });

    Ok(ParsedMetadata {
        exif: ExifData {
            make: get(TAG_MAKE).and_then(Entry::ascii).map(str::to_owned),
            model: get(TAG_MODEL).and_then(Entry::ascii).map(str::to_owned),
            software: get(TAG_SOFTWARE).and_then(Entry::ascii).map(str::to_owned),
            lens_model: get(TAG_LENS_MODEL)
                .and_then(Entry::ascii)
                .map(str::to_owned),
            orientation: get(TAG_ORIENTATION)
                .and_then(Entry::u16_value)
                .and_then(Orientation::from_raw),
            focal_length: get(TAG_FOCAL_LENGTH)
                .and_then(Entry::rational)
                .and_then(FocalLength::new),
            iso: get(TAG_ISO).and_then(Entry::unsigned).and_then(Iso::new),
            exposure_time: get(TAG_EXPOSURE_TIME)
                .and_then(Entry::rational)
                .and_then(ExposureTime::new),
            f_number: get(TAG_F_NUMBER)
                .and_then(Entry::rational)
                .and_then(Aperture::new),
            exposure_compensation: get(TAG_EXPOSURE_BIAS)
                .and_then(Entry::signed_rational)
                .and_then(ExposureCompensation::new),
            capture_time,
            body_serial: get(TAG_BODY_SERIAL)
                .and_then(Entry::ascii)
                .and_then(|serial| RawSerial::new(serial.as_bytes())),
            image_unique_id: get(TAG_IMAGE_UNIQUE_ID)
                .and_then(Entry::ascii)
                .and_then(ImageUniqueId::new),
            has_maker_note,
            gps_present: ifd0.has(TAG_GPS_IFD)
                || exif_ifd.as_ref().is_some_and(|ifd| ifd.has(TAG_GPS_IFD)),
            image_description_present: get(TAG_IMAGE_DESCRIPTION).is_some(),
            artist_present: get(TAG_ARTIST).is_some(),
            copyright_present: get(TAG_COPYRIGHT).is_some(),
            user_comment_present: get(TAG_USER_COMMENT).is_some(),
            owner_name_present: get(TAG_OWNER_NAME).is_some(),
            lens_serial_present: get(TAG_LENS_SERIAL).is_some(),
        },
        fuji,
        firmware: None,
        render_tag: parse_user_comment(get(TAG_USER_COMMENT)),
    })
}

fn parse_user_comment(entry: Option<&Entry<'_>>) -> Option<RenderTag> {
    let bytes = entry.and_then(Entry::undefined)?;
    let bytes = bytes
        .strip_prefix(b"ASCII\0\0\0")
        .or_else(|| bytes.strip_prefix(b"\0\0\0\0\0\0\0\0"))
        .unwrap_or(bytes);
    let end = bytes
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |index| index + 1);
    parse_render_tag(core::str::from_utf8(&bytes[..end]).ok()?).ok()
}

fn parse_capture_time(datetime: &str, subsec: Option<&str>) -> Option<CaptureTime> {
    let bytes = datetime.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    if bytes[4] != b':'
        || bytes[7] != b':'
        || bytes[10] != b' '
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let number = |start: usize, end: usize| -> Option<u32> {
        core::str::from_utf8(&bytes[start..end]).ok()?.parse().ok()
    };
    let subsec = subsec.and_then(SubSec::new);
    CaptureTime::new(
        u16::try_from(number(0, 4)?).ok()?,
        u8::try_from(number(5, 7)?).ok()?,
        u8::try_from(number(8, 10)?).ok()?,
        u8::try_from(number(11, 13)?).ok()?,
        u8::try_from(number(14, 16)?).ok()?,
        u8::try_from(number(17, 19)?).ok()?,
        subsec,
    )
}
