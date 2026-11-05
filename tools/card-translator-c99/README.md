# Card translator, portable C99

Reference translator for a looklab firmware card (`docs/04` §8). It reads a card through a caller-provided callback, fits the two stages most hacked firmware can program (a 3×3 matrix followed by a per-channel tone curve), and prints the result. No allocator, no filesystem, no threads.

`src/llk.h` is the whole API: `llk_card_open` validates and loads the card, `llk_translate` fits, and `llk_apply` runs the fitted model.

## Build

```
nix shell nixpkgs#gcc nixpkgs#clang -c make -C tools/card-translator-c99 check
```

`make` builds `build/llk-translate` with `cc`. `make check` additionally builds with gcc and clang under `-std=c99 -Wall -Wextra -Werror -pedantic`, builds an ASan+UBSan binary and an allocator-guard binary (`-Wl,--wrap=malloc` and friends abort), and fails if `nm` finds a reference to `malloc`, `calloc`, `realloc` or `free`.

Host use:

```
build/llk-translate look.llk          # fit, print JSON
build/llk-translate --inspect look.llk
```

A rejected card exits 2 and leaves nothing printed; a host or loader error exits 1. `crates/looklab-export/tests/card_translator.rs` builds this tool and runs it against cards written by `looklab_export::card::write_card_sections`, including the conformance files in `data/card/conformance-v1`.

## Reading a card without an allocator

The reader takes an `llk_read_fn` that fills exactly `len` bytes at an absolute `uint32_t` offset. Offsets may be revisited: the fit streams the `FIT` section once per stage pass, in batches of 8 records, and `NEUTRAL` in batches of 64 points. Firmware maps this onto its own block reader; the host binary implements it with `fseek`/`fread`.

Validation follows `docs/04` §8.3 and rejects on: bad magic, version or header length; a header length, file length or reserved field that disagrees with the layout; the header CRC-32 (header plus section table, CRC field zeroed); a section table that is unsorted, duplicated, unaligned, overlapping, or outside the file; a missing `META`, `SEMANTICS`, `NEUTRAL` or `FIT`; the SHA-256 content id over every byte after the section table; the CRC-32 of `META`, `SEMANTICS`, `NEUTRAL`, `FIT` and, when present, `GRID`; a semantics field that is not 1 or a non-zero pad; a `NEUTRAL` count that is not 257; a `FIT` record count that does not match its section length; a `GRID` side that is not 33 or 65, a `GRID` length that does not match the side, or a non-finite `f16` grid value; a non-finite `NEUTRAL` value; a non-finite FIT input, output or weight; and a NaN or negative FIT `σ_ΔE`. The FIT `+∞` sigma sentinel is accepted and the record is not used. GRID values are checked but never used, so a 65³ grid costs a 1.6 MB read at startup. Unknown sections are skipped.

Two bounds come from this implementation, not the format: at most 64 sections and at most 4096 `META` bytes. The card struct and the parsing buffers total roughly 10 KB of stack.

## The fit

The model is `out = curve(matrix * in)`, with `curve` applied per channel over EV relative to mid gray 0.18.

- **Tone from `NEUTRAL`.** The curve has `LLK_CURVE_POINTS` knots (28, matching x70view; compile-time override). Knot `j` sits at `NEUTRAL` record `round(j * 256 / 27)`, so knot values are read without interpolation. `curve_ev[j]` is the EV, `curve_input[j] = 0.18 * 2^ev` the linear input, and `curve[ch][j]` the linear Rec.2020 output. The curve is piecewise linear in EV and held outside the knots. A tinted neutral axis is represented, since each channel has its own knots.
- **Matrix from `FIT`.** Only records with flag bit 0, a positive weight and a finite `σ_ΔE` enter. The weight is `w / (1 + σ²)` from the record. Each supported output is pushed through the tone inverse to get the linear value the matrix must produce, then a weighted least-squares solve gives the seed. A Gauss-Newton stage then minimises weighted CIELAB ΔE (`Rec.2020` linear → XYZ D65 → L\*a\*b\*) by re-streaming `FIT`; `lab_rms` and `lab_max` report the result.
- **Exclusions.** A probe whose output lies outside the neutral curve's range cannot be produced by the model and is not used; `fitted` counts the records that were.

The fit runs per channel; the reference application is `llk_apply`.

Assumptions worth knowing:

- **Normalization convention.** `llk_translate` returns a row-normalized matrix: each row sums to one within fit error. The tone curve absorbs each row's original neutral gain, so `curve(matrix * in)` reproduces the card on the FIT probes even when the card's matrix rows did not sum to one. The matrix numbers are therefore not the original parameters when the original rows summed to something else; parameter identity is only expected for rows that already summed to one. Flat neutral responses (zero row sums) are not invertible by this fit.
- A matrix output at or below zero, and any output at or below the first knot, is the curve's black point. Probes there carry no matrix information.
- CIELAB uses D65 and the Rec.2020→XYZ matrix. No output encoding or clip is applied: card values are pre-clip by definition.
- The fit is deterministic: fixed probe order, no randomness.

## Firmware integration

1. At startup call `llk_card_open`. It hashes the payload and rejects a bad card, so keep the previous state on error.
2. If `card.content_id` equals the id cached with the previous translation, reuse the cached `llk_translation` and skip the fit.
3. Otherwise call `llk_translate`, then cache the struct keyed by the 32-byte content id. The fit re-reads the `FIT` section: one seed pass plus up to 8 refinement iterations with up to 5 trial passes each, so a few dozen reads of that section (a 1057-probe `FIT` is about 42 KB per pass). Refit only on the first boot after a card change.
4. Program `matrix` and `curve` into the camera's stage, or consume the knots directly: apply the matrix to linear Rec.2020, then per channel `s = log2(v / 0.18)`, then the piecewise-linear curve in `s`.
5. Write the render tag of `docs/04` §8.4 into `EXIF UserComment` of every JPEG rendered with the card.

Where startup time or the FPU cannot afford the fit, run this binary on a host and convert the JSON to the firmware's native format; the struct field names match the JSON keys.
