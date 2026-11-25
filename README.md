# RustClip

A clipboard history manager for macOS, written entirely in Rust and inspired by
[CopyClip](https://fiplab.com/apps/copyclip-for-mac). No Swift or Objective-C
sources: AppKit is driven through the [`objc2`](https://crates.io/crates/objc2)
bindings, and the few C APIs (Carbon hot key, CoreGraphics events, pasteboard)
are called through small FFI shims.

## Features

- Lives in the menu bar (`LSUIElement`), no Dock icon.
- Watches the general pasteboard and keeps a searchable history of:
  text, rich text (RTF), HTML, images (PNG/TIFF/JPEG) and file URLs.
- Separate **Text** and **Images** tabs; images get pure-Rust thumbnails from a
  disposable, boot-cleared cache, generated on a background worker.
- Skips concealed/transient pasteboard items (password managers).
- Remembers the app a clipping came from and shows it in the list.
- Re-copying something already in the history moves it to the top instead of
  duplicating it.
- Pin clippings with the pin icon on each row (or `⌘P`) to keep them above the
  list and exempt from automatic trimming until explicitly deleted. Pinned rows
  are grouped under a **PINNED** header in their own card, separate from
  **RECENT**.
- Global shortcut (⇧⌘V by default, recordable in Preferences) opens the popup.
- Popup with live search, keyboard navigation, ⏎ to paste, ⌫ to delete, esc to
  close. Choosing a clipping writes it to the pasteboard, reactivates the
  previous app and synthesises ⌘V.
- Status menu with Preferences, Export Images to PDF…, Clear All Clippings
  (history + thumbnails), About and Quit.
- Footer buttons that clear only the **Text** section or only the **Images**
  section, with the same delete confirmation.
- **Export images to PDF**: pick and drag-reorder images in a dedicated window
  (Images-tab **PDF…** button, status menu or `⌘E`) and save one A4 page per
  image with the system save dialog. Original blobs are embedded, not
  thumbnails; large images are scaled to fit the page and small ones keep their
  native size.
- Preferences: history limit, record toggle, confirm-before-delete, start at
  login, global shortcut, and an ignored-apps list.
- History limit from 1 to 9999 clippings (200 by default).
- Start at login through a per-user LaunchAgent.

## Requirements

- macOS 13 or newer.
- A Rust toolchain (edition 2024, Rust 1.88+; required by the imaging crate).
- The Accessibility permission, granted the first time RustClip tries to paste
  for you (System Settings → Privacy & Security → Accessibility).

## Documentation

| Document | Contents |
| --- | --- |
| [Architecture](docs/architecture.md) | Modules, startup, threading, state, storage, paste flow |
| [Behaviour](docs/behavior.md) | Exact feature contract and edge cases |
| [Development](docs/development.md) | Build, run, bundle, debugging, permissions |
| [Testing](docs/testing.md) | Unit tests, GUI matrix, hostile inputs, isolation |
| [Roadmap](ROADMAP.md) | Pinned-clipping status and image export work |
| [Decisions](docs/decisions.md) | Why AppKit/objc2, JSON blobs, Carbon, CGEvent, LaunchAgent |
| [Platforms](docs/platforms.md) | What exists per OS and what is left to implement |
| [Agent playbook](docs/agent-playbook.md) | Safe recipes and crash-avoidance patterns |
| [AGENTS.md](AGENTS.md) | Rules for coding agents in this repository |

## Build and run

```sh
cargo run -p rustclip
```

For a real app bundle:

```sh
./make-app.sh
open dist/RustClip.app
```

To keep it around, copy `dist/RustClip.app` to `/Applications`.

Because it is an accessory app, there is no Dock icon: look for the clipboard
symbol in the menu bar. Left-click opens the history, right-click opens the
status menu.

`rustclip --help` lists the test/automation environment variables;
`rustclip --version` prints the version.

### Checks

```sh
./scripts/check.sh   # fmt, clippy -D warnings, unit tests
./scripts/smoke.sh   # isolated 3 s launch, fails on a panic
```

The smoke script starts the app with throwaway history, preferences and
login-item paths. It never touches your real clippings or LaunchAgents.

### Testing safely

Never point a development run at your real data. The four overrides are:

| Variable | Effect |
| --- | --- |
| `RUSTCLIP_DATA_DIR` | History directory |
| `RUSTCLIP_DEFAULTS_SUITE` | Isolated `NSUserDefaults` suite |
| `RUSTCLIP_LAUNCH_AGENT_DIR` | Directory for the login-item plist |
| `RUSTCLIP_THUMB_CACHE_DIR` | Absolute, dedicated thumbnail cache directory |

Root/temp-root/home and relative cache paths are ignored; the default `/tmp`
location is used instead. `RUSTCLIP_OPEN_PREFS=1`, `RUSTCLIP_OPEN_PANEL=1` and
`RUSTCLIP_OPEN_IMAGES=1` open those surfaces after launch for automation. See
[docs/testing.md](docs/testing.md) before any manual GUI test.

## Keyboard

| Shortcut | Action |
| --- | --- |
| ⇧⌘V | Open/close the history popup (recordable) |
| ↑ / ↓ | Move the selection |
| ⏎ | Paste the selected clipping |
| ⌘C | Copy the selected clipping without pasting |
| ⌘P | Pin / unpin the selected clipping |
| ⌘1 / ⌘2 | Switch to the Text / Images tab |
| ⌫ | Delete the selected clipping (when search is empty) |
| esc | Close the popup |

## Storage

```
~/Library/Application Support/RustClip/history.json   # index
~/Library/Application Support/RustClip/blobs/*.bin    # images / RTF / HTML
/tmp/RustClip-<user>-<hash>/thumbnails/thumb-*.png     # macOS, boot-cleared
```

The index is plain JSON and binary clips live in separate files, so pruning and
deleting never rewrites large blobs. Thumbnails are regenerable and have their
own temporary cache; **Clear Thumbnail Cache** in Preferences removes it.
Preferences live in the standard user defaults domain under `com.rustclip.app`.

Start at login writes:

```
~/Library/LaunchAgents/com.rustclip.app.plist
```

## Layout

```
Cargo.toml                  workspace only
apps/rustclip-macos/        AppKit shell (menu bar, popup, native services)
crates/rustclip-core/       platform-neutral model, storage, settings, filters, thumbnails
crates/rustclip-imaging/    pure-Rust image metadata and thumbnails
```

| Path | Purpose |
| --- | --- |
| `apps/rustclip-macos/src/main.rs` | Entry point |
| `apps/rustclip-macos/src/app.rs` | App delegate, state, polling, popup/preferences actions |
| `apps/rustclip-macos/src/views.rs` | Objective-C classes for the popup, list views, preferences |
| `apps/rustclip-macos/src/pasteboard.rs` | Pasteboard reading and writing |
| `apps/rustclip-macos/src/paste.rs` | CoreGraphics ⌘V synthesis, Accessibility prompt |
| `apps/rustclip-macos/src/hotkey.rs` | Carbon `RegisterEventHotKey` wrapper |
| `apps/rustclip-macos/src/settings.rs` | User defaults, modifier migration, LaunchAgent |
| `apps/rustclip-macos/src/thumbnails.rs` | Boot-cleared cache path and AppKit image loading |
| `crates/rustclip-core` | `Clip`/`Store`, settings, filtering, thumbnail worker |
| `crates/rustclip-imaging` | Pure-Rust image metadata and thumbnail generation (`image` crate) |

## Known gaps

- The custom-drawn list has no per-row accessibility elements yet.
- The Images tab is a compact list, not a grid.
- The first Images draw may show an `IMG` placeholder briefly while the
  background worker generates the thumbnail.
- EXIF orientation is not applied; a phone photo copied with an orientation tag
  may be thumbnailed sideways.
- File URLs that point at images stay in the Text tab; only bitmap pasteboard
  data appears in Images.
- History is plain JSON plus blob files; it is not encrypted.
- Start at login uses a LaunchAgent, not `SMAppService`; moving the app requires
  relaunching it so the plist is rewritten.
- A global shortcut already owned by another app must be re-recorded.

## Notes

- Carbon's hot key API is deprecated for UI but is still the supported way to
  register a system-wide shortcut without Accessibility access.
- Pasting needs Accessibility because RustClip sends a synthetic ⌘V to the app
  that was frontmost when the popup opened.
- Panics in AppKit, timer and Carbon callbacks are caught and logged rather than
  being allowed to abort the process.
- Thumbnail decoding lives in a pure-Rust crate with `#![forbid(unsafe_code)]`,
  so it can be tested and reused outside macOS. Its only new dependency is
  `image` (MIT OR Apache-2.0); see [THIRD_PARTY.md](THIRD_PARTY.md).
- One `rustclip-thumbnails` worker handles decode/cache I/O; AppKit stays on the
  main thread.
- The default macOS cache is under `/tmp`, which the OS clears at boot. An
  explicit `RUSTCLIP_THUMB_CACHE_DIR` override is not boot-cleared.
- This is an independent reimplementation: it shares no code, assets or wording
  with CopyClip.
