# Roadmap

> Last reviewed: 2026-11-25 · Current release: 0.1.0

This file records planned features. The implemented behaviour is in
[docs/behavior.md](docs/behavior.md); platform status is in
[docs/platforms.md](docs/platforms.md).

## Implemented: pinned clippings (0.1.0)

Status: the core data model, ordering, trim exemption, `⌘P` toggle and row pin
glyph are implemented. A row context menu (pin/paste/delete on right-click) is a
follow-up, not required for the core guarantee.

### Goal

Let the operator pin selected text or image clippings so they stay at the top of
their section and are protected from automatic trimming. A pinned clipping is
removed only by an explicit delete or clear action.

### Behaviour to implement

- Pin and unpin with the clickable pin icon at the right of each row or `⌘P`;
  show a clear pin indicator on pinned rows. A row context menu is a follow-up.
- Pinned clips appear in a pinned group before unpinned clips in both the Text
  and Images sections. Within each group, newest first.
- `Store::trim` never removes a pinned clip. The configured maximum applies to
  unpinned clips; pinned clips may make the total exceed it intentionally.
- Re-copying content that is already pinned moves it to the top of the pinned
  group and keeps `pinned = true`.
- Deleting one row removes it even if pinned, after the normal confirmation.
- **Clear Text**, **Clear Images** and **Clear All Clippings** remove pinned
  clips in their scope after confirmation; pinning protects against automatic
  eviction, not against an explicit clear.
- Search and section filtering include pinned clips. The pin indicator remains
  visible when a pinned row is selected.
- Pinned rows sit in a rounded card under a **PINNED** header, with a **RECENT**
  header before the unpinned groups; both headers scroll with the list.
- The pin state survives relaunch and a corrupt/missing history uses
  `pinned = false`.
- The macOS menu/bar and future shells share the same core ordering and trim
  rules.

### Data model

- Add `pinned: bool` with `#[serde(default)]` to `rustclip_core::Clip` (and
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
- [x] Clear Text / Clear Images / Clear All remove pinned clips after
      confirmation.
- [ ] Row context menu for pin/paste/delete (follow-up).

### Open questions

- Maximum count: should pinned clips be excluded from the limit (recommended)
  or count against it while being protected from eviction?
- Should pinning also be available from a row context menu? The row pin icon
  and `⌘P` are the current entry points.
- Do we need manual ordering inside the pinned group, or is newest-first enough?

## Image export to PDF (v0.1)

### Implemented

- Entry points: the **PDF…** footer button on the Images tab, **Export Images to
  PDF…** in the status menu, and `⌘E` while the popup is open.
- A dedicated export window lists image clips in history order with checkboxes,
  drag reordering (the list order is the page order), **Select All** (⌘A),
  **Select None** (⌘D), **Save PDF…** (⌘S) and **Cancel**/esc.
- The system save dialog (`NSSavePanel`) writes the file; the default name is
  `Clipboard Images.pdf`.
- `crates/rustclip-pdf` is a pure-Rust, `#![forbid(unsafe_code)]` writer: one A4
  page per image, 12 mm margins, aspect ratio preserved, original blobs decoded
  through `rustclip-imaging`, RGB samples compressed with Flate. Images larger
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

## Quick Look preview (v0.1)

- Space on the Images tab (empty search) or `⌘Y` opens the selected image in
  the shared Quick Look panel; Space/Esc closes it and returns focus.
- The full image is written to a private file under the thumbnail cache and
  removed when the preview closes or the app quits.

### Remaining

- [ ] Preview text, rich text and image file URLs from the Text tab.
- [ ] Page through several selected images in one Quick Look session.
- [ ] Zoom to the source row (Quick Look's zoom animation).

## Cross-platform notes

- Pinning is a core model/ordering/trim feature; every shell must expose the
  same actions and display the indicator.
- PDF generation belongs in a portable crate; the selection window and save
  dialog belong to each platform shell.
- Do not implement these only in the macOS shell if the core can own the rules.
  Add tests to `rustclip-core` and `rustclip-pdf`.

## Non-goals for this roadmap

Cloud sync, encryption at rest, video export, OCR, AI image search, and copying
another product's visual identity.
