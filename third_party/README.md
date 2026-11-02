# third_party

Patched copies of upstream crates, wired in through `[patch.crates-io]` in
`Cargo.toml` and `fuzz/Cargo.toml`. Do not name this directory `vendor/`:
the OpenMandriva build extracts its `cargo vendor` tarball there.

## image 0.25.10

Unmodified `image-0.25.10.crate` from crates.io (SHA-256
`85ab80394333c02fe689eaf900ab500fbd0c2213da414687ebf995a65d5a6104`, the
checksum previously recorded in `Cargo.lock`), plus one change:

- `src/codecs/avif/decoder.rs`, `read_until_ready`: backport of
  image-rs/image@3d74c477. An AVIF whose AV1 stream never yields a picture
  made `AvifDecoder::new` spin forever. Found by `scripts/fuzz.py`; the
  regression test is `avif_without_decodable_picture_terminates` in
  `crates/bigimage-core/tests/fidelity.rs`.

Remove this copy and the `[patch.crates-io]` entries once an `image` release
newer than 0.25.10 includes that fix.
