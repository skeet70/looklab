// SPDX-License-Identifier: AGPL-3.0-or-later
//! Writes the committed pattern fixture and its viewer page.
//!
//! The seed is fixed and public: the 32 bytes of SHA-256 over
//! `looklab-fixture-pattern-v1`. `cargo run -p looklab-prep --example
//! fixture_pattern` regenerates `fixtures/pattern/pattern.png` and
//! `fixtures/pattern/index.html`.

use std::fs;
use std::path::PathBuf;

use looklab_color::RgbSpace;
use looklab_prep::Image3;
use looklab_prep::pattern::{self, PatternSeed};
use sha2::{Digest, Sha256};

const SEED_TEXT: &str = "looklab-fixture-pattern-v1";
const WIDTH: u32 = 2560;
const HEIGHT: u32 = 1440;

const PAGE: &str = r#"<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<title>looklab contribution pattern</title>
<style>
  html, body { margin: 0; height: 100%; background: #000; overflow: hidden; }
  img { position: fixed; inset: 0; width: 100vw; height: 100vh; object-fit: contain; }
  p { position: fixed; left: 0; right: 0; bottom: 0; margin: 0; padding: 0.6rem;
      color: #8a8a8a; background: #000; font: 14px system-ui, sans-serif; text-align: center; }
  :fullscreen p, :-webkit-full-screen p { display: none; }
</style>
<img src="pattern.png" alt="Contribution pattern">
<p>Tap or click for full screen, then raise the brightness.</p>
<script>
  document.addEventListener("click", function () {
    if (!document.fullscreenElement && !document.webkitFullscreenElement) {
      var root = document.documentElement;
      var request = root.requestFullscreen || root.webkitRequestFullscreen;
      if (request) { request.call(root).catch(function () {}); }
    }
  });
</script>
</html>
"#;

fn fixture_seed() -> PatternSeed {
    let digest = Sha256::digest(SEED_TEXT.as_bytes());
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&digest);
    PatternSeed::from_bytes(bytes)
}

fn encode_png(image: &Image3) -> Vec<u8> {
    let mut raw = Vec::with_capacity((image.width * image.height * 3) as usize);
    for pixel in &image.data {
        for channel in pixel {
            let encoded = RgbSpace::Srgb.encode(f64::from(*channel).clamp(0.0, 1.0));
            raw.push((encoded * 255.0 + 0.5).round() as u8);
        }
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, image.width, image.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("PNG header");
        writer.write_image_data(&raw).expect("PNG data");
    }
    out
}

#[allow(clippy::disallowed_methods)]
fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let directory = root.join("fixtures/pattern");
    fs::create_dir_all(&directory).expect("fixtures/pattern exists");

    let seed = fixture_seed();
    let image = pattern::render(&seed, WIDTH, HEIGHT).expect("the fixture size renders");
    fs::write(directory.join("pattern.png"), encode_png(&image)).expect("writes pattern.png");
    fs::write(directory.join("index.html"), PAGE).expect("writes index.html");
    println!(
        "wrote {} ({WIDTH}×{HEIGHT}) for seed {} digest {}",
        directory.display(),
        seed.to_hex(),
        seed.digest()
    );
}
