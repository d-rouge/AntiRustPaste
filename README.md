# AntiRustPaste

A clipboard history manager for macOS, written entirely in Rust and inspired by
[CopyClip](https://fiplab.com/apps/copyclip-for-mac). No Swift or Objective-C
sources: AppKit is driven through the [`objc2`](https://crates.io/crates/objc2)
bindings, and the few C APIs (Carbon hot key, CoreGraphics events, pasteboard)
are called through small FFI shims.

## Features

- Lives in the menu bar (`LSUIElement`), no Dock icon.
- Watches the general pasteboard and keeps a searchable history of:
  text, rich text (RTF), HTML, images (PNG/TIFF/JPEG) and file URLs.
- **Text**, **Images** and **Commands** tabs, shown as icon segments with
  tooltips. Images get pure-Rust thumbnails from a disposable, boot-cleared
  cache, generated on a background worker.
- The **Commands** tab reads zsh and bash history files on demand, when you open
  the tab or press **Refresh** (`⌘R`); it never watches them in the background.
  Commands support the same live search, pinning and one-key paste as clippings.
- Skips concealed/transient pasteboard items (password managers).
- Remembers the app a clipping came from and shows it in the list.
- Re-copying something already in the history moves it to the top instead of
  duplicating it.
- Quick Look preview for images: press Space on the Images tab (or `⌘Y`) to
  open the selected image in the system preview panel, exactly like Finder.
- Pin clippings with the pin icon on each row (or `⌘P`) to keep them above the
  list and exempt from automatic trimming until explicitly deleted. Pinned rows
  are grouped under a **PINNED** header in their own card, separate from
  **RECENT**.
- Global shortcut (⇧⌘V by default, recordable in Preferences) opens the popup.
- Popup with live search, keyboard navigation, ⏎ to paste, ⌫ to delete, esc to
  close. Choosing a clipping writes it to the pasteboard, reactivates the
  previous app and synthesises ⌘V.
- Status menu with Preferences, Export Images to PDF…, Clear All Clippings
  (clippings and thumbnails; terminal commands are kept), About and Quit.
- Footer buttons that clear only the **Text** section or only the **Images**
  section, with the same delete confirmation. **Clear All Clippings** keeps the
  commands, because they come from your own shell history; delete a command row
  with ⌫ to remove it.
- **Export images to PDF**: pick and drag-reorder images in a dedicated window
  (Images-tab **PDF…** button, status menu or `⌘E`) and save one A4 page per
  image with the system save dialog. Original blobs are embedded, not
  thumbnails; large images are scaled to fit the page and small ones keep their
  native size.
- Preferences: history limit, record toggle, confirm-before-delete, start at
  login, global shortcut, and an ignored-apps list.
- History limit from 1 to 9999 items per section (200 by default); clippings and
  commands keep independent budgets.
- Start at login through a per-user LaunchAgent.

## Download

Grab the latest universal build from the
[releases page](https://github.com/d-rouge/AntiRustPaste/releases/latest). The
`.dmg`, `.zip` and `SHA256SUMS.txt` are **assets of a release** (Releases → a
version → Assets); the repository's *Packages* section is for package
registries and stays empty.

The build is ad-hoc signed but not notarized: on first launch, right-click
**AntiRustPaste.app** and choose **Open**.

## Requirements

- macOS 13 or newer.
- A Rust toolchain (edition 2024, Rust 1.88+; required by the imaging crate).
- The Accessibility permission, granted the first time AntiRustPaste tries to paste
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
| [Releasing](docs/releasing.md) | Version bump, release tag, GitHub Actions build and publish |
| [Agent playbook](docs/agent-playbook.md) | Safe recipes and crash-avoidance patterns |
| [AGENTS.md](AGENTS.md) | Rules for coding agents in this repository |

## Build and run

```sh
cargo run -p antirustpaste
```

For a real app bundle:

```sh
./make-app.sh
open dist/AntiRustPaste.app
```

To keep it around, copy `dist/AntiRustPaste.app` to `/Applications`.

Because it is an accessory app, there is no Dock icon: look for the clipboard
symbol in the menu bar. Left-click opens the history, right-click opens the
status menu.

`antirustpaste --help` lists the test/automation environment variables;
`antirustpaste --version` prints the version.

### Checks

```sh
./scripts/check.sh   # fmt, clippy -D warnings, unit tests
./scripts/smoke.sh   # isolated 3 s launch, fails on a panic
```

The smoke script starts the app with throwaway history, preferences and
login-item paths. It never touches your real clippings or LaunchAgents.

### Testing safely

Never point a development run at your real data. The overrides are:

| Variable | Effect |
| --- | --- |
| `ANTIRUSTPASTE_DATA_DIR` | History directory |
| `ANTIRUSTPASTE_DEFAULTS_SUITE` | Isolated `NSUserDefaults` suite |
| `ANTIRUSTPASTE_LAUNCH_AGENT_DIR` | Directory for the login-item plist |
| `ANTIRUSTPASTE_THUMB_CACHE_DIR` | Absolute, dedicated thumbnail cache directory |
| `ANTIRUSTPASTE_SHELL_HISTORY_DIR` | Directory holding synthetic `.zsh_history`/`.bash_history` |

Root/temp-root/home and relative cache paths are ignored; the default `/tmp`
location is used instead. When `ANTIRUSTPASTE_DATA_DIR` is set without
`ANTIRUSTPASTE_SHELL_HISTORY_DIR`, the Commands tab reads nothing, so an
isolated test never falls back to the operator's real shell history.
`ANTIRUSTPASTE_OPEN_PREFS=1`, `ANTIRUSTPASTE_OPEN_PANEL=1`,
`ANTIRUSTPASTE_OPEN_IMAGES=1` and `ANTIRUSTPASTE_OPEN_COMMANDS=1` open those
surfaces after launch for automation. See [docs/testing.md](docs/testing.md)
before any manual GUI test.

## Keyboard

| Shortcut | Action |
| --- | --- |
| ⇧⌘V | Open/close the history popup (recordable) |
| ↑ / ↓ | Move the selection |
| ⏎ | Paste the selected clipping |
| ⌘1 … ⌘9 | Paste the 1st–9th visible row of the active tab |
| ⌘C | Copy the selected clipping without pasting |
| ⌘P | Pin / unpin the selected clipping |
| ⌘⇧1 / ⌘⇧2 / ⌘⇧3 | Switch to the Text / Images / Commands tab |
| ⌘R | Re-read the shell history (Commands tab) |
| ⌫ | Delete the selected clipping (when search is empty) |
| esc | Close the popup |

## Storage

```
~/Library/Application Support/AntiRustPaste/history.json        # index (clippings + commands)
~/Library/Application Support/AntiRustPaste/blobs/*.bin         # images / RTF / HTML
~/Library/Application Support/AntiRustPaste/shell-history.json  # zsh/bash read cursors

Upgrading from 0.1.x renames the old `Application Support/RustClip` directory
once, automatically; no history is copied or deleted by hand.
/tmp/AntiRustPaste-<user>-<hash>/thumbnails/thumb-*.png     # macOS, boot-cleared
```

The index is plain JSON and binary clips live in separate files, so pruning and
deleting never rewrites large blobs. Terminal commands live in the same index
but with their own retention budget, so a busy shell cannot evict clippings.
`shell-history.json` only stores byte offsets and a fingerprint per shell; the
command text itself is never duplicated there. Thumbnails are regenerable and
have their own temporary cache; **Clear Thumbnail Cache** in Preferences removes
it. Preferences live in the standard user defaults domain under `com.rustclip.app`.

Commands are plain text in the history, exactly like clippings, and are not
encrypted. The **Commands** tab reads `~/.zsh_history` and `~/.bash_history` only
when opened or refreshed; a custom `$HISTFILE` is not discovered.

Start at login writes:

```
~/Library/LaunchAgents/com.rustclip.app.plist
```

## Layout

```
Cargo.toml                  workspace only
apps/antirustpaste-macos/        AppKit shell (menu bar, popup, native services)
crates/antirustpaste-core/       platform-neutral model, storage, settings, filters, thumbnails
crates/antirustpaste-imaging/    pure-Rust image metadata and thumbnails
```

| Path | Purpose |
| --- | --- |
| `apps/antirustpaste-macos/src/main.rs` | Entry point |
| `apps/antirustpaste-macos/src/app.rs` | App delegate, state, polling, popup/preferences actions |
| `apps/antirustpaste-macos/src/views.rs` | Objective-C classes for the popup, list views, preferences |
| `apps/antirustpaste-macos/src/pasteboard.rs` | Pasteboard reading and writing |
| `apps/antirustpaste-macos/src/paste.rs` | CoreGraphics ⌘V synthesis, Accessibility prompt |
| `apps/antirustpaste-macos/src/hotkey.rs` | Carbon `RegisterEventHotKey` wrapper |
| `apps/antirustpaste-macos/src/settings.rs` | User defaults, modifier migration, LaunchAgent |
| `apps/antirustpaste-macos/src/thumbnails.rs` | Boot-cleared cache path and AppKit image loading |
| `crates/antirustpaste-core` | `Clip`/`Store`, settings, filtering, shell-history reader, thumbnail worker |
| `crates/antirustpaste-imaging` | Pure-Rust image metadata and thumbnail generation (`image` crate) |

## Known gaps

- The custom-drawn list has no per-row accessibility elements yet.
- The Images tab is a compact list, not a grid.
- The first Images draw may show an `IMG` placeholder briefly while the
  background worker generates the thumbnail.
- EXIF orientation is not applied; a phone photo copied with an orientation tag
  may be thumbnailed sideways.
- File URLs that point at images stay in the Text tab; only bitmap pasteboard
  data appears in Images.
- zsh writes commands to `~/.zsh_history` on exit by default; without
  `SHARE_HISTORY`/`INC_APPEND_HISTORY` the Commands tab updates only after that
  shell exits. Use **Refresh** (`⌘R`) after the file changes.
- Only the default `~/.zsh_history` and `~/.bash_history` are read; a custom
  `$HISTFILE` or fish history is not discovered.
- There is no **Clear Commands** button: delete individual command rows with ⌫.
  **Clear All Clippings** keeps commands.
- History is plain JSON plus blob files; it is not encrypted.
- Start at login uses a LaunchAgent, not `SMAppService`; moving the app requires
  relaunching it so the plist is rewritten.
- A global shortcut already owned by another app must be re-recorded.

## Notes

- Carbon's hot key API is deprecated for UI but is still the supported way to
  register a system-wide shortcut without Accessibility access.
- Pasting needs Accessibility because AntiRustPaste sends a synthetic ⌘V to the app
  that was frontmost when the popup opened.
- Panics in AppKit, timer and Carbon callbacks are caught and logged rather than
  being allowed to abort the process.
- Thumbnail decoding lives in a pure-Rust crate with `#![forbid(unsafe_code)]`,
  so it can be tested and reused outside macOS. Its only new dependency is
  `image` (MIT OR Apache-2.0); see [THIRD_PARTY.md](THIRD_PARTY.md).
- One `antirustpaste-thumbnails` worker handles decode/cache I/O; AppKit stays on the
  main thread.
- The default macOS cache is under `/tmp`, which the OS clears at boot. An
  explicit `ANTIRUSTPASTE_THUMB_CACHE_DIR` override is not boot-cleared.
- This is an independent reimplementation: it shares no code, assets or wording
  with CopyClip.
