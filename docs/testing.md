# Testing

> Last reviewed: 2026-11-25 · Version 0.3.1

AntiRustPaste is a macOS GUI app, but most of its logic is testable without a screen.
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
| `antirustpaste-core` | `store` | Preview, dedup, pinned ordering/trim exemption, trim/clear blobs, corrupt index, missing blobs, legacy records, blob traversal, command upsert and independent budgets |
| `antirustpaste-core` | `settings` | Ignored-app dedup/sort, platform-neutral modifier bits |
| `antirustpaste-core` | `filter` | Text/Images/Commands sections, query across preview/text/source |
| `antirustpaste-core` | `shell_history` | zsh/bash parsing, timestamps, tail/delta reads, truncation/rewrite fallback, partial lines, oversized/invalid input, cursor persistence |
| `antirustpaste-core` | `thumbnails` | Cache naming, prune, blob→thumbnail generation, background worker result, clear |
| `antirustpaste-imaging` | codec | PNG/TIFF/JPEG metadata, aspect-preserving thumbnails, no upscaling, hostile inputs, dimension caps, RGB decode with alpha composited onto white |
| `antirustpaste-pdf` | export | Empty selection, invalid image, one page per image, upright image transform, cross-reference offsets point at every object |
| `antirustpaste` (macOS) | `settings` | Legacy NSEvent modifier migration |
| `antirustpaste` (macOS) | `hotkey` | Shortcut formatting, Carbon modifier mapping |
| `antirustpaste` (macOS) | `thumbnails` | Unsafe cache roots, filesystem-safe user key |
| `antirustpaste` (macOS) | `app` | PDF destination is replaced atomically; a missing directory is reported without leaving a file; drag moves shift rows in both directions; order keeps drags and appends new images; list layout headers, card, hit-testing and visible range |
| `antirustpaste` (macOS) | `update` | Version parsing and ordering, release JSON and asset selection, running/older/draft releases ignored, checksum lookup, bundle-root detection, translocation |

Tests that touch the filesystem use `std::env::temp_dir()` plus a UUID and remove
their directory. They never call AppKit or touch the pasteboard. The PDF tests
check the container bytes, object offsets and page transforms without an
external renderer; during development the output was also rendered with PDFKit
to confirm page size, margins, colour and upright orientation.

## Isolated launch smoke test

```sh
./scripts/smoke.sh
ANTIRUSTPASTE_SMOKE_PREFS=1 ./scripts/smoke.sh   # also opens Preferences
ANTIRUSTPASTE_SMOKE_PANEL=1 ./scripts/smoke.sh   # also opens the history popup
ANTIRUSTPASTE_SMOKE_IMAGES=1 ./scripts/smoke.sh  # opens the popup on Images
ANTIRUSTPASTE_SMOKE_COMMANDS=1 ./scripts/smoke.sh # opens Commands with synthetic history
```

The script sets all isolation variables (history, defaults, login item,
thumbnail cache and shell history), starts the app for three seconds, fails if
the process exits or logs `panicked at`, and kills only the process it started.
The Commands smoke variant writes synthetic `echo antirustpaste-smoke-*` entries
to a scratch directory; the operator's real history is never read. Thumbnail
generation is asynchronous, so the smoke check only proves the UI and worker
start cleanly.

## Manual GUI matrix

Ask the operator before any check that changes the clipboard, captures the
screen or grants Accessibility. Use a scratch history:

```sh
SCRATCH="$(mktemp -d)"
mkdir -p "$SCRATCH/shell"
printf ': 1700000000:0;echo antirustpaste-smoke-1\n' >"$SCRATCH/shell/.zsh_history"
printf 'echo antirustpaste-smoke-2\n' >"$SCRATCH/shell/.bash_history"
ANTIRUSTPASTE_DATA_DIR="$SCRATCH/data" \
ANTIRUSTPASTE_DEFAULTS_SUITE="com.antirustpaste.test.$$" \
ANTIRUSTPASTE_LAUNCH_AGENT_DIR="$SCRATCH/LaunchAgents" \
ANTIRUSTPASTE_THUMB_CACHE_DIR="$SCRATCH/thumbnails" \
ANTIRUSTPASTE_SHELL_HISTORY_DIR="$SCRATCH/shell" \
./target/debug/antirustpaste
```

Use only synthetic command markers in the scratch history; never copy the
operator's `~/.zsh_history` or `~/.bash_history` into a fixture.

| # | Check | Expected |
| --- | --- | --- |
| 1 | Left-click the menu-bar icon | Popup opens under the icon, search focused |
| 2 | Press Escape | Popup closes |
| 3 | Right-click the icon | Status menu with Preferences/Export Images to PDF…/Clear All Clippings/About/Check for Updates…/Quit |
| 4 | Copy a synthetic text marker | A new row appears within ~1 s, with source app and "just now" |
| 5 | Copy the same marker again | Row count unchanged, marker moves to the top |
| 6 | Type part of the marker | List filters live; footer count updates |
| 7 | Arrow keys + Return | Selected row is written to the clipboard and ⌘V is sent to the previous app |
| 8 | ⌘C on a row | Clipboard changes, popup closes, no synthetic paste |
| 9 | Delete with empty search | Confirmation (if enabled), then row and blob disappear |
| 10 | Clear All Clippings | Confirmation, clippings and thumbnails removed; terminal commands remain |
| 11 | Add an ignored app | New clips from it are not captured; existing ones remain |
| 12 | Change the limit below the current count | Confirmation, oldest rows trimmed |
| 13 | Record a new shortcut | Old one stops working, new one opens the popup |
| 14 | Turn recording off, copy, turn it on | Nothing captured while off; later copies captured |
| 15 | Open Preferences twice | Same window is reused, no duplicate windows |
| 16 | Start at login toggle with scratch `ANTIRUSTPASTE_LAUNCH_AGENT_DIR` | Plist created/removed only in the scratch directory |
| 17 | Copy a PNG/JPEG and a file in Finder | Image appears in Images, the file URL in Text; both paste back |
| 18 | Copy from a password manager | Concealed item is not captured |
| 19 | Switch to Images with the segmented control and ⌘⇧2/⌘⇧1 | Tab changes, search placeholder and footer update, selection resets; tooltips name each icon |
| 19a | Start with synthetic `.zsh_history`/`.bash_history`, open Commands | `zsh`/`bash` rows appear; no `shell-history.json` exists before the tab is opened |
| 19b | Press ⌘1…⌘9 in each tab | The Nth visible row (after search and pinned grouping) is pasted; with fewer rows the extra digits do nothing |
| 19c | Press ⌘⇧1 / ⌘⇧2 / ⌘⇧3 | Text / Images / Commands become active without pasting a row |
| 19d | Append a synthetic line to the scratch `.zsh_history`, press **Refresh** and ⌘R | Only the new command appears; existing rows do not duplicate |
| 19e | Truncate or rewrite the scratch history, press Refresh | The tail is re-read without duplicating or mass-reordering stored commands |
| 19f | Pin a command and search for it | It moves to **PINNED**, filters like a clipping, and survives repeated refreshes |
| 19g | Delete a command row with ⌫, then Refresh | The command stays deleted; it is not re-added by a normal append |
| 19h | **Clear All Clippings** with commands present | Clippings and thumbnails are removed; command rows remain |
| 19i | Commands tab, press ⌘E or Space | No PDF/preview action: those belong to Images |
| 20 | Open Images with an image in history | `IMG` placeholder may appear first; thumbnail replaces it within ~0.1–1 s; cache file exists under `ANTIRUSTPASTE_THUMB_CACHE_DIR` |
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
| 30 | Open About and click **Check for Updates…** | The alert closes; a second alert reports "up to date" or offers the newer release. The UI never freezes while the request runs |
| 31 | Launch a packaged build from `dist/AntiRustPaste.app` with the current version | Startup check is silent when it is the latest release, or offers the newer version once |
| 32 | Launch with `ANTIRUSTPASTE_DISABLE_UPDATE_CHECK=1` | No release request is made and no update alert appears |

The automatic download and bundle swap can only be exercised against a published
newer release; it replaces `dist/AntiRustPaste.app`, so do that only with the
operator's agreement. The staged copy is verified against `SHA256SUMS.txt` and
its `CFBundleIdentifier`/`CFBundleShortVersionString` before the install script
runs.

After a manual run: quit the app, remove the scratch directory, and confirm no
`antirustpaste` process remains. Do not run `pkill -f antirustpaste`; stop the PID you
started.

## Hostile-input cases

Add a regression case here (and a unit test when the code is pure) for every
crash fix.

| Input | Expected |
| --- | --- |
| `history.json` is empty, truncated or not JSON | Renamed to `history.json.bak`, app starts empty, no crash |
| Index larger than 64 MiB | Moved aside, app starts empty |
| `shell-history.json` is empty, truncated or not JSON | Ignored; the next Commands refresh reads the tail |
| Shell history cursor points past the end or at different bytes | Treated as truncation/rewrite; tail read, no crash |
| Shell history line is longer than 16 KiB or contains NUL | Skipped; later lines are still read |
| Shell history contains invalid UTF-8 | Line skipped; no panic |
| Shell history ends with a line without `\n` | Line waits for the next refresh |
| Shell history is one enormous line above the tail window | Skipped; the cursor moves past it |
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
| Target app quits between opening the popup and pasting | Falls back to the last non-AntiRustPaste app |
| Accessibility denied | Clipboard updated, no keystroke, no crash |
| Second instance starts | No shared-memory corruption; hot key may fail to register |
| Quit while a paste timer is pending | Timer is invalidated with the process; no crash |

## Safety rules for tests

- Never point `ANTIRUSTPASTE_DATA_DIR` at the real `Application Support/AntiRustPaste`.
- Never change the real `com.rustclip.app` defaults in a test; use
  `ANTIRUSTPASTE_DEFAULTS_SUITE`.
- Never write to the real `~/Library/LaunchAgents`; use
  `ANTIRUSTPASTE_LAUNCH_AGENT_DIR`. Never run `launchctl`.
- Prefer an absolute, dedicated `ANTIRUSTPASTE_THUMB_CACHE_DIR` pointing at a scratch
  directory; the cache contains private images. Root/temp-root/home and relative
  paths are ignored. Without the override macOS uses
  `/tmp/AntiRustPaste-<user>-<hash>/thumbnails`, which is boot-cleared but still
  readable by the current user during the session.
- Point `ANTIRUSTPASTE_SHELL_HISTORY_DIR` at a scratch directory with synthetic
  `.zsh_history`/`.bash_history` files. Never read the operator's real shell
  history in a test, and never copy command text into a fixture, prompt or log.
  When `ANTIRUSTPASTE_DATA_DIR` is set without this override, the app reads no
  shell history at all.
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
   test (imaging failures belong in `crates/antirustpaste-imaging/src/lib.rs`; cache
   path/pruning/worker failures in `crates/antirustpaste-core/src/thumbnails.rs`,
   macOS image/cache policy in `apps/antirustpaste-macos/src/thumbnails.rs`).
6. If thumbnails stop appearing but the app is alive, check the stderr line
   `could not start thumbnail worker` or `thumbnail generation failed`; the
   worker is the only non-main thread.
6. Re-run `./scripts/check.sh` and `./scripts/smoke.sh`.
