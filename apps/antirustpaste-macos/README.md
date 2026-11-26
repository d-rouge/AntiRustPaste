# antirustpaste-macos

The implemented macOS shell. It owns AppKit (`NSStatusItem`, the spray-can template icon, `NSPanel`,
`NSVisualEffectView`), `NSPasteboard`, Carbon hot keys, optional CoreGraphics
paste with Accessibility, `NSUserDefaults` and the LaunchAgent.

Domain code belongs in `crates/antirustpaste-core`; image decoding belongs in
`crates/antirustpaste-imaging`. Do not put platform-neutral logic here without a
reason.

```sh
cargo run -p antirustpaste
cargo test -p antirustpaste
../../make-app.sh
```

Status and work for Windows/Linux: [`docs/platforms.md`](../../docs/platforms.md).
