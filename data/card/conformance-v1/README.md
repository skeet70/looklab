# Card conformance vectors, v1

Reference inputs for the firmware card format (docs/04 §8.6).

## Regeneration

```
cargo run -p looklab-export --example gen_card_conformance -- data/card/conformance-v1
```

The generator is deterministic: two fixed synthetic Looks (`valid/look.looklab` and its all-zero-support variant `valid/look-experimental.looklab`), one fixed seed for the grid vectors, no timestamps, and sorted JSON keys. Running it twice produces identical bytes.

## Files

| Path | Content |
|---|---|
| `index.json` | Every file with its expected reader outcome: `valid` or `rejected`, and a short reason. |
| `valid/card-33.llk` | A complete card with a 33³ GRID, META, SEMANTICS, NEUTRAL and FIT. |
| `valid/card-no-grid.llk` | The same card without GRID. |
| `valid/card-experimental.llk` | The same card written from a target whose published support mask supports no probe, with META status `experimental`. |
| `valid/unknown-section.llk` | The no-grid card plus section kind 77, which readers must skip. |
| `valid/look.looklab` | The Look document the cards were written from. |
| `valid/look-experimental.looklab` | The Look document the experimental card was written from: the same Look with an all-zero support mask. |
| `malformed/*.llk` | One file per rejection rule: truncations, magic, version, header length, file length, header CRC, section CRC, content id, missing section, duplicate section, unsorted table, overlap, out-of-range section, semantics ids, status value, grid side, and non-finite NEUTRAL, FIT and GRID values. |
| `grid-vectors.json` | Shaped input triples and the expected output under the normative tetrahedral interpolation, evaluated on `valid/card-33.llk`. |
| `render-tags.json` | Render-tag strings with the parsed fields, or an error marker for strings a parser must reject. |

`index.json` and `grid-vectors.json` start with a `format` tag. `render-tags.json` is an array of `{"input": string, "result": {...}}` records. A result carries `firmware_id`, `firmware_version`, `look_slug`, `look_version` and `cid_prefix`, or `{"error": string}`.

Grid outputs are compared within 2 f16 ulp; the reference test uses the ulp around the expected value.

The qualified source Look document carries a deterministic published support mask (every third probe unsupported) and per-probe post-clip σ; the experimental source document is the same Look with an all-zero support mask. FIT records therefore cover both supported records and unsupported records, the latter with `σ_ΔE = +∞` and weight 0, and the experimental card has no supported record.
