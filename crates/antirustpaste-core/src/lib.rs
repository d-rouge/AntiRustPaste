//! Platform-neutral AntiRustPaste domain.
//!
//! Nothing in this crate may depend on AppKit, Win32, GTK or another UI
//! framework. Platform shells (`apps/antirustpaste-*`) own tray icons, popups,
//! native clipboard access, global hotkeys and accessibility permissions.

pub mod filter;
pub mod settings;
pub mod shell_history;
pub mod store;
pub mod thumbnails;

pub use filter::{Section, filter_indices};
pub use settings::{
    DEFAULT_COMMAND_LIMIT, DEFAULT_IMAGE_LIMIT, DEFAULT_PINNED_BACKGROUND,
    DEFAULT_PINNED_INDICATOR, DEFAULT_TEXT_LIMIT, HistoryLimits, IgnoredApp, MAX_LIMIT, Modifiers,
    Rgb, Settings,
};
pub use shell_history::{
    Shell, ShellCommand, ShellCursors, ShellFile, ShellRead, load_cursors, now_ms, read_new,
    save_cursors,
};
pub use store::{
    Captured, ClearScope, Clip, ClipKind, EditTextError, MAX_EDIT_TEXT_BYTES, MAX_TITLE_CHARS,
    StorageUsage, Store, file_preview, human_size, make_preview, make_title, relative_time,
};
pub use thumbnails::{JobResult, ThumbnailService};
