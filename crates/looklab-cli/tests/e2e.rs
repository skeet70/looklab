// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end runs of the `looklab` binary.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use looklab_color::trc::srgb_encode;
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
fn synthetic_flow_fits_reports_compares_exports_and_validates() {
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

    let profile = workspace().join("data/interfaces/example-firmware-sim-v1.yaml");
    let cube = temp.path().join("look-a.cube");
    let export = |force: bool| {
        let mut command = looklab();
        command
            .args(["export"])
            .arg(&look_a)
            .arg("--profile")
            .arg(&profile)
            .arg("--body-transform")
            .arg(a.join("transform.yaml"))
            .args(["--format", "cube", "--out"])
            .arg(&cube);
        if force {
            command.arg("--force");
        }
        command.output().expect("run export")
    };
    let forced_refusal = export(false);
    let first_stdout = String::from_utf8_lossy(&forced_refusal.stdout);
    assert!(first_stdout.contains("median"), "{first_stdout}");
    assert!(first_stdout.contains("flagged: yes"), "{first_stdout}");
    assert!(
        !forced_refusal.status.success(),
        "a flagged export wrote {} anyway",
        cube.display()
    );
    let stderr = String::from_utf8_lossy(&forced_refusal.stderr);
    assert!(stderr.contains("flagged"), "{stderr}");
    assert!(!cube.exists(), "a flagged export wrote {}", cube.display());

    let forced = export(true);
    assert!(
        forced.status.success(),
        "forced export failed: {}",
        String::from_utf8_lossy(&forced.stderr)
    );
    let text = std::fs::read_to_string(&cube).expect("cube");
    assert!(text.contains("LUT_3D_SIZE"));
    assert!(text.contains("TITLE"));

    ok({
        let mut command = looklab();
        command.arg("validate").arg(workspace().join("data"));
        command
    });
}

#[test]
fn prep_reproduces_blobs_from_the_seed_file() {
    let workspace = workspace();
    let fixture = workspace.join("fixtures/fujifilm-x-s10.raf");
    let raw = std::fs::read(&fixture).expect("RAF fixture");
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

