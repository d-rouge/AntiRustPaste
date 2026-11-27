# Platforms

> Last reviewed: 2026-11-27 · Version 0.4.1

AntiRustPaste is structured so the domain works on any desktop OS, while each OS
has its own native shell. macOS is the primary, fully exercised implementation.
The workspace also contains a Linux GTK4/X11 shell; it builds on Linux but was
not run on the macOS host used to prepare this documentation. Windows has a
defined place in the tree and a concrete list of work, but no code yet.

Concrete porting checklists are in the [roadmap](../ROADMAP.md#planned-linux-port)
for Linux and [Windows](../ROADMAP.md#planned-windows-port).

## Workspace layout

```
Cargo.toml                       workspace only
apps/
  antirustpaste-macos/                implemented AppKit shell
    src/app.rs                   status item, state, actions, timers
    src/app/control.rs           local MCP loopback HTTP endpoint
    src/views.rs                 NSPanel, history list, Preferences
    src/pasteboard.rs            NSPasteboard read/write
    src/paste.rs                 CoreGraphics ⌘V + Accessibility
    src/hotkey.rs                Carbon RegisterEventHotKey
    src/settings.rs              NSUserDefaults + LaunchAgent
    src/thumbnails.rs            boot-cleared cache path + NSImage loader
  antirustpaste-mcp/                MCP stdio bridge
  antirustpaste-linux/              GTK4 + X11 shell (Linux only)
crates/
  antirustpaste-control/             local loopback HTTP control protocol + client
  antirustpaste-core/                 platform-neutral domain
    src/store.rs                 Clip/Captured, JSON + blobs, dedup, trim
    src/settings.rs              Settings, IgnoredApp, Modifiers
    src/filter.rs                Section and search filtering
    src/shell_history.rs         zsh/bash parsing, bounded reads, cursors
    src/thumbnails.rs            cache worker, jobs, prune, clear
  antirustpaste-imaging/              pure-Rust image metadata + thumbnails
  antirustpaste-pdf/                  pure-Rust image-to-PDF writer
```

Future shells are added as `apps/antirustpaste-windows`. They depend on core;
core never depends on a shell. The MCP bridge is optional and only the macOS
shell currently binds a control endpoint.

## Boundaries

1. `antirustpaste-core` MUST NOT depend on AppKit, Win32, GTK, a UI toolkit, or a
   platform clipboard API. Its only dependencies are `serde`, `serde_json`,
   `chrono`, `uuid`, `dirs` and `antirustpaste-imaging`.
2. `antirustpaste-imaging` MUST NOT depend on a UI toolkit, filesystem or environment
   variables. It has `#![forbid(unsafe_code)]`.
3. `antirustpaste-pdf` MUST NOT depend on a UI toolkit, filesystem or environment
   variables. It takes encoded image bytes and returns PDF bytes, and keeps
   `#![forbid(unsafe_code)]`.
4. A platform shell owns:
   - tray icon / menu bar item;
   - popup window and its activation, focus, resign and positioning rules;
   - native clipboard read/write and change notification;
   - global hot key registration;
   - synthetic paste and Accessibility/permission prompts;
   - settings persistence and login-item installation;
   - the thumbnail cache directory and native image type (e.g. `NSImage`).
5. Core owns the data contract: `Captured`, `Clip`, `ClipKind`, `Store`,
   `Settings`, `IgnoredApp`, `Modifiers`, `Section`, filtering and the
   thumbnail generation worker.
6. The core settings model uses platform-neutral `Modifiers` bits
   (`SHIFT=1`, `CONTROL=2`, `OPTION=4`, `COMMAND=8`). Each shell converts to and
   from its native modifier mask. macOS also migrates old `NSEvent` values.

## Support matrix

| Capability | macOS | Windows | Linux |
| --- | --- | --- | --- |
| Tray / menu bar | ✅ `NSStatusItem` | ⬜ Win32 `Shell_NotifyIcon` | ✅ `ksni` StatusNotifierItem |
| Popup | ✅ AppKit `NSPanel` | ⬜ Win32 borderless window or chosen UI toolkit | ✅ GTK4 + libadwaita |
| Clipboard text/image/RTF/HTML/file | ✅ `NSPasteboard` | ⬜ Win32 clipboard formats | ✅ GDK clipboard (X11; Wayland varies) |
| Clipboard change signal | ✅ `changeCount` poll | ⬜ `AddClipboardFormatListener` | ✅ GDK clipboard change |
| Global hot key | ✅ Carbon | ⬜ `RegisterHotKey` | ✅ X11 `XGrabKey` |
| Synthetic paste | ✅ CGEvent + Accessibility | ⬜ `SendInput` + UIAccess | ✅ X11 XTest (X11 sessions) |
| Settings persistence | ✅ `NSUserDefaults` | ⬜ registry or JSON | ✅ JSON (XDG config) |
| Start at login | ✅ LaunchAgent | ⬜ Startup folder / Run key | ✅ XDG autostart `.desktop` |
| Thumbnail image loading | ✅ `NSImage` | ⬜ decoder → texture/bitmap | ✅ `gdk::Texture` |
| Thumbnail cache path | ✅ `/tmp/...` (boot-cleared) | ⬜ `%TEMP%\AntiRustPaste` | ✅ `$XDG_CACHE_HOME`/tmp |
| Image export to PDF | ✅ `antirustpaste-pdf` + `NSSavePanel` | ⬜ shell save dialog | ✅ GTK file dialog |
| Quick Look preview | ✅ `QLPreviewPanel` | ⬜ shell preview | ⬜ no native equivalent wired |
| Edit clip text | ✅ in-app editor + TextEdit import | ⬜ shell editor | ⬜ not wired |
| Shell command history | ✅ zsh/bash on demand | ⬜ PowerShell/`ConsoleHost_history.txt` | ✅ zsh/bash on demand |
| Pause/resume from tray | ✅ status menu | ⬜ | ⬜ Preferences switch only |
| Scoped clipping clears | ✅ all/text/images + unpinned variants | ⬜ | ⬜ section clear/all only |
| Local MCP control | ✅ `antirustpaste-mcp` + loopback HTTP | ⬜ | ⬜ |
| Tests | ✅ 29 shell + 55 core + 12 imaging + 6 PDF + 2 control + 2 MCP | ⬜ | ⬜ builds only; not run here |

Legend: ✅ done, ⬜ not implemented or not wired. The Linux row describes the
in-tree GTK4/X11 shell; it was not launched on the macOS host used for this
documentation, so treat its runtime behaviour as unverified here.

## macOS (implemented)

- Menu bar item with the spray-can template icon (tooltip
  `AntiRustPaste — clipboard history`).
- Left-click toggles the popup; right-click opens the status menu.
- Popup: search, Text/Images/Commands tabs, keyboard navigation, choose/copy,
  optional paste, delete, tab-scoped **Clear All** / **Clear Unpinned** and
  one-key choice with ⌘1…⌘9.
- Recording can be paused and resumed from the status menu; the item and the
  Preferences checkbox share one saved flag.
- Shell history: zsh and bash files are read on demand when the Commands tab is
  opened or refreshed; no background watcher.
- Native pasteboard capture: text, RTF/RTFD, HTML, PNG/TIFF/JPEG images, file
  URLs. Concealed/transient items are skipped.
- Preferences: max clips, recording, confirmations, optional paste after
  choosing, login item, shortcut recorder, ignored apps, clear thumbnail cache,
  and a **History** tab with scoped clipping deletes and disk usage.
- Global shortcut through Carbon. Copying needs no permission; the optional
  paste uses CGEvent and Accessibility.
- MCP control through a bundled `antirustpaste-mcp` bridge and a private
  loopback HTTP endpoint. The app binds only `127.0.0.1`, publishes an ephemeral
  port and bearer token in a mode-`0600` file, and never listens on a LAN
  interface. The bridge can read and modify the same live state as the menu-bar
  UI.
- Thumbnail cache under `/tmp/AntiRustPaste-<user>-<hash>/thumbnails`, cleared by
  macOS at boot; one worker thread generates thumbnails.
- Image export to PDF: a dedicated selection window, one A4 page per image,
  original blobs, Flate-compressed RGB, atomic destination write.
- Quick Look preview of the selected row with Space (empty search, any tab) or
  ⌘Y: text, rich text, HTML, image, command and file clips.
- Edit text and command clips with **Edit Text…** (`⌘⇧E`); text previews can
  also round-trip through Quick Look's **Open with TextEdit**, and saved changes
  are imported on the next 1 s poll.

Files: `apps/antirustpaste-macos/src/`.

Known macOS gaps: no per-row accessibility elements in the hand-drawn list, no
image grid, EXIF orientation is not applied, no encryption at rest, PDF export
has no page options or progress indicator, and shell history appears only when
the shell flushes its file (zsh does so on exit unless configured otherwise).

## Clipboard backend candidates

For a new shell, evaluate these in order:

1. **Native clipboard API** — best fidelity and permissions behaviour, but one
   implementation per OS (macOS already uses `NSPasteboard`).
2. **`clipboard-rs`** — one MIT-licensed crate for text, HTML, RTF, images, file
   lists, custom types and change notifications; has a Wayland feature. Closest
   match to AntiRustPaste's format matrix. It is younger than `arboard`, so test
   hostile formats and each desktop before relying on it.
3. **`arboard` + `clipboard-master`** — a solid pair for text/images/HTML/file
   lists. `arboard` has no watcher and does not expose RTF; `clipboard-master`
   only delivers change notifications and, on macOS, implements them with
   `NSPasteboard::changeCount` polling (so it does not remove polling there).
   It is a reasonable choice if rich text and exact encoded bytes are not
   required.

AntiRustPaste's macOS shell deliberately keeps the native API: it needs exact UTIs,
RTF/RTFD/HTML/file URLs, concealed-type handling and original encoded bytes.
`antirustpaste-core` only sees `Captured`/`Clip`, so a future shell can choose any of
these without changing storage or UI code.

## Windows (not implemented)

Recommended first tasks, in order:

1. Create `apps/antirustpaste-windows` with a Win32 tray icon
   (`Shell_NotifyIcon`) and a native menu (`TrackPopupMenu`).
2. Clipboard adapter: `AddClipboardFormatListener`, `GetClipboardData` for
   `CF_UNICODETEXT`, `CF_DIBV5`/PNG, RTF, HTML and `CF_HDROP`; write the same
   formats back. Map them to `Captured`/`Clip`.
3. Popup window: borderless `WS_POPUP` window, `SetForegroundWindow`,
   position under the tray icon using the monitor work area; hide on
   `WM_KILLFOCUS` unless the tray icon was clicked (same 150 ms debounce as
   macOS).
4. Hot key: `RegisterHotKey`/`WM_HOTKEY`; persist core `Modifiers` to `MOD_*`.
5. Paste: `SendInput` with Ctrl+V; do not require administrator rights.
6. Settings: registry (`HKCU\Software\AntiRustPaste`) or JSON in `%APPDATA%`.
7. Login item: Startup folder shortcut or `HKCU\...\Run`.
8. Thumbnail cache: `%TEMP%\AntiRustPaste\thumbnails`, delete on logout/reboot via
   the OS or a startup sweep; load PNG into the UI's image type.
9. Smoke test and docs.

Windows has the fewest platform caveats: tray click events, global hot keys and
synthetic input are all supported without a portal.

## Linux (implemented in-tree, unverified on this host)

The workspace contains `apps/antirustpaste-linux`, a GTK4 + libadwaita shell using
`ksni` for the StatusNotifierItem tray and `x11rb` for X11 integration. It uses
the shared core, imaging and PDF crates. It builds on Linux but was not launched
on the macOS host used for this documentation, so treat the following as what
the code intends, not as a verified runtime result:

- Tray: StatusNotifierItem menu with Show History, Preferences, Export Images to
  PDF, Clear All Clippings and Quit. Click events are not reliable on every
  desktop, so the menu remains the fallback.
- Popup: GTK4/libadwaita with Text/Images/Commands tabs, search, pinned group,
  row pin/delete controls and keyboard selection.
- Clipboard: GDK read/write with concealed-MIME filtering and bounded image
  bytes; change notification comes from the toolkit.
- X11: `XGrabKey` for the global shortcut, `_NET_ACTIVE_WINDOW` for the source app
  and XTest for synthetic Ctrl+V. A pure Wayland session degrades gracefully and
  loses the hot key and synthetic paste.
- Settings: JSON under `$XDG_CONFIG_HOME/antirustpaste/settings.json`; start at
  login writes an XDG autostart `.desktop` file.
- Thumbnails: `$XDG_CACHE_HOME/antirustpaste/thumbnails` or a temporary
  directory; the core worker generates them.
- Shell history: zsh/bash read on demand with `ANTIRUSTPASTE_SHELL_HISTORY_DIR`
  isolation.
- PDF export: GTK file dialog plus the shared `antirustpaste-pdf` writer.

Not wired in the Linux shell yet: the local MCP control endpoint, the scoped
clear/unpinned actions, a tray pause/resume item, Quick Look-style preview, and
in-app text/title editing. Wayland clipboard monitoring and global shortcuts are
limited by the compositor; document the tested desktop/compositor in any
handoff.

## What core already gives a new shell

- `Clip.pinned`, pinned-first ordering, `Store::set_pinned` and a trim that
  skips pinned clips.
- `Clip.title`, `Store::set_title` and `make_title` for bounded, optional clip
  names that search can match; each shell owns the rename UI and the row menu.
- `Store::open`, `Store::open_at`, insert/dedup/trim/remove/clear, atomic JSON
  saves, corrupt-index quarantine, blob validation and missing-blob recovery.
- Preview normalisation, relative time, human-readable sizes.
- `Settings` and `IgnoredApp` models; `Modifiers` bits independent of any OS.
- `Section` and `filter_indices(clips, section, query)` for Text/Images/Commands.
- `shell_history`: zsh/bash parsing, bounded tail/delta reads and cursor state;
  a shell supplies the paths and its own `ANTIRUSTPASTE_SHELL_HISTORY_DIR`-style
  override.
- `ThumbnailService` with one worker, pending-job de-duplication, generation
  epochs, prune and clear; `prune_now(dir, valid)` for startup.
- `ClearScope`, `Store::count_scope` and `Store::clear_scope` for
  all/text/images/unpinned text/unpinned images without ever matching commands.
- `StorageUsage` and `Store::storage_usage` for bounded history-directory
  accounting; the platform shell adds its cache size.
- `antirustpaste_imaging::info` and `thumbnail_png` with 32 MP/512 px caps.

## What every shell still has to implement

- A tray item and a popup lifecycle that matches the OS conventions.
- Clipboard change detection and format conversion to `Captured`.
- Writing a `Clip` back in the best native format.
- Global hot key registration and a display string for the shortcut.
- Synthetic paste plus the OS permission flow.
- Settings persistence and login-item installation.
- A cache directory policy and conversion of PNG bytes to the UI image type.
- Keyboard handling, accessibility, screen/multi-display positioning, and
  click-away behaviour.
- Optional: a private loopback HTTP control endpoint and MCP bridge. Only the
  macOS shell currently implements this; a shell that adds it must reuse the
  core actions and keep clipboard content out of logs.

## Adding a platform: checklist

- [ ] Create `apps/antirustpaste-<os>/Cargo.toml` depending on `antirustpaste-core` and
      `antirustpaste-imaging`; add it to workspace `members`.
- [ ] Implement `main` with `--help`/`--version` and the same environment hooks
      where meaningful.
- [ ] Map native clipboard formats ↔ `antirustpaste_core::Captured`/`Clip`; keep the
      25-item and 50 MiB limits.
- [ ] Map native modifier flags ↔ `antirustpaste_core::Modifiers`; migrate old
      settings if needed.
- [ ] Reuse core filtering, storage and thumbnail worker; do not duplicate them.
- [ ] Implement tray + popup, including click-to-toggle and click-away.
- [ ] Implement hot key, paste and permission handling.
- [ ] Choose a cache directory and native image loader.
- [ ] Implement settings persistence and login item.
- [ ] Add a smoke script and update `docs/platforms.md`, `README.md` and
      `AGENTS.md`.
- [ ] List tested OS versions/desktops in the handoff.

## Environment variables today

| Variable | Scope | Meaning |
| --- | --- | --- |
| `ANTIRUSTPASTE_DATA_DIR` | core | history directory override |
| `ANTIRUSTPASTE_CONTROL_FILE` | macOS shell | absolute local MCP control endpoint file |
| `ANTIRUSTPASTE_DEFAULTS_SUITE` | macOS shell | isolated `NSUserDefaults` suite |
| `ANTIRUSTPASTE_LAUNCH_AGENT_DIR` | macOS shell | login-item plist directory |
| `ANTIRUSTPASTE_THUMB_CACHE_DIR` | shells | absolute, dedicated thumbnail cache |
| `ANTIRUSTPASTE_SHELL_HISTORY_DIR` | shells | directory holding `.zsh_history`/`.bash_history` |
| `ANTIRUSTPASTE_CONFIG_DIR` | Linux shell | settings JSON directory |
| `ANTIRUSTPASTE_AUTOSTART_DIR` | Linux shell | XDG autostart `.desktop` directory |
| `ANTIRUSTPASTE_OPEN_PREFS` | shells | automation: open Preferences |
| `ANTIRUSTPASTE_OPEN_PANEL` | shells | automation: open popup |
| `ANTIRUSTPASTE_OPEN_IMAGES` | shells | automation: open popup on Images |
| `ANTIRUSTPASTE_OPEN_COMMANDS` | shells | automation: open popup on Commands |

Future shells should keep `ANTIRUSTPASTE_DATA_DIR` and add equivalents for their
native settings/login-item locations. When `ANTIRUSTPASTE_DATA_DIR` is set
without `ANTIRUSTPASTE_SHELL_HISTORY_DIR`, the macOS shell reads no shell
history, so isolated test runs never fall back to the operator's real files.
