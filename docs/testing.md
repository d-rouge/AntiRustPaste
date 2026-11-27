# Testing

> Last reviewed: 2026-11-27 · Version 0.4.1

AntiRustPaste's primary shell is a macOS GUI app, but most of its logic is
testable without a screen. The Linux shell shares the same core. The goal is
deterministic local checks, with a small, explicit manual matrix for AppKit
behaviour.

## Automated checks

```sh
./scripts/check.sh
```

Runs:

1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace` (macOS shell + core + imaging + PDF + control + MCP)

Current unit tests cover:

| Crate | Area | Behaviour |
| --- | --- | --- |
| `antirustpaste-core` | `store` | Preview, dedup, pinned ordering/trim exemption, title normalisation/round-trip, trim/clear blobs, clear scopes, corrupt index, missing blobs, legacy records, blob traversal, command upsert and independent budgets |
| `antirustpaste-core` | `settings` | Ignored-app dedup/sort, platform-neutral modifier bits, pinned colour defaults and RGB packing |
| `antirustpaste-core` | `filter` | Text/Images/Commands sections, query across preview/text/source/title |
| `antirustpaste-core` | `shell_history` | zsh/bash parsing, timestamps, tail/delta reads, truncation/rewrite fallback, partial lines, oversized/invalid input, cursor persistence |
| `antirustpaste-core` | `thumbnails` | Cache naming, prune, blob→thumbnail generation, background worker result, clear |
| `antirustpaste-imaging` | codec | PNG/TIFF/JPEG metadata, aspect-preserving thumbnails, no upscaling, hostile inputs, dimension caps, RGB decode with alpha composited onto white |
| `antirustpaste-pdf` | export | Empty selection, invalid image, one page per image, upright image transform, cross-reference offsets point at every object |
| `antirustpaste` (macOS) | `settings` | Legacy NSEvent modifier migration |
| `antirustpaste` (macOS) | `hotkey` | Shortcut formatting, Carbon modifier mapping |
| `antirustpaste` (macOS) | `thumbnails` | Unsafe cache roots, filesystem-safe user key, bounded cache-usage scan |
| `antirustpaste` (macOS) | `app` | PDF destination is replaced atomically; a missing directory is reported without leaving a file; drag moves shift rows in both directions; order keeps drags and appends new images; list layout headers, card, hit-testing and visible range |
| `antirustpaste` (macOS) | `update` | Version parsing and ordering, release JSON and asset selection, running/older/draft releases ignored, checksum lookup, bundle-root detection, translocation |
| `antirustpaste-control` | protocol | Base64 vectors, request/response round-trip and optional empty fields |
| `antirustpaste-mcp` | MCP | Tool names are unique and every tool has a schema; initialize echoes the client protocol version |

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

## MCP bridge check

Use a scratch app and a scratch endpoint file; never point an MCP check at the
real history.

```sh
SCRATCH="$(mktemp -d)"
export ANTIRUSTPASTE_DATA_DIR="$SCRATCH/data"
export ANTIRUSTPASTE_CONTROL_FILE="$SCRATCH/control.json"
export ANTIRUSTPASTE_DEFAULTS_SUITE="com.antirustpaste.mcp-test.$$"
export ANTIRUSTPASTE_LAUNCH_AGENT_DIR="$SCRATCH/LaunchAgents"
export ANTIRUSTPASTE_THUMB_CACHE_DIR="$SCRATCH/thumbnails"
export ANTIRUSTPASTE_SHELL_HISTORY_DIR="$SCRATCH/shell"
export ANTIRUSTPASTE_DISABLE_UPDATE_CHECK=1
./target/debug/antirustpaste &
APP_PID=$!
for _ in {1..50}; do
  [ -f "$ANTIRUSTPASTE_CONTROL_FILE" ] && break
  sleep 0.1
done
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"status","arguments":{}}}' \
  | ./target/debug/antirustpaste-mcp
kill "$APP_PID"
wait "$APP_PID" 2>/dev/null || true
```

The expected responses are an `initialize` result, a non-empty tool list and a
`status` result for the scratch app. Do not run `list_clips`, `get_clip` or
`copy_clip` in this automated check; add synthetic clips to the scratch history
first if those paths need manual coverage.

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
| 3 | Right-click the icon | Status menu with Pause/Resume Recording, Preferences/Export Images to PDF…/About/Check for Updates…/Quit |
| 4 | Copy a synthetic text marker | A new row appears within ~1 s, with source app and "just now" |
| 5 | Copy the same marker again | Row count unchanged, marker moves to the top |
| 6 | Type part of the marker | List filters live; footer count updates |
| 7 | Arrow keys + Return, default preferences | Selected row is written to the clipboard, popup closes, previous app regains focus, no ⌘V and no Accessibility prompt |
| 7a | Enable **Paste after choosing a clipping**, then Return | Accessibility prompt once, then ⌘V is sent to the previous app |
| 8 | ⌘C on a row | Clipboard changes, popup closes, no synthetic paste |
| 9 | Delete with empty search | Confirmation (if enabled), then row and blob disappear |
| 10 | Preferences → **History** → **Delete All Clippings…** | Confirmation, clippings and thumbnails removed; terminal commands remain |
| 11 | Add an ignored app | New clips from it are not captured; existing ones remain |
| 12 | Change one limit below that group's current count, then click another field | The typed value is committed on focus loss, confirmation appears, only that group's oldest rows are trimmed |
| 12a | Change only the Images limit and leave the Text and Commands counts alone | Text clippings and commands keep their previous limits and rows |
| 13 | Record a new shortcut | Old one stops working, new one opens the popup |
| 14 | Turn recording off, copy, turn it on | Nothing captured while off; later copies captured |
| 14a | **Pause Recording** in the status menu, copy, then **Resume Recording** | Menu title and Preferences checkbox agree; the copy made while paused is not captured; later copies are |
| 14b | Toggle recording from Preferences, reopen the status menu | The menu item reads Pause/Resume Recording and matches the checkbox; no checkmark is shown |
| 15 | Open Preferences twice | Same window is reused, no duplicate windows |
| 16 | Start at login toggle with scratch `ANTIRUSTPASTE_LAUNCH_AGENT_DIR` | Plist created/removed only in the scratch directory |
| 17 | Copy a PNG/JPEG and a file in Finder | Image appears in Images, the file URL in Text; both can be pasted back |
| 18 | Copy from a password manager | Concealed item is not captured |
| 19 | Switch to Images with the segmented control and ⌘⇧2/⌘⇧1 | Tab changes, search placeholder and footer update, selection resets; tooltips name each icon |
| 19a | Start with synthetic `.zsh_history`/`.bash_history`, open Commands | `zsh`/`bash` rows appear; no `shell-history.json` exists before the tab is opened |
| 19b | Press ⌘1…⌘9 in each tab | The Nth visible row (after search and pinned grouping) is chosen; with fewer rows the extra digits do nothing |
| 19c | Press ⌘⇧1 / ⌘⇧2 / ⌘⇧3 | Text / Images / Commands become active without pasting a row |
| 19d | Append a synthetic line to the scratch `.zsh_history`, press **Refresh** and ⌘R | Only the new command appears; existing rows do not duplicate |
| 19e | Truncate or rewrite the scratch history, press Refresh | The tail is re-read without duplicating or mass-reordering stored commands |
| 19f | Pin a command and search for it | It moves to **PINNED**, filters like a clipping, and survives repeated refreshes |
| 19g | Delete a command row with ⌫, then Refresh | The command stays deleted; it is not re-added by a normal append |
| 19h | Preferences → **History** → **Delete All Clippings…** with commands present | Clippings and thumbnails are removed; command rows remain |
| 19i | Commands tab, press ⌘E | No PDF action: PDF export belongs to Images |
| 19j | Commands tab, empty search, press Space or ⌘Y | Quick Look shows the selected command's text and closes with Space/Esc |
| 20 | Open Images with an image in history | `IMG` placeholder may appear first; thumbnail replaces it within ~0.1–1 s; cache file exists under `ANTIRUSTPASTE_THUMB_CACHE_DIR` |
| 21 | Clear Thumbnail Cache in Preferences | Thumbnails disappear and regenerate in the background on the next draw; history remains |
| 22 | Delete/clear/trim image clips | Their thumbnail files are pruned; no orphan files remain |
| 22a | **Clear All** on the Text tab with text + image clips | Text/HTML/RTF/file clips removed, pinned included; images remain; button follows confirmation setting |
| 22b | **Clear All** on the Images tab with text + image clips | Images and thumbnails removed, pinned included; text/file clips remain; button follows confirmation setting |
| 22c | **Clear Unpinned** on either tab with pinned and unpinned rows | Only unpinned rows in that tab disappear; pinned rows keep their place; the other tab and commands are untouched |
| 22c1 | Clear buttons with nothing to remove | The relevant button is disabled; no alert or beep |
| 22c2 | Preferences → **History** → **Delete All Images…** | Only images and their thumbnails are removed; text and command rows remain |
| 22c3 | Preferences → **History** → **Delete All Text…** | Text, rich text, HTML and file clippings are removed; images and command rows remain |
| 22c4 | Preferences → **History** → **Delete Unpinned Images… / Delete Unpinned Text…** | Only the matching unpinned rows are removed; pinned rows survive |
| 22c5 | Open Preferences → **History** with an empty section | The matching delete button is disabled; counts and disk usage in the History tab match the list |
| 22c6 | Copy a large image, then open **History**; delete it | The disk-usage line shows history and cache sizes and falls after the delete; **Clear Thumbnail Cache** reduces the cache part |
| 22c7 | Look at the footer on Text, Images and Commands | **Clear All**, **Clear Unpinned**, **PDF…** and **Refresh** share the right edge; no action button floats in the middle |
| 22d | Pin a row with its right-side marker or ⌘P | Marker fills, row moves to the pinned group and survives repeated copies/trim |
| 22e | Unpin a row | Marker becomes hollow; row leaves the pinned group; explicit delete still removes it |
| 22f | History with pinned and unpinned clips | **PINNED** header and tinted rounded card above **RECENT**; filled marker on pinned rows; headers scroll and do nothing when clicked |
| 22k | Change the pinned row background and marker colours in Preferences | The list uses the new colours when reopened; the values survive relaunch and the text stays readable in light and dark mode |
| 22l | Change both pinned colours, then press **Restore Defaults** | Both colour wells return to the warm-yellow card and deep violet marker, and the history list matches after reopening |
| 22g | Click the first row of each group after scrolling | Selection and pin toggle hit the row under the pointer, not a header |
| 22h | Scroll a long list to the end and back | Rows and the pinned card stop at the list edges; nothing paints over the search field or the footer |
| 22i | Any tab, empty search, press Space | Quick Look opens on the selected row; text shows its text, an image sizes itself to the picture, a file shows its target; the popup stays visible underneath |
| 22j | Press Space or Esc in Quick Look, then Space again | Preview closes, a temporary file disappears and the popup takes key back so Space opens it again |
| 22k | Type a query containing a space in the Text or Images tab | Space is typed into the search field instead of opening Quick Look |
| 22l | Preview a text, rich text, HTML and image row, quit the app | No file remains under `<thumbnail cache>/previews/`; previewing a file row leaves the user's file untouched |
| 22m | Right-click a text row | Context menu with Pin/Unpin, Copy, Edit Text…, Set Title…, Quick Look, Delete; **Copy as Plain Text** is absent because the row is already plain text; **Paste** appears only when automatic pasting is enabled; the row under the pointer is the target |
| 22m1 | Right-click a rich text or HTML row and choose **Copy as Plain Text**, then paste into a rich editor | The paste contains only the stored plain text without formatting; the history row keeps its type and preview |
| 22m2 | Quick Look a text row and a command row | The panel shows the full stored text, not the truncated row preview; closing removes the temporary `.txt` file |
| 22m3 | Quick Look an RTF row and an RTFD row | RTF keeps its formatting; RTFD shows its plain-text fallback because the pasteboard wrapper is not a flat document |
| 22m4 | Quick Look an HTML row | The panel renders the stored HTML page; the app itself makes no network request |
| 22m5 | Quick Look a file row copied from Finder, then delete or move its target and reopen the menu | The panel previews the original file or folder first; afterwards the Quick Look item is disabled and ⌘Y only beeps, leaving no stale preview file |
| 22m6 | Quick Look a text clip larger than 8 MiB | The preview opens promptly and is cut on a character boundary; the history and clipboard keep the full text |
| 22m7 | Choose **Edit Text…** or press ⌘⇧E on a text row, change the text and Save | The row preview and size update, search finds the new words, and pin, title, source icon and capture time stay; the clipboard still holds the old value until the row is copied |
| 22m8 | Open **Edit Text…**, change the text and Cancel | The stored clip, preview and clipboard are unchanged |
| 22m9 | Save empty/whitespace-only text, or text that matches another row | An explanatory alert appears and the stored clip is unchanged; the other row is not removed or merged |
| 22m10 | Quick Look a text row, click the system **Open with TextEdit** button, edit and save | Within about a second the clip text, preview and size update; reopening Quick Look shows the new text and the temporary file stays under `<thumbnail cache>/previews/` until quit, cache clear or watch eviction |
| 22m11 | Edit a command with **Edit Text…**, then refresh the Commands tab | The edited row is saved; the original command from the history file may be added back as a separate row |
| 22m12 | Right-click a rich text, HTML, image or file row | **Edit Text…** is disabled; ⌘⇧E only beeps |
| 22m13 | Edit Text… on a text clip above 1 MiB | The editor is not opened; an alert explains the limit and the clip is unchanged |
| 22n | Choose **Set Title…** for a text clip, save a name, then search for the name and for the original words | The first line shows the name followed by an **Edited** badge, the second line keeps the app icon and time, and both searches match; the clipboard content is unchanged |
| 22o | Re-copy the named clip, quit and relaunch | One row remains, the name survives, and clearing the name restores the preview |
| 22p | Right-click an image row and choose **Quick Look** | The preview opens for the image under the pointer; the menu and popup leave the selection unchanged |
| 22q | Open a row menu and press Escape | Menu closes, selection and scroll position are unchanged, the search field keeps focus |
| 22r | Move the pointer over the list rows and out of the list | A subtle highlight follows the row under the pointer without changing the selection; leaving the list removes it |
| 22s | Rest the pointer on a row for about 0.6 s | A floating callout shows the original content, the source-app icon when available, and `app · kind · size`; it ignores clicks, closes when the pointer leaves, and never takes focus |
| 22t | Look at rows captured from installed apps and from commands | The subtitle starts with the app icon when the bundle id resolves; commands and unknown apps keep the text without an icon |
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
| 33 | Start the app with a scratch data dir, then call MCP `status` | PID/version/recording/section/counts/storage come from the running scratch app |
| 34 | MCP `list_clips` on a scratch history, then `copy_clip` on a synthetic text row | The tool returns that row only; the clipboard ends with the synthetic marker and no real history is read |
| 35 | MCP `clear_clips` with `scope: "unpinned_text"` on scratch data | Unpinned text rows disappear, pinned rows and commands remain, and the Preferences History counts/disk usage update |

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
| Quick Look file target vanished, relative or special | Beep; the menu item is disabled; no stale preview file |
| Quick Look text clip larger than 8 MiB | Preview truncated on a UTF-8 boundary; history and clipboard keep the full text |
| TextEdit saves empty, invalid UTF-8, duplicate or over 1 MiB | Not imported; the stored clip and other rows stay unchanged |
| Watched preview file is replaced by a symlink or directory | The watch is dropped and a symlink is removed; no outside file is read |
| Edited text is re-copied | Dedup uses the new hash; no second row appears |
| Preview field is extremely long | Truncated to 200 characters on load; drawing stays bounded |
| Title field is extremely long or contains control characters | Normalised to one bounded line (160 characters) on load and when saved; drawing stays bounded |
| Text clip larger than 50 MiB | Skipped |
| Image/data blob larger than 50 MiB or empty | Skipped |
| Pasteboard has only concealed/transient types | Whole change ignored |
| Pasteboard has 100 supported items | At most 25 captured |
| Malformed `ignoredApps` JSON in defaults | Treated as an empty list |
| Negative/huge `maxTextClips`, `maxImageClips` or `maxCommandClips` in defaults | Clamped to 1–9999 at load; a legacy `maxClips` migrates to text and images, and commands start at 500 or the old value if it was higher |
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
