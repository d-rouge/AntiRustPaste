# Development

> Last reviewed: 2026-11-26 · Version 0.3.2

## Prerequisites

- macOS 13 or newer.
- Rust 1.88+ (edition 2024; `image` 0.25 sets the minimum). No Node, Swift, Xcode project or C toolchain is
  required; the crates use the system AppKit/Carbon/CoreGraphics frameworks.
- Accessibility permission for the finished app if you want to test synthetic
  paste. Unit tests and the smoke script do not need it.

## Commands

```sh
cargo build -p antirustpaste           # debug macOS app
cargo build -p antirustpaste-imaging   # pure-Rust imaging crate
cargo run -p antirustpaste             # runs the menu-bar app (blocks until quit)
cargo test --workspace            # core + imaging + macOS shell tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
./scripts/check.sh                # fmt + clippy + workspace tests
./scripts/smoke.sh                # isolated 3 s launch, checks for panics
./make-app.sh                     # release bundle in dist/AntiRustPaste.app
```

The workspace has four packages:

- `apps/antirustpaste-macos` (binary `antirustpaste`) — AppKit shell;
- `crates/antirustpaste-core` — platform-neutral model/storage/settings/filtering/
  thumbnail worker;
- `crates/antirustpaste-imaging` — pure codec with `#![forbid(unsafe_code)]`;
- `crates/antirustpaste-pdf` — pure image-to-PDF writer.

Core must stay free of UI toolkits and native clipboard APIs. See
[platforms.md](platforms.md) before adding Windows or Linux.

`cargo run -p antirustpaste` starts a GUI app and does not return. For unattended work use
`./scripts/smoke.sh` or run the binary in the background and stop only the PID
you started.

## Safe local runs

Never point a development run at your real history or preferences. Use scratch
paths:

```sh
SCRATCH="$(mktemp -d)"
mkdir -p "$SCRATCH/shell"
ANTIRUSTPASTE_DATA_DIR="$SCRATCH/data" \
ANTIRUSTPASTE_DEFAULTS_SUITE="com.antirustpaste.dev.$$" \
ANTIRUSTPASTE_LAUNCH_AGENT_DIR="$SCRATCH/LaunchAgents" \
ANTIRUSTPASTE_THUMB_CACHE_DIR="$SCRATCH/thumbnails" \
ANTIRUSTPASTE_SHELL_HISTORY_DIR="$SCRATCH/shell" \
ANTIRUSTPASTE_OPEN_IMAGES=1 ANTIRUSTPASTE_OPEN_PANEL=1 \
./target/debug/antirustpaste
```

`./scripts/smoke.sh` does this for you. `ANTIRUSTPASTE_SMOKE_PREFS=1` opens
Preferences; `ANTIRUSTPASTE_SMOKE_PANEL=1` opens the history popup;
`ANTIRUSTPASTE_SMOKE_COMMANDS=1` writes synthetic shell history and opens the
Commands tab. The script kills only its own process and removes its scratch
directory on success.

| Variable | Purpose | Default |
| --- | --- | --- |
| `ANTIRUSTPASTE_DATA_DIR` | History directory | `~/Library/Application Support/AntiRustPaste` |
| `ANTIRUSTPASTE_DEFAULTS_SUITE` | `NSUserDefaults` suite | bundle/process domain |
| `ANTIRUSTPASTE_LAUNCH_AGENT_DIR` | Login-item plist directory | `~/Library/LaunchAgents` |
| `ANTIRUSTPASTE_THUMB_CACHE_DIR` | Absolute, dedicated thumbnail cache | `/tmp/AntiRustPaste-<user>-<hash>/thumbnails` (boot-cleared on macOS) |
| `ANTIRUSTPASTE_SHELL_HISTORY_DIR` | Directory with `.zsh_history`/`.bash_history` | `$HOME` |
| `ANTIRUSTPASTE_DISABLE_UPDATE_CHECK=1` | Skip the GitHub release check | off |
| `ANTIRUSTPASTE_OPEN_PREFS=1` | Open Preferences at launch | off |
| `ANTIRUSTPASTE_OPEN_PANEL=1` | Open the popup at launch | off |
| `ANTIRUSTPASTE_OPEN_IMAGES=1` | Start the popup on Images | off |
| `ANTIRUSTPASTE_OPEN_COMMANDS=1` | Start the popup on Commands (reads shell history) | off |

The thumbnail cache override must be an absolute, dedicated directory. `/`,
`/tmp`, the home directory and relative paths are ignored and the default is
used instead. The shell history override must point at a scratch directory with
synthetic files, never at the operator's real home directory. When
`ANTIRUSTPASTE_DATA_DIR` is set without `ANTIRUSTPASTE_SHELL_HISTORY_DIR`, the
Commands tab reads no files.

The `ANTIRUSTPASTE_OPEN_*` hooks exist for automation and documentation. They are
inert unless set. `ANTIRUSTPASTE_SMOKE_IMAGES=1 ./scripts/smoke.sh` exercises the
Images tab in the isolated launch.

## Permissions

AntiRustPaste writes the chosen clip to the pasteboard without asking for anything.
To send the synthetic ⌘V it needs **System Settings → Privacy & Security →
Accessibility**. The first paste attempt triggers the system prompt. If access is
denied:

- the clipboard still receives the clip;
- the popup closes and the target app is activated;
- no keystroke is delivered.

Agents MUST NOT grant, revoke or script this permission. Ask the operator.

## Bundle and release

The bundle id stays `com.rustclip.app` so existing preferences, the LaunchAgent
label and the Accessibility grant survive the rename. Everything else uses the
product name: crates, the binary, the environment variables and the history
directory (`~/Library/Application Support/AntiRustPaste`, migrated once from the
old `RustClip` directory at launch).

`./scripts/make-dmg.sh dist/AntiRustPaste.app 0.3.2` builds the styled installer
window from the bundle (Finder automation is required for the icon layout).

`./make-app.sh`:

1. builds `--release` (LTO, size optimisation);
2. creates `dist/AntiRustPaste.app/Contents/MacOS/antirustpaste`;
3. copies `packaging/Info.plist` and sets `CFBundleShortVersionString` from
   `Cargo.toml`;
4. ad-hoc signs the bundle (`codesign --force --sign -`).

Set `ANTIRUSTPASTE_UNIVERSAL=1` to build both slices and `lipo` them into a universal
binary (`rustup target add aarch64-apple-darwin x86_64-apple-darwin` first).

If `cargo` comes from Homebrew instead of rustup, the rustup-installed standard
library is invisible to it and cross-builds fail with "can't find crate for
`core`". Put the rustup toolchain's `bin` directory first in `PATH` or use a
rustup-managed shell; GitHub's runners always use rustup.
Tag-driven releases are built and published by
[`.github/workflows/release.yml`](../.github/workflows/release.yml); the process
is in [releasing.md](releasing.md).

The bundle declares `LSUIElement=true`, `CFBundleIdentifier=com.rustclip.app`,
`NSPrincipalClass=NSApplication` and macOS 13 as the minimum. Ad-hoc signing
keeps the bundle valid and gives Accessibility a stable identity per build; a
rebuilt binary may need the permission re-approved.

To install: copy `dist/AntiRustPaste.app` to `/Applications`. Moving the app after
enabling "Start at login" leaves the old LaunchAgent path until the app is
launched again, which rewrites the plist.

## Debugging

- Panics in callbacks are caught and logged to stderr as
  `antirustpaste: recovered from a panic in <name>`. Run from a terminal to see them.
- A real crash log is in `~/Library/Logs/DiagnosticReports/`. Read the last
  frames and map them to `apps/antirustpaste-macos/src/app.rs`, `views.rs`,
  `hotkey.rs` or `paste.rs`.
- Set `RUST_BACKTRACE=1` when running a debug build to see the Rust side.
- `log stream --process antirustpaste` shows system messages; AntiRustPaste only writes to
  stderr.
- To check the event path without Accessibility, test that the clipboard content
  changes and the target app is activated; the synthetic key is the last step.

## Code conventions

- Modules and functions use sentence-case comments that explain *why*.
- AppKit objects are `Retained<T>`; keep delegates and panels alive in
  `AppState`.
- Never call AppKit while holding `with_state` if the call can re-enter. Clone
  first.
- Every new OS callback goes through `guard`/`guard_value`.
- AppKit, `AppState`, timers and pasteboard work stay on the main thread. The
  only worker is `antirustpaste-thumbnails`, which may touch files and the imaging
  crate but never AppKit or `AppState`.
- Errors that the user cannot act on are logged; errors they can act on use an
  alert.
- User-facing strings are English; keep them short and specific.
- Avoid new dependencies. The current set is `objc2*`, `block2`, `serde`,
  `serde_json`, `chrono`, `uuid`, `dirs` and the imaging crate's `image`
  dependency (MIT OR Apache-2.0, PNG/TIFF/JPEG features only). See
  [`../THIRD_PARTY.md`](../THIRD_PARTY.md).

## Troubleshooting

| Symptom | Check |
| --- | --- |
| No menu-bar icon | Is the process running? Did `applicationDidFinishLaunching` log a panic? Is another instance already using the status item? |
| Hot key does nothing | Another app owns the combination; record a different one in Preferences. Registration errors are only shown while recording. |
| Popup opens but nothing is captured | Recording enabled? Source app ignored? Pasteboard types concealed? History directory writable? |
| Pasting only copies | Accessibility not granted for this build; grant it manually in System Settings. |
| Images tab shows `IMG` permanently | The blob is missing/corrupt, the image exceeds 32 MP, or the worker could not write the cache; the clip is still pasteable. |
| Images tab takes a moment to fill | Thumbnails are generated by the background worker and installed by the 0.1 s timer. |
| Thumbnail cache survives a reboot | Check `ANTIRUSTPASTE_THUMB_CACHE_DIR`; an override can point outside `/tmp`. |
| Old data appears in a test | `ANTIRUSTPASTE_DATA_DIR` was not set or points at the real directory. Stop and fix the test; do not delete the operator's history. |
| Login item test polluted the machine | This should never happen: `ANTIRUSTPASTE_LAUNCH_AGENT_DIR` must be set. Remove only the test plist if you created one, and tell the operator. |
| Preferences window disappears | It is kept alive in `AppState::prefs`; check that the delegate/window is not dropped. |
| App quits when the last window closes | `applicationShouldTerminateAfterLastWindowClosed` must return `false`. |

## Repository state

This working directory may not be a Git repository. Do not assume you can
recover a deleted file with Git. Inspect before overwriting, and keep unrelated
user changes.
