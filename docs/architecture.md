# Architecture

> Last reviewed: 2026-11-27 · Version 0.4.1

AntiRustPaste is a single-process macOS accessory app. AppKit runs the main loop and
a repeating `NSTimer` polls the pasteboard. There is no webview, database engine
or async runtime. One background worker exists for thumbnails: it reads blobs,
decodes them with `antirustpaste-imaging` and writes cache files; it never touches
AppKit or `AppState`.

The app's only own network requests are for the update check: `NSURLSession`
data tasks for the GitHub Releases API, `SHA256SUMS.txt` and the universal zip.
They use a session whose delegate queue is the main queue, so every callback
stays on the AppKit thread and runs through a panic guard. HTML Quick Look
previews are rendered by the system panel, which may load resources the
document references; the app itself never fetches them. The app never checks for updates in a
`target/` build or in tests; `ANTIRUSTPASTE_DISABLE_UPDATE_CHECK=1` turns it off.
The MCP control bridge is local-only and never opens a network connection.

The repository is a Cargo workspace:

- `apps/antirustpaste-macos` — the primary AppKit shell;
- `apps/antirustpaste-mcp` — the MCP stdio bridge for the macOS shell;
- `apps/antirustpaste-linux` — a GTK4/X11 shell that builds on Linux but was not
  exercised on the macOS host used for this documentation;
- `crates/antirustpaste-control` — the local HTTP control protocol, endpoint
  discovery and client shared by the app and the MCP bridge;
- `crates/antirustpaste-core` — platform-neutral model, storage, settings, filtering
  and thumbnail worker;
- `crates/antirustpaste-imaging` — a pure-Rust, `#![forbid(unsafe_code)]` codec with
  no AppKit, filesystem or environment access;
- `crates/antirustpaste-pdf` — a pure-Rust, `#![forbid(unsafe_code)]` PDF writer for
  the image export, with no AppKit, filesystem or environment access.

Future Windows shells are `apps/antirustpaste-<os>`. They depend on core; core
never depends on a shell. See [platforms.md](platforms.md).

## Module map

| Module | Responsibility |
| --- | --- |
| `apps/antirustpaste-macos/src/main.rs` | `--help`/`--version`; starts `app::run()` |
| `apps/antirustpaste-macos/src/app.rs` | `NSApplicationDelegate`, thread-local `AppState`, status item and menu, clipboard polling, on-demand shell history, hot key, keyboard handling, all user actions, panic guards |
| `apps/antirustpaste-macos/src/views.rs` | Objective-C classes: `RCClipPanel`, `RCClipListView`, `RCIgnoredAppsView`, `RCPanelDelegate`; construction of the popup and Preferences window |
| `apps/antirustpaste-macos/src/pasteboard.rs` | Reads and writes `NSPasteboard` with type/size limits |
| `apps/antirustpaste-macos/src/paste.rs` | Optional CoreGraphics ⌘V synthesis and the Accessibility trust prompt |
| `apps/antirustpaste-macos/src/hotkey.rs` | Carbon `RegisterEventHotKey` wrapper and shortcut formatting |
| `apps/antirustpaste-macos/src/settings.rs` | `NSUserDefaults` keys, modifier migration, LaunchAgent management |
| `apps/antirustpaste-macos/src/thumbnails.rs` | Boot-cleared cache path, private directory policy, `NSImage` loading |
| `apps/antirustpaste-macos/src/app/control.rs` | Non-blocking loopback HTTP endpoint, request dispatch and MCP actions on the main thread |
| `apps/antirustpaste-linux/src/` | GTK4/X11 shell: tray, popup, clipboard, X11 hot key/paste, XDG settings; no MCP bridge |
| `apps/antirustpaste-macos/src/update.rs` | Release JSON/version/checksum parsing, bundle detection and the detached install script (no AppKit) |
| `apps/antirustpaste-mcp/src/main.rs` | MCP stdio server, tool schemas and forwarding to the control endpoint |
| `crates/antirustpaste-control/src/lib.rs` | HTTP request/response types, endpoint discovery, client and base64 helper |
| `crates/antirustpaste-core/src/store.rs` | `Clip`/`Captured`, JSON index, blob files, dedup, trim, command upsert, clear scopes, storage usage, previews, titles, text edits, relative time |
| `crates/antirustpaste-core/src/settings.rs` | Platform-neutral `Settings`, `IgnoredApp`, `Modifiers` bits |
| `crates/antirustpaste-core/src/filter.rs` | `Section` (Text/Images/Commands) and search filtering |
| `crates/antirustpaste-core/src/shell_history.rs` | zsh/bash history parsing, bounded tail/delta reads, cursor sidecar |
| `crates/antirustpaste-core/src/thumbnails.rs` | Thumbnail worker, file cache, prune/clear, generation epochs |
| `crates/antirustpaste-imaging` | Pure-Rust image metadata, thumbnails and RGB decoding (`image` crate) |
| `crates/antirustpaste-pdf` | PDF container, A4 page layout and Flate-compressed image embedding |

`app.rs` is the macOS hub. Nothing else calls AppKit directly except `views.rs`
(views), `thumbnails.rs` (loading a finished PNG into `NSImage` on the main
thread) and `pasteboard.rs` (the pasteboard). `antirustpaste-core`,
`antirustpaste-imaging`, `antirustpaste-pdf` and the pure parts of `hotkey.rs` have no
AppKit dependency.
`docs/platforms.md` lists what every future shell must implement.

## Startup

```
main
 └── app::run
      ├── NSApplication::sharedApplication
      ├── activation policy Accessory (no Dock icon)
      ├── AppDelegate retained in a thread-local
      └── applicationDidFinishLaunching
           └── setup_app  (guarded against panics)
                ├── Settings::load
                ├── Store::open, trim to the limit, save
                ├── prune stale thumbnail files
                ├── apply_start_at_login if enabled (rewrites the plist)
                ├── record the initial pasteboard changeCount
                ├── NSStatusItem + status menu
                ├── build popup panel, search field, list, footer, panel delegate
                ├── bind the private MCP loopback HTTP endpoint (failure only disables MCP)
                ├── install AppState in the thread-local (starts the thumbnail worker)
                ├── install key monitor, 1 s pasteboard poll and 0.1 s thumbnail/control timer
                ├── register the Carbon hot key
                └── optional automation hooks (ANTIRUSTPASTE_OPEN_*)
```

No shell-history file is read at startup. The Commands tab reads on demand when
it is selected (or when an automation hook opens it), so startup stays free of
user history files.

Autorelease pools and AppKit objects are created on the main thread. The only
extra thread is the named `antirustpaste-thumbnails` worker started by
`ThumbnailService::new`; every AppKit callback into Rust is still on the main
thread. PDF export does not add a worker: it encodes on the main thread after
the save dialog closes.

## State and threading

`AppState` lives in a `thread_local! { RefCell<Option<AppState>> }`. `with_state`
borrows it mutably for a short critical section.

Two rules keep this safe:

1. **Do not call re-entrant AppKit while holding the borrow.** `orderOut`,
   `makeKeyAndOrderFront`, menu tracking (`popUpStatusItemMenu`), modal alerts,
   `runOpenPanel` and delegate-backed `setStringValue` can all run a nested
   event loop that calls back into Rust. Clone the `Retained` handles, release
   the borrow, then call AppKit. This bug class caused the only known crash in
   0.1.0 development.
2. **Guard every OS entry point.** AppKit action/delegate methods, `NSTimer`
   blocks, the `NSEvent` monitor and the Carbon handler run through
   `app::guard`/`guard_value`. A panic that crosses `extern "C"` aborts instead
   of unwinding; the guard turns it into a logged recovery.

`MainThreadMarker` is obtained with a checked constructor in `run()` and with a
documented unsafe `main_thread()` helper elsewhere, where the AppKit contract
guarantees the thread.

### Thumbnail worker

```
main thread                              antirustpaste-thumbnails worker
-----------                              --------------------------
draw Images row
  ├─ in-memory hit? draw
  ├─ cache file hit? load NSImage, draw
  └─ miss: send Generate(hash, blob)
       pending.insert(hash)
                                      read blob (<= 64 MiB)
                                      antirustpaste_imaging::thumbnail_png
                                      write cache/thumb-<hash>.png atomically
                                      send JobResult(hash, ok, generation)
0.1 s timer: drain
  ├─ ok -> NSImage::initWithData (main)
  └─ err -> IMG placeholder / failed set
```

- The worker only owns `Receiver<Command>` and `Sender<JobResult>`. It does not
  hold `AppState`, `Retained` objects or `MainThreadMarker`.
- `Generate`, `Prune` and `Clear` are queued in order. `Clear` bumps a
  generation so results from before the clear are discarded.
- Pending hashes prevent duplicate jobs while a row is visible.
- A failed job leaves an `IMG` placeholder; **Clear Thumbnail Cache** resets the
  failure set.

## Clipboard flow

```
1 s timer
 └── poll_pasteboard
      ├── NSPasteboard::changeCount unchanged? stop
      ├── remember the frontmost non-AntiRustPaste app
      ├── recording disabled? stop
      ├── pasteboard::read_items
      │    ├── skip org.nspasteboard.ConcealedType / TransientType
      │    ├── up to 25 items
      │    ├── file URL -> ClipKind::File
      │    ├── PNG/TIFF -> ClipKind::Image, blob bytes (<= 50 MiB)
      │    ├── RTFD/RTF/HTML -> rich clip + plain-text fallback (<= 50 MiB)
      │    └── UTF-8/plain text -> ClipKind::Text
      ├── ignored source app? stop
      ├── Store::insert (hash dedup, move to top, write blob)
      ├── trim to the configured limit
      └── save and redraw the list
```

Polling is intentionally simple. `changeCount` is the only change signal; there
is no polling of item contents and no global event tap.

## Shell history flow

```
Commands tab / Refresh button / ⌘R
 └── app::refresh_shell_history
      ├── load <data>/shell-history.json (offset + fingerprint per shell)
      ├── for ~/.zsh_history and ~/.bash_history
      │    ├── cursor valid? read only the appended delta (<= 256 KiB)
      │    └── otherwise read the tail (<= 1 MiB) and keep the newest commands
      ├── Store::upsert_command  (dedup by command text, refresh if newer)
      ├── save history.json and, when it advanced, the cursor sidecar
      └── refresh_ui
```

- `crates/antirustpaste-core/src/shell_history.rs` owns parsing and bounded
  reads; `app.rs` owns the home paths and the on-demand trigger.
  `ANTIRUSTPASTE_SHELL_HISTORY_DIR` redirects the paths in tests.
- zsh extended entries (`: <epoch>:<elapsed>;<command>`) keep their timestamp;
  bash `#<epoch>` comments do too. Untimestamped tail entries use the file
  mtime, and appended lines use the read time.
- A saved cursor is checked against a 64-byte fingerprint. A mismatch (truncate,
  rotation, in-place rewrite) falls back to a tail read. Commands that are
  already stored are only moved when the file carries a newer timestamp, so a
  bulk re-read does not reorder the list.
- Commands are `ClipKind::Command` records with no blob and the shell name in
  `source_name`. `Store::trim_sections` and the insert path keep independent
  budgets for `command`, `image` and the remaining text-like records.
- Nothing runs on a timer. Shell history is read only when the tab is selected,
  the Refresh button is pressed or `⌘R` is used; the files are never watched.
- Bounds: 1 MiB tail, 256 KiB delta, 16 KiB per command, 64 KiB cursor file.
  Invalid UTF-8, empty lines, NUL bytes and oversized lines are skipped.

## Paste flow

```
list row / Enter / menu item
 └── paste_store_index
      ├── clone the Clip and resolve the target NSRunningApplication
      ├── pasteboard::write_clip (clear, then the clip's best type)
      ├── record the new changeCount so the write is not re-captured
      ├── hide the panel
      ├── activate the target app
      ├── stop here unless "Paste after choosing" is enabled
      ├── first time with the preference on: AXIsProcessTrustedWithOptions prompt
      └── 0.25 s one-shot NSTimer -> CGEvent ⌘V down/up
```

The target app is captured when the popup opens (or before the status menu is
shown), not when the paste happens. If the target quit, `resolve_target_app`
falls back to the last non-AntiRustPaste frontmost app by bundle id. With the
preference off, `paste_store_index` returns after activating the target, so the
default path never touches CoreGraphics events or Accessibility.

## Popup and preferences

`RCClipPanel` is a borderless `NSPanel` subclass with
`NSWindowStyleMask::NonactivatingPanel`, level `NSStatusWindowLevel`, join-all-spaces
and full-screen-auxiliary behaviour. Its content is an `NSVisualEffectView`
(popover material) with rounded corners. The history rows are drawn by
`RCClipListView::drawRect:`; only the visible range is painted. An icon-only
segmented control switches between **Text**, **Images** and **Commands**;
⌘⇧1/⌘⇧2/⌘⇧3 do the same, while ⌘1…⌘9 choose the Nth visible row of the active
tab. The search field is the first responder, and a local key monitor handles
arrows, Enter, Escape, Delete, ⌘⇧1–3, ⌘1–9, ⌘C, ⌘P, ⌘⇧R, ⌘E and ⌘R before
AppKit's responder chain. The footer holds **Clear All** and **Clear Unpinned**
buttons on the Text and Images tabs; each maps the active tab plus the pin rule
to a core `ClearScope` and then prunes thumbnails. The action buttons form a
right-aligned cluster: **PDF…** (Images), **Clear All** and **Clear Unpinned**
share the right edge, and **Refresh** alone is right-aligned on Commands. There
is no clear action on the Commands tab: commands are deleted row by row and
every clipping clear keeps them. On the Images tab the footer also holds
**PDF…**, which opens the export window, and the footer hint switches to
`⌘E PDF`. On the Commands tab it holds **Refresh**, and the hint switches to
`⌘R refresh`. `⌘P` toggles the pin of the selected row; the row’s
right-side marker is a hollow circle until pinned and fills when pinned. Pinned
rows sit in a rounded card under a **PINNED** header. The card uses the
configured background colour as a soft tint, and the filled marker uses the
configured marker colour; the defaults are a warm-yellow card and a deep violet
marker, stored in `NSUserDefaults`.
Right-clicking a row opens an `NSMenu` for that row (pin/unpin, copy, paste
when auto-paste is enabled, rename, Quick Look, delete). Items carry the clip id
in `representedObject`, so the action looks the clip up again after the nested
menu run loop returns; the selection and scroll are untouched when the menu is
dismissed. Renaming uses an
`NSAlert` accessory field and stores an optional, bounded title in core. A titled
row draws the title followed by an **Edited** badge with a pencil and keeps the
source-app icon, app name and time on the subtitle line, so the name cannot crowd
out the row metadata. The list view resolves icons for the visible rows before drawing
and installs an `NSTrackingArea` for a subtle hover highlight. After a short
dwell it opens a non-activating floating callout with the original content plus
`source · kind · size` and the same source-app icon. Icons are resolved by
bundle identifier through `NSWorkspace` and cached in `AppState`. The callout
reuses a single lazily built `NSPanel`; it ignores mouse events and is ordered
out when the hover changes, the popup hides, or another surface opens. The
history panel suppresses its resign-key hide while the menu is open and re-keys
the search field afterwards.
`ListLayout` (in `app.rs`) owns the row geometry: header heights, group
offsets, the pinned card, hit-testing and the visible range. Drawing, click
mapping, scrolling and keyboard navigation all go through it, so headers are
gaps rather than clickable rows. `draw_list` clips drawing to the list bounds
explicitly: while the popover scrolls, AppKit can hand this flipped view a dirty
rect that reaches above its frame, and without the clip the pinned card and rows
painted over the search field and footer. Presentation is
deferred by one 50 ms timer after a status-item click: making the panel key
inside the mouse event lets the status bar take key back and immediately fires
`windowDidResignKey`.

Preferences are a regular `NSWindow` with an `NSTabView`:

- **General** — separate text, image and command limits, record toggle,
  confirm-before-delete, start at login, shortcut recorder.
- **Appearance** — pinned-row background and marker colour wells.
- **Exceptions** — ignored apps in an `NSScrollView` with a custom list view and
  Add/Remove buttons.
- **History** — counts plus scoped delete actions backed by the core
  `ClearScope` rules: everything, images, text, unpinned images and unpinned
  text. It also reports disk usage for the history directory (index, blobs and
  sidecars) and the temporary thumbnail/preview cache. `Store::storage_usage`
  and `thumbnails::cache_usage_bytes` bound their directory reads and never
  follow symlinks. Terminal commands are never included; the tab is reached
  through Preferences and has no separate status-menu item. The same recording
  flag is exposed by the status menu as **Pause Recording** / **Resume
  Recording**; both entry points call `set_recording`, which saves the setting
  and updates the checkbox and menu title without holding a state borrow across
  AppKit.

The preferences window is built lazily on first open and kept alive in
`AppState::prefs`; the panel delegate is stored separately because AppKit
delegates are not retained.

## MCP control

```
MCP client
 └── antirustpaste-mcp (stdio, JSON-RPC)
      └── control::call -> <data>/control.json (0600) -> POST /control
           └── 127.0.0.1:<ephemeral port>, Bearer token
                └── app/control.rs poll on the 0.1 s timer
                     ├── parse one bounded HTTP request
                     ├── check Host, bearer token and POST /control
                     ├── dispatch on the main thread with with_state
                     ├── reuse the same actions as menus and keyboard
                     └── queue an HTTP response with Content-Length
```

The app binds a non-blocking `TcpListener` to `127.0.0.1:0` and publishes an
endpoint file next to the history. The file contains the port, a random bearer
token and the app pid, is mode `0600`, and is written atomically. The path can be
moved with `ANTIRUSTPASTE_CONTROL_FILE`, but only an absolute path is accepted.
A stale regular file is replaced at the next start; any other file disables
control instead of being replaced. The listener never binds a LAN interface.

`crates/antirustpaste-control` owns the request/response types, endpoint
discovery, HTTP framing and the client used by `apps/antirustpaste-mcp`. The
bridge owns only the MCP tool schemas and JSON-RPC envelope; it never opens
history files. The app's `app/control.rs` owns the actions and reuses `Store`,
settings, pasteboard, Quick Look, PDF and UI helpers, so MCP and the UI share one
state.

The HTTP surface is deliberately minimal: one `POST /control`, an exact `Host`
check, `Authorization: Bearer`, no CORS headers and `Connection: close`. Header
and body sizes are bounded, the token is stored only in the private endpoint
file, and responses are capped at 16 MiB. A request body is the JSON control
request; a response body is the JSON control response.

`ControlServer::poll` collects requests first, then each request is dispatched
after the `with_state` borrow is dropped, and responses are queued afterwards.
This is the same re-entrancy rule as AppKit callbacks and menu tracking. The
listener and connections are non-blocking; output is buffered per connection and
flushed on later timer ticks. No new thread, dispatch queue or blocking sleep is
introduced.

## Images and thumbnails

```
pasteboard PNG/TIFF/JPEG
 └── pasteboard::read_items
      ├── antirustpaste_imaging::info(bytes) -> width, height (32 MP cap)
      └── Clip { kind: Image, width, height, blob, hash }
           └── thumbnail view (Images tab)
                ├── in-memory HashMap<hash, NSImage> hit -> draw
                ├── cache hit -> NSImage::initWithData -> draw
                └── miss -> queue Generate(hash, blob)
                      └── worker: read blob, thumbnail_png(128),
                                  write <cache>/thumb-<hash>.png,
                                  send JobResult
                           └── 0.1 s drain -> NSImage on main -> redraw
```

- Cache location: an absolute, dedicated `ANTIRUSTPASTE_THUMB_CACHE_DIR`, otherwise on macOS
  `/tmp/AntiRustPaste-<user>-<hash>/thumbnails`. macOS clears `/tmp` at boot, so this
  guarantees the cache is gone after a reboot. Other platforms use the OS temp
  directory. For the default location the directory and its parent are private
  (`0700`) and files are `0600` on Unix.
- Thumbnails are never stored in `history.json`; the original blob is the source
  of truth. Deleting the cache is safe and is exposed as **Clear Thumbnail
  Cache** in Preferences. A legacy cache under `<OS temp>/AntiRustPaste/thumbnails` is
  removed once at startup.
- `thumbnails::prune_now` removes files whose hash is no longer in the history
  at startup (synchronously, before the worker starts). `ThumbnailService::prune`
  queues the same cleanup after delete/clear/limit changes, behind in-flight
  jobs. Inserts prune files without dropping the in-memory images.
- The imaging crate rejects empty data, unsupported formats, zero dimensions and
  more than 32 million pixels. A zero thumbnail edge is an error; edges above
  512 are clamped. It caps a decoded image at roughly 128 MiB of RGBA before
  resizing.
- Only the header/dimension parse (`info`) runs at capture on the main thread.
  Full decode and PNG encoding happen on the worker. The first draw shows an
  `IMG` placeholder; the 0.1 s drain timer installs the image and redraws. The
  in-memory cache is capped at 256 entries (it clears when full). AppKit image
  creation stays on the main thread.

## Image export to PDF

```
PDF… / status menu / ⌘E
  └── app::open_pdf_export
       ├── refuse with an alert when the history has no images
       ├── views::build_pdf_export  (560 pt window, selectable list, Save)
       └── makeKeyAndOrderFront
PDF window
  ├── row click            -> toggle id in PdfExportState.selected
  ├── row drag (>= 3 pt)   -> reorder PdfExportState.order, accent drop line
  ├── Select All/None      -> replace the set
  ├── Save PDF… (⌘S)       -> NSSavePanel::runModal
  │     └── app::save_pdf
  │          ├── read original blobs inside one with_state borrow
  │          ├── antirustpaste_pdf::write_pdf (decode, lay out, deflate)
  │          └── write to `<name>.pdf.tmp`, then rename over the destination
  └── Cancel / esc / close -> close_pdf_export (orders the window out)
```

- Both order and selection are stored by clip id, so new captures cannot
  disturb them. `pdf_sync_order` keeps the dragged order, appends new images
  unselected and drops ids that left the history; `moved_order` performs the
  drag move without indexing past the vector.
- `RCPdfSelectionView` tracks the pressed row, the 3 pt drag threshold and the
  insertion boundary in `Cell`s. `mouseDragged:` updates the drop line and keeps
  the pointer row visible; `mouseUp:` either reorders or toggles.
- `PdfExportParts` retains the window and its controls. The window has no
  delegate: closing it only orders it out, and the next export replaces the
  stored parts. Dropping the window from inside its own close callback would be
  unsafe, so the state is not torn down there.
- The writer emits the catalogue, page tree, one page and content stream per
  image and one `/DeviceRGB` image XObject compressed with Flate. Pages follow
  `PdfExportState.order`. Rows are
  negated in height because image samples start at the top while PDF user space
  grows upwards. Placement scales an image down only when it exceeds the
  printable area; smaller images keep 1 px = 1 pt.
- Encoding runs on the main thread: the input is bounded by the history limit,
  the 50 MiB blob cap and the 32 MP imaging cap, and no second worker is allowed
  without the operator's approval.

## Quick Look preview

```
Space (any tab, empty query) / ⌘Y
  └── app::toggle_quick_look (row menu: toggle_preview_for_clip)
       ├── shared QLPreviewPanel already visible -> close and clean up
       ├── preview_source_for_clip
       │    ├── text / command <= 1 MiB -> TextBytes, stable preview-<id>.txt
       │    ├── text / command > 1 MiB  -> hash-named .txt, read-only
       │    ├── rich text      -> stored blob as "rtf"; RTFD uses its plain fallback
       │    ├── HTML           -> stored blob as "html"
       │    ├── image          -> stored blob, extension from imaging::info
       │    └── file           -> the existing target NSURL, no copy
       ├── temporary kinds: write <thumb cache>/previews/... (0700 dir, 0600 file)
       ├── editable text: watch the file (at most 8 watched paths)
       ├── panel.dataSource/delegate = AppDelegate; reloadData
       └── makeKeyAndOrderFront
QLPreviewPanel
  ├── numberOfPreviewItemsInPreviewPanel:  -> 1
  ├── previewPanel:previewItemAtIndex:     -> the file or target NSURL
  └── previewPanel:handleEvent:            -> Space/Esc close the preview
1 s poll
  ├── poll_edited_text_previews: metadata changed?
  │    └── read <= 1 MiB -> Store::set_text -> save -> refresh_ui
  └── finish_closed_preview: delete an unwatched temporary file, clear the data
      source, and give the popup key back when the app is still active
```

- `QLPreviewPanel` is the system Quick Look singleton (QuickLookUI), so the
  preview looks and behaves like Finder's for every supported kind. `NSURL`
  itself conforms to `QLPreviewItem`, so no custom preview-item class is needed.
- `can_preview_clip` decides the row-menu enablement cheaply: text is present,
  the blob file exists, or a `File` target is an absolute existing file or
  directory. `preview_source_for_clip` does the actual blob read when the panel
  opens; every read keeps the `Store` bounds.
- A `File` clip points Quick Look at the real target instead of copying it, so
  folders, PDFs, videos and other system-previewable files work without a blob
  in the history.
- Editable text uses one stable file per clip, so Quick Look's **Open with
  TextEdit** and later previews address the same path. `poll_edited_text_previews`
  compares mtime and length, reads with a `MAX_EDIT_TEXT_BYTES + 1` cap and calls
  `Store::set_text`. Empty, invalid UTF-8 and oversized saves are skipped;
  duplicates beep and leave the clip untouched.
- Watched text files are not deleted when Quick Look closes because TextEdit may
  still hold them. They live in the boot-cleared preview directory, at most
  eight are watched, and the oldest is removed when the watch list overflows.
  **Clear Thumbnail Cache** also clears them. Unwatched previews are deleted as
  before; real file targets are never deleted.
- The popup normally hides when it resigns key; `schedule_hide_from_resign`
  skips that while the Quick Look panel is visible, and closing the preview
  re-keys the popup. If the user switched apps instead, the popup hides.

## Editing clip text

```
row menu Edit Text… / ⌘⇧E
  └── app::edit_selected -> edit_clip
       ├── allow only ClipKind::Text and ClipKind::Command
       ├── reject current text > MAX_EDIT_TEXT_BYTES with an alert
       ├── NSAlert + NSScrollView/NSTextView, Save/Cancel
       └── after runModal (no state borrow held)
            └── Store::set_text(index, text)
                 ├── Empty / TooLarge / Duplicate -> alert or beep
                 ├── Ok(false) -> unchanged
                 └── Ok(true) -> save, refresh_ui
```

- `Store::set_text` lives in core so the hash, preview and dedup rules are
  testable without AppKit. It preserves id, kind, source metadata, date, title
  and pin; it recomputes preview, `byte_len` and hash, and refuses a text that
  already exists on another clip.
- The editor is a modal `NSAlert` with a scrollable `NSTextView`. It is reached
  from a panic-guarded action or key monitor, and the state borrow is released
  before `runModal`, as required for all re-entrant AppKit calls.
- The text view is built once per invocation and bounded by the core limit; no
  new thread, worker or persistent window is added.
- The clipboard is untouched until the row is copied again. Command rows can be
  edited too, but a shell-history refresh may re-add the original command.

## Storage

`antirustpaste_core::Store` uses `dirs::data_dir()/AntiRustPaste` by default, so each shell
gets its platform data directory: on macOS `~/Library/Application
Support/AntiRustPaste/`, Windows `%APPDATA%\AntiRustPaste\`, Linux
`$XDG_DATA_HOME/antirustpaste/` (or the platform equivalent). `ANTIRUSTPASTE_DATA_DIR`
overrides it.

On macOS the default is `~/Library/Application Support/AntiRustPaste/`.

```
history.json        ordered array of Clip records, newest first
blobs/<uuid>.bin    image / RTF / HTML bytes
shell-history.json  zsh/bash byte offset + fingerprint (no command text)
```

`Clip` fields: `id`, `kind`, `preview`, `text`, `data_type`, `blob`, `byte_len`,
`width`, `height`, `source_name`, `source_bundle`, `date_ms`, `hash`, `title`,
`pinned`. Image dimensions, `title` and `pinned` are optional/defaulted for older
histories. The
vector is maintained pinned-first; `Store::trim` removes only unpinned clips and
`Store::set_pinned` moves a clip into the matching group. `ClearScope` is the
platform-neutral rule used by the footer and the Preferences History tab:
`Everything`, `Text`, `Images`, `UnpinnedText` and `UnpinnedImages`, none of
which ever match a command. A shell command is a
`Clip` with `kind: command`, the command in `text`, the shell in `source_name`
and no blob; `Store::upsert_command` dedups by command text. Trimming keeps
independent budgets for command and non-command records.

Guarantees:

- `save` writes `history.json.tmp` and renames it over the index.
- Unparseable JSON is renamed to `history.json.bak` and the app starts empty
  instead of panicking.
- Records whose blob file disappeared are dropped on load.
- Dedup uses a hash of kind + text + data type + bytes; re-copying moves the
  existing entry to the top and deletes its old blob before writing the new one.
- Trim/remove/clear delete blob files only after they leave the index.

Schema changes must keep existing JSON readable. The format currently has no
version field; adding one is the first step before any breaking change.

## Preferences and login item

The **model** lives in `antirustpaste-core::Settings` and uses platform-neutral
`Modifiers` bits. The **macOS persistence** in
`apps/antirustpaste-macos/src/settings.rs` stores keys in `NSUserDefaults`
(`maxTextClips`, `maxImageClips`, `maxCommandClips`, `recordClips`,
`confirmDelete`, `startAtLogin`, `ignoredApps`, `hotkeyKeyCode`,
`hotkeyModifiers`, `hotkeyDisplay`, `hotkeyEnabled`), migrates the old shared
`maxClips` value to the text and image groups, starts commands at 500 unless the
old limit was higher, migrates old NSEvent modifier values and redirects to
`ANTIRUSTPASTE_DEFAULTS_SUITE` in tests.
Future shells implement their own persistence for the same core model.

Start at login writes `~/Library/LaunchAgents/com.rustclip.app.plist` with the
current executable path and `RunAtLoad`. It deliberately does **not** run
`launchctl`, so enabling the option cannot kill or duplicate the running app.
`ANTIRUSTPASTE_LAUNCH_AGENT_DIR` redirects the path in tests.

## Global shortcut

`hotkey.rs` calls Carbon's `RegisterEventHotKey` on the application event target.
The handler is a plain C function and is panic-guarded. NSEvent modifier flags
are converted to Carbon flags; the default is key code 9 (`V`) with Command and
Shift. Registration failure is reported only when the user records a conflicting
combination; the previous shortcut is restored.

## Failure modes and responses

| Failure | Response |
| --- | --- |
| Corrupt history JSON | rename to `.bak`, start empty, log the error |
| Missing blob | drop the record on load |
| Oversized or empty pasteboard data | skip the item |
| Concealed pasteboard type | skip the whole change |
| Hot key already taken | keep the old shortcut, beep and show an alert when recording |
| Accessibility not granted | system prompt on first paste; the clipboard is still written |
| Target app quit | fall back to the last frontmost app |
| Thumbnail file missing/corrupt | regenerate from the blob on the worker; a corrupt file is overwritten |
| Image decode failure | `IMG` placeholder in the Images tab; the clip is still pasteable |
| Thumbnail worker cannot start | no thumbnails; the rest of the app works and failures stay non-fatal |
| Thumbnail cache unwritable | `IMG` placeholder; history is unaffected |
| Quick Look source missing or unreadable | beep; no temporary file; the popup keeps focus |
| Quick Look file target gone | the row-menu item is disabled and ⌘Y beeps |
| Edited text is empty, oversized or a duplicate | not imported; the stored clip is unchanged |
| Watched preview file disappears | its watch is dropped; no alert |
| Text preview cache cleared while TextEdit is open | the temporary file is removed; TextEdit keeps its in-memory copy |
| Shell history file missing/unreadable | Commands tab stays empty; no alert |
| Shell history file truncated or rewritten | re-read as a tail; stored entries keep their order |
| Shell history cursor corrupt | ignored; the next read starts from the tail |
| Panic in a callback | caught by `guard`, logged, app continues |
| State not initialized | invariant panic in `with_state`, caught if it happens inside a guarded callback |

## Deliberate non-goals

- Cloud sync or network access.
- Multiple named clipboards.
- Encryption at rest (the history is plain JSON; document this).
- A background watcher for shell history files.
- Reading a custom `$HISTFILE` or fish history.
- A plugin or scripting surface.
- Copying another app's assets, icons or wording.
