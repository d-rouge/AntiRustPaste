//! Linux settings persistence (JSON under XDG config) and XDG autostart.

use std::fs;
use std::path::{Path, PathBuf};

use antirustpaste_core::{
    DEFAULT_LIMIT, DEFAULT_PINNED_BACKGROUND, DEFAULT_PINNED_INDICATOR, IgnoredApp, MAX_LIMIT,
    Modifiers, Rgb, Settings,
};
use serde::{Deserialize, Serialize};

const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;
const DESKTOP_ENTRY_NAME: &str = "antirustpaste.desktop";

/// X11 keysym for lowercase 'v'.
pub const DEFAULT_KEYSYM_V: u16 = 0x0076;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
struct StoredSettings {
    max_clips: usize,
    record: bool,
    confirm_delete: bool,
    paste_after_select: bool,
    start_at_login: bool,
    ignored_apps: Vec<IgnoredApp>,
    hotkey_keycode: u16,
    hotkey_modifiers: u32,
    hotkey_display: String,
    hotkey_enabled: bool,
    pinned_background: u32,
    pinned_indicator: u32,
}

impl Default for StoredSettings {
    fn default() -> Self {
        let defaults = linux_defaults();
        Self {
            max_clips: defaults.max_clips,
            record: defaults.record,
            confirm_delete: defaults.confirm_delete,
            paste_after_select: defaults.paste_after_select,
            start_at_login: defaults.start_at_login,
            ignored_apps: defaults.ignored_apps,
            hotkey_keycode: defaults.hotkey_keycode,
            hotkey_modifiers: defaults.hotkey_modifiers.bits(),
            hotkey_display: defaults.hotkey_display,
            hotkey_enabled: defaults.hotkey_enabled,
            pinned_background: defaults.pinned_background.to_u32(),
            pinned_indicator: defaults.pinned_indicator.to_u32(),
        }
    }
}

pub fn linux_defaults() -> Settings {
    let modifiers = Modifiers::CONTROL | Modifiers::SHIFT;
    Settings {
        max_clips: DEFAULT_LIMIT,
        record: true,
        confirm_delete: true,
        paste_after_select: false,
        start_at_login: false,
        ignored_apps: Vec::new(),
        hotkey_keycode: DEFAULT_KEYSYM_V,
        hotkey_modifiers: modifiers,
        hotkey_display: format_hotkey(modifiers, "V"),
        hotkey_enabled: true,
        pinned_background: DEFAULT_PINNED_BACKGROUND,
        pinned_indicator: DEFAULT_PINNED_INDICATOR,
    }
}

pub fn config_dir() -> PathBuf {
    if let Some(override_dir) = std::env::var_os("ANTIRUSTPASTE_CONFIG_DIR")
        && !override_dir.is_empty()
    {
        return PathBuf::from(override_dir);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("antirustpaste")
}

pub fn autostart_dir() -> PathBuf {
    if let Some(override_dir) = std::env::var_os("ANTIRUSTPASTE_AUTOSTART_DIR")
        && !override_dir.is_empty()
    {
        return PathBuf::from(override_dir);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("autostart")
}

pub fn load() -> Settings {
    load_from(&config_dir().join("settings.json"))
}

pub fn save(settings: &Settings) {
    save_to(&config_dir().join("settings.json"), settings);
}

pub fn load_from(path: &Path) -> Settings {
    let Ok(meta) = fs::metadata(path) else {
        return linux_defaults();
    };
    if meta.len() > MAX_SETTINGS_BYTES {
        eprintln!("antirustpaste-linux: settings file too large; using defaults");
        return linux_defaults();
    }
    let Ok(raw) = fs::read(path) else {
        return linux_defaults();
    };
    let Ok(stored) = serde_json::from_slice::<StoredSettings>(&raw) else {
        eprintln!("antirustpaste-linux: could not parse settings; using defaults");
        return linux_defaults();
    };
    Settings {
        max_clips: stored.max_clips.clamp(1, MAX_LIMIT),
        record: stored.record,
        confirm_delete: stored.confirm_delete,
        paste_after_select: stored.paste_after_select,
        start_at_login: stored.start_at_login,
        ignored_apps: stored.ignored_apps,
        hotkey_keycode: if stored.hotkey_keycode == 0 {
            DEFAULT_KEYSYM_V
        } else {
            stored.hotkey_keycode
        },
        hotkey_modifiers: Modifiers(stored.hotkey_modifiers & 0x0f),
        hotkey_display: if stored.hotkey_display.trim().is_empty() {
            "Ctrl+Shift+V".to_string()
        } else {
            stored.hotkey_display
        },
        hotkey_enabled: stored.hotkey_enabled,
        pinned_background: Rgb::from_u32(stored.pinned_background),
        pinned_indicator: Rgb::from_u32(stored.pinned_indicator),
    }
}

pub fn save_to(path: &Path, settings: &Settings) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let stored = StoredSettings {
        max_clips: settings.max_clips.clamp(1, MAX_LIMIT),
        record: settings.record,
        confirm_delete: settings.confirm_delete,
        paste_after_select: settings.paste_after_select,
        start_at_login: settings.start_at_login,
        ignored_apps: settings.ignored_apps.clone(),
        hotkey_keycode: settings.hotkey_keycode,
        hotkey_modifiers: settings.hotkey_modifiers.bits(),
        hotkey_display: settings.hotkey_display.clone(),
        hotkey_enabled: settings.hotkey_enabled,
        pinned_background: settings.pinned_background.to_u32(),
        pinned_indicator: settings.pinned_indicator.to_u32(),
    };
    let Ok(bytes) = serde_json::to_vec_pretty(&stored) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if let Err(err) = fs::write(&tmp, bytes).and_then(|_| fs::rename(&tmp, path)) {
        eprintln!("antirustpaste-linux: could not save settings: {err}");
    }
}

/// Write or remove the XDG autostart `.desktop` entry for `start_at_login`.
pub fn sync_autostart(enabled: bool) {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("antirustpaste-linux"));
    sync_autostart_in(&autostart_dir(), &exe, enabled);
}

pub fn sync_autostart_in(dir: &Path, exe: &Path, enabled: bool) {
    let path = dir.join(DESKTOP_ENTRY_NAME);
    if !enabled {
        let _ = fs::remove_file(path);
        return;
    }
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let exec_line = desktop_escape(&exe.to_string_lossy());
    let contents = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=AntiRustPaste\n\
         Comment=Clipboard history manager\n\
         Exec={exec_line}\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n"
    );
    let tmp = path.with_extension("desktop.tmp");
    if let Err(err) = fs::write(&tmp, contents).and_then(|_| fs::rename(&tmp, &path)) {
        eprintln!("antirustpaste-linux: could not write autostart entry: {err}");
    }
}

fn desktop_escape(path: &str) -> String {
    if path.contains([' ', '\t', '"', '\\']) {
        let escaped = path.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        path.to_string()
    }
}

pub fn format_hotkey(modifiers: Modifiers, key_label: &str) -> String {
    let mut parts = Vec::new();
    if modifiers.contains(Modifiers::CONTROL) {
        parts.push("Ctrl");
    }
    if modifiers.contains(Modifiers::OPTION) {
        parts.push("Alt");
    }
    if modifiers.contains(Modifiers::SHIFT) {
        parts.push("Shift");
    }
    if modifiers.contains(Modifiers::COMMAND) {
        parts.push("Super");
    }
    parts.push(key_label);
    parts.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "antirustpaste-linux-test-{tag}-{}-{nanos}",
                std::process::id()
            ));
            let _ = fs::create_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn settings_round_trip_and_clamp() {
        let tmp = TempDir::new("settings");
        let path = tmp.0.join("settings.json");
        let mut settings = linux_defaults();
        settings.max_clips = 50_000;
        settings.record = false;
        settings.paste_after_select = true;
        settings.add_ignored(IgnoredApp {
            name: "KeePassXC".to_string(),
            bundle_id: "org.keepassxc.KeePassXC".to_string(),
        });

        save_to(&path, &settings);
        let loaded = load_from(&path);
        assert_eq!(loaded.max_clips, MAX_LIMIT);
        assert!(!loaded.record);
        assert!(loaded.paste_after_select);
        assert!(loaded.is_ignored(Some("org.keepassxc.KeePassXC")));
    }

    #[test]
    fn corrupt_settings_fall_back_to_defaults() {
        let tmp = TempDir::new("corrupt");
        let path = tmp.0.join("settings.json");
        let _ = fs::write(&path, b"{not valid json");
        let loaded = load_from(&path);
        assert_eq!(loaded.max_clips, DEFAULT_LIMIT);
        assert_eq!(loaded.hotkey_display, "Ctrl+Shift+V");
    }

    #[test]
    fn autostart_entry_is_created_and_removed() {
        let tmp = TempDir::new("autostart");
        let exe = Path::new("/usr/local/bin/antirustpaste-linux");
        sync_autostart_in(&tmp.0, exe, true);
        let desktop = tmp.0.join(DESKTOP_ENTRY_NAME);
        assert!(desktop.exists());
        let text = fs::read_to_string(&desktop).unwrap_or_default();
        assert!(text.contains("Exec=/usr/local/bin/antirustpaste-linux"));

        sync_autostart_in(&tmp.0, exe, false);
        assert!(!desktop.exists());
    }

    #[test]
    fn hotkey_formatting_uses_linux_labels() {
        let label = format_hotkey(Modifiers::CONTROL | Modifiers::SHIFT, "V");
        assert_eq!(label, "Ctrl+Shift+V");
        let super_alt = format_hotkey(Modifiers::COMMAND | Modifiers::OPTION, "Space");
        assert_eq!(super_alt, "Alt+Super+Space");
    }
}
