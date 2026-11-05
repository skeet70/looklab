# Fixtures

RAW samples for `looklab-raw` and the CLI. `fixtures/manifest.toml` is the authority: it lists each file's sha256, size, source URL, license, consent and whether it is committed.

## Licensing

Every sample here is CC0 1.0. raw.pixls.us marks each file's license column with the CC0 public-domain dedication (`https://creativecommons.org/publicdomain/zero/1.0/`), and the manifest records `consent = "CC0 public sample (raw.pixls.us)"`. The files are unmodified downloads; only the local file names are shortened.

## Committed subset

`fujifilm-x-s10.raf` (18,675,216 bytes, 1042×696 reduced) and `canon-eos-60d.dng` (471,476 bytes, 480×270 reduced) total 19,146,692 bytes, within the 20 MB committed cap.

The RAF is an X-Trans IV sensor, so it exercises the 6×6 tile path. The DNG is a small RGGB crop, so it exercises the 2×2 path and the DNG ColorMatrix1/2 path. Other samples in the manifest are fetch-only and land in `fixtures/cache/` (gitignored); `sigma-fp-8bit.dng` is an 8-bit Sigma fp DNG used as a smoke test when present.

## Preview photographs

The six `scene-*.dng` files are fetch-only CC0 sources for `site_data`. The filename suffix is the raw.pixls.us record number. They contain no identifiable people.

| File | Camera | Scene |
|---|---|---|
| `scene-1955.dng` | Ricoh GR II | Riverside light, sky, grass and neutral buildings |
| `scene-3115.dng` | Ricoh GR III | Coastal dunes, warm grasses, sea and sky |
| `scene-1824.dng` | Pentax KP | Autumn foliage against a pale sky |
| `scene-1797.dng` | Leica Q (Typ 116) | Blue boat, green waterway and sunlit foliage |
| `scene-3094.dng` | Pentax K-30 | Frog, lily pads and water reflections |
| `scene-1603.dng` | Leica M10 | Warm brickwork, neutral windows and sunlit grass |

Source URLs, original byte sizes, SHA-256 digests, license and consent are recorded in the manifest. Each generated scene repeats its source URL, license and source digest in both site scene indexes. All six DNGs expose color matrices through `looklab-raw`; `looklab-body` solves the as-shot white and adapts the transform to D65.

Run `nix develop -c tools/fetch-fixtures.sh` before `CARGO_TARGET_DIR=target-ui nix develop -c cargo run -p looklab-publish --example site_data -j 6`. Missing or corrupt sources fail generation and name the fetch command. `site_data --charts-only` explicitly omits photographs. Photo scenes are area-averaged in linear light to a 1000 px long edge, reduced further when needed to fit 2.5 MB; thumbnails have a 220 px long edge. Scene PNGs store 12-bit-quantized LUT coordinates in 16-bit RGB, not display-ready sRGB pixels.


## Pattern fixture

`fixtures/pattern/` holds a rendered contribution pattern and the page that shows it. The seed is public and fixed: the 32 bytes of SHA-256 over `looklab-fixture-pattern-v1`, which gives digest `c58d`. Regenerate the files with

```sh
nix develop -c cargo run -p looklab-prep --example fixture_pattern
```

`pattern-portrait.png` is 1080×2340 and `pattern-landscape.png` is 2560×1600, both 8-bit sRGB with the pattern filling the frame. `index.html` picks the PNG that matches the screen orientation, shows it letterboxed on black, and asks for fullscreen on a click or tap. `crates/looklab-prep/tests/pattern.rs` photographs both PNGs, upright and letterboxed, and checks that detection recovers the digest, so a stale PNG fails the test.

## Fetching

```sh
tools/fetch-fixtures.sh          # download missing or corrupt files
tools/fetch-fixtures.sh --check  # verify only
```

The script reads the manifest with `python3`, downloads with `curl`, and verifies size and sha256 with `sha256sum`; whichever of those is missing from PATH is run through `nix shell`.

## Reference decode

The reference reduced image is produced with LibRaw, independently of rawler:

```sh
nix shell nixpkgs#libraw nixpkgs#python3 -c \
    python3 tools/ref-decode.py --file fixtures/fujifilm-x-s10.raf \
    --pattern GGRGGBGGBGGRBRGRBGGGBGGRGGRGGBRBGBRG --tile 6 \
    --crop 0,5,6252,4176 --black 1022 --white 16382 \
    --out crates/looklab-raw/tests/golden/fujifilm-x-s10.json

nix shell nixpkgs#libraw nixpkgs#python3 -c \
    python3 tools/ref-decode.py --file fixtures/canon-eos-60d.dng \
    --pattern RGGB --tile 2 --crop 0,0,960,540 --black 2048 --white 15000 \
    --out crates/looklab-raw/tests/golden/canon-eos-60d.json
```

`unprocessed_raw` writes the mosaic exactly as stored, byte-identical to rawler's samples (verified on the X-S10). The script then divides each sample by the same per-channel black/white levels rawler uses, clips negatives, and averages the same 2×2 / 6×6 tiles. The levels are metadata, not pixel data: black comes from LibRaw's own identification (`raw-identify -v`) and white is the DNG `WhiteLevel` tag or the camera-config white point. Crop is rawler's active area, in the raw dump's coordinate system.

The JSON stores per-channel means and an 8×8 grid of block means. `crates/looklab-raw/tests/fixtures.rs` compares both against the decoder with a 0.5% relative tolerance, and skips with a printed notice when a fixture is absent. Fetch-only fixtures have no reference JSON; the test only checks that they decode.

## Tests

```sh
nix develop -c cargo test -p looklab-raw                                  # native, includes the reference comparison
nix develop -c cargo test -p looklab-raw --target wasm32-unknown-unknown  # Node wasm32, digest parity with native
```

The wasm test embeds the committed RAF and DNG with `include_bytes!` and compares the FNV-1a-64 digest of the reduced f32 image against the same constants the native tests use. The Canon DNG test also checks that DNG `ForwardMatrix1`/`ForwardMatrix2` are exposed with their calibration illuminants. `.cargo/config.toml` selects `wasm-bindgen-test-runner`, which runs under Node.

## Fuzzing

`fuzz/fuzz_targets/raw_decode.rs` fuzzes the decoder with these fixtures as seeds; the committed corpus is 995,764 bytes.

```sh
cd fuzz
nix develop ..#fuzz -c cargo fuzz run raw_decode -- -max_total_time=600 -rss_limit_mb=4096 -timeout=30
```

The harness records rawler panic locations and saves the triggering input; findings are triaged in `docs/upstream/rawler-panics.md`.
