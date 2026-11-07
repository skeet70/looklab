// SPDX-License-Identifier: AGPL-3.0-or-later

#[path = "../../looklab-meta/tests/support/mod.rs"]
mod metadata_fixtures;

use looklab_meta::ParsedMetadata;
use looklab_wasm::pairing::UnpairedReasonView;
use looklab_wasm::{
    MetadataFile, UnpairedEntry, inspect_metadata, prefill_settings, prefill_settings_parsed,
};
use metadata_fixtures::{FujiValue, Value, build_jpeg, build_makernote, build_tiff};

#[test]
fn rendering_metadata_takes_precedence_over_the_raw_capture() {
    let raw =
        metadata_fixtures::build_raf_with_firmware(&metadata_fixtures::full_exif_jpeg(), b"0100");
    let schema = include_str!("../../../data/bodies/fujifilm-x-t5.yaml");
    for (software, expected_firmware) in [
        (Some("Digital Camera X-T5 Ver1.50"), "1.50"),
        (None, "1.00"),
    ] {
        let mut fields = vec![(0x010f, Value::Ascii("FUJIFILM".to_owned()))];
        if let Some(software) = software {
            fields.push((0x0131, Value::Ascii(software.to_owned())));
        }
        let maker = build_makernote(&[(0x1401, FujiValue::Short(0x0000))]);
        let jpeg = build_jpeg(&build_tiff(
            &fields,
            &[(0x927c, Value::Undefined(maker))],
            None,
        ));
        let prefill = prefill_settings(
            schema,
            &[
                ("capture.raf", raw.as_slice()),
                ("render.jpg", jpeg.as_slice()),
            ],
        )
        .expect("rendering metadata");
        assert_eq!(prefill.firmware, expected_firmware);
        assert_eq!(prefill.sim.as_deref(), Some("provia"));
    }
}

fn render_jpeg(software: Option<&str>, makernote: &[(u16, FujiValue)]) -> Vec<u8> {
    render_jpeg_at("2024:05:17 10:11:12", software, makernote)
}

fn render_jpeg_at(
    captured: &str,
    software: Option<&str>,
    makernote: &[(u16, FujiValue)],
) -> Vec<u8> {
    let mut fields = vec![
        (0x010f, Value::Ascii("FUJIFILM".to_owned())),
        (0x0110, Value::Ascii("X-T4".to_owned())),
    ];
    if let Some(software) = software {
        fields.push((0x0131, Value::Ascii(software.to_owned())));
    }
    build_jpeg(&build_tiff(
        &fields,
        &[
            (0x9003, Value::Ascii(captured.to_owned())),
            (0x927c, Value::Undefined(build_makernote(makernote))),
        ],
        None,
    ))
}

fn prefill_for<'a>(
    schema: &str,
    raw: (&'a str, &'a ParsedMetadata),
    render: (&'a str, &'a ParsedMetadata),
) -> looklab_wasm::SettingsPrefill {
    prefill_settings_parsed(schema, &[raw, render]).expect("prefill")
}

fn control<'a>(prefill: &'a looklab_wasm::SettingsPrefill, key: &str) -> Option<i64> {
    prefill
        .controls
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, step)| *step)
}

#[test]
fn a_shared_raw_reads_each_renders_own_sim_and_settings() {
    let raw = metadata_fixtures::build_raf_with_firmware(
        &render_jpeg(
            None,
            &[
                (0x1401, FujiValue::Short(0x0700)),
                (0x1003, FujiValue::Short(0x0100)),
            ],
        ),
        b"0100",
    );
    let provia = render_jpeg(
        Some("Digital Camera X-T4 Ver1.10"),
        &[(0x1401, FujiValue::Short(0x0000))],
    );
    let velvia = render_jpeg(
        Some("Digital Camera X-T4 Ver1.10"),
        &[(0x1401, FujiValue::Short(0x0200))],
    );
    let acros = render_jpeg(
        Some("Digital Camera X-T4 Ver1.10"),
        &[(0x1003, FujiValue::Short(0x0500))],
    );
    let punchy = render_jpeg(
        Some("Digital Camera X-T4 Ver1.10"),
        &[
            (0x1401, FujiValue::Short(0x0000)),
            (0x1003, FujiValue::Short(0x0080)),
        ],
    );
    let plan = inspect_metadata(&[
        MetadataFile::new("DSCF9000.RAF".to_owned(), &raw),
        MetadataFile::new("DSCF9000.JPG".to_owned(), &provia),
        MetadataFile::new("RENDER-VELVIA.JPG".to_owned(), &velvia),
        MetadataFile::new("RENDER-ACROS.JPG".to_owned(), &acros),
        MetadataFile::new("RENDER-PUNCHY.JPG".to_owned(), &punchy),
    ])
    .expect("pairing plan");
    assert_eq!(plan.groups.len(), 1);
    assert_eq!(plan.groups[0].raw, "DSCF9000.RAF");
    let mut jpegs = plan.groups[0].jpegs.clone();
    jpegs.sort();
    assert_eq!(
        jpegs,
        vec![
            "DSCF9000.JPG".to_owned(),
            "RENDER-ACROS.JPG".to_owned(),
            "RENDER-PUNCHY.JPG".to_owned(),
            "RENDER-VELVIA.JPG".to_owned(),
        ]
    );

    let schema = include_str!("../../../data/bodies/fujifilm-x-t4.yaml");
    let raw_meta = looklab_meta::parse(&raw).expect("raw metadata");
    let provia_meta = looklab_meta::parse(&provia).expect("provia metadata");
    let velvia_meta = looklab_meta::parse(&velvia).expect("velvia metadata");
    let acros_meta = looklab_meta::parse(&acros).expect("acros metadata");
    let punchy_meta = looklab_meta::parse(&punchy).expect("punchy metadata");

    let provia_prefill = prefill_for(
        schema,
        ("DSCF9000.RAF", &raw_meta),
        ("DSCF9000.JPG", &provia_meta),
    );
    assert_eq!(provia_prefill.sim.as_deref(), Some("provia"));
    assert_eq!(provia_prefill.firmware, "1.10");
    assert_eq!(control(&provia_prefill, "color"), None);
    assert!(provia_prefill.monochrome.is_empty());

    let velvia_prefill = prefill_for(
        schema,
        ("DSCF9000.RAF", &raw_meta),
        ("RENDER-VELVIA.JPG", &velvia_meta),
    );
    assert_eq!(velvia_prefill.sim.as_deref(), Some("velvia"));

    let punchy_prefill = prefill_for(
        schema,
        ("DSCF9000.RAF", &raw_meta),
        ("RENDER-PUNCHY.JPG", &punchy_meta),
    );
    assert_eq!(punchy_prefill.sim.as_deref(), Some("provia"));
    assert_eq!(control(&punchy_prefill, "color"), Some(1));

    let acros_prefill = prefill_for(
        schema,
        ("DSCF9000.RAF", &raw_meta),
        ("RENDER-ACROS.JPG", &acros_meta),
    );
    assert_eq!(acros_prefill.sim.as_deref(), Some("acros"));
    assert_eq!(
        acros_prefill.monochrome,
        vec![looklab_wasm::MonochromeRender {
            file: "RENDER-ACROS.JPG".to_owned(),
            sim: "acros".to_owned(),
        }]
    );
}

#[test]
fn a_same_name_jpeg_from_another_shot_is_not_a_render() {
    let raw = metadata_fixtures::build_raf_with_firmware(
        &render_jpeg(None, &[(0x1401, FujiValue::Short(0x0000))]),
        b"0100",
    );
    let other_shot = render_jpeg_at(
        "2024:05:17 10:12:00",
        Some("Digital Camera X-T4 Ver1.10"),
        &[(0x1401, FujiValue::Short(0x0200))],
    );
    let plan = inspect_metadata(&[
        MetadataFile::new("DSCF9001.RAF".to_owned(), &raw),
        MetadataFile::new("DSCF9001.JPG".to_owned(), &other_shot),
    ])
    .expect("pairing plan");

    assert!(plan.groups.is_empty());
    assert_eq!(
        plan.unpaired,
        vec![
            UnpairedEntry {
                name: "DSCF9001.JPG".to_owned(),
                reason: UnpairedReasonView::SeparateShot,
            },
            UnpairedEntry {
                name: "DSCF9001.RAF".to_owned(),
                reason: UnpairedReasonView::NoCounterpart,
            },
        ]
    );
}
