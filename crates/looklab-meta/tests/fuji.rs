// SPDX-License-Identifier: AGPL-3.0-or-later
mod support;

use looklab_meta::{
    ColorSaturation, DynamicRange, DynamicRangeSetting, EffectStrength, FilmMode, FujiTag,
    GrainRoughness, GrainSize, ImageGeneration, LookSettings, NoiseReduction, Sharpness,
    WhiteBalanceMode, parse_jpeg, parse_tiff, sanitize, software_matches_firmware,
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
    let warm_cool = look.bw_adjustment.expect("BWAdjustment");
    assert_eq!(warm_cool.tag, FujiTag::BwAdjustment);
    assert_eq!(warm_cool.raw, 7);
    assert_eq!(warm_cool.value.expect("warm/cool").steps(), 7);
    let magenta_green = look.bw_magenta_green.expect("BWMagentaGreen");
    assert_eq!(magenta_green.tag, FujiTag::BwMagentaGreen);
    assert_eq!(magenta_green.raw, -3);
    assert_eq!(magenta_green.value.expect("magenta/green").steps(), -3);
}

#[test]
fn decodes_wc_mg_stored_as_unsigned_bytes() {
    let makernote = support::build_makernote(&[
        (0x1049, support::FujiValue::Byte(0)),
        (0x104b, support::FujiValue::Byte(0)),
    ]);
    let tiff = support::build_tiff(&[], &[(0x927c, support::Value::Undefined(makernote))], None);
    let look = parse_tiff(&tiff)
        .expect("synthetic TIFF")
        .fuji
        .expect("Fujifilm makernote")
        .look;
    assert_eq!(
        look.bw_adjustment
            .expect("BWAdjustment")
            .value
            .expect("warm/cool")
            .steps(),
        0
    );
    assert_eq!(
        look.bw_magenta_green
            .expect("BWMagentaGreen")
            .value
            .expect("magenta/green")
            .steps(),
        0
    );

    let makernote = support::build_makernote(&[
        (0x1049, support::FujiValue::Byte(0xff)),
        (0x104b, support::FujiValue::Byte(0xfe)),
    ]);
    let tiff = support::build_tiff(&[], &[(0x927c, support::Value::Undefined(makernote))], None);
    let look = parse_tiff(&tiff)
        .expect("synthetic TIFF")
        .fuji
        .expect("Fujifilm makernote")
        .look;
    assert_eq!(
        look.bw_adjustment
            .expect("BWAdjustment")
            .value
            .expect("warm/cool")
            .steps(),
        -1
    );
    assert_eq!(
        look.bw_magenta_green
            .expect("BWMagentaGreen")
            .value
            .expect("magenta/green")
            .steps(),
        -2
    );
}

#[test]
fn decodes_shot_number_and_maker_note_version() {
    let fuji = parse_tiff(&support::full_exif_tiff())
        .expect("synthetic TIFF")
        .fuji
        .expect("Fujifilm makernote");

    assert_eq!(fuji.maker_note_version.as_deref(), Some("0101"));
    assert_eq!(
        fuji.image_generation,
        Some(ImageGeneration::ReDevelopedFromRaw)
    );
    assert_eq!(fuji.image_count.expect("ImageCount").value(), 1234);
    assert_eq!(fuji.sequence_number.expect("SequenceNumber").value(), 77);

    let internal = fuji.internal_serial.expect("InternalSerialNumber");
    let ascii = b"FPX20017035 592D31313034060427796060";
    let mut envelope = Vec::new();
    internal.write_envelope_field(&mut envelope);
    assert_eq!(envelope[0], 1);
    assert_eq!(envelope[1] as usize, ascii.len());
    assert_eq!(&envelope[2..], ascii);
}

#[test]
fn keeps_unrecognized_values_with_their_tag() {
    let makernote = support::build_makernote(&[
        (0x100f, support::FujiValue::SLong(6000)),
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

    let clarity = fuji.look.clarity.expect("Clarity");
    assert_eq!(clarity.raw, 6000);
    assert_eq!(clarity.value, None);
}

#[test]
fn maker_note_version_is_not_firmware() {
    let makernote =
        support::build_makernote(&[(0x0000, support::FujiValue::Undefined(b"0130".to_vec()))]);
    let tiff = support::build_tiff(
        &[(
            0x0131,
            support::Value::Ascii("Digital Camera X-T5 Ver1.00".into()),
        )],
        &[(0x927c, support::Value::Undefined(makernote))],
        None,
    );
    let parsed = parse_jpeg(&support::build_jpeg(&tiff)).expect("synthetic JPEG");

    assert_eq!(parsed.firmware.as_deref(), Some("1.00"));
    assert_eq!(
        parsed
            .fuji
            .as_ref()
            .expect("makernote")
            .maker_note_version
            .as_deref(),
        Some("0130")
    );
    assert!(software_matches_firmware(
        parsed.exif.software.as_deref(),
        parsed.firmware.as_deref()
    ));

    let (sanitized, _) = sanitize(&parsed);
    assert_eq!(sanitized.firmware.as_deref(), Some("1.00"));
}

#[test]
fn hostile_ifd_offset_is_rejected() {
    let mut makernote = b"FUJIFILM".to_vec();
    makernote.extend_from_slice(&u32::MAX.to_le_bytes());
    let tiff = support::build_tiff(&[], &[(0x927c, support::Value::Undefined(makernote))], None);
    let parsed = parse_tiff(&tiff).expect("synthetic TIFF");
    assert!(parsed.fuji.is_none());
}

#[test]
fn drops_unknown_tags() {
    let look = makernote_look();
    let json = serde_json::to_string(&look).expect("look settings are JSON");
    assert!(!json.contains("9999"));
    assert!(!json.contains("unknown"));
}
