// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end runs of the `looklab` binary.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use looklab_color::trc::srgb_encode;
use looklab_export::{
    Artifact, ArtifactAtCct, BakeIlluminant, BakeOutput, Baker, CctK, DownstreamModel,
    InsertionSpec, Interpolation, Lut3d, Precision, read_cube, read_cube_1d, validate,
};
use looklab_model::{Look, LookDocument};
use looklab_raw::{DecodedRaw, RawDecoder, RawlerDecoder};
use serde_json::Value;

fn looklab() -> Command {
    Command::cargo_bin("looklab").expect("built binary")
}

fn ok(mut command: Command) -> (String, String) {
    let output = command.output().expect("run");
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        output.status.success(),
        "command failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    (stdout, stderr)
}

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn synth(dir: &Path, seed: &str, images: &str, samples: &str) {
    let (stdout, _) = ok({
        let mut command = looklab();
        command.args(["synth", "--out"]).arg(dir).args([
            "--seed",
            seed,
            "--images",
            images,
            "--samples",
            samples,
        ]);
        command
    });
    assert!(stdout.contains("synth"), "{stdout}");
}

fn fit(dir: &Path, slug: &str, out: &Path) {
    let (stdout, _) = ok({
        let mut command = looklab();
        command
            .args(["fit", "--samples"])
            .arg(dir)
            .args(["--transform", "from-metadata", "--look-slug", slug, "--out"])
            .arg(out);
        command
    });
    assert!(stdout.contains("rms ΔE"), "no per-image verdicts: {stdout}");
    assert!(out.exists());
}

#[test]
fn synthetic_flow_fits_reports_compares_and_attempts_export() {
    let temp = tempfile::tempdir().expect("tempdir");
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    synth(&a, "11", "6", "400");
    synth(&b, "22", "6", "400");

    let look_a = temp.path().join("a.looklab");
    let look_b = temp.path().join("b.looklab");
    fit(&a, "look-a", &look_a);
    fit(&b, "look-b", &look_b);

    let report = temp.path().join("report.html");
    ok({
        let mut command = looklab();
        command
            .args(["report"])
            .arg(&look_a)
            .arg("--out")
            .arg(&report);
        command
    });
    let html = std::fs::read_to_string(&report).expect("report");
    assert!(html.starts_with("<!DOCTYPE html>"));
    assert!(html.contains("look-a"));

    let comparison = temp.path().join("compare.html");
    let (summary, _) = ok({
        let mut command = looklab();
        command
            .args(["compare"])
            .arg(&look_a)
            .arg(&look_b)
            .args(["--out"])
            .arg(&comparison)
            .arg("--json");
        command
    });
    assert!(!summary.trim().is_empty());
    assert!(summary.trim().lines().count() == 1, "{summary}");
    assert!(
        std::fs::read_to_string(&comparison)
            .expect("comparison")
            .contains("look-b")
    );
    let json: Value = serde_json::from_str(
        &std::fs::read_to_string(temp.path().join("compare.json")).expect("json"),
    )
    .expect("comparison json");
    assert!(json["summary"].is_string());

    let profile = workspace().join("data/interfaces/example-app-lut3d-v1.yaml");
    let cube = temp.path().join("look-a.cube");
    let export = |profile: &Path, out: &Path, force: bool| {
        let mut command = looklab();
        command
            .args(["export"])
            .arg(&look_a)
            .arg("--profile")
            .arg(profile)
            .arg("--body-transform")
            .arg(a.join("transform.yaml"))
            .args(["--format", "cube", "--out"])
            .arg(out);
        if force {
            command.arg("--force");
        }
        command.output().expect("run export")
    };

    let document = LookDocument::decode(&std::fs::read(&look_a).expect("look")).expect("decode");
    if document.match_refusal().is_some() {
        for force in [false, true] {
            let output = export(&profile, &cube, force);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success(),
                "a non-matchable Look exported with force={force}\n{stderr}"
            );
            assert!(
                !cube.exists(),
                "a non-matchable Look wrote {}\n{stderr}",
                cube.display()
            );
        }
    } else {
        let profile_text = std::fs::read_to_string(&profile).expect("profile");
        let profile_schema =
            looklab_schema::validate_interface_profile(&profile_text).expect("profile");
        let transform =
            looklab_cli::TransformFile::load(&a.join("transform.yaml")).expect("transform");
        let body = transform.input_transform().expect("input transform");
        let probes = looklab_model::probes::v1();
        let support: Option<Vec<bool>> = document
            .uncertainty
            .support
            .as_ref()
            .filter(|mask| mask.values.len() == probes.probes.len())
            .map(|mask| mask.values.iter().map(|value| *value > 0.5).collect());
        let baked = Baker::new(&profile_schema, &document.look, &body)
            .bake(BakeIlluminant::Single { cct: CctK::D65 })
            .expect("bake");
        let report = validate(&baked, &document.look, &body, &probes, support.as_deref())
            .expect("validation");

        let first = export(&profile, &cube, false);
        let first_stderr = String::from_utf8_lossy(&first.stderr);
        if report.flagged() {
            assert!(
                !first.status.success(),
                "a flagged export wrote {} anyway\n{first_stderr}",
                cube.display()
            );
            assert!(
                !cube.exists(),
                "a flagged export wrote {}\n{first_stderr}",
                cube.display()
            );
            let forced = export(&profile, &cube, true);
            assert!(
                forced.status.success(),
                "forced export failed: {}",
                String::from_utf8_lossy(&forced.stderr)
            );
        } else {
            assert!(
                first.status.success(),
                "an unflagged export failed: {first_stderr}"
            );
        }
        let text = std::fs::read_to_string(&cube).expect("cube");
        assert!(text.contains("LUT_3D_SIZE 33"), "{text}");
        assert!(text.contains("TITLE"));
    }

    ok({
        let mut command = looklab();
        command.arg("validate").arg(workspace().join("data"));
        command
    });
}

/// A colourimetric tone with a 6% lift, a warm shift, 4% chroma gain and a 1.5°
/// hue rotation.
fn analytic_look() -> Look {
    let mut look = Look::colorimetric_spline().expect("tone spline");
    if let looklab_model::Tone::Spline { c0, beta } = &mut look.tone {
        *c0 *= 1.06;
        for coefficient in beta.iter_mut() {
            *coefficient *= 1.03;
        }
    }
    look.tint.a.fill(1.0);
    look.tint.b.fill(-0.6);
    look.color.g.plane.fill(0.04);
    look.color.g.volume.fill(0.01);
    look.color.r.plane.fill(1.5);
    look.color.lambda.plane.fill(0.3);
    look
}

/// The X-S10 calibration matrices, as the body transform the analytic Look bakes through.
const ANALYTIC_TRANSFORM: &str = "\
format: looklab-transform/1
version: 1
body: synthetic/looklab
color_matrices:
  - cct: 2850
    xyz_to_camera: [[1.6064, -0.9942, 0.112], [-0.376, 1.2096, 0.1853], [-0.0007, 0.0545, 0.6654]]
  - cct: 6500
    xyz_to_camera: [[1.3426, -0.6334, -0.1177], [-0.4244, 1.2136, 0.2371], [-0.058, 0.1303, 0.598]]
";

/// A body-agnostic linear insertion that accepts one 3D LUT side.
fn analytic_profile(side: u32) -> String {
    format!(
        r#"id: test/analytic-lut3d
name: "Analytic 3D LUT module"
version: 1
insertion:
  point: camera_rgb_wb_linear
  encoding:
    transfer: linear
    primaries: camera_native
    range: [0.0, 8.0]
  illuminant_handling: single
accepts:
  - type: lut3d
    sizes: [{side}]
    interpolation: tetrahedral
    precision: f16
    with_shaper: true
downstream:
  - output_transform: {{ primaries: srgb, transfer: srgb }}
  - gamut_clip: hard
formats: [cube]
limits: {{ max_bytes: 16777216 }}
"#
    )
}

fn write_look(path: &Path, slug: &str, look: &Look) {
    let identity = looklab_model::file::identity(slug, "1.0.0").expect("identity");
    let document = LookDocument::from_look(identity, look.clone()).expect("document");
    std::fs::write(path, document.encode().expect("encode")).expect("write look");
}

#[test]
fn analytic_look_exports_cleanly_and_refuses_a_coarse_grid() {
    let temp = tempfile::tempdir().expect("tempdir");
    let look = analytic_look();
    let look_path = temp.path().join("analytic.looklab");
    write_look(&look_path, "analytic-tone", &look);
    let transform_path = temp.path().join("transform.yaml");
    std::fs::write(&transform_path, ANALYTIC_TRANSFORM).expect("transform");

    let export = |profile: &Path, out: &Path, force: bool| {
        let mut command = looklab();
        command
            .args(["export"])
            .arg(&look_path)
            .arg("--profile")
            .arg(profile)
            .arg("--body-transform")
            .arg(&transform_path)
            .args(["--format", "cube", "--out"])
            .arg(out);
        if force {
            command.arg("--force");
        }
        command.output().expect("run export")
    };

    let profile_text = analytic_profile(65);
    let profile_path = temp.path().join("analytic-65.yaml");
    std::fs::write(&profile_path, &profile_text).expect("profile");
    let cube_path = temp.path().join("analytic.cube");
    let output = export(&profile_path, &cube_path, false);
    assert!(
        output.status.success(),
        "clean export failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(cube_path.exists(), "an unflagged export wrote no cube");

    let grid_text = std::fs::read_to_string(&cube_path).expect("cube");
    let grid = read_cube(&grid_text).expect("grid cube");
    assert_eq!(grid.lut.size, 65);
    assert_eq!(grid.domain, [0.0, 1.0]);
    let shaper_text =
        std::fs::read_to_string(temp.path().join("analytic-shaper.cube")).expect("shaper cube");
    let shaper = read_cube_1d(&shaper_text).expect("shaper");
    assert_eq!(shaper.domain, [0.0, 8.0]);
    let serialized = Lut3d {
        size: grid.lut.size,
        data: grid.lut.data,
        shaper: Some(shaper.shaper),
    };

    let profile = looklab_schema::validate_interface_profile(&profile_text).expect("profile");
    let transform = looklab_cli::TransformFile::parse(ANALYTIC_TRANSFORM).expect("transform");
    let body = transform.input_transform().expect("input transform");
    let document = LookDocument::decode(&std::fs::read(&look_path).expect("look")).expect("decode");
    assert_eq!(document.look, look, "the document round-trips the Look");
    let artifact = BakeOutput {
        profile_id: profile.id.clone(),
        profile_version: profile.version,
        bake: BakeIlluminant::Single { cct: CctK::D65 },
        insertion: InsertionSpec::from_profile(&profile, None).expect("insertion"),
        downstream: DownstreamModel::from_profile(&profile).expect("downstream"),
        interpolation: Some(Interpolation::Tetrahedral),
        precision: Some(Precision::F16),
        artifacts: vec![ArtifactAtCct {
            cct: CctK::D65,
            artifact: Artifact::Lut3d(serialized),
        }],
    };
    let probes = looklab_model::probes::v1();
    let report = validate(&artifact, &document.look, &body, &probes, None).expect("validation");
    assert!(
        report.primary.stats.p95 <= 1.0,
        "serialized artifact p95 {} over {} probes",
        report.primary.stats.p95,
        report.primary.stats.count
    );
    assert!(
        report.primary.unsupported < probes.probes.len(),
        "{} of {} probes unsupported",
        report.primary.unsupported,
        probes.probes.len()
    );
    let eligible = probes.probes.len() - report.primary.unsupported;
    assert!(
        report.primary.stats.count * 10 >= eligible * 9,
        "evaluated {} of {eligible} supported probes",
        report.primary.stats.count
    );
    assert!(report.primary.clipped > 0, "no clipped targets counted");

    let coarse_text = analytic_profile(17);
    let coarse_path = temp.path().join("analytic-17.yaml");
    std::fs::write(&coarse_path, &coarse_text).expect("profile");
    let coarse_profile =
        looklab_schema::validate_interface_profile(&coarse_text).expect("coarse profile");
    let coarse_baked = Baker::new(&coarse_profile, &look, &body)
        .bake(BakeIlluminant::Single { cct: CctK::D65 })
        .expect("coarse bake");
    let coarse_report =
        validate(&coarse_baked, &look, &body, &probes, None).expect("coarse validation");
    assert!(
        coarse_report.flagged(),
        "17^3 p95 {} is not flagged",
        coarse_report.primary.stats.p95
    );

    let coarse_cube = temp.path().join("analytic-coarse.cube");
    let refused = export(&coarse_path, &coarse_cube, false);
    assert!(
        !refused.status.success(),
        "a flagged export wrote {} anyway: {}",
        coarse_cube.display(),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        !coarse_cube.exists(),
        "a flagged export wrote {}",
        coarse_cube.display()
    );

    let forced = export(&coarse_path, &coarse_cube, true);
    assert!(
        forced.status.success(),
        "forced export failed: {}",
        String::from_utf8_lossy(&forced.stderr)
    );
    let text = std::fs::read_to_string(&coarse_cube).expect("cube");
    assert!(text.contains("LUT_3D_SIZE 17"), "{text}");
}

#[test]
fn prep_reproduces_blobs_from_the_seed_file() {
    let workspace = workspace();
    let fixture = workspace.join("fixtures/cache/fujifilm-x-s10.raf");
    let raw = std::fs::read(&fixture).unwrap_or_else(|error| {
        panic!(
            "read {}: {error}; run tools/fetch-fixtures.sh",
            fixture.display()
        )
    });
    let decoded = RawlerDecoder.decode(&raw).expect("decode RAF");
    let jpeg = render_jpeg(&decoded);

    let temp = tempfile::tempdir().expect("tempdir");
    let jpeg_path = temp.path().join("x-s10.jpg");
    std::fs::write(&jpeg_path, &jpeg).expect("write jpeg");
    let seeds = temp.path().join("seeds.json");

    let help = String::from_utf8(
        looklab()
            .args(["prep", "--help"])
            .output()
            .expect("help")
            .stdout,
    )
    .expect("utf-8");
    assert!(
        help.contains("SENSITIVE"),
        "seed help is not labelled: {help}"
    );

    let first = temp.path().join("first");
    let (_, stderr) = ok({
        let mut command = looklab();
        command
            .args(["prep", "--raw"])
            .arg(&fixture)
            .arg("--jpeg")
            .arg(&jpeg_path)
            .args(["--body-prior", "from-raw", "--out"])
            .arg(&first)
            .arg("--seed-file")
            .arg(&seeds);
        command
    });
    assert!(stderr.contains("SENSITIVE"), "{stderr}");

    let second = temp.path().join("second");
    ok({
        let mut command = looklab();
        command
            .args(["prep", "--raw"])
            .arg(&fixture)
            .arg("--jpeg")
            .arg(&jpeg_path)
            .args(["--body-prior", "from-raw", "--out"])
            .arg(&second)
            .arg("--seed-file")
            .arg(&seeds);
        command
    });

    let blobs = |dir: &Path| -> Vec<(String, Vec<u8>)> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .expect("dir")
            .map(|entry| entry.expect("entry").path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "llsamp")
            })
            .collect();
        entries.sort();
        assert!(!entries.is_empty(), "no blobs in {}", dir.display());
        entries
            .into_iter()
            .map(|path| {
                let name = path
                    .file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned();
                (name, std::fs::read(&path).expect("blob"))
            })
            .collect()
    };
    let first_blobs = blobs(&first);
    let second_blobs = blobs(&second);
    assert_eq!(first_blobs, second_blobs);

    for (_, bytes) in &first_blobs {
        let samples = looklab_samples::decode(bytes).expect("blob decodes");
        assert!(samples.n() > 0);
    }

    let summary = std::fs::read_to_string(first.join("prep.json")).expect("summary");
    assert_no_location_fields(serde_json::from_str(&summary).expect("summary json"));
    assert!(first.join("transform.yaml").exists());

    let folder = temp.path().join("folder");
    std::fs::create_dir(&folder).expect("folder");
    std::fs::copy(&fixture, folder.join("x-s10.raf")).expect("copy raw");
    std::fs::copy(&jpeg_path, folder.join("x-s10.jpg")).expect("copy jpeg");
    let (stdout, _) = ok({
        let mut command = looklab();
        command.arg("prep").arg("--dir").arg(&folder);
        command
    });
    assert!(stdout.contains("pair x-s10.raf"), "{stdout}");
    assert!(stdout.contains("preselection order"), "{stdout}");

    std::fs::write(folder.join("broken.raf"), b"not a raw").expect("corrupt raw");
    let (stdout, _) = ok({
        let mut command = looklab();
        command.arg("prep").arg("--dir").arg(&folder);
        command
    });
    assert!(stdout.contains("unreadable broken.raf"), "{stdout}");
}

fn assert_no_location_fields(value: Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                let lowered = key.to_ascii_lowercase();
                assert!(
                    !lowered.contains("latitude")
                        && !lowered.contains("longitude")
                        && !lowered.contains("gps"),
                    "summary carries {key}"
                );
                assert!(
                    !(lowered.contains("serial") || lowered.contains("unique_id")),
                    "sanitized metadata carries {key}"
                );
                assert_no_location_fields(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                assert_no_location_fields(value);
            }
        }
        _ => {}
    }
}

/// A plausible in-camera render: white balance, one simple tone curve, sRGB encode.
fn render_jpeg(raw: &DecodedRaw) -> Vec<u8> {
    let mut codes = Vec::with_capacity(raw.rgb.len());
    for pixel in raw.rgb.as_chunks::<3>().0 {
        for (channel, value) in pixel.iter().enumerate() {
            let linear = f64::from(*value) * raw.as_shot_wb[channel];
            let toned = linear / (1.0 + linear);
            codes.push((srgb_encode(toned).clamp(0.0, 1.0) * 255.0).round() as u8);
        }
    }
    let mut jpeg = Vec::new();
    jpeg_encoder::Encoder::new(&mut jpeg, 92)
        .encode(
            &codes,
            raw.width as u16,
            raw.height as u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .expect("encode jpeg");
    jpeg
}

#[test]
fn validate_exits_non_zero_and_names_the_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let interfaces = temp.path().join("interfaces");
    std::fs::create_dir(&interfaces).expect("interfaces dir");
    std::fs::write(interfaces.join("broken.yaml"), "id: [\n").expect("write profile");
    let output = looklab()
        .arg("validate")
        .arg(temp.path())
        .output()
        .expect("run validate");
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("broken.yaml"), "{stdout}");
}

#[test]
fn fit_skips_an_undecodable_sample_and_keeps_going() {
    let temp = tempfile::tempdir().expect("tempdir");
    let samples = temp.path().join("samples");
    synth(&samples, "33", "4", "300");
    std::fs::write(samples.join("zzz-corrupt.llsamp"), b"LLSAMP").expect("corrupt sample");
    let out = temp.path().join("corrupt.looklab");
    let (stdout, stderr) = ok({
        let mut command = looklab();
        command
            .args(["fit", "--samples"])
            .arg(&samples)
            .args([
                "--transform",
                "from-metadata",
                "--look-slug",
                "corrupt",
                "--out",
            ])
            .arg(&out);
        command
    });
    assert!(stdout.contains("skipped 1 undecodable file"), "{stdout}");
    assert!(
        stderr.contains("This file couldn't be decoded (decoder error)."),
        "{stderr}"
    );
    assert!(out.exists());
}

#[test]
fn fit_refuses_a_monochrome_look_slug() {
    let temp = tempfile::tempdir().expect("tempdir");
    let samples = temp.path().join("samples");
    synth(&samples, "42", "3", "200");
    let bodies = temp.path().join("bodies");
    std::fs::create_dir(&bodies).expect("bodies dir");
    std::fs::write(bodies.join("synthetic-looklab.yaml"), MONOCHROME_BODY).expect("schema");
    let out = temp.path().join("acros.looklab");
    let output = looklab()
        .args(["fit", "--samples"])
        .arg(&samples)
        .args([
            "--transform",
            "from-metadata",
            "--look-slug",
            "acros",
            "--bodies-dir",
        ])
        .arg(&bodies)
        .arg("--out")
        .arg(&out)
        .output()
        .expect("run fit");
    assert!(!output.status.success(), "fit wrote {}", out.display());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("acros_ye"), "{stderr}");
    assert!(
        stderr.contains("B&W renders aren't supported yet"),
        "{stderr}"
    );
    assert!(!out.exists());
}

/// One body whose `acros` renders are monochrome, under the synthetic body id.
const MONOCHROME_BODY: &str = "\
body: synthetic/looklab
epochs:
  - id: 1
    firmware: { from: \"1.00\", to: \"9.99\" }
    native_sims:
      - key: acros_ye
        look: acros
        makernote: { tag: Saturation, value: 0x0502 }
        monochrome: true
      - key: provia
        look: provia
        makernote: { tag: FilmMode, value: 0x0000 }
    exposure_modes: []
    controls: []
    solve_plan: []
";

#[test]
fn card_inspect_and_eval_read_the_conformance_card() {
    let path = workspace().join("data/card/conformance-v1/valid/card-33.llk");
    let (stdout, _) = ok({
        let mut command = looklab();
        command.args(["card", "inspect"]).arg(&path);
        command
    });
    assert!(stdout.contains("magic LLCF"), "{stdout}");
    assert!(stdout.contains("content id: "), "{stdout}");
    assert!(stdout.contains("slug: conformance-tone"), "{stdout}");
    assert!(stdout.contains("grid: 33^3"), "{stdout}");

    let (stdout, _) = ok({
        let mut command = looklab();
        command
            .args(["card", "eval"])
            .arg(&path)
            .args(["0.18", "0.18", "0.18"]);
        command
    });
    assert!(
        stdout.contains("in [0.180000, 0.180000, 0.180000]"),
        "{stdout}"
    );
}
