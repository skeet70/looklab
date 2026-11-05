# rawler 0.8.0 panic and OOM report

Fuzzing `looklab_raw::RawlerDecoder::decode` found three panic sites in a 609-second run and one allocation-exhaustion class in earlier runs of the same target, all in `rawler` 0.8.0. Every finding reproduces with the minimized inputs below, which total under 2.4 KB.

rawler catches unwinds inside `RawLoader::decode`, so these panics return `RawlerError::DecoderFailed` on native builds. On `wasm32-unknown-unknown` the same inputs abort the process. The fuzz target installs a panic hook that records the panic location and saves the input; `OMP_FUZZ_ABORT_ON_PANIC=1` switches the hook to `abort`, which is how `cargo fuzz tmin` recognizes a crash.

## Harness

Target: `fuzz/fuzz_targets/raw_decode.rs`, binary `raw_decode`. All commands below run from `fuzz/`. The target records each panic location and saves the first input that reaches it. Reproduction and `cargo fuzz tmin` run with `OMP_FUZZ_ABORT_ON_PANIC=1`, which selects this hook instead:

```rust
std::panic::set_hook(Box::new(|info| {
    eprintln!("panic: {info}");
    eprintln!("{}", std::backtrace::Backtrace::force_capture());
    std::process::abort();
}));
```

The committed seeds are `fixtures/canon-eos-60d.dng` and three truncated prefixes of the fetch-only `fujifilm-x-s10.raf` (64, 192 and 256 KiB), 995,764 bytes in total. The prefixes are committed under `fuzz/corpus/raw_decode/`; the full RAF comes from `tools/fetch-fixtures.sh`.

```sh
nix develop .#fuzz -c cargo fuzz run raw_decode -- \
    -fork=1 -max_total_time=600 -rss_limit_mb=4096 -timeout=30 \
    -ignore_crashes=1 -ignore_ooms=1 -ignore_timeouts=1
```

Fork mode is required to continue after OOMs and crashes, because `-ignore_ooms` and `-ignore_crashes` only apply there. Without it the same command stops at the first crash, which happened after 24 seconds on Issue 1. The fork-mode run reached 4,704 edges and 167 corpus entries with no timeouts.

## Issue 1: DNG ActiveArea with inverted bounds panics

Location: `src/imgop/mod.rs:130` in `Rect::new_with_points`, reached from `get_active_area` (`src/decoders/dng.rs:376`), `make_camera` (`dng.rs:247`) and `raw_image` (`dng.rs:43`).

Input class: a DNG whose `ActiveArea` tag (`0xC68D`, four LONG values in top, left, bottom, right order) has `left > right` or `top > bottom`. The minimized input sets `ActiveArea = [2352777946, 3667956468, 2651797306, 1463394606]`, so `new_with_dng` builds `p1 = (left, top) = (3667956468, 2352777946)` and `p2 = (right, bottom) = (1463394606, 2651797306)`, and `assert!(p1.x <= p2.x)` fails.

size 1025 bytes
sha256 114060cf4b960da9cee4661e15f63fe32f99c0d6497247d9145bef5cb74b643b

```
SUkqAAgAAAApAP4ABAABAAAAAAAAAAABBAABAAAAwAMAAAEBBAABAAAAHAIAAAIBAwABAAAADgAAAAMBAwABAAAABwAAAAYBAwABAAAAI4AAAAoBAwABAAAAAQAAAA8BAgAGAAAAhAIAABABAgAOAAAAigIAABEBBAABAAAAfgQAABIBAwABAAAAAQAAABUBAwABAAAAAQAAABYBAwABAAAAHAIAABcBBAABAAAANi0HABwBAwABAAAAAQAAADEBAgAIAAAAmAIAADIBAgAUAAAAoAIAAI2CAwACAAAAAgACAI6CAQAEAAAAAAEBAmmHBAABAAAA+gEAABLGAQAEAAAAAQQAABTGAgAOAAAAtAIAABrGBAABAAAAAAgAAB3GBAABAAAAmDoAAB7GBQACAAAAwgIAAB/GAwACAAAAAAAAACDGAwACAAAAwAMcAiHGCgAJAAAA0gIAACLGCgAJAAAAGgMAACjGBQADAAAAYgMAACrGCgABAAAAegMAAC/GAgAJAAAAggMAAFrGAwABAAAAEQAAAFvGAwABAAAAFQAAAI3GBAAEAAAAjAMAABTHCgAJAAAAnAMAABXHCgAJAAAA5AMAAGPHAQAIAAAALAQAAGTHCgABAAAANAQAAInHAgAKAAAAPAQAAKXHCgABAAAARgQAAAAAAAALAJqCBQABAAAATgQAAJ2CBQABAAAAVgQAACeIAwABjY1mIpWXj/YFTxOHKSMnoZex3R3msvYIqCJboeyVvXjPCYg+Yr/8yJmMvHRsxadbxZrmT7KxdieLyQqlhOLyvym8HxDSvzZvkM7THmNi5uHrcz88DFYV+drEzXIGGHTpipT1PlQSVFlX4gD499J56rm/e0SSuhlI2iWGiz0dqJ+nUcWsz4VQJYxYN/8A+cSWU8wDPxtc8DFcycSg3k9iiHmWR2IcdfPK7jOE2g+UDeB0Hbj1zWUVkd2YtepiNLByxZSKSjD1yIt8DiTrTqMipr/4XsreueHWxK8dJtumli5YXahef/d1LQvEuwCeP/w/aoyT9X5qGaje8VY9KMw27t47hMjMU5JU9mLIbaF507pcvVtz6sgsXVdvzvCrTVSdU9qTPV7ivFgauNvz2YhDQMpAtp8lSpoKA60a1Or9gPJ02zTYYVeSrw/ocd84tmld+dT0YKxMz8yd/wDlpXqzQv1C6XV/0IUV1WNveK02p2oWcVbaijyM9JKg2jo3D54uoTlXkVVTmuyOdphJo9yf9WztjImT+YLgXVvfCJ6//F5wK/dId+8NdofqdrEzfJqbrVn0N/8AP7iA+1ZznRe4JisbKuYhJWWRYFXYNqJK/wD9XS6NUQtfLxEuiuqgrJAJbA9lhEwQ/9k=
```

Reproduce:

```sh
OMP_FUZZ_ABORT_ON_PANIC=1 cargo fuzz run raw_decode <minimized-input> -- -runs=1 -rss_limit_mb=4096
```

Abridged stack:

```
panic: assertion failed: p1.x <= p2.x
   8: new_with_points
   9: new_with_dng           src/imgop/mod.rs:147
  10: get_active_area        src/decoders/dng.rs:376
  11: make_camera            src/decoders/dng.rs:247
  12: raw_image              src/decoders/dng.rs:43
```

Suggested fix: validate the DNG rectangle before constructing a `Rect`. In `get_active_area_borders`, reject `left > right` or `top > bottom` (return `None`, or an error that names the tag), and have `Rect::new_with_points` return a `Result` instead of asserting. `Rect::new_with_borders` needs the same treatment because it also subtracts unchecked borders.

## Issue 2: ActiveArea offsets subtract with overflow

Location: `src/imgop/mod.rs:166` in `Rect::as_ltrb_offsets`, reached from `get_active_area` (`src/decoders/dng.rs:376`).

Input class: a DNG whose `ActiveArea` is internally ordered but extends past the image dimensions. The minimized input sets `ActiveArea = [966864542, 1398116695, 1989078170, 3701688728]` with `ImageWidth = 960`. The rectangle is `x = 1398116695`, width `2303572033`, and `width - (x + w)` underflows with overflow checks enabled, as in the fuzz build. A release build without checks wraps and returns a nonsensical border instead. Either way the rectangle is not validated against the image.

size 1014 bytes
sha256 3fa6b422a57a103cbaa39c58e712e1682df3b88d89c4aa0b76bc071e6b708d9d

```
SUkqAAgAAAApAP4ABAABAAAAAAAAAAABBAABAAAAwAMAAAEBBAABAAAAHAIAAAIBAwABAAAADgAAAAMBAwABAAAABwAAAAYBAwABAAAAI4AAAAoBAwABAAAAAQAAAA8BAgAGAAAAhAIAABABAgAOAAAAigIAABEBBAABAAAAfgQAABIBAwABAAAAAQAAABUBAwABAAAAAQAAABYBAwABAAAAHAIAABcBBAABAAAANi0HABwBAwABAAAAAQAAADEBAgAIAAAAmAIAADIBAgAUAAAAoAIAAI2CAwACAAAAAgACAI6CAQAEAAAAAAEBAmmHBAABAAAA+gEAABLGAQAEAAAAAQQAABTGAgAOAAAAtAIAABrGBAABAAAAAAgAAB3GBAABAAAAmDoAAB7GBQACAAAAwgIAAB/GAwACAAAAAAAAACDGAwACAAAAwAMcAiHGCgAJAAAA0gIAACLGCgAJAAAAGgMAACjGBQADAAAAYgMAACrGCgABAAAAegMAAC/GAgAJAAAAggMAAFrGAwABAAAAEQAAAFvGAwABAAAAFQAAAI3GBAAEAAAAjAMAABTHCgAJAAAAnAMAABXHCgAJAAAA5AMAAGPHAQAIAAAALAQAAGTHCgABAAAANAQAAInHAgAKAAAAPAQAAKXHCgABAAAARgQAAAAAAAALAJqCBQABAAAATgQAAJ2CBQABAAAAVgQAACeIAwABjY1mIpWXj/YFTxOHKSMnoZex3R3msvYIqCJboeyVvXjPCYg+Yr/8yJmMvHRsxadbxZrmT7KxdieLyQqlhOLyvym8HxDSvzZvkM7THmNi5uHrcz88DFYV+drEzXIGGHTpipT1PlQSVFlX4gD499J56rm/e0SSuhlI2iWGiz0dqJ+nUcWsz4VQJYxYN/8A+cSWU8wDPxtc8DFcycSg3k9iiHmWR2IcdfPK7jOE2g+UDeB0Hbj1zWUVkd2YtepiNLByxZSKSjD1yIt8DiTrTqMipr/4XsreueHWxK8dJtumli5YXahef/d1LQvEuwCeP/w/aoyT9X5qGaje8VY9KMw27t47hMjMU5JU9mLIbaF507pcvVtz6sgsXVdvzvCrTVSdU9qTPV7ivFgauNvz2YhDQMpAtp8lSpoKA60a1Or9gPJ02zTYYVeSrw/ocd84tmld+dT0YKxMz8yd/wDlpXqzQv1C6XV/0IUV1WNveK02p9o6Nw+eLqE5V5FVU5rsjnaYSaPcn/Vs7YyJk/mC4F1b3wiev/xecCv3SHfvDXaH6naxM3yam61Z9Df/AD+4gPtWc50XuCYrGyrmISVlkWBV2DaiSv8A/V0ujVELXy8RLorqoKyQCWwPZYRMEP/Z
```

Reproduce:

```sh
OMP_FUZZ_ABORT_ON_PANIC=1 cargo fuzz run raw_decode <minimized-input> -- -runs=1 -rss_limit_mb=4096
```

Abridged stack:

```
panic: attempt to subtract with overflow
   7: core::panicking::panic_const::panic_const_sub_overflow
   8: make_camera            (as_ltrb_offsets inlined)
   9: raw_image              src/decoders/dng.rs:43
```

Suggested fix: use `checked_sub` in `as_ltrb_offsets` and `as_tlbr_offsets`, and validate `ActiveArea` against `ImageWidth`/`ImageLength` in `get_active_area`. The offsets it returns feed `crop_area`, so an out-of-bounds value should become a decode error rather than a panic or an impossible crop.

## Issue 3: Unsupported PhotometricInterpretation hits `todo!()`

Location: `src/decoders/dng.rs:66` in `raw_image`, in the match on `TiffCommonTag::PhotometricInt`.

Input class: a DNG whose `PhotometricInterpretation` (`0x0106`) is not 1, 32803 or 34892. The minimized input uses 19592. The `_ => todo!()` branch panics.

size 333 bytes
sha256 2c71bb5aed1c9c9b3052e576aac5c95946b0a7feafe3d61d8a4727153c375d92

```
SUkqAAgAAAApAP4ABAABAAAAAAAAAAABBAABAAAAwAMAAAEBBAABAAAAHAIAAAIBAwABAAAADgAAAAMBAwABAAAABwAAAAYBAwABAAAAiEwAAAoBAwABAAAAAQBtb2RlcwAGAAAAhAIAABABAgAOAAAAigIAABEBBAABAAAAfgQAABIBAwABAAAAAQAAABUBAwABAAAAAQAAABYBAwABAAAAHAIAABcBBAABAAAANi0HABwBAwABAAAAAQAAADEBAgAIAAAAmAIAADIBAgAUAAAAoAIAAI2CAwACAAAAAgACAI6CAQAEAAAAAAEBAmmHBAABAAAA+gEAABLGAQAEAAAAAQQAABTGAgAOAAAAtAIAABoniF9g94rZpRylMr151psjdIGdnRylmjfoMPIe2DaiSv8A/V0ujVELXy8RLorqoKyQCWwPZYRMEP/Z
```

Reproduce:

```sh
OMP_FUZZ_ABORT_ON_PANIC=1 cargo fuzz run raw_decode <minimized-input> -- -runs=1 -rss_limit_mb=4096
```

Abridged stack:

```
panic: not yet implemented
   8: core::panicking::todo_default_msg
   9: raw_image              src/decoders/dng.rs:66
  10: decode_unsafe          src/decoders/mod.rs:1089
```

Suggested fix: return `RawlerError::unsupported(&cam, "photometric interpretation ...")` (or `DecoderFailed`) for unknown values instead of `todo!()`. The other `todo!()` and `unimplemented!()` branches in the DNG decoder should get the same treatment; unknown tags are file-controlled.

## Issue 4: TIFF entry allocation is sized by file-controlled count

Locations: `src/formats/tiff/entry.rs:129` (`TYPE_SHORT`, `vec![0u16; count]`) and `entry.rs:138` (`TYPE_LONG`, `vec![0u32; count]`). Reached through `IFD::new` for every TIFF-based format, including DNG, CR2, NEF and ARW, and through the RAF parser's embedded TIFF (`src/decoders/raf.rs:164`).

Input class: a TIFF IFD entry whose type and count claim more storage than the file contains. The 22-byte minimized input is:

- bytes: `49 49 2a 00 08 00 00 00 29 00 fe 00 04 00 01 9d a0 ac 4c 10 ff d9`
- a little-endian TIFF header whose root IFD offset is 8
- an IFD with declared entry count 41, but only one 12-byte entry is present
- that entry is `tag = 0x00FE`, `type = LONG (4)`, `count = 0xACA09D01 = 2896207105`, so `Entry::parse` calls `vec![0u32; 2896207105]`, a 11,584,828,420-byte zeroed allocation, then `read_u32_into` fails with EOF

size 22 bytes
sha256 e62602df48d78f562dbfc0ab25fdfcac18a5558e670d027bc7a1df3294c02fd4

```
SUkqAAgAAAApAP4ABAABnaCsTBD/2Q==
```

Reproduce (the allocation exceeds the 4 GB limit and libFuzzer exits with `out-of-memory`):

```sh
cargo fuzz run raw_decode <minimized-input> -- -runs=1 -rss_limit_mb=4096
```

Backtrace from a `fuzzer::MallocHook` breakpoint:

```
fuzzer::MallocHook (size=11584828420)
#4  __interceptor_calloc
#9  try_allocate_in
#11 with_capacity_zeroed_in<u32>
#14 Entry::parse        src/formats/tiff/entry.rs:138
#15 IFD::new            src/formats/tiff/ifd.rs:147
#16 IFD::new_root_with_correction src/formats/tiff/ifd.rs:80
#18 parse_raf           src/decoders/raf.rs:164
#20 get_decoder         src/decoders/mod.rs:916
```

Variants of the same input hit `entry.rs:129` (`TYPE_SHORT`) with 4.5 GB and 6.6 GB allocations; tmin also produced 23, 24 and 26-byte variants that all OOM.

Suggested fix: in `Entry::parse`, compute the value size with checked arithmetic and compare it against the bytes remaining in the stream before allocating. Reject the entry with `TiffError::Overflow` or `UnexpectedEof` when `base + offset + bytesize` exceeds the buffer, and use `Vec::try_reserve` plus a hard cap (for example the remaining file size) so a malformed count cannot abort the process. `MAX_IFD_ENTRIES` bounds the entry count but not the size of one entry's value.

## Rawler version and upstream

All findings are against `rawler` 0.8.0 as published on crates.io, used through `looklab-raw`. Rawler's own repository is `https://github.com/dnglab/dnglab`. The panic sites are in shipped decoder code, and the OOM site is in the public TIFF parser that both `rawler::decode` and downstream callers such as `looklab-raw`'s DNG tag reader use.

The worker-respawn design in looklab contains these failures in the browser. Reporting them upstream is still worthwhile because rawler's `catch_unwind` turns the panics into generic decode errors and the OOM cannot be caught at all.
