# Testing

> Last reviewed: 2026-11-25 · Version 0.1.0

RustClip is a macOS GUI app, but most of its logic is testable without a screen.
The goal is deterministic local checks, with a small, explicit manual matrix for
AppKit behaviour.

## Automated checks

```sh
./scripts/check.sh
```

Runs:

1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace` (macOS shell + core + imaging + PDF)

Current unit tests cover:

| Crate | Area | Behaviour |
| --- | --- | --- |
| `rustclip-core` | `store` | Preview, dedup, pinned ordering/trim exemption, trim/clear blobs, corrupt index, missing blobs, legacy records, blob traversal |
| `rustclip-core` | `settings` | Ignored-app dedup/sort, platform-neutral modifier bits |
| `rustclip-core` | `filter` | Text/Images sections, query across preview/text/source |
| `rustclip-core` | `thumbnails` | Cache naming, prune, blob→thumbnail generation, background worker result, clear |
| `rustclip-imaging` | codec | PNG/TIFF/JPEG metadata, aspect-preserving thumbnails, no upscaling, hostile inputs, dimension caps, RGB decode with alpha composited onto white |
| `rustclip-pdf` | export | Empty selection, invalid image, one page per image, upright image transform, cross-reference offsets point at every object |
| `rustclip` (macOS) | `settings` | Legacy NSEvent modifier migration |
| `rustclip` (macOS) | `hotkey` | Shortcut formatting, Carbon modifier mapping |
| `rustclip` (macOS) | `thumbnails` | Unsafe cache roots, filesystem-safe user key |
| `rustclip` (macOS) | `app` | PDF destination is replaced atomically; a missing directory is reported without leaving a file; drag moves shift rows in both directions; order keeps drags and appends new images; list layout headers, card, hit-testing and visible range |

Tests that touch the filesystem use `std::env::temp_dir()` plus a UUID and remove
their directory. They never call AppKit or touch the pasteboard. The PDF tests
check the container bytes, object offsets and page transforms without an
external renderer; during development the output was also rendered with PDFKit
to confirm page size, margins, colour and upright orientation.

## Isolated launch smoke test

```sh
./scripts/smoke.sh
RUSTCLIP_SMOKE_PREFS=1 ./scripts/smoke.sh   # also opens Preferences
RUSTCLIP_SMOKE_PANEL=1 ./scripts/smoke.sh   # also opens the history popup
RUSTCLIP_SMOKE_IMAGES=1 ./scripts/smoke.sh  # opens the popup on Images
```

The script sets all four isolation variables (history, defaults, login item and
thumbnail cache), starts the app for three seconds, fails if the process exits
or logs `panicked at`, and kills only the process it started. It does not touch
the clipboard, screen, TCC or LaunchAgents. Thumbnail generation is asynchronous,
so the smoke check only proves the UI and worker start cleanly.

## Manual GUI matrix

Ask the operator before any check that changes the clipboard, captures the
screen or grants Accessibility. Use a scratch history:

```sh
SCRATCH="$(mktemp -d)"
RUSTCLIP_DATA_DIR="$SCRATCH/data" \
RUSTCLIP_DEFAULTS_SUITE="com.rustclip.test.$$" \
RUSTCLIP_LAUNCH_AGENT_DIR="$SCRATCH/LaunchAgents" \
RUSTCLIP_THUMB_CACHE_DIR="$SCRATCH/thumbnails" \
./target/debug/rustclip
```

| # | Check | Expected |
| --- | --- | --- |
| 1 | Left-click the menu-bar icon | Popup opens under the icon, search focused |
| 2 | Press Escape | Popup closes |
| 3 | Right-click the icon | Status menu with Preferences/Export Images to PDF…/Clear All Clippings/About/Quit |
| 4 | Copy a synthetic text marker | A new row appears within ~1 s, with source app and "just now" |
| 5 | Copy the same marker again | Row count unchanged, marker moves to the top |
| 6 | Type part of the marker | List filters live; footer count updates |
| 7 | Arrow keys + Return | Selected row is written to the clipboard and ⌘V is sent to the previous app |
| 8 | ⌘C on a row | Clipboard changes, popup closes, no synthetic paste |
| 9 | Delete with empty search | Confirmation (if enabled), then row and blob disappear |
| 10 | Clear All Clippings | Confirmation, text and images removed, all blobs and thumbnails removed |
| 11 | Add an ignored app | New clips from it are not captured; existing ones remain |
| 12 | Change the limit below the current count | Confirmation, oldest rows trimmed |
| 13 | Record a new shortcut | Old one stops working, new one opens the popup |
| 14 | Turn recording off, copy, turn it on | Nothing captured while off; later copies captured |
| 15 | Open Preferences twice | Same window is reused, no duplicate windows |
| 16 | Start at login toggle with scratch `RUSTCLIP_LAUNCH_AGENT_DIR` | Plist created/removed only in the scratch directory |
| 17 | Copy a PNG/JPEG and a file in Finder | Image appears in Images, the file URL in Text; both paste back |
| 18 | Copy from a password manager | Concealed item is not captured |
| 19 | Switch to Images with the segmented control and ⌘1/⌘2 | Tab changes, search placeholder and footer update, selection resets |
| 20 | Open Images with an image in history | `IMG` placeholder may appear first; thumbnail replaces it within ~0.1–1 s; cache file exists under `RUSTCLIP_THUMB_CACHE_DIR` |
| 21 | Clear Thumbnail Cache in Preferences | Thumbnails disappear and regenerate in the background on the next draw; history remains |
| 22 | Delete/clear/trim image clips | Their thumbnail files are pruned; no orphan files remain |
| 22a | **Clear Text** with text + image clips | Text/HTML/RTF/file clips removed; images remain; button follows confirmation setting |
| 22b | **Clear Images** with text + image clips | Images and thumbnails removed; text/file clips remain; button follows confirmation setting |
| 22c | Clear buttons with an empty section | Buttons are disabled; no alert or beep |
| 22d | Pin a row with its right-side pin icon or ⌘P | Pin glyph fills, row moves to the pinned group and survives repeated copies/trim |
| 22e | Unpin a row | Row leaves the pinned group; explicit delete still removes it |
| 22f | History with pinned and unpinned clips | **PINNED** header and rounded card above **RECENT**; accent pin on pinned rows; headers scroll and do nothing when clicked |
| 22g | Click the first row of each group after scrolling | Selection and pin toggle hit the row under the pointer, not a header |
| 22h | Scroll a long list to the end and back | Rows and the pinned card stop at the list edges; nothing paints over the search field or the footer |
| 22i | Images tab, empty search, press Space | Quick Look opens on the selected image and sizes itself to the picture; the popup stays visible underneath |
| 22j | Press Space or Esc in Quick Look, then Space again | Preview closes, the temporary file disappears and the popup takes key back so Space opens it again |
| 22k | Type a query containing a space in the Images tab | Space is typed into the search field instead of opening Quick Look |
| 22l | Preview an image, quit the app | No file remains under `<thumbnail cache>/previews/` |
| 23 | Multi-monitor / full-screen app | Popup appears on the status item's screen and above full-screen spaces |
| 24 | Quit and relaunch | History and preferences persist; thumbnails regenerate if the temp cache was purged |
| 25 | Images tab, one or more images, click **PDF…** | Export window lists every image selected; status reads `N of N images selected` |
| 26 | Click a row, then Select All/Select None | Selection and status follow; **Save PDF…** is disabled with nothing selected |
| 26a | Drag a row above and below its neighbours | The drop line follows the pointer; on release the rows shift and the PDF page order matches the list |
| 26b | Press a row and move less than 3 pt | Treated as a click: the checkbox toggles, no reorder |
| 27 | Save PDF… into a scratch folder after reordering and open the file in Preview | One A4 page per image in the dragged order, centred with white margins, original resolution, right way up |
| 28 | Cancel the save dialog, then close the export window | No file is written and the window closes; the app stays usable |
| 29 | Remove a selected blob behind the app, then save | Alert reports the problem, no partial PDF, export window stays open |

After a manual run: quit the app, remove the scratch directory, and confirm no
`rustclip` process remains. Do not run `pkill -f rustclip`; stop the PID you
started.

## Hostile-input cases

Add a regression case here (and a unit test when the code is pure) for every
crash fix.

| Input | Expected |
| --- | --- |
| `history.json` is empty, truncated or not JSON | Renamed to `history.json.bak`, app starts empty, no crash |
| Index larger than 64 MiB | Moved aside, app starts empty |
| Image data is truncated, corrupt or not an image | Imaging crate returns an error; Images row shows `IMG`; clip stays pasteable |
| Image dimensions exceed 32 MP | Rejected before decode; no large allocation |
| Image has zero width or height | Rejected; no thumbnail |
| Thumbnail cache file is corrupt | Deleted/overwritten on the next generation |
| Thumbnail cache directory is read-only | Thumbnail is shown for the session, not cached; no crash |
| Cache contains a path-like or non-hash filename | Ignored by pruning; never executed or opened |
| `blob` is `../secret`, an absolute path or lacks `.bin` | Record dropped; `read_blob`/`delete_blob` refuse it |
| Blob file missing | Record dropped on load |
| Export selected image whose blob vanished | Export aborts with an alert and writes no file |
| Export destination is not writable | Temporary write fails, no partial file, alert |
| Quick Look blob is missing or unreadable | Beep, no preview file, app stays responsive |
| Preview field is extremely long | Truncated to 200 characters on load; drawing stays bounded |
| Text clip larger than 50 MiB | Skipped |
| Image/data blob larger than 50 MiB or empty | Skipped |
| Pasteboard has only concealed/transient types | Whole change ignored |
| Pasteboard has 100 supported items | At most 25 captured |
| Malformed `ignoredApps` JSON in defaults | Treated as an empty list |
| Negative/huge `maxClips` in defaults | Clamped to 1–9999 at load |
| Hot-key combination already registered | Recording fails with an alert; old shortcut restored |
| Target app quits between opening the popup and pasting | Falls back to the last non-RustClip app |
| Accessibility denied | Clipboard updated, no keystroke, no crash |
| Second instance starts | No shared-memory corruption; hot key may fail to register |
| Quit while a paste timer is pending | Timer is invalidated with the process; no crash |

## Safety rules for tests

- Never point `RUSTCLIP_DATA_DIR` at the real `Application Support/RustClip`.
- Never change the real `com.rustclip.app` defaults in a test; use
  `RUSTCLIP_DEFAULTS_SUITE`.
- Never write to the real `~/Library/LaunchAgents`; use
  `RUSTCLIP_LAUNCH_AGENT_DIR`. Never run `launchctl`.
- Prefer an absolute, dedicated `RUSTCLIP_THUMB_CACHE_DIR` pointing at a scratch
  directory; the cache contains private images. Root/temp-root/home and relative
  paths are ignored. Without the override macOS uses
  `/tmp/RustClip-<user>-<hash>/thumbnails`, which is boot-cleared but still
  readable by the current user during the session.
- Never grant or revoke Accessibility, or edit TCC.
- Never use the operator's clipboard contents as a fixture. Use synthetic text.
  Replacing their clipboard is a side effect: ask first.
- `screencapture` captures the whole screen and may contain private data. Do not
  run it unless the operator asked for a screenshot; delete images afterwards.
- Kill only the PID you started. A broad `pkill` can kill the operator's own
  installed copy.
- Do not leave test apps, plists, preference suites or scratch directories
  behind.

## Crash triage

1. Reproduce with `RUST_BACKTRACE=1` and scratch paths.
2. Look for `refcell already borrowed` in the log — a `with_state` borrow was
   held across a re-entrant AppKit call. Clone handles first.
3. Look for a panic in a callback that is not wrapped in `guard`.
4. Check whether the input was bounded (item count, byte size, index size, blob
   name).
5. Add the failing input to the hostile-input table and, where possible, a unit
   test (imaging failures belong in `crates/rustclip-imaging/src/lib.rs`; cache
   path/pruning/worker failures in `crates/rustclip-core/src/thumbnails.rs`,
   macOS image/cache policy in `apps/rustclip-macos/src/thumbnails.rs`).
6. If thumbnails stop appearing but the app is alive, check the stderr line
   `could not start thumbnail worker` or `thumbnail generation failed`; the
   worker is the only non-main thread.
6. Re-run `./scripts/check.sh` and `./scripts/smoke.sh`.
