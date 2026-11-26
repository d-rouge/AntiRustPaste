//! AntiRustPaste — Linux GTK4 + X11 shell.
//!
//! Domain logic, storage, filtering, image decoding and PDF export live in the
//! workspace core crates; this binary owns the tray icon, popup, native
//! clipboard, X11 global hot key, synthetic paste and XDG settings/autostart.

#[cfg(target_os = "linux")]
mod app;
#[cfg(target_os = "linux")]
mod clipboard;
#[cfg(target_os = "linux")]
mod settings;
#[cfg(target_os = "linux")]
mod thumbnails;
#[cfg(target_os = "linux")]
mod tray;
#[cfg(target_os = "linux")]
mod ui;
#[cfg(target_os = "linux")]
mod x11;

const HELP: &str = "\
AntiRustPaste — clipboard history in the system tray (Linux).

Usage: antirustpaste-linux [--help | --version]

Environment (for tests and automation):
  ANTIRUSTPASTE_DATA_DIR           history directory (default: $XDG_DATA_HOME/AntiRustPaste)
  ANTIRUSTPASTE_CONFIG_DIR         config directory for settings.json (default: $XDG_CONFIG_HOME/antirustpaste)
  ANTIRUSTPASTE_AUTOSTART_DIR      directory for autostart .desktop file (default: $XDG_CONFIG_HOME/autostart)
  ANTIRUSTPASTE_OPEN_PREFS=1       open Preferences at launch
  ANTIRUSTPASTE_OPEN_PANEL=1       open the history popup at launch
  ANTIRUSTPASTE_OPEN_IMAGES=1      start the popup on the Images tab
  ANTIRUSTPASTE_OPEN_COMMANDS=1    start the popup on the Commands tab
  ANTIRUSTPASTE_SHELL_HISTORY_DIR  directory holding .zsh_history/.bash_history (default: home)
  ANTIRUSTPASTE_THUMB_CACHE_DIR    absolute dedicated thumbnail cache

See AGENTS.md and docs/platforms.md before changing behaviour.";

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--help" | "-h") => println!("{HELP}"),
        Some("--version" | "-V") => println!("antirustpaste-linux {}", env!("CARGO_PKG_VERSION")),
        _ => run_platform(),
    }
}

#[cfg(target_os = "linux")]
fn run_platform() {
    app::run();
}

#[cfg(not(target_os = "linux"))]
fn run_platform() {
    eprintln!("antirustpaste-linux only runs on Linux; use apps/antirustpaste-macos on macOS.");
}
