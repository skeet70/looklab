// SPDX-License-Identifier: AGPL-3.0-or-later
//! Writes the committed pattern fixtures and their viewer page.

use std::fs;
use std::path::{Path, PathBuf};

use looklab_color::RgbSpace;
use looklab_prep::Image3;
use looklab_prep::pattern::{self, PatternSeed};
use sha2::{Digest, Sha256};

const SEED_TEXT: &str = "looklab-fixture-pattern-v1";
const PORTRAIT: (u32, u32) = (1080, 2340);
const LANDSCAPE: (u32, u32) = (2560, 1600);

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
<img id="pattern" alt="Contribution pattern">
<p>Tap or click for full screen, then raise the brightness.</p>
<script>
  var image = document.getElementById("pattern");
  var query = window.matchMedia("(orientation: portrait)");
  function show() {
    image.src = query.matches ? "pattern-portrait.png" : "pattern-landscape.png";
  }
  show();
  if (query.addEventListener) { query.addEventListener("change", show); }
  else { query.addListener(show); }
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
fn write_fixture(directory: &Path, seed: &PatternSeed, name: &str, size: (u32, u32)) {
    let image = pattern::render(seed, size.0, size.1).expect("the fixture size renders");
    let bytes = encode_png(&image);
    let path = directory.join(name);
    fs::write(&path, &bytes).expect("writes the fixture");
    println!(
        "{} {}×{} {} bytes",
        path.display(),
        size.0,
        size.1,
        bytes.len()
    );
}

#[allow(clippy::disallowed_methods)]
fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let directory = root.join("fixtures/pattern");
    fs::create_dir_all(&directory).expect("fixtures/pattern exists");

    let seed = fixture_seed();
    write_fixture(&directory, &seed, "pattern-portrait.png", PORTRAIT);
    write_fixture(&directory, &seed, "pattern-landscape.png", LANDSCAPE);
    let page = directory.join("index.html");
    fs::write(&page, PAGE).expect("writes index.html");
    println!(
        "{} {} bytes for seed {} digest {}",
        page.display(),
        PAGE.len(),
        seed.to_hex(),
        seed.digest()
    );
}
