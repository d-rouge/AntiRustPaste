# rustclip-imaging

Pure-Rust image metadata and thumbnail generation for [RustClip](../../README.md).

This crate deliberately has no AppKit, filesystem or environment dependencies:

- `info(bytes)` reads the dimensions and format without a full decode.
- `thumbnail_png(bytes, max_edge)` decodes, preserves aspect ratio, does not
  upscale, and returns encoded PNG bytes.
- Source images are capped at 32 megapixels before decode; thumbnails are capped
  at 512 px on the long edge.
- `#![forbid(unsafe_code)]`.

The application owns the temporary cache, the background worker and the
`NSImage` used for drawing; this crate stays synchronous and filesystem-free.
Dependency: `image` 0.25 with only `png`, `tiff` and `jpeg` features (MIT OR
Apache-2.0; see [`THIRD_PARTY.md`](../../THIRD_PARTY.md)).

```rust
let info = rustclip_imaging::info(&bytes)?;
let thumb = rustclip_imaging::thumbnail_png(&bytes, 128)?;
```
