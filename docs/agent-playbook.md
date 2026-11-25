# Agent playbook

> Last reviewed: 2026-11-25 · Version 0.2.0

Practical recipes for working on AntiRustPaste. Read [`../AGENTS.md`](../AGENTS.md)
first; this file explains *how*, not what the rules are.

## Before touching anything

```sh
pgrep -fl 'AntiRustPaste.app/Contents/MacOS/antirustpaste|target/.*/antirustpaste' || true
git status --short 2>/dev/null || echo "not a git repository"
```

- If an instance is running and you did not start it, do not kill it. The
  operator may be using it.
- This tree may have uncommitted work or no Git at all. Never delete a file you
  have not read.
- Read `docs/architecture.md` and the module you are changing. For a UI change,
  also read `docs/behavior.md` and `docs/testing.md`.

## Safe run and stop

Do not use `cargo run -p antirustpaste` in an unattended loop: it blocks and owns the menu bar.
Use the smoke script:

```sh
./scripts/smoke.sh
ANTIRUSTPASTE_SMOKE_PREFS=1 ./scripts/smoke.sh
ANTIRUSTPASTE_SMOKE_PANEL=1 ./scripts/smoke.sh
ANTIRUSTPASTE_SMOKE_IMAGES=1 ./scripts/smoke.sh
```

For a longer interactive run, start the process yourself and keep its PID:

```sh
SCRATCH="$(mktemp -d)"
ANTIRUSTPASTE_DATA_DIR="$SCRATCH/data" \
ANTIRUSTPASTE_DEFAULTS_SUITE="com.antirustpaste.agent.$$" \
ANTIRUSTPASTE_LAUNCH_AGENT_DIR="$SCRATCH/LaunchAgents" \
ANTIRUSTPASTE_THUMB_CACHE_DIR="$SCRATCH/thumbnails" \
./target/debug/antirustpaste &
PID=$!
# ... observe ...
kill "$PID"
wait "$PID" 2>/dev/null || true
rm -rf "$SCRATCH"
```

Never run `pkill -f antirustpaste`, `killall antirustpaste` or a similar broad match: it
can kill the operator's installed app.

## Recipe: add a supported pasteboard type

1. Read `apps/antirustpaste-macos/src/pasteboard.rs` and the precedence list in
   `docs/behavior.md`.
2. Decide where the new type sits in the precedence order. A richer type should
   win over plain text when both are present.
3. Add a match arm in `read_items`:
   - reject empty data and anything over `MAX_ITEM_BYTES`;
   - set `kind`, `preview`, `data_type`, `data`/`text` and `byte_len`;
   - break out of the type loop once chosen.
4. Teach `write_clip` how to put it back on the pasteboard. Use the stored UTI
   for blob clips and add a plain-text fallback when the type is rich.
5. Add a preview label that stays useful when the data has no text.
6. Update `docs/behavior.md` and `docs/architecture.md`.
7. If the parsing is pure, add a unit test. If it needs `NSPasteboard`, add a
   row to the manual matrix instead and say it is untested automatically.
8. Run `./scripts/check.sh` and `./scripts/smoke.sh`.

Do not add a type by reading unbounded data or by calling `unwrap()` on the
pasteboard result.

## Recipe: change history storage

1. Read `crates/antirustpaste-core/src/store.rs` and ADR-002/ADR-009.
2. Keep index writes atomic (`tmp` + rename).
3. Keep blobs out of the index.
4. Preserve old files:
   - add fields with `#[serde(default)]` where possible;
   - if a breaking change is needed, add a version envelope and migration
     *first*, in a separate change.
5. Validate anything that becomes a filesystem path (`blob` names must stay in
   `blobs/`).
6. Add or update a unit test for load/save, corrupt input and missing blobs.
7. Never delete a blob before the index no longer references it.
8. Update `docs/architecture.md`, `docs/behavior.md` and the hostile-input table.

## Recipe: change image capture or thumbnails

1. Capture policy lives in `apps/antirustpaste-macos/src/pasteboard.rs`; cache path
   and AppKit loading live in `apps/antirustpaste-macos/src/thumbnails.rs`; the
   worker/cache is `crates/antirustpaste-core/src/thumbnails.rs`; decoding lives in
   `crates/antirustpaste-imaging`.
2. Keep `antirustpaste-imaging` free of AppKit, filesystem and environment access.
   It must keep `#![forbid(unsafe_code)]`.
3. New formats must be in `image`'s enabled features and in the `Format` match;
   add a fixture test for metadata and a thumbnail test.
4. Keep the 32 MP pre-decode cap and the 512 px thumbnail edge cap. A new
   decoder must not allocate before the dimension check.
5. App code must bound blob reads and handle `None`/`Err` with an `IMG`
   placeholder, never an `unwrap`.
6. Cache file names must remain `thumb-<16 hex>.png`; pruning must ignore
   anything else. The cache directory is private and disposable.
7. Keep generation off the main thread. Only the `antirustpaste-thumbnails` worker
   may read blobs/decode/write; only the main thread may create `NSImage` or
   touch `AppState`. Send results through the existing channel and drain them in
   `drain_thumbnail_results`.
8. Add the input to the hostile-input table in `docs/testing.md`.
9. Run `cargo test -p antirustpaste-imaging`, `./scripts/check.sh` and
   `ANTIRUSTPASTE_SMOKE_IMAGES=1 ./scripts/smoke.sh`.

## Recipe: change the popup or list

1. `views.rs` draws; `app.rs` owns state and actions. Keep that split.
2. Never call AppKit while a `with_state` closure is active if that call can
   re-enter. Clone `Retained` handles first:

   ```rust
   // Good: the borrow ends before AppKit runs.
   let (panel, search) = with_state(|s| (s.panel.clone(), s.search.clone()));
   panel.makeKeyAndOrderFront(None);
   panel.makeFirstResponder(Some(&search));

   // Bad: orderOut can synchronously call windowDidResignKey -> hide_panel
   // -> with_state while this closure still holds the borrow.
   with_state(|s| s.panel.orderOut(None));
   ```

3. Use semantic colours and system fonts so light/dark mode work.
4. Bound the drawing work to the visible rows.
5. Keep keyboard paths working: arrows, Return, Escape, Delete, ⌘C.
6. Wrap new delegate/action methods in `guard`.

## Recipe: add a preference

1. Add a field in `crates/antirustpaste-core/src/settings.rs` and its persistence
   mapping in `apps/antirustpaste-macos/src/settings.rs`.
2. Load with a safe default and clamp numeric values.
3. Save from the action in `app.rs`; do not write defaults from `drawRect:`.
4. Reflect the value in `open_preferences` when the window is (re)opened.
5. Add the control to the General or Exceptions tab in `views.rs`.
6. Add a test for pure clamping/dedup logic where possible.
7. Update `docs/behavior.md` and the preferences table in the README if it is
   user-visible.

An unknown or malformed stored value must fall back to the default, not panic.

## Recipe: debug a crash or a hang

1. Reproduce with scratch paths and a debug build:

   ```sh
   RUST_BACKTRACE=1 \
   ANTIRUSTPASTE_DATA_DIR="$SCRATCH/data" \
   ANTIRUSTPASTE_DEFAULTS_SUITE="com.antirustpaste.agent.$$" \
   ANTIRUSTPASTE_LAUNCH_AGENT_DIR="$SCRATCH/LaunchAgents" \
   ./target/debug/antirustpaste
   ```

2. Check stderr for `antirustpaste: recovered from a panic in ...` and
   `RefCell already borrowed`.
3. If the app exits, open the newest
   `~/Library/Logs/DiagnosticReports/antirustpaste*.ips` and map frames to modules.
4. Common causes:
   - AppKit call inside `with_state` → clone handles first;
   - missing `guard` on a new callback;
   - a delegate/window not retained;
   - unbounded input or unchecked index;
   - main-thread work that blocks for seconds.
5. Add a regression test or a hostile-input row, then run `./scripts/check.sh`.

## Recipe: test the global shortcut without harming the operator

- Set an isolated defaults suite so you do not overwrite their shortcut:
  `ANTIRUSTPASTE_DEFAULTS_SUITE=com.antirustpaste.agent.$$`.
- The shortcut still registers globally for the duration of the test. Do not
  run this while the operator is presenting or sharing their screen without
  asking.
- To test with a different combination, write `hotkeyKeyCode`,
  `hotkeyModifiers` and `hotkeyDisplay` into the isolated suite before launch,
  or use Preferences while the test app runs.
- To verify the popup without any global shortcut, use
  `ANTIRUSTPASTE_OPEN_PANEL=1`.
- Do not script System Settings or Accessibility. If ⌘V does not arrive, check
  `AXIsProcessTrusted` through the app's prompt and ask the operator.

## Recipe: add a platform shell

1. Read `docs/platforms.md`; it lists the support matrix and the work each OS
   still needs.
2. Create `apps/antirustpaste-<os>` depending on `antirustpaste-core` and
   `antirustpaste-imaging`; add it to workspace members.
3. Map the native clipboard to `Captured`/`Clip` and back; keep the 25-item and
   50 MiB limits.
4. Map native modifier flags to `antirustpaste_core::Modifiers`; do not leak native
   masks into core.
5. Reuse `Store`, `Settings`, `filter_indices` and `ThumbnailService`; never
   copy them into the shell.
6. Implement tray, popup, hot key, paste, permissions, settings persistence,
   login item and the native image loader.
7. Add a smoke path with isolated data/settings/cache and document the tested
   OS/compositor.

## Recipe: package and verify

```sh
./scripts/check.sh
./make-app.sh
./scripts/smoke.sh          # debug binary
ANTIRUSTPASTE_BIN=dist/AntiRustPaste.app/Contents/MacOS/antirustpaste ./scripts/smoke.sh
```

`make-app.sh` ad-hoc signs the bundle. Verify:

```sh
codesign -dv dist/AntiRustPaste.app
plutil -p dist/AntiRustPaste.app/Contents/Info.plist
```

Do not copy the bundle to `/Applications` or launch it at login unless the
operator asked.

## Crash-avoidance patterns

### Guard callbacks

```rust
#[unsafe(method(doThing:))]
fn do_thing(&self, sender: &AnyObject) {
    guard("do_thing", || {
        do_thing();
    });
}
```

Use `guard_value` when the method returns a value and pick a safe fallback:

```rust
let handled = guard_value("key_monitor", || handle_key_event(event)).unwrap_or(false);
```

### No unchecked indexing

```rust
// Bad
let clip = &store.clips[index];

// Good
let Some(clip) = store.clips.get(index) else { return; };
```

### Bounded reads and conversions

```rust
if data.is_empty() || data.len() > MAX_ITEM_BYTES {
    return None;
}
let count = value.max(1).min(MAX_LIMIT);
```

### Keep Objective-C objects alive

`NSWindow.delegate`, `NSSearchField.delegate`, menu targets and status-item
targets do not retain the target. Store the delegate in `AppState` and keep it
for the app's lifetime.

### No blocking on the main thread

Use `NSTimer` for delays (the paste timer is 0.25 s). Do not `sleep`, wait on a
channel or do file I/O in `drawRect:`.

## Handoff template

```text
Changed:
- apps/... or crates/...: what and why

Checks run:
- ./scripts/check.sh       pass
- ./scripts/smoke.sh       pass
- ANTIRUSTPASTE_SMOKE_PREFS=1 ./scripts/smoke.sh   pass
- manual: (which rows, with what scratch paths)

Not run:
- ... and why

Known limitations:
- ...
```

Only list checks you actually ran. Never claim a manual GUI check you did not
perform.
