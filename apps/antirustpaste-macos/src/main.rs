//! AntiRustPaste — macOS AppKit shell.
//!
//! Domain logic and storage live in `antirustpaste-core`; this binary owns the menu
//! bar item, popup, native clipboard, Carbon hot key, CoreGraphics paste and
//! Accessibility permission.

mod app;
mod hotkey;
mod paste;
mod pasteboard;
mod settings;
mod thumbnails;
mod views;

const HELP: &str = "\
AntiRustPaste — clipboard history in the menu bar.

Usage: antirustpaste [--help | --version]

Environment (for tests and automation):
  ANTIRUSTPASTE_DATA_DIR           history directory (default: Application Support/AntiRustPaste)
  ANTIRUSTPASTE_DEFAULTS_SUITE     user defaults suite name (isolates preferences)
  ANTIRUSTPASTE_LAUNCH_AGENT_DIR   directory for the login item plist
  ANTIRUSTPASTE_OPEN_PREFS=1       open Preferences at launch
  ANTIRUSTPASTE_OPEN_PANEL=1       open the history popup at launch
  ANTIRUSTPASTE_OPEN_IMAGES=1      start the popup on the Images tab
  ANTIRUSTPASTE_THUMB_CACHE_DIR    absolute dedicated thumbnail cache (default: /tmp on macOS, boot-cleared)

See AGENTS.md and docs/ before changing behaviour.";

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--help" | "-h") => println!("{HELP}"),
        Some("--version" | "-V") => println!("antirustpaste {}", env!("CARGO_PKG_VERSION")),
        _ => app::run(),
    }
}
