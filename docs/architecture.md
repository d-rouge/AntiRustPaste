# Architecture

> Last reviewed: 2026-11-25 · Version 0.2.1

AntiRustPaste is a single-process macOS accessory app. AppKit runs the main loop and
a repeating `NSTimer` polls the pasteboard. There is no webview, database engine
or async runtime. One background worker exists for thumbnails: it reads blobs,
decodes them with `antirustpaste-imaging` and writes cache files; it never touches
AppKit or `AppState`.

The repository is a Cargo workspace:

- `apps/antirustpaste-macos` — the implemented AppKit shell;
- `crates/antirustpaste-core` — platform-neutral model, storage, settings, filtering
  and thumbnail worker;
- `crates/antirustpaste-imaging` — a pure-Rust, `#![forbid(unsafe_code)]` codec with
  no AppKit, filesystem or environment access;
- `crates/antirustpaste-pdf` — a pure-Rust, `#![forbid(unsafe_code)]` PDF writer for
  the image export, with no AppKit, filesystem or environment access.

Future Windows/Linux shells are `apps/antirustpaste-<os>`. They depend on core; core
never depends on a shell. See [platforms.md](platforms.md).

## Module map

| Module | Responsibility |
| --- | --- |
| `apps/antirustpaste-macos/src/main.rs` | `--help`/`--version`; starts `app::run()` |
| `apps/antirustpaste-macos/src/app.rs` | `NSApplicationDelegate`, thread-local `AppState`, status item and menu, clipboard polling, hot key, keyboard handling, all user actions, panic guards |
| `apps/antirustpaste-macos/src/views.rs` | Objective-C classes: `RCClipPanel`, `RCClipListView`, `RCIgnoredAppsView`, `RCPanelDelegate`; construction of the popup and Preferences window |
| `apps/antirustpaste-macos/src/pasteboard.rs` | Reads and writes `NSPasteboard` with type/size limits |
| `apps/antirustpaste-macos/src/paste.rs` | CoreGraphics ⌘V synthesis and the Accessibility trust prompt |
| `apps/antirustpaste-macos/src/hotkey.rs` | Carbon `RegisterEventHotKey` wrapper and shortcut formatting |
| `apps/antirustpaste-macos/src/settings.rs` | `NSUserDefaults` keys, modifier migration, LaunchAgent management |
| `apps/antirustpaste-macos/src/thumbnails.rs` | Boot-cleared cache path, private directory policy, `NSImage` loading |
| `crates/antirustpaste-core/src/store.rs` | `Clip`/`Captured`, JSON index, blob files, dedup, trim, clear, previews, relative time |
| `crates/antirustpaste-core/src/settings.rs` | Platform-neutral `Settings`, `IgnoredApp`, `Modifiers` bits |
| `crates/antirustpaste-core/src/filter.rs` | `Section` (Text/Images) and search filtering |
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
                ├── install AppState in the thread-local (starts the thumbnail worker)
                ├── install key monitor, 1 s pasteboard poll and 0.1 s thumbnail drain
                ├── register the Carbon hot key
                └── optional automation hooks (ANTIRUSTPASTE_OPEN_*)
```

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

## Paste flow

```
list row / Enter / menu item
 └── paste_store_index
      ├── clone the Clip and resolve the target NSRunningApplication
      ├── pasteboard::write_clip (clear, then the clip's best type)
      ├── record the new changeCount so the write is not re-captured
      ├── hide the panel
      ├── activate the target app
      ├── first time: AXIsProcessTrustedWithOptions prompt
      └── 0.25 s one-shot NSTimer -> CGEvent ⌘V down/up
```

The target app is captured when the popup opens (or before the status menu is
shown), not when the paste happens. If the target quit, `resolve_target_app`
falls back to the last non-AntiRustPaste frontmost app by bundle id.

## Popup and preferences

`RCClipPanel` is a borderless `NSPanel` subclass with
`NSWindowStyleMask::NonactivatingPanel`, level `NSStatusWindowLevel`, join-all-spaces
and full-screen-auxiliary behaviour. Its content is an `NSVisualEffectView`
(popover material) with rounded corners. The history rows are drawn by
`RCClipListView::drawRect:`; only the visible range is painted. A segmented
control switches between **Text** and **Images**; ⌘1/⌘2 do the same. The search
field is the first responder, and a local key monitor handles arrows, Enter,
Escape, Delete, ⌘1/⌘2, ⌘C, ⌘P and ⌘E before AppKit's responder chain. The
footer holds **Clear Text** and **Clear Images** buttons; each calls
`Store::remove_where` for its section and then prunes thumbnails. On the Images
tab it also holds **PDF…**, which opens the export window, and the footer hint
switches to `⌘E PDF`. `⌘P` toggles the pin of the selected row; pinned rows draw
a tilting accent SF Symbol pin inside a rounded card under a **PINNED** header.
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

- **General** — max clippings, record toggle, confirm-before-delete, start at
  login, shortcut recorder.
- **Exceptions** — ignored apps in an `NSScrollView` with a custom list view and
  Add/Remove buttons.

The preferences window is built lazily on first open and kept alive in
`AppState::prefs`; the panel delegate is stored separately because AppKit
delegates are not retained.

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
Space (Images tab, empty query) / ⌘Y
  └── app::toggle_image_preview
       ├── shared QLPreviewPanel already visible -> close and clean up
       ├── read the selected clip's original blob
       ├── write <thumb cache>/previews/preview-<hash>.<ext> (dir 0700, file 0600)
       ├── panel.dataSource/delegate = AppDelegate; reloadData
       └── makeKeyAndOrderFront
QLPreviewPanel
  ├── numberOfPreviewItemsInPreviewPanel:  -> 1
  ├── previewPanel:previewItemAtIndex:     -> the file NSURL
  └── previewPanel:handleEvent:            -> Space/Esc close the preview
1 s poll / close
  └── finish_closed_preview: delete the file, clear the data source, and give
      the popup key back when the app is still active
```

- `QLPreviewPanel` is the system Quick Look singleton (QuickLookUI), so the
  preview looks and behaves like Finder's. `NSURL` itself conforms to
  `QLPreviewItem`, so no custom preview-item class is needed.
- The popup normally hides when it resigns key; `schedule_hide_from_resign`
  skips that while the Quick Look panel is visible, and closing the preview
  re-keys the popup. If the user switched apps instead, the popup hides.
- Preview files live beside the thumbnails so they are boot-cleared, and the
  directory is reset at launch and removed on quit.

## Storage

`antirustpaste_core::Store` uses `dirs::data_dir()/AntiRustPaste` by default, so each shell
gets its platform data directory: on macOS `~/Library/Application
Support/AntiRustPaste/`, Windows `%APPDATA%\AntiRustPaste\`, Linux
`$XDG_DATA_HOME/antirustpaste/` (or the platform equivalent). `ANTIRUSTPASTE_DATA_DIR`
overrides it.

On macOS the default is `~/Library/Application Support/AntiRustPaste/`.

```
history.json     ordered array of Clip records, newest first
blobs/<uuid>.bin image / RTF / HTML bytes
```

`Clip` fields: `id`, `kind`, `preview`, `text`, `data_type`, `blob`, `byte_len`,
`width`, `height`, `source_name`, `source_bundle`, `date_ms`, `hash`, `pinned`.
Image dimensions and `pinned` are optional/defaulted for older histories. The
vector is maintained pinned-first; `Store::trim` removes only unpinned clips and
`Store::set_pinned` moves a clip into the matching group.

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
(`maxClips`, `recordClips`, `confirmDelete`, `startAtLogin`, `ignoredApps`,
`hotkeyKeyCode`, `hotkeyModifiers`, `hotkeyDisplay`, `hotkeyEnabled`), migrates
old NSEvent modifier values and redirects to `ANTIRUSTPASTE_DEFAULTS_SUITE` in tests.
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
| Panic in a callback | caught by `guard`, logged, app continues |
| State not initialized | invariant panic in `with_state`, caught if it happens inside a guarded callback |

## Deliberate non-goals

- Cloud sync or network access.
- Multiple named clipboards.
- Encryption at rest (the history is plain JSON; document this).
- A plugin or scripting surface.
- Copying another app's assets, icons or wording.
