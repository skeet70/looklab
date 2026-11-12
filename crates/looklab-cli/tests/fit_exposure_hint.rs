// SPDX-License-Identifier: AGPL-3.0-or-later
//! `looklab fit` over sample headers that carry an absolute EV at ISO 100.

use std::path::Path;

use assert_cmd::Command;
use looklab_samples::{SampleSet, decode, encode};

/// The X-T4 studio scene's value: f/8, 1/60 s, ISO 160.
const ABSOLUTE_EV: f32 = 10.0;

fn looklab() -> Command {
    Command::cargo_bin("looklab").expect("built binary")
}

fn run(mut command: Command) -> String {
    let output = command.output().expect("run");
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        output.status.success(),
        "command failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    stdout
}

fn synthesise(dir: &Path) {
    let stdout = run({
        let mut command = looklab();
        command.args(["synth", "--out"]).arg(dir).args([
            "--seed",
            "11",
            "--images",
            "3",
            "--samples",
            "512",
        ]);
        command
    });
    assert!(stdout.contains("synth"), "{stdout}");
}

fn set_header_ev(dir: &Path, ev: f32) {
    for entry in std::fs::read_dir(dir).expect("samples dir") {
        let path = entry.expect("directory entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("llsamp") {
            continue;
        }
        let bytes = std::fs::read(&path).expect("read blob");
        let set = decode(&bytes).expect("decode blob");
        let mut header = set.header().clone();
        header.exposure_hint_ev = ev;
        let rebuilt = SampleSet::new(
            header,
            set.in_rgb().to_vec(),
            set.out_rgb().to_vec(),
            set.density_w().to_vec(),
            set.flags().to_vec(),
        )
        .expect("rebuild blob");
        std::fs::write(&path, encode(&rebuilt).expect("encode blob")).expect("write blob");
    }
}

fn held_out_median(dir: &Path, out: &Path) -> f64 {
    let stdout = run({
        let mut command = looklab();
        command
            .args(["fit", "--samples"])
            .arg(dir)
            .args([
                "--transform",
                "from-metadata",
                "--holdout",
                "1",
                "--look-slug",
                "exposure",
                "--out",
            ])
            .arg(out);
        command
    });
    let line = stdout
        .lines()
        .find(|line| line.trim_start().starts_with("held-out "))
        .unwrap_or_else(|| panic!("no held-out line in:\n{stdout}"));
    line.split_whitespace()
        .skip_while(|word| *word != "median")
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("no median on {line}"))
}

#[test]
fn an_absolute_ev_in_the_header_does_not_move_the_fit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let absolute = temp.path().join("absolute");
    let zero = temp.path().join("zero");
    synthesise(&absolute);
    synthesise(&zero);
    set_header_ev(&absolute, ABSOLUTE_EV);

    let absolute_median = held_out_median(&absolute, &temp.path().join("absolute.looklab"));
    let zero_median = held_out_median(&zero, &temp.path().join("zero.looklab"));
    assert!(
        absolute_median <= zero_median + 0.05,
        "absolute EV header moved the fit: held-out median {absolute_median:.3} vs {zero_median:.3}"
    );
}
