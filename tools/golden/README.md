# Golden vectors

Golden vectors lock the Rust ports in `looklab-color` to the reference implementation, colour-science 0.4.7 (BSD-3-Clause). The Python environment pins that version in `flake.nix` (`devShells.golden`), so the scripts never run against a system Python.

## Regenerating

From the repo root:

```
nix develop .#golden -c python3 tools/golden/gen_basics.py
nix develop .#golden -c python3 tools/golden/gen_body.py
nix develop .#golden -c python3 tools/golden/gen_cam16.py
nix develop .#golden -c python3 tools/golden/gen_dng.py
nix develop .#golden -c python3 tools/golden/gen_pointer.py
nix develop .#golden -c python3 tools/golden/gen_spectral.py
```

Commit the regenerated JSON with the code change that requires it. Never hand-edit the files.

## Determinism

- `common.SEED` and `common.rng(stream)` give every script a fixed numpy seed.
- `common.write_json` sorts keys and writes full-precision floats (`repr`), so a rerun produces identical bytes.
- `common.floats` rejects NaN and infinity; a function whose reference output is not finite needs inputs outside that region, as with Adobe RGB (1998) negative light.
- Scripts record the colour-science and numpy versions in the JSON, so a version bump is visible in the diff.

## Outputs

| Script | Writes |
|---|---|
| `gen_basics.py` | `crates/looklab-color/tests/golden/xyy.json`, `transfer_functions.json`, `lab.json`, `difference.json`, `illuminants.json`, `spaces.json`, `cats.json`, `data/charts/colorchecker24_2014.json` |
| `gen_body.py` | `crates/looklab-body/tests/golden/fixtures_cct.json` |
| `gen_cam16.py` | `crates/looklab-color/tests/golden/cam16_forward.json`, `cam16_inverse.json`, `cam16_ucs.json` |
| `gen_dng.py` | `crates/looklab-color/tests/golden/dng.json` |
| `gen_pointer.py` | `data/probes/pointer_gamut.json` |
| `gen_spectral.py` | `data/charts/colorchecker24_spectral.json` |

`gen_basics.py` covers XYZ/xyY, the five RGB transfer functions, RGB/XYZ conversions per space, the three chromatic adaptation transforms, CIELAB, LCh, CIEDE2000 and the standard illuminants. The chart file holds `CCS_COLOURCHECKERS["ColorChecker24 - After November 2014"]`: xyY under D50, BabelColor/X-Rite reference values.

`gen_spectral.py` writes the BabelColor ColorChecker reflectances, the CIE 1931 2 degree observer and the A and D65 power distributions on one 5 nm grid. `gen_body.py` reads the manifest fixtures with LibRaw's `raw-identify` (nixpkgs `libraw` in `devShells.golden`) and solves the DNG neutral with colour-science's Robertson (1968) conversion.

The Rust tests are `crates/looklab-color/tests/golden_basics.rs` and `golden_cam16.rs`, which load the JSON with `include_str!`.
