# Roadmap

> Last reviewed: 2026-11-27 · Current release: 0.4.2

This file records planned features. The implemented behaviour is in
[docs/behavior.md](docs/behavior.md); platform status is in
[docs/platforms.md](docs/platforms.md).

## Planned: Linux port

Status: in progress. The workspace contains `apps/antirustpaste-linux`, a GTK4 +
libadwaita + X11 shell using the shared core, imaging and PDF crates. It builds
on Linux but was not run on the macOS host used for this documentation.

Already implemented in the port:

- [x] StatusNotifierItem tray via `ksni`, with Show History, Preferences,
      Export Images to PDF, Clear All Clippings and Quit.
- [x] GTK4 popup with Text/Images/Commands tabs, search, pinned group,
      row pin/delete controls and keyboard selection.
- [x] GDK clipboard read/write, concealed-item filtering and bounded image
      data.
- [x] X11 global shortcut, active-window detection and synthetic Ctrl+V through
      `x11rb`; graceful degradation when X11 is unavailable.
- [x] XDG settings JSON, autostart `.desktop` file, thumbnail cache and PDF
      export.
- [x] zsh/bash history read on demand.

Remaining:

- [ ] Verify the shell on at least one X11 desktop and document Wayland
      behaviour; do not claim Wayland support without a tested compositor.
- [ ] Add tab-scoped **Clear All** / **Clear Unpinned** and the Preferences
      History tab with disk usage.
- [ ] Add the status-menu pause/resume recording toggle.
- [ ] Add a Quick Look-style preview or an equivalent native viewer.
- [ ] Add in-app text and title editing.
- [ ] Bind the local loopback HTTP control endpoint and ship the MCP bridge on
      Linux, reusing `crates/antirustpaste-control`.
- [ ] Add Linux smoke coverage and platform tests.

## Planned: Windows port

Status: not started. The target is a native Win32 shell using the shared core,
imaging and PDF crates; no new cross-platform UI abstraction is planned before
the port exists.

- [ ] Create `apps/antirustpaste-windows` with a `Shell_NotifyIcon` tray icon
      and `TrackPopupMenu` menu.
- [ ] Clipboard adapter: `AddClipboardFormatListener`, `CF_UNICODETEXT`,
      `CF_DIBV5`/PNG, RTF, HTML and `CF_HDROP`, mapped to `Captured`/`Clip`.
- [ ] Borderless popup with search, Text/Images/Commands tabs, pinning, titles,
      row menu, Quick Look equivalent and keyboard navigation.
- [ ] `RegisterHotKey` global shortcut and `SendInput` Ctrl+V; document any
      permission or focus requirements.
- [ ] JSON settings under `%APPDATA%`, startup entry and thumbnail cache.
- [ ] Shell command history from PowerShell and/or the configured shell, read on
      demand.
- [ ] Scoped clear actions plus the Preferences History tab and disk usage.
- [ ] Pause/resume recording from the tray and the Preferences switch.
- [ ] Local loopback HTTP control endpoint and MCP bridge through
      `antirustpaste-control`.
- [ ] PDF export with a native save dialog.
- [ ] Automated tests, smoke script, README and platform documentation.

## Implemented: independent history limits (0.4.1)

- Text, images and commands have separate budgets: `max_text` (default 200),
  `max_images` (default 200) and `max_commands` (default 500, the bash
  `HISTSIZE` default). Trimming one group no longer deletes rows from another.
- Preferences shows three fields, one under the other. A value is committed on
  Return **and** when the field loses focus, so clicking elsewhere or closing
  the window keeps it.
- A reduction below the current count asks for confirmation when confirmations
  are on, then trims the oldest unpinned rows in that group only.
- An older shared `maxClips` becomes the text and image limit; commands keep the
  larger of that value and 500, so an upgrade never deletes command history.
- MCP still accepts `max_clips` as an alias for all three and exposes
  `max_text`, `max_images` and `max_commands`.

## Implemented: terminal command history (0.3.1)

- A **Commands** tab reads `~/.zsh_history` (extended or plain) and
  `~/.bash_history` only when the tab is opened or **Refresh** (`⌘R`) is pressed.
  There is no background watcher.
- `crates/antirustpaste-core/src/shell_history.rs` owns parsing, bounded
  tail/delta reads and the cursor sidecar; the macOS shell owns the paths and
  `ANTIRUSTPASTE_SHELL_HISTORY_DIR`.
- Commands are `ClipKind::Command` records in the same index, with the shell in
  `source_name`, no blob and an independent trim budget. They support search,
  pinning, delete and `⌘1…⌘9` paste.
- There is no **Clear Commands** button; every clipping clear (including
  **Clear All** and the Preferences **History** tab) keeps commands. Delete a
  command row to remove it.

### Remaining

- [ ] Live updates for zsh without `SHARE_HISTORY`/`INC_APPEND_HISTORY` (would
      require changing the user's shell configuration or another data source).
- [ ] A preference for a custom `$HISTFILE` path and additional shells (fish).
- [ ] An explicit **Clear Commands** action, if the operator asks for one.
- [ ] Show whether a command came from zsh or bash as a distinct glyph.

## Implemented: MCP control

- A bundled `antirustpaste-mcp` stdio bridge exposes the running app through a
  private loopback HTTP endpoint (`127.0.0.1`, ephemeral port). The app publishes
  `control.json` with the port, bearer token and pid, mode `0600`, and services
  requests from the existing 0.1 s main-thread timer. There is no new thread.
- Tools cover history listing/reading, base64 blobs, copy/pin/delete, scoped
  clears, title and text edits, settings, ignored apps, popup/search/selection,
  Quick Look, PDF export and quit.
- `crates/antirustpaste-control` owns the protocol, endpoint discovery, HTTP
  framing and client. MCP calls reuse the UI actions and the same storage rules.
- `make-app.sh` copies and signs the bridge into
  `AntiRustPaste.app/Contents/MacOS/antirustpaste-mcp`.

### Acceptance checks

- [x] The app creates a private loopback endpoint and removes its file on normal quit.
- [x] The MCP bridge answers `initialize`, `tools/list` and `tools/call`.
- [x] Session, action and UI tools dispatch on the main thread without holding a
      `with_state` borrow across AppKit.
- [x] Requests and responses are bounded; clipboard content is never logged.
- [x] The app bundle contains a signed MCP bridge binary.

## Implemented: recording pause and scoped clearing (0.4.0)

- **Pause Recording** / **Resume Recording** in the status menu toggles the same
  saved flag as the Preferences checkbox. The menu title and the checkbox stay in
  sync, and resuming starts from the current pasteboard change count so a copy
  made while paused is never captured.
- The Text and Images footers expose **Clear All** and **Clear Unpinned** for the
  active tab. **Clear All** includes pinned rows; **Clear Unpinned** keeps them.
  The Commands tab has no clear action.
- The **History** tab in Preferences offers **Delete All Clippings**, **Delete
  All Images**, **Delete All Text**, **Delete Unpinned Images** and **Delete
  Unpinned Text**. It also reports disk usage for the history files and the
  temporary preview cache. There is no separate status-menu shortcut for it.
- The footer action buttons are right-aligned on every tab.
- `antirustpaste_core::ClearScope` owns the platform-neutral rules; a command
  never matches any scope. Every action removes the blobs and cached thumbnails
  of the rows it deletes and follows *Confirm before deleting*.

### Acceptance checks

- [x] Pinned rows survive **Clear Unpinned** and the unpinned History actions.
- [x] **Clear All** and the non-unpinned History actions remove pinned rows.
- [x] Terminal commands are untouched by every clipping clear.
- [x] Removed image blobs and thumbnails are deleted; the cache is cleared by
      **Delete All Clippings**.
- [x] The menu title and Preferences checkbox reflect one recording flag;
      nothing is captured while paused.
- [x] The History tab reports history and cache disk usage and updates after a
      delete.
- [x] Core unit tests cover all five scopes and storage accounting.

## Implemented: pinned clippings (0.1.0)

Status: the core data model, ordering, trim exemption, `⌘P` toggle and the
right-side row marker are implemented. The row context menu now exposes the same
actions plus rename, copy, delete and Quick Look.

### Goal

Let the operator pin selected text or image clippings so they stay at the top of
their section and are protected from automatic trimming. A pinned clipping is
removed only by an explicit delete or clear action.

### Behaviour to implement

- Pin and unpin with the clickable marker at the right of each row or `⌘P`;
  show a clear filled marker on pinned rows. The row context menu also toggles
  the pin.
- Pinned clips appear in a pinned group before unpinned clips in the Text,
  Images and Commands sections. Within each group, newest first.
- `Store::trim` never removes a pinned clip. The configured maximum applies to
  unpinned clips; pinned clips may make the total exceed it intentionally.
- Re-copying content that is already pinned moves it to the top of the pinned
  group and keeps `pinned = true`.
- Deleting one row removes it even if pinned, after the normal confirmation.
- **Clear All** removes pinned clips in the active tab after confirmation;
  **Clear Unpinned** leaves pinned clips in place. Pinned commands are kept by
  every clipping clear and are deleted row by row. Pinning protects against
  automatic eviction, not against an explicit clear.
- Search and section filtering include pinned clips. The pin indicator remains
  visible when a pinned row is selected.
- Pinned rows sit in a rounded card under a **PINNED** header, with a **RECENT**
  header before the unpinned groups; both headers scroll with the list.
- The pin state survives relaunch and a corrupt/missing history uses
  `pinned = false`.
- The macOS menu/bar and future shells share the same core ordering and trim
  rules.

### Data model

- Add `pinned: bool` with `#[serde(default)]` to `antirustpaste_core::Clip` (and
  `Captured` if capture needs an initial value), so existing histories load
  without migration.
- Add ordered access helpers in core, e.g. `Store::ordered_indices(section)` or
  a partition step, so every shell uses the same pinned-first rule.
- Keep `hash` dedup semantics: pinning is metadata, not part of the content
  hash.
- Trimming code must skip pinned records and delete blobs only for records it
  actually removes.

### Acceptance checks

- [x] Pin a text clip, copy new text repeatedly past the limit: pinned row
      stays, oldest unpinned rows disappear.
- [x] Pin/unpin via `⌘P`; pinned ordering survives section changes and search.
- [x] Re-copy a pinned clip: one row remains, still pinned and at the top.
- [x] Existing history JSON without `pinned` loads unchanged.
- [x] Pin/unpin persists across relaunch.
- [x] Clear All / Clear Unpinned remove the expected rows after confirmation;
      every clipping clear keeps pinned commands.
- [x] Row context menu for pin/paste/delete (follow-up).

### Open questions

- Do we need manual ordering inside the pinned group, or is newest-first enough?

## Implemented: custom clip titles and row context menu (0.4.0)

- `Clip` has an optional, bounded `title` (additive JSON, default absent). A
  titled row draws the title followed by an **Edited** badge, keeps the source
  app icon, name and time on the second line, and includes the title in live
  search. Hovering a row shows the original content in a floating callout.
- **Set Title…** in the row context menu or `⌘⇧R` edits the title; an empty value
  clears it. Commands cannot be renamed.
- Right-clicking a row opens pin/unpin, copy, copy as plain text (rich clips
  only), edit text, rename, Quick Look and delete for that row; **Paste**
  appears only when automatic pasting is enabled. Items that do not apply are
  disabled.
- The menu targets the row under the pointer, not the selection; dismissing it
  leaves the selection and scroll position unchanged.
- Re-copying the same content keeps the title and the pin.

### Remaining

- [ ] Optional title for shell commands, if the operator wants it.

## Implemented: text editing (0.4.0)

- **Edit Text…** in the row menu and `⌘⇧E` open a scrollable plain-text editor
  with Save/Cancel for text and command clips.
- `Store::set_text` recomputes preview, byte length and hash and refuses empty,
  oversized (>1 MiB) and duplicate text. The clip keeps its id, source app,
  capture time, title and pin.
- Text previews use a stable per-clip `.txt` file. Quick Look's **Open with
  TextEdit** edits it; the 1 s poll imports the saved text, bounded to 1 MiB and
  valid UTF-8. Up to eight watched files stay in the private, boot-cleared cache.
- Rich text, HTML, images and files remain outside the first slice.

## Image export to PDF (v0.1)

### Implemented

- Entry points: the **PDF…** footer button on the Images tab, **Export Images to
  PDF…** in the status menu, and `⌘E` while the popup is open.
- A dedicated export window lists image clips in history order with checkboxes,
  drag reordering (the list order is the page order), **Select All** (⌘A),
  **Select None** (⌘D), **Save PDF…** (⌘S) and **Cancel**/esc.
- The system save dialog (`NSSavePanel`) writes the file; the default name is
  `Clipboard Images.pdf`.
- `crates/antirustpaste-pdf` is a pure-Rust, `#![forbid(unsafe_code)]` writer: one A4
  page per image, 12 mm margins, aspect ratio preserved, original blobs decoded
  through `antirustpaste-imaging`, RGB samples compressed with Flate. Images larger
  than the printable area are scaled to fit; smaller ones keep their native
  size.
- The PDF is written to `<name>.pdf.tmp` and renamed into place; cancelling the
  dialog or a failure leaves no partial file.
- Unit tests cover the container, cross-reference offsets, page count, upright
  image transform and the atomic destination write.

### Remaining

- [ ] Page size (A4/Letter), orientation and margin options.
- [ ] Progress indicator and off-main-thread encoding (a second worker thread
      needs the operator's approval).
- [ ] Downsampling very large images to keep PDFs small.
- [ ] EXIF orientation.
- [ ] Optional captions with source app and date.
- [ ] Include image file URLs that currently live in the Text tab.

### Acceptance checks

- [x] Select images, drag them into order and save a PDF; pages follow the
      list order and the file opens in Preview (verified by rendering a
      generated file with PDFKit).
- [x] Images are embedded from the original blobs, not the 128 px thumbnails.
- [x] Cancelling the save dialog leaves no file.
- [x] A missing or unreadable blob aborts with a clear message and no file.
- [x] PDF generation works without the app UI (library test).
- [ ] Large selections show progress and do not freeze the popup.

### Open questions

- One image per page vs. a configurable grid (one per page for now).
- Default page size and whether to downsample very large images.
- Captions only, or images plus a title page?
- Should pinned images sort first in the PDF, or always follow history order?

## Quick Look preview

- Space (empty search) or `⌘Y` opens the selected row from any tab in the shared
  Quick Look panel; Space/Esc closes it and returns focus.
- Text, rich text, HTML, image and command clips are written to private files
  under the thumbnail cache and removed when the preview closes or the app
  quits. File clips preview their existing target directly.
- Text previews are bounded at 8 MiB; blob reads keep the `Store` limit.

### Remaining

- [ ] Page through several selected images in one Quick Look session.
- [ ] Zoom to the source row (Quick Look's zoom animation).

## Cross-platform notes

- Pinning is a core model/ordering/trim feature; every shell must expose the
  same actions and display the indicator.
- PDF generation belongs in a portable crate; the selection window and save
  dialog belong to each platform shell.
- Do not implement these only in the macOS shell if the core can own the rules.
  Add tests to `antirustpaste-core` and `antirustpaste-pdf`.
- The Linux GTK4/X11 shell uses the same core but does not yet bind the MCP
  control endpoint or expose the scoped clear actions; see
  [docs/platforms.md](docs/platforms.md).

## Non-goals for this roadmap

Cloud sync, encryption at rest, video export, OCR, AI image search, and copying
another product's visual identity.
