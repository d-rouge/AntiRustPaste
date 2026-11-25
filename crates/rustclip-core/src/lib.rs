//! Platform-neutral RustClip domain.
//!
//! Nothing in this crate may depend on AppKit, Win32, GTK or another UI
//! framework. Platform shells (`apps/rustclip-*`) own tray icons, popups,
//! native clipboard access, global hotkeys and accessibility permissions.

pub mod filter;
pub mod settings;
pub mod store;
pub mod thumbnails;

pub use filter::{Section, filter_indices};
pub use settings::{DEFAULT_LIMIT, IgnoredApp, MAX_LIMIT, Modifiers, Settings};
pub use store::{
    Captured, Clip, ClipKind, Store, file_preview, human_size, make_preview, relative_time,
};
pub use thumbnails::{JobResult, ThumbnailService};
