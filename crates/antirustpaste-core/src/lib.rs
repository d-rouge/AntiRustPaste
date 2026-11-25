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
pub use settings::{DEFAULT_LIMIT, IgnoredApp, MAX_LIMIT, Modifiers, Settings};
pub use shell_history::{
    Shell, ShellCommand, ShellCursors, ShellFile, ShellRead, load_cursors, now_ms, read_new,
    save_cursors,
};
pub use store::{
    Captured, Clip, ClipKind, Store, file_preview, human_size, make_preview, relative_time,
};
pub use thumbnails::{JobResult, ThumbnailService};
