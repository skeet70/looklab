// SPDX-License-Identifier: AGPL-3.0-or-later
mod support;

use looklab_meta::{
    ColorSaturation, DynamicRange, DynamicRangeSetting, EffectStrength, FilmMode, FujiTag,
    GrainRoughness, GrainSize, ImageGeneration, LookSettings, NoiseReduction, Sharpness,
    WhiteBalanceMode, parse_tiff,
};

fn makernote_look() -> LookSettings {
    parse_tiff(&support::full_exif_tiff())
        .expect("synthetic TIFF")
        .fuji
        .expect("Fujifilm makernote")
        .look
}

#[test]
fn decodes_film_and_range_tags() {
    let look = makernote_look();

    let film = look.film_mode.expect("FilmMode");
    assert_eq!(film.tag, FujiTag::FilmMode);
    assert_eq!(film.raw, 0x700);
    assert_eq!(film.value, Some(FilmMode::Eterna));

    let range = look.dynamic_range.expect("DynamicRange");
    assert_eq!(range.raw, 3);
    assert_eq!(range.value, Some(DynamicRange::Wide));

    let setting = look.dynamic_range_setting.expect("DynamicRangeSetting");
    assert_eq!(setting.raw, 0x201);
    assert_eq!(setting.value, Some(DynamicRangeSetting::Wide2_400));

    let development = look
        .development_dynamic_range
        .expect("DevelopmentDynamicRange");
    assert_eq!(development.value.expect("percent").percent(), 400);
}

#[test]
fn decodes_color_and_white_balance_tags() {
    let look = makernote_look();

    let saturation = look.saturation.expect("Saturation");
    assert_eq!(saturation.raw, 0x100);
    assert_eq!(saturation.value, Some(ColorSaturation::Step(2)));

    let white_balance = look.white_balance.expect("WhiteBalance");
    assert_eq!(white_balance.raw, 0x200);
    assert_eq!(white_balance.value, Some(WhiteBalanceMode::Cloudy));

    let fine_tune = look.white_balance_fine_tune.expect("WhiteBalanceFineTune");
    assert_eq!(fine_tune.raw, [-40, 20]);
    let fine_tune = fine_tune.value.expect("fine tune values");
    assert_eq!(fine_tune.red_shift(), -2.0);
    assert_eq!(fine_tune.blue_shift(), 1.0);
}

#[test]
fn decodes_tone_and_detail_tags() {
    let look = makernote_look();

    assert_eq!(
        look.highlight_tone
            .expect("HighlightTone")
            .value
            .expect("tone")
            .stops(),
        -2.0
    );
    assert_eq!(
        look.shadow_tone
            .expect("ShadowTone")
            .value
            .expect("tone")
            .stops(),
        1.0
    );
    assert_eq!(
        look.sharpness.expect("Sharpness").value,
        Some(Sharpness::Step(1))
    );
    assert_eq!(
        look.noise_reduction.expect("NoiseReduction").value,
        Some(NoiseReduction::Step(-2))
    );
    assert_eq!(
        look.noise_reduction.expect("NoiseReduction").tag,
        FujiTag::NoiseReduction
    );
    assert_eq!(
        look.clarity
            .expect("Clarity")
            .value
            .expect("clarity")
            .steps(),
        2
    );
    assert_eq!(
        look.grain_roughness.expect("GrainEffectRoughness").value,
        Some(GrainRoughness::Weak)
    );
    assert_eq!(
        look.grain_size.expect("GrainEffectSize").value,
        Some(GrainSize::Small)
    );
    assert_eq!(
        look.color_chrome_effect.expect("ColorChromeEffect").value,
        Some(EffectStrength::Strong)
    );
    assert_eq!(
        look.color_chrome_fx_blue.expect("ColorChromeFXBlue").value,
        Some(EffectStrength::Weak)
    );
}

#[test]
fn decodes_shot_number_and_firmware_tags() {
    let fuji = parse_tiff(&support::full_exif_tiff())
        .expect("synthetic TIFF")
        .fuji
        .expect("Fujifilm makernote");

    assert_eq!(fuji.firmware.as_deref(), Some("0101"));
    assert_eq!(
        fuji.image_generation,
        Some(ImageGeneration::ReDevelopedFromRaw)
    );
    assert_eq!(fuji.image_count.expect("ImageCount").value(), 1234);
    assert_eq!(fuji.sequence_number.expect("SequenceNumber").value(), 77);
    assert_eq!(
        fuji.internal_serial
            .expect("InternalSerialNumber")
            .envelope_ascii(),
        &b"FPX20017035 592D31313034060427796060"[..]
    );
}

#[test]
fn keeps_unrecognized_values_with_their_tag() {
    let makernote = support::build_makernote(&[
        (0x1401, support::FujiValue::Short(0x9999)),
        (0x1400, support::FujiValue::Short(99)),
    ]);
    let tiff = support::build_tiff(&[], &[(0x927c, support::Value::Undefined(makernote))], None);
    let fuji = parse_tiff(&tiff)
        .expect("synthetic TIFF")
        .fuji
        .expect("makernote");

    let film = fuji.look.film_mode.expect("FilmMode");
    assert_eq!(film.tag, FujiTag::FilmMode);
    assert_eq!(film.raw, 0x9999);
    assert_eq!(film.value, None);

    let range = fuji.look.dynamic_range.expect("DynamicRange");
    assert_eq!(range.raw, 99);
    assert_eq!(range.value, None);
}

#[test]
fn drops_unknown_tags() {
    let look = makernote_look();
    let json = serde_json::to_string(&look).expect("look settings are JSON");
    assert!(!json.contains("9999"));
    assert!(!json.contains("unknown"));
}
