// SPDX-License-Identifier: AGPL-3.0-or-later

use serde::ser::{SerializeSeq, SerializeStruct};
use serde::{Serialize, Serializer};

use crate::exif::ParsedMetadata;
use crate::fuji::LookSettings;
use crate::types::{
    Aperture, ExposureCompensation, ExposureTime, FocalLength, Iso, WhiteBalanceMultipliers,
    YearMonth,
};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SanitizedMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub make: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lens_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focal_length: Option<FocalLength>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iso: Option<Iso>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shutter: Option<ExposureTime>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aperture: Option<Aperture>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposure_compensation: Option<ExposureCompensation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_shot_wb: Option<WhiteBalanceMultipliers>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_month: Option<YearMonth>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub software: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub look: Option<LookSettings>,
}

impl SanitizedMetadata {
    pub fn with_as_shot_wb(mut self, multipliers: WhiteBalanceMultipliers) -> Self {
        self.as_shot_wb = Some(multipliers);
        self
    }

    pub fn set_as_shot_wb(&mut self, multipliers: WhiteBalanceMultipliers) {
        self.as_shot_wb = Some(multipliers);
    }
}

pub fn sanitize(parsed: &ParsedMetadata) -> (SanitizedMetadata, RemovedFields) {
    let exif = &parsed.exif;
    let fuji = parsed.fuji.as_ref();

    let metadata = SanitizedMetadata {
        make: exif.make.clone(),
        model: exif.model.clone(),
        firmware: parsed.firmware.clone(),
        lens_model: exif.lens_model.clone(),
        focal_length: exif.focal_length,
        iso: exif.iso,
        shutter: exif.exposure_time,
        aperture: exif.f_number,
        exposure_compensation: exif.exposure_compensation,
        as_shot_wb: None,
        capture_month: exif
            .capture_time
            .as_ref()
            .and_then(|time| YearMonth::new(time.year, time.month)),
        software: exif.software.clone(),
        look: fuji
            .map(|fuji| fuji.look.clone())
            .filter(|look| !look.is_empty()),
    };

    let mut removed = Vec::new();
    if exif.gps_present {
        removed.push(RemovedField::GpsLocation);
    }
    if exif.image_description_present {
        removed.push(RemovedField::ImageDescription);
    }
    if exif.artist_present {
        removed.push(RemovedField::Artist);
    }
    if exif.copyright_present {
        removed.push(RemovedField::Copyright);
    }
    if exif.user_comment_present {
        removed.push(RemovedField::UserComment);
    }
    if exif.owner_name_present {
        removed.push(RemovedField::CameraOwnerName);
    }
    if exif.lens_serial_present {
        removed.push(RemovedField::LensSerialNumber);
    }
    if exif.image_unique_id.is_some() {
        removed.push(RemovedField::ImageUniqueId);
    }
    if exif.body_serial.is_some() {
        removed.push(RemovedField::BodySerialNumber);
    }
    if fuji.is_some_and(|fuji| fuji.internal_serial.is_some()) {
        removed.push(RemovedField::InternalSerialNumber);
    }
    if exif.has_maker_note {
        removed.push(RemovedField::MakerNote);
    }

    (metadata, RemovedFields(removed))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RemovedField {
    GpsLocation,
    ImageDescription,
    Artist,
    Copyright,
    UserComment,
    CameraOwnerName,
    LensSerialNumber,
    ImageUniqueId,
    BodySerialNumber,
    InternalSerialNumber,
    MakerNote,
}

impl RemovedField {
    pub const REDACTED: &'static str = "[redacted]";

    pub const fn label(self) -> &'static str {
        match self {
            Self::GpsLocation => "GPS location",
            Self::ImageDescription => "Image description",
            Self::Artist => "Artist",
            Self::Copyright => "Copyright",
            Self::UserComment => "User comment",
            Self::CameraOwnerName => "Camera owner name",
            Self::LensSerialNumber => "Lens serial number",
            Self::ImageUniqueId => "Image unique id",
            Self::BodySerialNumber => "Body serial number",
            Self::InternalSerialNumber => "Internal serial number",
            Self::MakerNote => "MakerNote",
        }
    }
}

impl Serialize for RemovedField {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("RemovedField", 2)?;
        state.serialize_field("field", self.label())?;
        state.serialize_field("value", Self::REDACTED)?;
        state.end()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemovedFields(Vec<RemovedField>);

impl RemovedFields {
    pub fn iter(&self) -> impl Iterator<Item = RemovedField> + '_ {
        self.0.iter().copied()
    }

    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.0.iter().map(|field| field.label())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, field: RemovedField) -> bool {
        self.0.contains(&field)
    }
}

impl Serialize for RemovedFields {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for field in &self.0 {
            sequence.serialize_element(field)?;
        }
        sequence.end()
    }
}
