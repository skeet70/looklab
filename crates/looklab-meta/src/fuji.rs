// SPDX-License-Identifier: AGPL-3.0-or-later

use serde::{Serialize, Serializer};

use crate::serial::RawSerial;
use crate::types::ImageNumber;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FujiTag {
    Version,
    InternalSerialNumber,
    Sharpness,
    WhiteBalance,
    Saturation,
    WhiteBalanceFineTune,
    NoiseReduction,
    NoiseReductionLegacy,
    Clarity,
    ShadowTone,
    HighlightTone,
    GrainEffectRoughness,
    ColorChromeEffect,
    BwAdjustment,
    BwMagentaGreen,
    GrainEffectSize,
    ColorChromeFxBlue,
    DynamicRange,
    FilmMode,
    DynamicRangeSetting,
    DevelopmentDynamicRange,
    ImageGeneration,
    ImageCount,
    SequenceNumber,
}

impl FujiTag {
    pub const ALL: [Self; 24] = [
        Self::Version,
        Self::InternalSerialNumber,
        Self::Sharpness,
        Self::WhiteBalance,
        Self::Saturation,
        Self::WhiteBalanceFineTune,
        Self::NoiseReduction,
        Self::NoiseReductionLegacy,
        Self::Clarity,
        Self::ShadowTone,
        Self::HighlightTone,
        Self::GrainEffectRoughness,
        Self::ColorChromeEffect,
        Self::BwAdjustment,
        Self::BwMagentaGreen,
        Self::GrainEffectSize,
        Self::ColorChromeFxBlue,
        Self::DynamicRange,
        Self::FilmMode,
        Self::DynamicRangeSetting,
        Self::DevelopmentDynamicRange,
        Self::ImageGeneration,
        Self::ImageCount,
        Self::SequenceNumber,
    ];

    /// Both noise-reduction tags share one ExifTool name; this returns the
    /// current tag.
    pub fn from_exiftool_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|tag| tag.exiftool_name() == name)
    }

    pub const fn id(self) -> u16 {
        match self {
            Self::Version => 0x0000,
            Self::InternalSerialNumber => 0x0010,
            Self::Sharpness => 0x1001,
            Self::WhiteBalance => 0x1002,
            Self::Saturation => 0x1003,
            Self::WhiteBalanceFineTune => 0x100a,
            Self::NoiseReductionLegacy => 0x100b,
            Self::NoiseReduction => 0x100e,
            Self::Clarity => 0x100f,
            Self::ShadowTone => 0x1040,
            Self::HighlightTone => 0x1041,
            Self::GrainEffectRoughness => 0x1047,
            Self::ColorChromeEffect => 0x1048,
            Self::BwAdjustment => 0x1049,
            Self::BwMagentaGreen => 0x104b,
            Self::GrainEffectSize => 0x104c,
            Self::ColorChromeFxBlue => 0x104e,
            Self::SequenceNumber => 0x1101,
            Self::DynamicRange => 0x1400,
            Self::FilmMode => 0x1401,
            Self::DynamicRangeSetting => 0x1402,
            Self::DevelopmentDynamicRange => 0x1403,
            Self::ImageGeneration => 0x1436,
            Self::ImageCount => 0x1438,
        }
    }

    pub const fn exiftool_name(self) -> &'static str {
        match self {
            Self::Version => "Version",
            Self::InternalSerialNumber => "InternalSerialNumber",
            Self::Sharpness => "Sharpness",
            Self::WhiteBalance => "WhiteBalance",
            Self::Saturation => "Saturation",
            Self::WhiteBalanceFineTune => "WhiteBalanceFineTune",
            Self::NoiseReduction => "NoiseReduction",
            Self::NoiseReductionLegacy => "NoiseReduction",
            Self::Clarity => "Clarity",
            Self::ShadowTone => "ShadowTone",
            Self::HighlightTone => "HighlightTone",
            Self::GrainEffectRoughness => "GrainEffectRoughness",
            Self::ColorChromeEffect => "ColorChromeEffect",
            Self::BwAdjustment => "BWAdjustment",
            Self::BwMagentaGreen => "BWMagentaGreen",
            Self::GrainEffectSize => "GrainEffectSize",
            Self::ColorChromeFxBlue => "ColorChromeFXBlue",
            Self::DynamicRange => "DynamicRange",
            Self::FilmMode => "FilmMode",
            Self::DynamicRangeSetting => "DynamicRangeSetting",
            Self::DevelopmentDynamicRange => "DevelopmentDynamicRange",
            Self::ImageGeneration => "ImageGeneration",
            Self::ImageCount => "ImageCount",
            Self::SequenceNumber => "SequenceNumber",
        }
    }
}

pub struct FujiMakernote {
    /// MakerNote `Version` (tag 0x0000); this is not the camera firmware version.
    pub maker_note_version: Option<String>,
    pub internal_serial: Option<RawSerial>,
    pub look: LookSettings,
    pub image_generation: Option<ImageGeneration>,
    pub image_count: Option<ImageNumber>,
    pub sequence_number: Option<ImageNumber>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ParsedTag<T> {
    pub tag: FujiTag,
    pub raw: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<T>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ParsedPair {
    pub tag: FujiTag,
    pub raw: [i32; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<WhiteBalanceFineTune>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WhiteBalanceFineTune {
    pub red: i32,
    pub blue: i32,
}

impl WhiteBalanceFineTune {
    pub fn red_shift(self) -> f64 {
        f64::from(self.red) / 20.0
    }

    pub fn blue_shift(self) -> f64 {
        f64::from(self.blue) / 20.0
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct LookSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub film_mode: Option<ParsedTag<FilmMode>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dynamic_range: Option<ParsedTag<DynamicRange>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dynamic_range_setting: Option<ParsedTag<DynamicRangeSetting>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub development_dynamic_range: Option<ParsedTag<DevelopmentDynamicRange>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saturation: Option<ParsedTag<ColorSaturation>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub white_balance: Option<ParsedTag<WhiteBalanceMode>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub white_balance_fine_tune: Option<ParsedPair>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub highlight_tone: Option<ParsedTag<ToneAdjustment>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow_tone: Option<ParsedTag<ToneAdjustment>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sharpness: Option<ParsedTag<Sharpness>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub noise_reduction: Option<ParsedTag<NoiseReduction>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clarity: Option<ParsedTag<Clarity>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grain_roughness: Option<ParsedTag<GrainRoughness>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grain_size: Option<ParsedTag<GrainSize>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bw_adjustment: Option<ParsedTag<WarmCool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bw_magenta_green: Option<ParsedTag<MagentaGreen>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_chrome_effect: Option<ParsedTag<EffectStrength>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_chrome_fx_blue: Option<ParsedTag<EffectStrength>>,
}

impl LookSettings {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FilmMode {
    Provia,
    StudioPortrait,
    StudioPortraitEnhancedSaturation,
    Astia,
    StudioPortraitIncreasedSharpness,
    Velvia,
    StudioPortraitEx,
    VelviaF4,
    ProNegStd,
    ProNegHi,
    ClassicChrome,
    Eterna,
    ClassicNegative,
    BleachBypass,
    NostalgicNeg,
    RealaAce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DynamicRange {
    Standard,
    Wide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DynamicRangeSetting {
    Auto,
    Manual,
    Standard100,
    Wide1_230,
    Wide2_400,
    FilmSimulation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct DevelopmentDynamicRange(u16);

impl DevelopmentDynamicRange {
    pub const fn percent(self) -> u16 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSaturation {
    Step(i8),
    Low,
    Monochrome(MonochromeFilter),
    Acros(MonochromeFilter),
    FilmSimulation,
}

impl ColorSaturation {
    /// Decodes the `Saturation` tag's raw value.
    pub fn from_raw(raw: u16) -> Option<Self> {
        decode_saturation(raw)
    }
}

impl Serialize for ColorSaturation {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Step(step) => serializer.serialize_i8(*step),
            Self::Low => serializer.serialize_str("low"),
            Self::Monochrome(filter) => serializer.serialize_str(filter.monochrome_name()),
            Self::Acros(filter) => serializer.serialize_str(filter.acros_name()),
            Self::FilmSimulation => serializer.serialize_str("film_simulation"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MonochromeFilter {
    NoFilter,
    Red,
    Yellow,
    Green,
    Sepia,
}

impl MonochromeFilter {
    const fn monochrome_name(self) -> &'static str {
        match self {
            Self::NoFilter => "monochrome",
            Self::Red => "monochrome_red",
            Self::Yellow => "monochrome_yellow",
            Self::Green => "monochrome_green",
            Self::Sepia => "monochrome_sepia",
        }
    }

    const fn acros_name(self) -> &'static str {
        match self {
            Self::NoFilter => "acros",
            Self::Red => "acros_red",
            Self::Yellow => "acros_yellow",
            Self::Green => "acros_green",
            Self::Sepia => "acros_sepia",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WhiteBalanceMode {
    Auto,
    AutoWhitePriority,
    AutoAmbiancePriority,
    Daylight,
    Cloudy,
    DaylightFluorescent,
    DayWhiteFluorescent,
    WhiteFluorescent,
    WarmWhiteFluorescent,
    LivingRoomWarmWhiteFluorescent,
    Incandescent,
    Flash,
    Underwater,
    Custom,
    Custom2,
    Custom3,
    Custom4,
    Custom5,
    Kelvin,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ToneAdjustment(f64);

impl ToneAdjustment {
    pub fn from_raw(raw: i32) -> Self {
        Self(-f64::from(raw) / 16.0)
    }

    pub const fn stops(self) -> f64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sharpness {
    Step(i8),
    FilmSimulation,
    NotApplicable,
}

impl Serialize for Sharpness {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Step(step) => serializer.serialize_i8(*step),
            Self::FilmSimulation => serializer.serialize_str("film_simulation"),
            Self::NotApplicable => serializer.serialize_str("not_applicable"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseReduction {
    Step(i8),
    Low,
    NotApplicable,
}

impl Serialize for NoiseReduction {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Step(step) => serializer.serialize_i8(*step),
            Self::Low => serializer.serialize_str("low"),
            Self::NotApplicable => serializer.serialize_str("not_applicable"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Clarity(i8);

impl Clarity {
    pub const fn steps(self) -> i8 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GrainRoughness {
    Off,
    WeakSmall,
    Weak,
    StrongSmall,
    Strong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GrainSize {
    Off,
    Small,
    Large,
}

/// `BWAdjustment`: positive is warm, negative is cool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct WarmCool(i8);

impl WarmCool {
    pub const fn steps(self) -> i8 {
        self.0
    }
}

/// `BWMagentaGreen`: positive is green, negative is magenta.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct MagentaGreen(i8);

impl MagentaGreen {
    pub const fn steps(self) -> i8 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectStrength {
    Off,
    Weak,
    Strong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageGeneration {
    Original,
    ReDevelopedFromRaw,
}

enum EntryData<'a> {
    Inline([u8; 4]),
    At(&'a [u8]),
}

struct FujiEntry<'a> {
    tag: u16,
    typ: u16,
    count: u32,
    data: EntryData<'a>,
}

impl FujiEntry<'_> {
    fn bytes(&self) -> &[u8] {
        match &self.data {
            EntryData::Inline(bytes) => bytes,
            EntryData::At(bytes) => bytes,
        }
    }

    fn u16(&self) -> Option<u16> {
        if self.typ != 3 || self.count < 1 {
            return None;
        }
        Some(u16::from_le_bytes(self.bytes().get(..2)?.try_into().ok()?))
    }

    fn i32_at(&self, index: usize) -> Option<i32> {
        if self.typ != 9 || index >= self.count as usize {
            return None;
        }
        let start = index.checked_mul(4)?;
        let end = start.checked_add(4)?;
        Some(i32::from_le_bytes(
            self.bytes().get(start..end)?.try_into().ok()?,
        ))
    }

    fn i8_at(&self, index: usize) -> Option<i8> {
        if index >= self.count as usize {
            return None;
        }
        match self.typ {
            1 | 6 | 7 => self.bytes().get(index).map(|byte| *byte as i8),
            3 => {
                let start = index.checked_mul(2)?;
                let raw = u16::from_le_bytes(self.bytes().get(start..start + 2)?.try_into().ok()?);
                u8::try_from(raw).ok().map(|byte| byte as i8)
            }
            _ => None,
        }
    }

    fn ascii(&self) -> Option<&str> {
        if self.typ != 2 {
            return None;
        }
        let bytes = self.bytes();
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let text = core::str::from_utf8(&bytes[..end]).ok()?;
        let text = text.trim_end_matches(['\0', ' ']);
        (!text.is_empty()).then_some(text)
    }
}

fn read_entries(bytes: &[u8], offset: usize) -> Option<Vec<FujiEntry<'_>>> {
    let count_end = offset.checked_add(2)?;
    let count = u16::from_le_bytes(bytes.get(offset..count_end)?.try_into().ok()?) as usize;
    if count > 1024 {
        return None;
    }
    let mut entries = Vec::with_capacity(count);
    let mut position = count_end;
    for _ in 0..count {
        let entry_end = position.checked_add(12)?;
        let raw = bytes.get(position..entry_end)?;
        position = entry_end;
        let tag = u16::from_le_bytes(raw.get(..2)?.try_into().ok()?);
        let typ = u16::from_le_bytes(raw.get(2..4)?.try_into().ok()?);
        let count = u32::from_le_bytes(raw.get(4..8)?.try_into().ok()?);
        let size = match typ {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 => 4,
            5 | 10 => 8,
            _ => continue,
        };
        let Some(total) = (count as usize).checked_mul(size) else {
            continue;
        };
        let data = if total <= 4 {
            EntryData::Inline([raw[8], raw[9], raw[10], raw[11]])
        } else {
            let start = u32::from_le_bytes(raw.get(8..12)?.try_into().ok()?) as usize;
            let Some(end) = start.checked_add(total) else {
                continue;
            };
            let Some(slice) = bytes.get(start..end) else {
                continue;
            };
            EntryData::At(slice)
        };
        entries.push(FujiEntry {
            tag,
            typ,
            count,
            data,
        });
    }
    Some(entries)
}

pub(crate) fn parse(bytes: &[u8]) -> Option<FujiMakernote> {
    if !bytes.starts_with(b"FUJIFILM") {
        return None;
    }
    let offset = u32::from_le_bytes(bytes.get(8..12)?.try_into().ok()?) as usize;
    let entries = read_entries(bytes, offset)?;
    let find = |id: u16| entries.iter().find(|entry| entry.tag == id);

    let look = LookSettings {
        film_mode: tagged_u16(find(0x1401), FujiTag::FilmMode, decode_film_mode),
        dynamic_range: tagged_u16(find(0x1400), FujiTag::DynamicRange, decode_dynamic_range),
        dynamic_range_setting: tagged_u16(
            find(0x1402),
            FujiTag::DynamicRangeSetting,
            decode_dynamic_range_setting,
        ),
        development_dynamic_range: tagged_u16(
            find(0x1403),
            FujiTag::DevelopmentDynamicRange,
            |raw| Some(DevelopmentDynamicRange(raw)),
        ),
        saturation: tagged_u16(find(0x1003), FujiTag::Saturation, decode_saturation),
        white_balance: tagged_u16(find(0x1002), FujiTag::WhiteBalance, decode_white_balance),
        white_balance_fine_tune: find(0x100a).and_then(|entry| {
            let red = entry.i32_at(0)?;
            let blue = entry.i32_at(1)?;
            Some(ParsedPair {
                tag: FujiTag::WhiteBalanceFineTune,
                raw: [red, blue],
                value: Some(WhiteBalanceFineTune { red, blue }),
            })
        }),
        highlight_tone: tagged_i32(find(0x1041), FujiTag::HighlightTone, decode_tone),
        shadow_tone: tagged_i32(find(0x1040), FujiTag::ShadowTone, decode_tone),
        sharpness: tagged_u16(find(0x1001), FujiTag::Sharpness, decode_sharpness),
        noise_reduction: if let Some(entry) = find(0x100e) {
            tagged_u16(Some(entry), FujiTag::NoiseReduction, decode_noise_reduction)
        } else {
            tagged_u16(
                find(0x100b),
                FujiTag::NoiseReductionLegacy,
                decode_noise_reduction_legacy,
            )
        },
        clarity: tagged_i32(find(0x100f), FujiTag::Clarity, decode_clarity),
        grain_roughness: tagged_i32(
            find(0x1047),
            FujiTag::GrainEffectRoughness,
            decode_grain_roughness,
        ),
        grain_size: tagged_u16(find(0x104c), FujiTag::GrainEffectSize, decode_grain_size),
        bw_adjustment: tagged_i8(find(0x1049), FujiTag::BwAdjustment, WarmCool),
        bw_magenta_green: tagged_i8(find(0x104b), FujiTag::BwMagentaGreen, MagentaGreen),
        color_chrome_effect: tagged_i32(
            find(0x1048),
            FujiTag::ColorChromeEffect,
            decode_effect_strength,
        ),
        color_chrome_fx_blue: tagged_i32(
            find(0x104e),
            FujiTag::ColorChromeFxBlue,
            decode_effect_strength,
        ),
    };

    let maker_note_version = find(0x0000).and_then(version_string);
    let internal_serial = find(0x0010)
        .and_then(FujiEntry::ascii)
        .and_then(|text| RawSerial::new(text.as_bytes()));
    let image_generation = tagged_u16(
        find(0x1436),
        FujiTag::ImageGeneration,
        decode_image_generation,
    );
    let image_count = find(0x1438)
        .and_then(FujiEntry::u16)
        .map(|raw| ImageNumber::new(raw & 0x7fff));
    let sequence_number = find(0x1101).and_then(FujiEntry::u16).map(ImageNumber::new);

    Some(FujiMakernote {
        maker_note_version,
        internal_serial,
        look,
        image_generation: image_generation.and_then(|tagged| tagged.value),
        image_count,
        sequence_number,
    })
}

fn tagged_u16<T>(
    entry: Option<&FujiEntry>,
    tag: FujiTag,
    decode: fn(u16) -> Option<T>,
) -> Option<ParsedTag<T>> {
    let raw = entry?.u16()?;
    Some(ParsedTag {
        tag,
        raw: i64::from(raw),
        value: decode(raw),
    })
}

fn tagged_i32<T>(
    entry: Option<&FujiEntry>,
    tag: FujiTag,
    decode: fn(i32) -> Option<T>,
) -> Option<ParsedTag<T>> {
    let raw = entry?.i32_at(0)?;
    Some(ParsedTag {
        tag,
        raw: i64::from(raw),
        value: decode(raw),
    })
}

fn tagged_i8<T>(
    entry: Option<&FujiEntry>,
    tag: FujiTag,
    wrap: fn(i8) -> T,
) -> Option<ParsedTag<T>> {
    let raw = entry?.i8_at(0)?;
    Some(ParsedTag {
        tag,
        raw: i64::from(raw),
        value: Some(wrap(raw)),
    })
}

fn version_string(entry: &FujiEntry) -> Option<String> {
    let bytes = match entry.typ {
        2 | 7 => entry.bytes(),
        _ => return None,
    };
    let text = core::str::from_utf8(bytes).ok()?;
    let text = text.trim_matches(|c: char| c == '\0' || c == ' ');
    (!text.is_empty()).then(|| text.to_owned())
}

fn decode_film_mode(raw: u16) -> Option<FilmMode> {
    Some(match raw {
        0x000 => FilmMode::Provia,
        0x100 => FilmMode::StudioPortrait,
        0x110 => FilmMode::StudioPortraitEnhancedSaturation,
        0x120 => FilmMode::Astia,
        0x130 => FilmMode::StudioPortraitIncreasedSharpness,
        0x200 => FilmMode::Velvia,
        0x300 => FilmMode::StudioPortraitEx,
        0x400 => FilmMode::VelviaF4,
        0x500 => FilmMode::ProNegStd,
        0x501 => FilmMode::ProNegHi,
        0x600 => FilmMode::ClassicChrome,
        0x700 => FilmMode::Eterna,
        0x800 => FilmMode::ClassicNegative,
        0x900 => FilmMode::BleachBypass,
        0xa00 => FilmMode::NostalgicNeg,
        0xb00 => FilmMode::RealaAce,
        _ => return None,
    })
}

fn decode_dynamic_range(raw: u16) -> Option<DynamicRange> {
    Some(match raw {
        1 => DynamicRange::Standard,
        3 => DynamicRange::Wide,
        _ => return None,
    })
}

fn decode_dynamic_range_setting(raw: u16) -> Option<DynamicRangeSetting> {
    Some(match raw {
        0x000 => DynamicRangeSetting::Auto,
        0x001 => DynamicRangeSetting::Manual,
        0x100 => DynamicRangeSetting::Standard100,
        0x200 => DynamicRangeSetting::Wide1_230,
        0x201 => DynamicRangeSetting::Wide2_400,
        0x8000 => DynamicRangeSetting::FilmSimulation,
        _ => return None,
    })
}

fn decode_saturation(raw: u16) -> Option<ColorSaturation> {
    Some(match raw {
        0x000 => ColorSaturation::Step(0),
        0x080 => ColorSaturation::Step(1),
        0x100 => ColorSaturation::Step(2),
        0x0c0 => ColorSaturation::Step(3),
        0x0e0 => ColorSaturation::Step(4),
        0x180 => ColorSaturation::Step(-1),
        0x400 => ColorSaturation::Step(-2),
        0x4c0 => ColorSaturation::Step(-3),
        0x4e0 => ColorSaturation::Step(-4),
        0x200 => ColorSaturation::Low,
        0x300 => ColorSaturation::Monochrome(MonochromeFilter::NoFilter),
        0x301 => ColorSaturation::Monochrome(MonochromeFilter::Red),
        0x302 => ColorSaturation::Monochrome(MonochromeFilter::Yellow),
        0x303 => ColorSaturation::Monochrome(MonochromeFilter::Green),
        0x310 => ColorSaturation::Monochrome(MonochromeFilter::Sepia),
        0x500 => ColorSaturation::Acros(MonochromeFilter::NoFilter),
        0x501 => ColorSaturation::Acros(MonochromeFilter::Red),
        0x502 => ColorSaturation::Acros(MonochromeFilter::Yellow),
        0x503 => ColorSaturation::Acros(MonochromeFilter::Green),
        0x8000 => ColorSaturation::FilmSimulation,
        _ => return None,
    })
}

fn decode_white_balance(raw: u16) -> Option<WhiteBalanceMode> {
    Some(match raw {
        0x000 => WhiteBalanceMode::Auto,
        0x001 => WhiteBalanceMode::AutoWhitePriority,
        0x002 => WhiteBalanceMode::AutoAmbiancePriority,
        0x100 => WhiteBalanceMode::Daylight,
        0x200 => WhiteBalanceMode::Cloudy,
        0x300 => WhiteBalanceMode::DaylightFluorescent,
        0x301 => WhiteBalanceMode::DayWhiteFluorescent,
        0x302 => WhiteBalanceMode::WhiteFluorescent,
        0x303 => WhiteBalanceMode::WarmWhiteFluorescent,
        0x304 => WhiteBalanceMode::LivingRoomWarmWhiteFluorescent,
        0x400 => WhiteBalanceMode::Incandescent,
        0x500 => WhiteBalanceMode::Flash,
        0x600 => WhiteBalanceMode::Underwater,
        0xf00 => WhiteBalanceMode::Custom,
        0xf01 => WhiteBalanceMode::Custom2,
        0xf02 => WhiteBalanceMode::Custom3,
        0xf03 => WhiteBalanceMode::Custom4,
        0xf04 => WhiteBalanceMode::Custom5,
        0xff0 => WhiteBalanceMode::Kelvin,
        _ => return None,
    })
}

fn decode_sharpness(raw: u16) -> Option<Sharpness> {
    Some(match raw {
        0x00 => Sharpness::Step(-4),
        0x01 => Sharpness::Step(-3),
        0x02 => Sharpness::Step(-2),
        0x03 => Sharpness::Step(0),
        0x04 => Sharpness::Step(2),
        0x05 => Sharpness::Step(3),
        0x06 => Sharpness::Step(4),
        0x82 => Sharpness::Step(-1),
        0x84 => Sharpness::Step(1),
        0x8000 => Sharpness::FilmSimulation,
        0xffff => Sharpness::NotApplicable,
        _ => return None,
    })
}

fn decode_noise_reduction(raw: u16) -> Option<NoiseReduction> {
    Some(match raw {
        0x000 => NoiseReduction::Step(0),
        0x100 => NoiseReduction::Step(2),
        0x180 => NoiseReduction::Step(1),
        0x1c0 => NoiseReduction::Step(3),
        0x1e0 => NoiseReduction::Step(4),
        0x200 => NoiseReduction::Step(-2),
        0x280 => NoiseReduction::Step(-1),
        0x2c0 => NoiseReduction::Step(-3),
        0x2e0 => NoiseReduction::Step(-4),
        _ => return None,
    })
}

fn decode_noise_reduction_legacy(raw: u16) -> Option<NoiseReduction> {
    Some(match raw {
        0x40 => NoiseReduction::Low,
        0x80 => NoiseReduction::Step(0),
        0x100 => NoiseReduction::NotApplicable,
        _ => return None,
    })
}

fn decode_clarity(raw: i32) -> Option<Clarity> {
    let steps = raw / 1000;
    (raw % 1000 == 0 && (-5..=5).contains(&steps)).then_some(Clarity(steps as i8))
}

fn decode_tone(raw: i32) -> Option<ToneAdjustment> {
    Some(ToneAdjustment::from_raw(raw))
}

fn decode_grain_roughness(raw: i32) -> Option<GrainRoughness> {
    Some(match raw {
        0 => GrainRoughness::Off,
        16 => GrainRoughness::WeakSmall,
        32 => GrainRoughness::Weak,
        48 => GrainRoughness::StrongSmall,
        64 => GrainRoughness::Strong,
        _ => return None,
    })
}

fn decode_grain_size(raw: u16) -> Option<GrainSize> {
    Some(match raw {
        0 => GrainSize::Off,
        16 => GrainSize::Small,
        32 => GrainSize::Large,
        _ => return None,
    })
}

fn decode_effect_strength(raw: i32) -> Option<EffectStrength> {
    Some(match raw {
        0 => EffectStrength::Off,
        32 => EffectStrength::Weak,
        64 => EffectStrength::Strong,
        _ => return None,
    })
}

fn decode_image_generation(raw: u16) -> Option<ImageGeneration> {
    Some(match raw {
        0 => ImageGeneration::Original,
        1 => ImageGeneration::ReDevelopedFromRaw,
        _ => return None,
    })
}
