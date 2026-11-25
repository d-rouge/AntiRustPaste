# rustclip-macos

The implemented macOS shell. It owns AppKit (`NSStatusItem`, the highlighter template icon, `NSPanel`,
`NSVisualEffectView`), `NSPasteboard`, Carbon hot keys, CoreGraphics paste,
Accessibility, `NSUserDefaults` and the LaunchAgent.

Domain code belongs in `crates/rustclip-core`; image decoding belongs in
`crates/rustclip-imaging`. Do not put platform-neutral logic here without a
reason.

```sh
cargo run -p rustclip
cargo test -p rustclip
../../make-app.sh
```

Status and work for Windows/Linux: [`docs/platforms.md`](../../docs/platforms.md).
