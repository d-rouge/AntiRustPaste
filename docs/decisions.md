# Decisions

> Last reviewed: 2026-11-25

Short records of choices that are expensive to reverse. Each one names the
alternatives and the trade-off we accepted.

## ADR-001: Native AppKit through `objc2`, not a cross-platform UI toolkit

**Decision.** Build the interface with `objc2`/`objc2-app-kit`/`objc2-foundation`
and a small amount of hand-written Rust.

**Why.** A menu-bar clipboard manager depends on `NSStatusItem`, `NSPanel`,
`NSPasteboard`, `NSVisualEffectView`, `NSSearchField` and native menus. AppKit
gives the expected macOS look, keyboard behaviour and Accessibility primitives;
a canvas UI or webview would reimplement them poorly.

**Alternatives.** `egui`/`eframe` (no native status item or pasteboard
integration), Tauri/Electron (not pure Rust, heavy), a Swift helper (violates the
pure-Rust goal).

**Trade-off.** `unsafe` Objective-C messaging, main-thread constraints and
manual object lifetimes. Mitigated by panic guards, the `MainThreadOnly` marker
and a narrow module boundary.

## ADR-002: JSON index plus blob files, not SQLite

**Decision.** Keep `history.json` as the ordered index and store image/RTF/HTML
bytes as `<uuid>.bin` files under `blobs/`.

**Why.** No C dependency, no database engine, easy to inspect and back up, and
large blobs are not rewritten when one row changes. Atomic rename gives crash
safety for the index.

**Alternatives.** SQLite via `rusqlite` (C dependency, migration machinery),
one JSON file with base64 blobs (rewrites everything on each capture), a custom
binary format (harder to inspect and recover).

**Trade-off.** No transactions across index and blobs. Order of operations is
chosen so a crash can only leave an orphan blob (harmless) or a missing blob
(dropped on load), never a corrupt index.

## ADR-003: Carbon `RegisterEventHotKey` for the global shortcut

**Decision.** Use Carbon's `RegisterEventHotKey` through a small FFI module.

**Why.** It is the supported way to register a system-wide shortcut without
asking for Accessibility, and it does not require an event tap that observes all
keystrokes.

**Alternatives.** `CGEventTap` (needs Accessibility, sees every key), an
`NSEvent` global monitor (needs Accessibility for key events), a third-party
hot-key crate (extra dependency for the same API).

**Trade-off.** Carbon is deprecated for UI but the hot-key API remains
functional. The wrapper is isolated in `apps/rustclip-macos/src/hotkey.rs` and can be replaced
without touching app state.

## ADR-004: Synthetic ⌘V through CoreGraphics

**Decision.** Write the clip to the pasteboard, activate the previous app, then
post a Command-V key down/up with `CGEventPost`.

**Why.** It matches the expected clipboard-manager interaction: the clip lands
in the focused text field, and apps that handle paste natively keep their own
behaviour.

**Alternatives.** AppleScript/`System Events` keystroke (slow, needs
Automation), Accessibility API insertion (only text fields, invasive), asking
the user to press ⌘V (worse UX).

**Trade-off.** Requires Accessibility permission and a short delay before
posting. If permission is missing, the clipboard still updates and the user can
paste manually.

## ADR-005: LaunchAgent for start at login

**Decision.** Write or remove
`~/Library/LaunchAgents/com.rustclip.app.plist`; never call `launchctl` from the
app.

**Why.** Works from a plain binary and a bundle, is inspectable, and avoids
killing or duplicating the running process when the switch is flipped. The plist
is picked up at the next login.

**Alternatives.** `SMAppService` (modern, but requires a signed/notarised bundle
and more setup), a login-item AppleScript (fragile), `launchctl bootstrap`
immediately (can start a second instance).

**Trade-off.** Moving the app leaves a stale path until the app is launched again
from the new location.

## ADR-006: Accessory app (`LSUIElement`)

**Decision.** No Dock icon and no application menu; the status item is the entry
point.

**Why.** Clipboard history is a utility, not a document app. This matches the
menu-bar genre and keeps the window model simple.

**Trade-off.** Standard menu commands (⌘W, ⌘Q) do not exist; the status menu and
window close button cover the same actions. The Preferences window does not own
the global menu bar.

## ADR-007: Test isolation through environment variables

**Decision.** `RUSTCLIP_DATA_DIR`, `RUSTCLIP_DEFAULTS_SUITE` and
`RUSTCLIP_LAUNCH_AGENT_DIR` redirect history, preferences and the login item.

**Why.** GUI tests otherwise read or destroy the operator's private history and
change their next login. The overrides are small, explicit and inert when unset;
they also make the smoke test safe.

**Alternatives.** A `--test-mode` flag (easy to ship accidentally), a separate
binary (duplicated wiring), a temporary macOS user (heavy).

**Trade-off.** Production code contains three environment checks. They are
documented in `--help`, `README.md` and `docs/development.md`.

## ADR-008: Panic guards at every OS boundary

**Decision.** Wrap AppKit actions/delegates, timer blocks, the event monitor and
the Carbon handler in `catch_unwind` through `guard`/`guard_value`.

**Why.** A Rust panic unwinding into Objective-C or a C callback is undefined
behaviour and usually aborts the process. A menu-bar utility must survive a
drawing or state bug; logging and continuing is better than losing the user's
session.

**Trade-off.** A bug can be masked. The guard logs the panic, and tests still
exercise the logic directly. The state invariant panics remain visible in the
log instead of killing the app.

## ADR-009: Version-less JSON and forward compatibility

**Decision (current).** `history.json` is a plain array of records with no
version field. Load drops records with missing/invalid blobs and quarantines
unparseable JSON.

**Why.** The app is at 0.1.0 and the format is small; serde's field defaults keep
old files readable when fields are added. `width` and `height` were added this
way (`#[serde(default)]`), so existing histories load without a migration.

**Risk.** A future breaking change has no version to branch on. Before making
one, add a `{ "version": N, "clips": [...] }` envelope and a migration path.
This is the first storage task for a 1.0 release.

## ADR-010: A separate pure-Rust imaging crate

**Decision.** Move image metadata and thumbnail generation into
`crates/rustclip-imaging`; keep AppKit loading and cache policy in the app.

**Why.** Thumbnail generation must work outside macOS, and the logic is reusable
without any RustClip UI. The crate can enforce portability with
`#![forbid(unsafe_code)]`, has no AppKit/filesystem/environment dependencies and
is tested independently. This follows the LightCraft rule that reusable codecs
and parsers live in a shared library while app-specific policy stays in the app.

**Dependency.** `image` 0.25.10 with `png`, `tiff` and `jpeg` features only.
Licence: MIT OR Apache-2.0, recorded in `THIRD_PARTY.md`. The crate rejects more
than 32 million source pixels before allocating a decoded buffer.

**Alternatives.** A module inside the app (less reusable, no enforced
portability), ImageIO/CoreGraphics (macOS-only), hand-written decoders (too much
risk for hostile input).

**Trade-off.** A workspace and one dependency. If the imaging crate later needs
to serve only RustClip again, folding it back into `rustclip-core`/the shell is mechanical because
its API is two functions.

## ADR-011: A disposable thumbnail cache outside the history

**Decision.** Store encoded thumbnails in `RUSTCLIP_THUMB_CACHE_DIR` or, by
default on macOS, `/tmp/RustClip-<user>-<hash>/thumbnails`, never in
`history.json`. Prune by hash; expose **Clear Thumbnail Cache** in Preferences.

**Why.** Thumbnails are regenerable and much smaller than originals; keeping them
out of the history avoids schema changes, keeps saves small, and makes clearing
safe. macOS clears `/tmp` at boot, which gives the operator the requested
guarantee that no previews survive a reboot. The per-user subdirectory and
`0700`/`0600` permissions keep clipboard images private. A legacy cache under
`<OS temp>/RustClip/thumbnails` is removed once at startup.

**Alternatives.** A durable cache under `~/Library/Caches` (survives more, but
the operator asked for temporary), thumbnails inside the history (rewrites and
migrations), in-memory only (recomputed after every launch).

**Trade-off.** The first draw shows a placeholder while the worker generates the
file. The in-memory `HashMap` and the file cache prevent repeats. An explicit
`RUSTCLIP_THUMB_CACHE_DIR` override loses the boot-clearing guarantee; tests are
responsible for cleaning it.

## ADR-012: One background worker for thumbnail generation

**Decision.** Keep AppKit, `AppState` and timers on the main thread; move blob
reads, decode and cache writes to a single named `rustclip-thumbnails` worker.
A 0.1 s main-thread timer drains completed jobs and creates `NSImage` values.

**Why.** Decoding a large image on the main thread can freeze the popup. The
imaging crate is `Send`-friendly and has no AppKit dependency, so the work can
move off-thread without exposing Objective-C objects. One worker bounds
concurrency and keeps the order of generation/prune/clear commands
deterministic.

**Alternatives.** Decode on the main thread (simple, stutters), a thread per
image (unbounded), GCD/async runtime (new dependency and lifecycle complexity).

**Trade-off.** A small amount of channel/timer code and one thread. Results are
discarded if a cache clear bumped the generation, and the UI briefly shows a
placeholder. The worker must never touch AppKit or `AppState`.

## ADR-013: Workspace with a platform-neutral core and native shells

**Decision.** Keep domain code in `crates/rustclip-core`, the image codec in
`crates/rustclip-imaging`, and the implemented UI in `apps/rustclip-macos`.
Future platforms add `apps/rustclip-<os>` beside it.

**Why.** The clipboard model, storage, settings schema, filtering and thumbnail
worker are identical on every OS and easy to test without a display. Tray icons,
popups, native clipboard formats, global hot keys, paste and permissions are
inherently platform-specific and behave better natively than behind a
cross-platform abstraction.

**Alternatives.** One `egui`/`tray-icon` shell for all platforms (less native
UX, Linux/Wayland caveats), Tauri (web UI), a trait-heavy platform API before a
second implementation exists.

**Trade-off.** Each OS needs a shell, but core changes remain shared and the
macOS UX is not compromised. There is deliberately no `Platform` trait yet: the
second shell will reveal the right interface. See
[platforms.md](platforms.md).

## ADR-014: Native macOS clipboard, portable backend for future shells

**Decision.** The macOS shell keeps `NSPasteboard` + `changeCount` polling.
Windows/Linux shells choose their own adapter; prefer `clipboard-rs` if the full
text/HTML/RTF/image/file matrix is needed, otherwise `arboard` +
`clipboard-master` is acceptable for text/images.

**Why.** `NSPasteboard` exposes exact UTIs, RTF/RTFD, HTML, file URLs,
concealed/transient types and original encoded bytes. `arboard` exposes text,
HTML, images and file lists, but not RTF; `clipboard-master` is only a change
notifier and on macOS is implemented with `changeCount` polling, so it would add
a dependency without improving the current shell. `clipboard-rs` covers the full
format matrix and watching, but is newer and needs platform testing.

**Boundary.** Core sees only `Captured`/`Clip`; no clipboard crate becomes a core
dependency. Platform shells own permissions and native formats.

## ADR-015: Pinned clips are core metadata with pinned-first ordering

**Decision.** Add `pinned: bool` to `rustclip_core::Clip`, keep pinned clips at
the front of the vector, and make `trim` count/remove only unpinned clips. Use
`⌘P` plus a row glyph in the macOS shell; a row context menu is a follow-up.

**Why.** Pinning changes ordering and eviction, not capture or rendering, so it
belongs in the platform-neutral store and every shell must apply the same rules.
The field is additive with `#[serde(default)]`, so old histories need no
migration. A pinned duplicate content hash keeps its pinned flag and moves to
the top of the pinned group. Explicit delete/section clear/clear-all still
removes pinned clips; pinning protects only against automatic eviction.

**Trade-off.** The history can exceed the configured maximum when many clips are
pinned. This is intentional and visible in the row count; the alternative
(evicting pinned clips) defeats the feature.

## ADR-016: Write the PDF container in-house instead of using `printpdf`

**Decision.** `crates/rustclip-pdf` writes the PDF catalogue, page tree, content
streams and image XObjects itself, using `rustclip-imaging` for decoding and
`flate2` for compression.

**Why.** `printpdf` 0.12 pulls in 104 crates for a job that only needs image
embedding: its default feature set includes the Azul text layout stack,
`allsorts` fonts, `pathfinder`, encryption primitives, Brotli and fax decoding.
The export needs none of that. A small writer keeps the dependency surface
reviewable, stays pure Rust, and gives byte-level tests: page count, object
offsets, image dimensions and the upright transform are all asserted in unit
tests. The output was also rendered with PDFKit during development to confirm
A4 size, margins, colour and orientation.

**Trade-off.** We own the container code and must keep it inside what PDF
readers accept (no compression of text objects, no linearisation, no metadata
streams). New page/layout features require changes in our writer rather than a
library call.

**Validation.** `cargo test -p rustclip-pdf` checks the header, trailer,
`startxref` offsets, one `/Type /Page` per image, `/FlateDecode` streams, and
that the content matrix negates the height so rows are not upside down.
