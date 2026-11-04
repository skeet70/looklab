// SPDX-License-Identifier: AGPL-3.0-or-later
//! Wire form of the sanitized per-image metadata.
//!
//! [`looklab_meta::SanitizedMetadata`] serializes but does not deserialize, so
//! the manifest carries this mirror.

use looklab_meta::{
    ColorSaturation as MetaSaturation, DevelopmentDynamicRange, DynamicRange as MetaDynamicRange,
    DynamicRangeSetting as MetaDynamicRangeSetting, EffectStrength as MetaEffectStrength,
    FilmMode as MetaFilmMode, GrainRoughness as MetaGrainRoughness, GrainSize as MetaGrainSize,
    LookSettings as MetaLookSettings, MonochromeFilter as MetaMonochromeFilter,
    NoiseReduction as MetaNoiseReduction, ParsedPair, ParsedTag, SanitizedMetadata,
    Sharpness as MetaSharpness, WhiteBalanceMode as MetaWhiteBalanceMode,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "MonthWire", into = "MonthWire")]
pub struct CaptureMonth {
    year: u16,
    month: u8,
}

#[derive(Serialize, Deserialize)]
struct MonthWire {
    year: u16,
    month: u8,
}

impl CaptureMonth {
    pub const fn new(year: u16, month: u8) -> Option<Self> {
        if year >= 1 && year <= 9999 && month >= 1 && month <= 12 {
            Some(Self { year, month })
        } else {
            None
        }
    }

    pub const fn year(self) -> u16 {
        self.year
    }

    pub const fn month(self) -> u8 {
        self.month
    }
}

impl TryFrom<MonthWire> for CaptureMonth {
    type Error = BadMonth;

    fn try_from(wire: MonthWire) -> Result<Self, Self::Error> {
        Self::new(wire.year, wire.month).ok_or(BadMonth {
            year: wire.year,
            month: wire.month,
        })
    }
}

impl From<CaptureMonth> for MonthWire {
    fn from(value: CaptureMonth) -> Self {
        Self {
            year: value.year,
            month: value.month,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("capture month {year}-{month} is out of range")]
pub struct BadMonth {
    pub year: u16,
    pub month: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WhiteBalanceMultipliers {
    pub r: f64,
    pub g: f64,
    pub b: f64,
}

/// A parsed makernote tag with its raw wire value, matching
/// [`looklab_meta::ParsedTag`] without its `FujiTag`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tagged<T> {
    pub raw: i64,
    pub value: Option<T>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhiteBalanceFineTune {
    pub raw: [i32; 2],
    pub shift: Option<WhiteBalanceShift>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhiteBalanceShift {
    pub red: i32,
    pub blue: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DynamicRange {
    Standard,
    Wide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DynamicRangeSetting {
    Auto,
    Manual,
    Standard100,
    Wide1_230,
    Wide2_400,
    FilmSimulation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MonochromeFilter {
    NoFilter,
    Red,
    Yellow,
    Green,
    Sepia,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Saturation {
    Step(i8),
    Low,
    Monochrome(MonochromeFilter),
    Acros(MonochromeFilter),
    FilmSimulation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sharpness {
    Step(i8),
    FilmSimulation,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoiseReduction {
    Step(i8),
    Low,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GrainRoughness {
    Off,
    WeakSmall,
    Weak,
    StrongSmall,
    Strong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GrainSize {
    Off,
    Small,
    Large,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectStrength {
    Off,
    Weak,
    Strong,
}

/// Mirror of [`looklab_meta::LookSettings`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LookSettings {
    pub film_mode: Option<Tagged<FilmMode>>,
    pub dynamic_range: Option<Tagged<DynamicRange>>,
    pub dynamic_range_setting: Option<Tagged<DynamicRangeSetting>>,
    pub development_dynamic_range: Option<Tagged<u16>>,
    pub saturation: Option<Tagged<Saturation>>,
    pub white_balance: Option<Tagged<WhiteBalanceMode>>,
    pub white_balance_fine_tune: Option<WhiteBalanceFineTune>,
    pub highlight_tone: Option<Tagged<f64>>,
    pub shadow_tone: Option<Tagged<f64>>,
    pub sharpness: Option<Tagged<Sharpness>>,
    pub noise_reduction: Option<Tagged<NoiseReduction>>,
    pub clarity: Option<Tagged<i8>>,
    pub grain_roughness: Option<Tagged<GrainRoughness>>,
    pub grain_size: Option<Tagged<GrainSize>>,
    pub color_chrome_effect: Option<Tagged<EffectStrength>>,
    pub color_chrome_fx_blue: Option<Tagged<EffectStrength>>,
}

/// Mirror of [`looklab_meta::SanitizedMetadata`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestMetadata {
    pub make: Option<String>,
    pub model: Option<String>,
    pub firmware: Option<String>,
    pub lens_model: Option<String>,
    pub focal_length_mm: Option<f64>,
    pub iso: Option<u32>,
    pub shutter_seconds: Option<f64>,
    pub aperture_f_number: Option<f64>,
    pub exposure_compensation_ev: Option<f64>,
    pub as_shot_wb: Option<WhiteBalanceMultipliers>,
    pub capture_month: Option<CaptureMonth>,
    pub software: Option<String>,
    pub look: Option<LookSettings>,
}

impl From<&SanitizedMetadata> for ManifestMetadata {
    fn from(meta: &SanitizedMetadata) -> Self {
        Self {
            make: meta.make.clone(),
            model: meta.model.clone(),
            firmware: meta.firmware.clone(),
            lens_model: meta.lens_model.clone(),
            focal_length_mm: meta.focal_length.map(|value| value.millimetres()),
            iso: meta.iso.map(|value| value.value()),
            shutter_seconds: meta.shutter.map(|value| value.seconds()),
            aperture_f_number: meta.aperture.map(|value| value.f_number()),
            exposure_compensation_ev: meta.exposure_compensation.map(|value| value.ev()),
            as_shot_wb: meta.as_shot_wb.map(|wb| WhiteBalanceMultipliers {
                r: wb.r,
                g: wb.g,
                b: wb.b,
            }),
            capture_month: meta
                .capture_month
                .and_then(|month| CaptureMonth::new(month.year(), month.month())),
            software: meta.software.clone(),
            look: meta.look.as_ref().map(LookSettings::from),
        }
    }
}

impl From<&MetaLookSettings> for LookSettings {
    fn from(look: &MetaLookSettings) -> Self {
        Self {
            film_mode: look.film_mode.map(tagged_film_mode),
            dynamic_range: look.dynamic_range.map(tagged_dynamic_range),
            dynamic_range_setting: look.dynamic_range_setting.map(tagged_dynamic_range_setting),
            development_dynamic_range: look
                .development_dynamic_range
                .map(tagged_development_dynamic_range),
            saturation: look.saturation.map(tagged_saturation),
            white_balance: look.white_balance.map(tagged_white_balance_mode),
            white_balance_fine_tune: look.white_balance_fine_tune.map(fine_tune),
            highlight_tone: look.highlight_tone.map(tagged_tone),
            shadow_tone: look.shadow_tone.map(tagged_tone),
            sharpness: look.sharpness.map(tagged_sharpness),
            noise_reduction: look.noise_reduction.map(tagged_noise_reduction),
            clarity: look.clarity.map(tagged_clarity),
            grain_roughness: look.grain_roughness.map(tagged_grain_roughness),
            grain_size: look.grain_size.map(tagged_grain_size),
            color_chrome_effect: look.color_chrome_effect.map(tagged_effect_strength),
            color_chrome_fx_blue: look.color_chrome_fx_blue.map(tagged_effect_strength),
        }
    }
}

fn tagged<T, U>(tag: ParsedTag<T>, convert: impl Fn(T) -> U) -> Tagged<U> {
    Tagged {
        raw: tag.raw,
        value: tag.value.map(convert),
    }
}

fn tagged_film_mode(tag: ParsedTag<MetaFilmMode>) -> Tagged<FilmMode> {
    tagged(tag, FilmMode::from)
}

fn tagged_dynamic_range(tag: ParsedTag<MetaDynamicRange>) -> Tagged<DynamicRange> {
    tagged(tag, |value| match value {
        MetaDynamicRange::Standard => DynamicRange::Standard,
        MetaDynamicRange::Wide => DynamicRange::Wide,
    })
}

fn tagged_dynamic_range_setting(
    tag: ParsedTag<MetaDynamicRangeSetting>,
) -> Tagged<DynamicRangeSetting> {
    tagged(tag, |value| match value {
        MetaDynamicRangeSetting::Auto => DynamicRangeSetting::Auto,
        MetaDynamicRangeSetting::Manual => DynamicRangeSetting::Manual,
        MetaDynamicRangeSetting::Standard100 => DynamicRangeSetting::Standard100,
        MetaDynamicRangeSetting::Wide1_230 => DynamicRangeSetting::Wide1_230,
        MetaDynamicRangeSetting::Wide2_400 => DynamicRangeSetting::Wide2_400,
        MetaDynamicRangeSetting::FilmSimulation => DynamicRangeSetting::FilmSimulation,
    })
}

fn tagged_development_dynamic_range(tag: ParsedTag<DevelopmentDynamicRange>) -> Tagged<u16> {
    tagged(tag, |value| value.percent())
}

fn tagged_saturation(tag: ParsedTag<MetaSaturation>) -> Tagged<Saturation> {
    tagged(tag, |value| match value {
        MetaSaturation::Step(step) => Saturation::Step(step),
        MetaSaturation::Low => Saturation::Low,
        MetaSaturation::Monochrome(filter) => {
            Saturation::Monochrome(MonochromeFilter::from(filter))
        }
        MetaSaturation::Acros(filter) => Saturation::Acros(MonochromeFilter::from(filter)),
        MetaSaturation::FilmSimulation => Saturation::FilmSimulation,
    })
}

fn tagged_white_balance_mode(tag: ParsedTag<MetaWhiteBalanceMode>) -> Tagged<WhiteBalanceMode> {
    tagged(tag, |value| match value {
        MetaWhiteBalanceMode::Auto => WhiteBalanceMode::Auto,
        MetaWhiteBalanceMode::AutoWhitePriority => WhiteBalanceMode::AutoWhitePriority,
        MetaWhiteBalanceMode::AutoAmbiancePriority => WhiteBalanceMode::AutoAmbiancePriority,
        MetaWhiteBalanceMode::Daylight => WhiteBalanceMode::Daylight,
        MetaWhiteBalanceMode::Cloudy => WhiteBalanceMode::Cloudy,
        MetaWhiteBalanceMode::DaylightFluorescent => WhiteBalanceMode::DaylightFluorescent,
        MetaWhiteBalanceMode::DayWhiteFluorescent => WhiteBalanceMode::DayWhiteFluorescent,
        MetaWhiteBalanceMode::WhiteFluorescent => WhiteBalanceMode::WhiteFluorescent,
        MetaWhiteBalanceMode::WarmWhiteFluorescent => WhiteBalanceMode::WarmWhiteFluorescent,
        MetaWhiteBalanceMode::LivingRoomWarmWhiteFluorescent => {
            WhiteBalanceMode::LivingRoomWarmWhiteFluorescent
        }
        MetaWhiteBalanceMode::Incandescent => WhiteBalanceMode::Incandescent,
        MetaWhiteBalanceMode::Flash => WhiteBalanceMode::Flash,
        MetaWhiteBalanceMode::Underwater => WhiteBalanceMode::Underwater,
        MetaWhiteBalanceMode::Custom => WhiteBalanceMode::Custom,
        MetaWhiteBalanceMode::Custom2 => WhiteBalanceMode::Custom2,
        MetaWhiteBalanceMode::Custom3 => WhiteBalanceMode::Custom3,
        MetaWhiteBalanceMode::Custom4 => WhiteBalanceMode::Custom4,
        MetaWhiteBalanceMode::Custom5 => WhiteBalanceMode::Custom5,
        MetaWhiteBalanceMode::Kelvin => WhiteBalanceMode::Kelvin,
    })
}

fn fine_tune(pair: ParsedPair) -> WhiteBalanceFineTune {
    WhiteBalanceFineTune {
        raw: pair.raw,
        shift: pair.value.map(|value| WhiteBalanceShift {
            red: value.red,
            blue: value.blue,
        }),
    }
}

fn tagged_tone(tag: ParsedTag<looklab_meta::ToneAdjustment>) -> Tagged<f64> {
    tagged(tag, |value| value.stops())
}

fn tagged_sharpness(tag: ParsedTag<MetaSharpness>) -> Tagged<Sharpness> {
    tagged(tag, |value| match value {
        MetaSharpness::Step(step) => Sharpness::Step(step),
        MetaSharpness::FilmSimulation => Sharpness::FilmSimulation,
        MetaSharpness::NotApplicable => Sharpness::NotApplicable,
    })
}

fn tagged_noise_reduction(tag: ParsedTag<MetaNoiseReduction>) -> Tagged<NoiseReduction> {
    tagged(tag, |value| match value {
        MetaNoiseReduction::Step(step) => NoiseReduction::Step(step),
        MetaNoiseReduction::Low => NoiseReduction::Low,
        MetaNoiseReduction::NotApplicable => NoiseReduction::NotApplicable,
    })
}

fn tagged_clarity(tag: ParsedTag<looklab_meta::Clarity>) -> Tagged<i8> {
    tagged(tag, |value| value.steps())
}

fn tagged_grain_roughness(tag: ParsedTag<MetaGrainRoughness>) -> Tagged<GrainRoughness> {
    tagged(tag, |value| match value {
        MetaGrainRoughness::Off => GrainRoughness::Off,
        MetaGrainRoughness::WeakSmall => GrainRoughness::WeakSmall,
        MetaGrainRoughness::Weak => GrainRoughness::Weak,
        MetaGrainRoughness::StrongSmall => GrainRoughness::StrongSmall,
        MetaGrainRoughness::Strong => GrainRoughness::Strong,
    })
}

fn tagged_grain_size(tag: ParsedTag<MetaGrainSize>) -> Tagged<GrainSize> {
    tagged(tag, |value| match value {
        MetaGrainSize::Off => GrainSize::Off,
        MetaGrainSize::Small => GrainSize::Small,
        MetaGrainSize::Large => GrainSize::Large,
    })
}

fn tagged_effect_strength(tag: ParsedTag<MetaEffectStrength>) -> Tagged<EffectStrength> {
    tagged(tag, |value| match value {
        MetaEffectStrength::Off => EffectStrength::Off,
        MetaEffectStrength::Weak => EffectStrength::Weak,
        MetaEffectStrength::Strong => EffectStrength::Strong,
    })
}

impl From<MetaMonochromeFilter> for MonochromeFilter {
    fn from(filter: MetaMonochromeFilter) -> Self {
        match filter {
            MetaMonochromeFilter::NoFilter => Self::NoFilter,
            MetaMonochromeFilter::Red => Self::Red,
            MetaMonochromeFilter::Yellow => Self::Yellow,
            MetaMonochromeFilter::Green => Self::Green,
            MetaMonochromeFilter::Sepia => Self::Sepia,
        }
    }
}

impl From<MetaFilmMode> for FilmMode {
    fn from(mode: MetaFilmMode) -> Self {
        match mode {
            MetaFilmMode::Provia => Self::Provia,
            MetaFilmMode::StudioPortrait => Self::StudioPortrait,
            MetaFilmMode::StudioPortraitEnhancedSaturation => {
                Self::StudioPortraitEnhancedSaturation
            }
            MetaFilmMode::Astia => Self::Astia,
            MetaFilmMode::StudioPortraitIncreasedSharpness => {
                Self::StudioPortraitIncreasedSharpness
            }
            MetaFilmMode::Velvia => Self::Velvia,
            MetaFilmMode::StudioPortraitEx => Self::StudioPortraitEx,
            MetaFilmMode::VelviaF4 => Self::VelviaF4,
            MetaFilmMode::ProNegStd => Self::ProNegStd,
            MetaFilmMode::ProNegHi => Self::ProNegHi,
            MetaFilmMode::ClassicChrome => Self::ClassicChrome,
            MetaFilmMode::Eterna => Self::Eterna,
            MetaFilmMode::ClassicNegative => Self::ClassicNegative,
            MetaFilmMode::BleachBypass => Self::BleachBypass,
            MetaFilmMode::NostalgicNeg => Self::NostalgicNeg,
            MetaFilmMode::RealaAce => Self::RealaAce,
        }
    }
}
