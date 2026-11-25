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

- Pin and unpin with `⌘P`; show a clear pin indicator on pinned rows. A row
  context menu is a follow-up.
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
- Should pinning be available from the popup footer, a row button, or only a
  context menu? Keep the first implementation to context menu + shortcut.
- Do we need manual ordering inside the pinned group, or is newest-first enough?

## Planned: export selected images to PDF

### Goal

Give the operator a dedicated place to select copied images, order them, and
write a PDF to a location they choose.

### UX to implement

- Entry point from the Images section and the status menu, e.g. **Export
  Images to PDF…**.
- A dedicated selection environment (separate window or modal sheet) showing a
  grid of image thumbnails with:
  - checkboxes / multi-selection;
  - Select All and Clear Selection;
  - drag or buttons to reorder the selected images;
  - a preview of page order;
  - page size (A4/Letter), orientation (automatic/portrait/landscape) and
    margin;
  - layout choice: one image per page (default) or a simple grid;
  - **Save PDF…** using `NSSavePanel`/native save dialog.
- The PDF uses the original image blobs, not the thumbnail cache. Show a
  progress indicator and keep the UI responsive for large images.
- Cancel leaves no partial file. A failed write reports the error and keeps the
  selection.
- Never log image contents or the chosen path; do not silently write to a
  hidden location.

### Architecture

- Add `crates/rustclip-pdf` as a pure-Rust library that accepts selected PNG
  bytes, dimensions, page settings and an output writer. It must not depend on
  AppKit and should be unit-testable with small fixtures.
- The platform shell owns the selection window, drag/reorder state, save
  dialog, progress and permission/security-scoped handling.
- Core/store only supplies image clips and blob paths; PDF generation does not
  change history or thumbnails.
- Evaluate `printpdf`/`pdf-writer` for the PDF container and image embedding;
  check licence and MSRV before adding. Alternative for a first macOS-only cut:
  CoreGraphics/PDFKit via `objc2`, but that does not advance the cross-platform
  architecture and should be avoided if a permissive pure-Rust crate fits.

### Acceptance checks

- [ ] Select 3 images, reorder them, save a PDF: pages are in the chosen order
      and the file opens in Preview.
- [ ] Images are embedded from the original blobs, not the 128 px thumbnails.
- [ ] Cancelling the save dialog leaves no file.
- [ ] Large images show progress and do not freeze the popup.
- [ ] An unreadable/missing blob reports an error and skips that image or
      aborts with a clear message (decide and test one).
- [ ] PDF generation works without the app UI (library test) and on every
      platform shell added later.

### Open questions

- One image per page vs. a configurable grid (start with one per page).
- Page size/quality defaults and whether to downsample very large images.
- Include source/app/date captions, or only images?
- Should the selection include only the Images section or also image file URLs?
- Should pinned images be selectable and marked in the grid? (Yes, but pinning
  does not change PDF order unless the operator reorders.)

## Cross-platform notes

- Pinning is a core model/ordering/trim feature; every shell must expose the
  same actions and display the indicator.
- PDF generation belongs in a portable crate; the selection window and save
  dialog belong to each platform shell.
- Do not implement these only in the macOS shell if the core can own the rules.
  Add tests to `rustclip-core` and, once created, `rustclip-pdf`.

## Non-goals for this roadmap

Cloud sync, encryption at rest, video export, OCR, AI image search, and copying
another product's visual identity.
