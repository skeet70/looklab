// SPDX-License-Identifier: AGPL-3.0-or-later
mod support;

use looklab_meta::{parse, parse_tiff, sanitize};
use proptest::prelude::*;

fn value_strategy() -> impl Strategy<Value = support::Value> {
    prop_oneof![
        "[ -~]{0,16}".prop_map(support::Value::Ascii),
        any::<u16>().prop_map(support::Value::Short),
        prop::collection::vec(any::<u16>(), 0..4).prop_map(support::Value::Shorts),
        any::<u32>().prop_map(support::Value::Long),
        (any::<u32>(), 1u32..=1000).prop_map(|(numerator, denominator)| {
            support::Value::Rational(numerator, denominator)
        }),
        any::<i32>().prop_map(support::Value::SLong),
        (any::<i32>(), prop_oneof![-1000i32..=-1, 1i32..=1000])
            .prop_map(|(numerator, denominator)| support::Value::SRational(numerator, denominator)),
        prop::collection::vec(any::<u8>(), 0..24).prop_map(support::Value::Undefined),
    ]
}

fn tag_strategy() -> impl Strategy<Value = u16> {
    prop_oneof![
        Just(0x010f),
        Just(0x0110),
        Just(0x0131),
        Just(0x013b),
        Just(0x8298),
        Just(0x8825),
        Just(0x927c),
        Just(0x9291),
        Just(0xa420),
        Just(0xa431),
        Just(0xa434),
        Just(0xa435),
        any::<u16>(),
    ]
}

fn entries_strategy() -> impl Strategy<Value = Vec<(u16, support::Value)>> {
    prop::collection::vec((tag_strategy(), value_strategy()), 0..10)
}

fn fuji_value_strategy() -> impl Strategy<Value = support::FujiValue> {
    prop_oneof![
        "[ -~]{0,16}".prop_map(support::FujiValue::Ascii),
        any::<u16>().prop_map(support::FujiValue::Short),
        any::<i32>().prop_map(support::FujiValue::SLong),
        (any::<i32>(), any::<i32>())
            .prop_map(|(red, blue)| support::FujiValue::SLongPair(red, blue)),
        prop::collection::vec(any::<u8>(), 0..16).prop_map(support::FujiValue::Undefined),
    ]
}

fn fuji_entries_strategy() -> impl Strategy<Value = Vec<(u16, support::FujiValue)>> {
    prop::collection::vec(
        (
            prop_oneof![
                Just(0x0000),
                Just(0x0010),
                Just(0x1001),
                Just(0x1003),
                Just(0x100a),
                Just(0x100e),
                Just(0x1040),
                Just(0x1041),
                Just(0x1048),
                Just(0x1401),
                Just(0x1438),
                any::<u16>(),
            ],
            fuji_value_strategy(),
        ),
        0..10,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn sanitized_json_uses_only_whitelisted_keys(
        ifd0 in entries_strategy(),
        mut exif in entries_strategy(),
        fuji in prop::option::of(fuji_entries_strategy()),
    ) {
        exif.retain(|(tag, _)| *tag != 0x927c);
        if let Some(entries) = fuji {
            exif.push((0x927c, support::Value::Undefined(support::build_makernote(&entries))));
        }
        let tiff = support::build_tiff(&ifd0, &exif, None);
        if let Ok(parsed) = parse_tiff(&tiff) {
            let (sanitized, removed) = sanitize(&parsed);
            let json = serde_json::to_string(&sanitized).expect("sanitized metadata is JSON");
            support::assert_whitelisted_keys(&json);
            let removed_json = serde_json::to_string(&removed).expect("removed fields are JSON");
            prop_assert!(!removed_json.contains("SERIAL123"));
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
        let _ = parse(&bytes);
    }
}
