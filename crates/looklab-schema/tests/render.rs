// SPDX-License-Identifier: AGPL-3.0-or-later
//! Render classification and family-scoped control applicability.

use looklab_meta::FujiTag;
use looklab_schema::{
    ControlKey, LookId, MakernoteValues, MappingError, MonoFilter, RenderClass,
    classify_card_render, classify_render, settings_from_makernote_for, validate_control_schema,
};

const SCHEMA: &str = r#"
body: test/body
epochs:
  - id: 1
    firmware: { from: "1.00", to: "1.99" }
    native_sims:
      - key: provia
        look: provia
        makernote: { tag: FilmMode, value: 0x0000 }
      - key: acros
        look: acros
        monochrome: true
        makernote: { tag: Saturation, value: 0x0500 }
      - key: acros_r
        look: acros
        monochrome: true
        makernote: { tag: Saturation, value: 0x0501 }
      - key: monochrome_ye
        look: monochrome
        monochrome: true
        makernote: { tag: Saturation, value: 0x0302 }
      - key: sepia
        look: sepia
        monochrome: true
        makernote: { tag: Saturation, value: 0x0310 }
    monochrome:
      families: [acros, monochrome, sepia]
      filter: [acros, monochrome]
      toning: [acros, monochrome]
    exposure_modes: []
    controls:
      - key: color
        type: int
        range: [-4, 4]
        step: 1
        default: 0
        look_affecting: true
        affects: [chroma]
        applies_to: [provia]
        makernote: { tag: Saturation, values: { 0: 0 } }
      - key: color_chrome_effect
        type: enum
        values: [off, weak, strong]
        default: off
        look_affecting: true
        affects: [chroma]
        applies_to: [provia]
        makernote: { tag: ColorChromeEffect, values: { off: 0, weak: 32, strong: 64 } }
      - key: mono_filter
        type: enum
        values: [none, yellow, red, green]
        default: none
        look_affecting: true
        affects: [tint]
        applies_to: [acros, monochrome]
        makernote:
          tag: Saturation
          values:
            none: [0x0500, 0x0300]
            yellow: [0x0502, 0x0302]
            red: [0x0501, 0x0301]
            green: [0x0503, 0x0303]
      - key: bw_adjustment
        type: int
        range: [-18, 18]
        step: 1
        default: 0
        look_affecting: true
        affects: [tint]
        applies_to: [acros, monochrome]
        makernote: { tag: BWAdjustment, encoding: { scale: 1 } }
      - key: bw_magenta_green
        type: int
        range: [-18, 18]
        step: 1
        default: 0
        look_affecting: true
        affects: [tint]
        applies_to: [acros, monochrome]
        makernote: { tag: BWMagentaGreen, encoding: { scale: 1 } }
      - key: grain
        type: enum
        values: [off, weak, strong]
        default: off
        look_affecting: false
        spatial: true
        tolerated: [off]
        makernote: { tag: GrainEffectRoughness, values: { off: 0, weak: 16, strong: 64 } }
    solve_plan: []
"#;

fn epoch() -> looklab_schema::Epoch {
    validate_control_schema(SCHEMA)
        .expect("schema validates")
        .epochs
        .remove(0)
}

fn values(pairs: &[(FujiTag, i64)]) -> MakernoteValues {
    let mut values = MakernoteValues::new();
    for (tag, raw) in pairs {
        values.insert(*tag, vec![*raw]);
    }
    values
}

fn keys(controls: &[ControlKey]) -> Vec<&str> {
    controls.iter().map(ControlKey::as_str).collect()
}

#[test]
fn colour_renders_keep_their_family_and_no_filter() {
    let classification =
        classify_render(&epoch(), &values(&[(FujiTag::FilmMode, 0x0000)])).expect("colour render");
    assert_eq!(classification.render_class, RenderClass::Colour);
    assert_eq!(classification.native_sim.as_str(), "provia");
    assert_eq!(classification.family.as_str(), "provia");
    assert_eq!(classification.mono_filter, None);
}

#[test]
fn acros_filter_variants_normalize_to_family_and_filter() {
    let epoch = epoch();
    let plain = classify_render(&epoch, &values(&[(FujiTag::Saturation, 0x0500)])).expect("acros");
    assert_eq!(plain.render_class, RenderClass::Monochrome);
    assert_eq!(plain.native_sim.as_str(), "acros");
    assert_eq!(plain.family.as_str(), "acros");
    assert_eq!(plain.mono_filter, Some(MonoFilter::None));

    let red = classify_render(&epoch, &values(&[(FujiTag::Saturation, 0x0501)])).expect("acros_r");
    assert_eq!(red.native_sim.as_str(), "acros_r");
    assert_eq!(red.family.as_str(), "acros");
    assert_eq!(red.mono_filter, Some(MonoFilter::Red));

    let yellow =
        classify_render(&epoch, &values(&[(FujiTag::Saturation, 0x0302)])).expect("monochrome_ye");
    assert_eq!(yellow.native_sim.as_str(), "monochrome_ye");
    assert_eq!(yellow.family.as_str(), "monochrome");
    assert_eq!(yellow.mono_filter, Some(MonoFilter::Yellow));
}

#[test]
fn sepia_is_monochrome_without_a_filter_value() {
    let classification =
        classify_render(&epoch(), &values(&[(FujiTag::Saturation, 0x0310)])).expect("sepia");
    assert_eq!(classification.render_class, RenderClass::Monochrome);
    assert_eq!(classification.native_sim.as_str(), "sepia");
    assert_eq!(classification.family.as_str(), "sepia");
    assert_eq!(classification.mono_filter, Some(MonoFilter::None));
}

#[test]
fn a_spoofed_colour_sim_does_not_hide_monochrome_metadata() {
    let classification = classify_render(
        &epoch(),
        &values(&[(FujiTag::FilmMode, 0x0000), (FujiTag::Saturation, 0x0501)]),
    )
    .expect("the monochrome match wins");
    assert_eq!(classification.render_class, RenderClass::Monochrome);
    assert_eq!(classification.native_sim.as_str(), "acros_r");
    assert_eq!(classification.family.as_str(), "acros");
    assert_eq!(classification.mono_filter, Some(MonoFilter::Red));
}

#[test]
fn no_matching_sim_is_undetermined() {
    assert_eq!(
        classify_render(&epoch(), &MakernoteValues::new()),
        Err(MappingError::UnknownSim)
    );
}

#[test]
fn applicable_controls_follow_the_render_family() {
    let epoch = epoch();
    let provia = epoch.applicable_controls(&LookId::new("provia").expect("family"));
    assert_eq!(keys(&provia.mapped), vec!["color", "color_chrome_effect"]);
    assert!(provia.unmapped.is_empty());
    assert_eq!(keys(&provia.spatial), vec!["grain"]);

    let acros = epoch.applicable_controls(&LookId::new("acros").expect("family"));
    assert_eq!(
        keys(&acros.mapped),
        vec!["mono_filter", "bw_adjustment", "bw_magenta_green"]
    );
    assert!(acros.unmapped.is_empty());
    assert_eq!(keys(&acros.spatial), vec!["grain"]);

    let sepia = epoch.applicable_controls(&LookId::new("sepia").expect("family"));
    assert!(sepia.mapped.is_empty());
    assert!(sepia.unmapped.is_empty());
    assert_eq!(keys(&sepia.spatial), vec!["grain"]);
}

#[test]
fn monochrome_settings_carry_filter_toning_and_grain() {
    let (settings, problems) = settings_from_makernote_for(
        &validate_control_schema(SCHEMA).expect("schema"),
        1,
        &LookId::new("acros").expect("family"),
        &values(&[
            (FujiTag::Saturation, 0x0501),
            (FujiTag::BwAdjustment, 7),
            (FujiTag::BwMagentaGreen, -3),
            (FujiTag::GrainEffectRoughness, 16),
        ]),
    );
    assert!(problems.is_empty(), "{problems:?}");
    let steps: Vec<(&str, i64)> = settings
        .entries()
        .iter()
        .map(|setting| (setting.key.as_str(), setting.step))
        .collect();
    assert_eq!(
        steps,
        vec![
            ("mono_filter", 2),
            ("bw_adjustment", 7),
            ("bw_magenta_green", -3),
            ("grain", 1),
        ]
    );
}

#[test]
fn missing_applicable_settings_are_not_defaulted() {
    let schema = validate_control_schema(SCHEMA).expect("schema");
    let (settings, problems) = settings_from_makernote_for(
        &schema,
        1,
        &LookId::new("acros").expect("family"),
        &values(&[(FujiTag::Saturation, 0x0501)]),
    );
    assert_eq!(
        settings
            .get(&ControlKey::new("mono_filter").expect("key"))
            .map(|setting| setting.step),
        Some(2)
    );
    assert!(
        settings
            .get(&ControlKey::new("bw_adjustment").expect("key"))
            .is_none()
    );
    assert!(
        settings
            .get(&ControlKey::new("bw_magenta_green").expect("key"))
            .is_none()
    );
    let missing: Vec<&str> = problems
        .iter()
        .filter_map(|problem| match problem {
            MappingError::MissingTag { control, .. } => Some(control.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(missing, vec!["bw_adjustment", "bw_magenta_green", "grain"]);
}

#[test]
fn colour_settings_do_not_require_monochrome_controls() {
    let schema = validate_control_schema(SCHEMA).expect("schema");
    let (settings, problems) = settings_from_makernote_for(
        &schema,
        1,
        &LookId::new("provia").expect("family"),
        &values(&[
            (FujiTag::FilmMode, 0x0000),
            (FujiTag::Saturation, 0),
            (FujiTag::ColorChromeEffect, 32),
            (FujiTag::GrainEffectRoughness, 0),
        ]),
    );
    assert!(problems.is_empty(), "{problems:?}");
    let steps: Vec<(&str, i64)> = settings
        .entries()
        .iter()
        .map(|setting| (setting.key.as_str(), setting.step))
        .collect();
    assert_eq!(
        steps,
        vec![("color", 0), ("color_chrome_effect", 1), ("grain", 0)]
    );
}

#[test]
fn applicability_rejects_an_undeclared_family() {
    let yaml = SCHEMA.replace("applies_to: [provia]", "applies_to: [missing-family]");
    let errors = validate_control_schema(&yaml).expect_err("unknown family");
    assert!(
        errors
            .iter()
            .any(|error| matches!(error.kind, looklab_schema::ErrorKind::UnknownFamily { .. }))
    );
}

#[test]
fn an_empty_applicability_list_is_rejected() {
    let yaml = SCHEMA.replace("applies_to: [provia]", "applies_to: []");
    let errors = validate_control_schema(&yaml).expect_err("empty scope");
    assert!(
        errors
            .iter()
            .any(|error| matches!(error.kind, looklab_schema::ErrorKind::InvalidValue { .. }))
    );
}

#[test]
fn all_is_the_default_scope() {
    let yaml = SCHEMA.replace("        applies_to: [provia]\n", "");
    let schema = validate_control_schema(&yaml).expect("schema");
    let control = schema.epochs[0]
        .controls
        .iter()
        .find(|control| control.key.as_str() == "color")
        .expect("color");
    assert!(
        control
            .scope
            .includes(&LookId::new("acros").expect("family"))
    );
}

#[test]
fn shipped_x_t4_classifies_and_scopes_monochrome_controls() {
    let schema = validate_control_schema(include_str!("../../../data/bodies/fujifilm-x-t4.yaml"))
        .expect("X-T4 schema validates");
    let epoch = &schema.epochs[0];

    let acros =
        classify_render(epoch, &values(&[(FujiTag::Saturation, 0x0501)])).expect("acros red");
    assert_eq!(acros.render_class, RenderClass::Monochrome);
    assert_eq!(acros.family.as_str(), "acros");
    assert_eq!(acros.mono_filter, Some(MonoFilter::Red));

    let acros_controls = epoch.applicable_controls(&acros.family);
    let acros_mapped = keys(&acros_controls.mapped);
    assert!(acros_mapped.contains(&"mono_filter"));
    assert!(acros_mapped.contains(&"bw_adjustment"));
    assert!(acros_mapped.contains(&"bw_magenta_green"));
    assert!(!acros_mapped.contains(&"color"));
    assert_eq!(
        keys(&acros_controls.spatial),
        vec!["grain", "grain_size", "clarity", "noise_reduction"]
    );

    let colour = classify_render(epoch, &values(&[(FujiTag::FilmMode, 0x0000)])).expect("provia");
    assert_eq!(colour.render_class, RenderClass::Colour);
    let colour_controls = epoch.applicable_controls(&colour.family);
    let colour_mapped = keys(&colour_controls.mapped);
    assert!(colour_mapped.contains(&"color"));
    assert!(!colour_mapped.contains(&"mono_filter"));
    assert!(!colour_mapped.contains(&"bw_adjustment"));
}

#[test]
fn a_monochrome_card_ignores_stock_colour_sim_metadata() {
    let classification = classify_card_render(
        &epoch(),
        &LookId::new("acros").expect("family"),
        RenderClass::Monochrome,
        &values(&[(FujiTag::FilmMode, 0x0000), (FujiTag::Saturation, 0x0501)]),
    )
    .expect("a card may borrow a colour stock sim");
    assert_eq!(classification.render_class, RenderClass::Monochrome);
    assert_eq!(classification.family.as_str(), "acros");
    assert_eq!(classification.mono_filter, Some(MonoFilter::Red));
}

#[test]
fn a_colour_card_does_not_take_a_filter_from_stock_metadata() {
    let classification = classify_card_render(
        &epoch(),
        &LookId::new("provia").expect("family"),
        RenderClass::Colour,
        &values(&[(FujiTag::Saturation, 0x0501)]),
    )
    .expect("colour card");
    assert_eq!(classification.render_class, RenderClass::Colour);
    assert_eq!(classification.mono_filter, None);
}

#[test]
fn a_card_class_that_contradicts_its_declared_family_is_rejected() {
    assert_eq!(
        classify_card_render(
            &epoch(),
            &LookId::new("provia").expect("family"),
            RenderClass::Monochrome,
            &MakernoteValues::new(),
        ),
        Err(MappingError::RenderClassConflict {
            family: LookId::new("provia").expect("family"),
            declared: RenderClass::Monochrome,
            resolved: RenderClass::Colour,
        })
    );
}

#[test]
fn a_look_family_mixes_no_render_classes() {
    let yaml = SCHEMA.replace(
        "      - key: provia\n        look: provia\n        makernote: { tag: FilmMode, value: 0x0000 }",
        "      - key: provia\n        look: provia\n        makernote: { tag: FilmMode, value: 0x0000 }\n      - key: provia_mono\n        look: provia\n        monochrome: true\n        makernote: { tag: FilmMode, value: 0x0001 }",
    );
    let errors = validate_control_schema(&yaml).expect_err("mixed family");
    assert!(errors.iter().any(|error| matches!(
        error.kind,
        looklab_schema::ErrorKind::FamilyRenderClassConflict { .. }
    )));
}

#[test]
fn unscoped_settings_infer_the_render_family() {
    let schema = validate_control_schema(include_str!("../../../data/bodies/fujifilm-x-t4.yaml"))
        .expect("X-T4 schema validates");
    let (mono, _) = looklab_schema::settings_from_makernote_partial(
        &schema,
        1,
        &values(&[(FujiTag::Saturation, 0x0501)]),
    );
    assert!(
        mono.get(&ControlKey::new("color").expect("key")).is_none(),
        "a monochrome render must not decode the colour control from Saturation"
    );
    assert_eq!(
        mono.get(&ControlKey::new("mono_filter").expect("key"))
            .map(|setting| setting.step),
        Some(2)
    );

    let (colour, _) = looklab_schema::settings_from_makernote_partial(
        &schema,
        1,
        &values(&[(FujiTag::FilmMode, 0x0000), (FujiTag::Saturation, 0)]),
    );
    assert!(
        colour
            .get(&ControlKey::new("mono_filter").expect("key"))
            .is_none()
    );
    assert_eq!(
        colour
            .get(&ControlKey::new("color").expect("key"))
            .map(|setting| setting.step),
        Some(0)
    );
}

#[test]
fn sepia_requires_no_filter_or_toning_controls() {
    let schema = validate_control_schema(SCHEMA).expect("schema");
    let (settings, problems) = settings_from_makernote_for(
        &schema,
        1,
        &LookId::new("sepia").expect("family"),
        &values(&[(FujiTag::Saturation, 0x0310)]),
    );
    assert!(
        settings
            .get(&ControlKey::new("mono_filter").expect("key"))
            .is_none()
    );
    assert!(
        settings
            .get(&ControlKey::new("bw_adjustment").expect("key"))
            .is_none()
    );
    assert!(
        settings
            .get(&ControlKey::new("bw_magenta_green").expect("key"))
            .is_none()
    );
    assert!(!problems.iter().any(|problem| matches!(
        problem,
        MappingError::MissingTag { control, .. }
            if control == "mono_filter"
                || control == "bw_adjustment"
                || control == "bw_magenta_green"
    )));
}

#[test]
fn a_monochrome_epoch_without_capability_gates_intake() {
    let yaml = SCHEMA.replace(
        "    monochrome:\n      families: [acros, monochrome, sepia]\n      filter: [acros, monochrome]\n      toning: [acros, monochrome]\n",
        "",
    );
    let schema = validate_control_schema(&yaml).expect("capability is optional");
    let epoch = &schema.epochs[0];
    assert!(epoch.monochrome_capability().is_none());
    assert!(!epoch.supports_monochrome_intake(&LookId::new("acros").expect("family")));
}

#[test]
fn capability_scope_must_match_the_controls() {
    let yaml = SCHEMA.replace(
        "        applies_to: [acros, monochrome]\n        makernote: { tag: BWAdjustment",
        "        applies_to: [acros]\n        makernote: { tag: BWAdjustment",
    );
    let errors = validate_control_schema(&yaml).expect_err("scope mismatch");
    assert!(errors.iter().any(|error| matches!(
        error.kind,
        looklab_schema::ErrorKind::MonochromeControlMismatch { .. }
    )));
}

#[test]
fn grain_size_capability_requires_the_control() {
    let yaml = SCHEMA.replace(
        "      toning: [acros, monochrome]\n",
        "      toning: [acros, monochrome]\n      grain_size: true\n",
    );
    let errors = validate_control_schema(&yaml).expect_err("grain_size control required");
    assert!(errors.iter().any(|error| matches!(
        error.kind,
        looklab_schema::ErrorKind::MonochromeGrainSizeMissing
    )));
}

#[test]
fn a_grain_size_control_requires_the_capability() {
    let yaml = SCHEMA.replace(
        "      - key: grain\n",
        "      - key: grain_size\n        type: enum\n        values: [off, small, large]\n        default: off\n        look_affecting: false\n        spatial: true\n        tolerated: [off]\n        makernote: { tag: GrainEffectSize, values: { off: 0, small: 16, large: 32 } }\n      - key: grain\n",
    );
    let errors = validate_control_schema(&yaml).expect_err("grain_size capability required");
    assert!(errors.iter().any(|error| matches!(
        error.kind,
        looklab_schema::ErrorKind::MonochromeGrainSizeNotDeclared
    )));
}

#[test]
fn capability_gates_intake_to_declared_families() {
    let epoch = epoch();
    assert!(epoch.supports_monochrome_intake(&LookId::new("acros").expect("family")));
    assert!(epoch.supports_monochrome_intake(&LookId::new("sepia").expect("family")));
    assert!(!epoch.supports_monochrome_intake(&LookId::new("provia").expect("family")));
    assert!(!epoch.supports_monochrome_intake(&LookId::new("velvia").expect("family")));
}

#[test]
fn shipped_bodies_declare_their_monochrome_capability() {
    let cases = [
        (
            include_str!("../../../data/bodies/fujifilm-x-t4.yaml"),
            true,
            true,
        ),
        (
            include_str!("../../../data/bodies/fujifilm-x-t5.yaml"),
            true,
            true,
        ),
        (
            include_str!("../../../data/bodies/fujifilm-x-s10.yaml"),
            true,
            true,
        ),
        (
            include_str!("../../../data/bodies/fujifilm-x-e3.yaml"),
            false,
            false,
        ),
    ];
    for (yaml, toning, grain_size) in cases {
        let schema = validate_control_schema(yaml).expect("body schema validates");
        let epoch = &schema.epochs[0];
        let capability = epoch.monochrome_capability().expect("capability");
        for family in ["acros", "monochrome", "sepia"] {
            assert!(
                capability.supports(&LookId::new(family).expect("family")),
                "{family} is not admitted"
            );
        }
        assert_eq!(
            capability
                .filter
                .iter()
                .map(LookId::as_str)
                .collect::<Vec<_>>(),
            vec!["acros", "monochrome"]
        );
        assert_eq!(!capability.toning.is_empty(), toning);
        assert_eq!(capability.grain_size, grain_size);
        if toning {
            for key in ["bw_adjustment", "bw_magenta_green"] {
                let control = epoch
                    .controls
                    .iter()
                    .find(|control| control.key.as_str() == key)
                    .expect("toning control");
                assert_eq!(control.steps(), -18..=18);
            }
        }
        let size = epoch
            .controls
            .iter()
            .find(|control| control.key.as_str() == "grain_size");
        assert_eq!(size.is_some(), grain_size);
        if let Some(size) = size {
            assert!(size.spatial);
            assert!(size.makernote.is_some());
        }
    }
}

#[test]
fn shipped_x_e3_has_filters_without_toning() {
    let schema = validate_control_schema(include_str!("../../../data/bodies/fujifilm-x-e3.yaml"))
        .expect("X-E3 schema validates");
    let epoch = &schema.epochs[0];
    let acros = epoch.applicable_controls(&LookId::new("acros").expect("family"));
    let mapped = keys(&acros.mapped);
    assert!(mapped.contains(&"mono_filter"));
    assert!(!mapped.contains(&"bw_adjustment"));
    assert!(!mapped.contains(&"bw_magenta_green"));

    let sepia = epoch.applicable_controls(&LookId::new("sepia").expect("family"));
    let mapped = keys(&sepia.mapped);
    assert!(!mapped.contains(&"mono_filter"));
    assert!(!mapped.contains(&"bw_adjustment"));
    assert!(!mapped.contains(&"bw_magenta_green"));
    assert!(
        !epoch
            .controls
            .iter()
            .any(|control| control.key.as_str() == "grain_size")
    );
}

#[test]
fn a_family_the_epoch_lacks_gets_only_unscoped_controls() {
    let controls = epoch().applicable_controls(&LookId::new("velvia").expect("family"));
    assert!(controls.mapped.is_empty());
    assert!(controls.unmapped.is_empty());
    assert_eq!(keys(&controls.spatial), vec!["grain"]);
}
