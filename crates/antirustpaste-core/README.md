# antirustpaste-core

Platform-neutral domain for AntiRustPaste. This crate must not depend on AppKit,
Win32, GTK or a UI toolkit.

It owns:

- `Clip`, `Captured`, `ClipKind` and `Store` (JSON index + blob files);
- preview normalisation, relative time and human-readable sizes;
- `Settings`, `IgnoredApp` and platform-neutral `Modifiers` bits;
- `Section` and `filter_indices`;
- `ThumbnailService`, the single thumbnail worker and cache pruning.

Platform shells (`apps/antirustpaste-*`) implement tray/popup, native clipboard,
global hot key, paste, permissions, settings persistence, login item and the
native image type. See [`docs/platforms.md`](../../docs/platforms.md).

```sh
cargo test -p antirustpaste-core
```
