# Third-party dependencies

RustClip is MIT/Apache-2.0 licensed. The crates below are the only direct
dependencies that are not part of the Rust standard library.

| Crate | Version | Licence | Used for |
| --- | --- | --- | --- |
| `objc2` | 0.6.5 | MIT OR Apache-2.0 | Objective-C runtime bindings |
| `objc2-app-kit` | 0.3.2 | MIT OR Apache-2.0 | AppKit bindings |
| `objc2-foundation` | 0.3.2 | MIT OR Apache-2.0 | Foundation types |
| `objc2-quartz-core` | 0.3.2 | MIT OR Apache-2.0 | `CALayer` used by the popup |
| `block2` | 0.6.2 | MIT OR Apache-2.0 | Objective-C blocks for timers/monitors |
| `serde` / `serde_json` | 1.0.x | MIT OR Apache-2.0 | History JSON |
| `chrono` | 0.4.x | MIT OR Apache-2.0 | Timestamps and relative time |
| `uuid` | 1.x | Apache-2.0 OR MIT | Clip ids and blob names |
| `dirs` | 7.x | MIT OR Apache-2.0 | Platform data/home directories |
| `image` | 0.25.10 | MIT OR Apache-2.0 | PNG/TIFF/JPEG decode and thumbnail encoding |
| `flate2` | 1.1.x | MIT OR Apache-2.0 | Flate compression of PDF image streams (default pure-Rust `miniz_oxide` backend) |

`image` is enabled with `default-features = false` and only the `png`, `tiff`
and `jpeg` features. No dependency has a C build step or network access at
runtime. Transitive licence texts come from the crates themselves; this file
records the direct choices, not the full dependency tree.
