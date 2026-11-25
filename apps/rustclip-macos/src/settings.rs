//! macOS persistence for the platform-neutral settings model.

use std::fs;
use std::path::PathBuf;

use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSString, NSUserDefaults};
use rustclip_core::{IgnoredApp, Modifiers, Settings};

const K_MAX: &str = "maxClips";
const K_RECORD: &str = "recordClips";
const K_CONFIRM: &str = "confirmDelete";
const K_LOGIN: &str = "startAtLogin";
const K_IGNORED: &str = "ignoredApps";
const K_KEYCODE: &str = "hotkeyKeyCode";
const K_MODIFIERS: &str = "hotkeyModifiers";
const K_DISPLAY: &str = "hotkeyDisplay";
const K_HOTKEY_ENABLED: &str = "hotkeyEnabled";

// Legacy NSEventModifierFlags stored by earlier builds. New values use
// `rustclip_core::Modifiers` bits (shift/control/option/command = 1/2/4/8).
const NS_SHIFT: u32 = 1 << 17;
const NS_CONTROL: u32 = 1 << 18;
const NS_OPTION: u32 = 1 << 19;
const NS_COMMAND: u32 = 1 << 20;

pub fn load() -> Settings {
    let defaults = defaults();
    let mut settings = Settings::default();

    let max_key = NSString::from_str(K_MAX);
    if defaults.objectForKey(&max_key).is_some() {
        let value = defaults.integerForKey(&max_key);
        if value > 0 {
            settings.max_clips = (value as usize).min(rustclip_core::MAX_LIMIT);
        }
    }

    settings.record = bool_default(&defaults, K_RECORD, settings.record);
    settings.confirm_delete = bool_default(&defaults, K_CONFIRM, settings.confirm_delete);
    settings.start_at_login = bool_default(&defaults, K_LOGIN, settings.start_at_login);
    settings.hotkey_enabled = bool_default(&defaults, K_HOTKEY_ENABLED, settings.hotkey_enabled);

    let key_key = NSString::from_str(K_KEYCODE);
    if defaults.objectForKey(&key_key).is_some() {
        settings.hotkey_keycode =
            defaults.integerForKey(&key_key).clamp(0, u16::MAX as isize) as u16;
    }
    let mods_key = NSString::from_str(K_MODIFIERS);
    if defaults.objectForKey(&mods_key).is_some() {
        let raw = defaults.integerForKey(&mods_key).max(0) as u32;
        settings.hotkey_modifiers = migrate_modifiers(raw);
    }
    let display_key = NSString::from_str(K_DISPLAY);
    if let Some(display) = defaults.stringForKey(&display_key) {
        settings.hotkey_display = display.to_string();
    }

    let ignored_key = NSString::from_str(K_IGNORED);
    if let Some(raw) = defaults.stringForKey(&ignored_key)
        && let Ok(apps) = serde_json::from_str::<Vec<IgnoredApp>>(&raw.to_string())
    {
        settings.ignored_apps = apps;
    }

    settings
}

pub fn save(settings: &Settings) {
    let defaults = defaults();
    defaults.setInteger_forKey(settings.max_clips as isize, &NSString::from_str(K_MAX));
    defaults.setBool_forKey(settings.record, &NSString::from_str(K_RECORD));
    defaults.setBool_forKey(settings.confirm_delete, &NSString::from_str(K_CONFIRM));
    defaults.setBool_forKey(settings.start_at_login, &NSString::from_str(K_LOGIN));
    defaults.setBool_forKey(
        settings.hotkey_enabled,
        &NSString::from_str(K_HOTKEY_ENABLED),
    );
    defaults.setInteger_forKey(
        settings.hotkey_keycode as isize,
        &NSString::from_str(K_KEYCODE),
    );
    defaults.setInteger_forKey(
        settings.hotkey_modifiers.bits() as isize,
        &NSString::from_str(K_MODIFIERS),
    );

    let display_obj: Retained<AnyObject> =
        Retained::from(NSString::from_str(&settings.hotkey_display));
    unsafe {
        defaults.setObject_forKey(Some(&display_obj), &NSString::from_str(K_DISPLAY));
    }

    let json = serde_json::to_string(&settings.ignored_apps).unwrap_or_else(|_| "[]".to_string());
    let json_obj: Retained<AnyObject> = Retained::from(NSString::from_str(&json));
    unsafe {
        defaults.setObject_forKey(Some(&json_obj), &NSString::from_str(K_IGNORED));
    }
}

/// Convert legacy NSEvent flags to platform-neutral modifier bits.
fn migrate_modifiers(raw: u32) -> Modifiers {
    if raw & 0xFFFF_0000 == 0 {
        return Modifiers(raw);
    }
    let mut bits = 0u32;
    if raw & NS_SHIFT != 0 {
        bits |= Modifiers::SHIFT.bits();
    }
    if raw & NS_CONTROL != 0 {
        bits |= Modifiers::CONTROL.bits();
    }
    if raw & NS_OPTION != 0 {
        bits |= Modifiers::OPTION.bits();
    }
    if raw & NS_COMMAND != 0 {
        bits |= Modifiers::COMMAND.bits();
    }
    Modifiers(bits)
}

fn defaults() -> Retained<NSUserDefaults> {
    if let Ok(suite) = std::env::var("RUSTCLIP_DEFAULTS_SUITE")
        && !suite.is_empty()
    {
        let name = NSString::from_str(&suite);
        if let Some(defaults) =
            NSUserDefaults::initWithSuiteName(NSUserDefaults::alloc(), Some(&name))
        {
            return defaults;
        }
    }
    NSUserDefaults::standardUserDefaults()
}

fn bool_default(defaults: &NSUserDefaults, key: &str, fallback: bool) -> bool {
    let key = NSString::from_str(key);
    if defaults.objectForKey(&key).is_some() {
        defaults.boolForKey(&key)
    } else {
        fallback
    }
}

/// `RUSTCLIP_LAUNCH_AGENT_DIR` redirects the login item to a scratch
/// directory. Agents and tests MUST set it: changing the real LaunchAgents
/// folder affects the operator's next login.
fn launch_agent_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("RUSTCLIP_LAUNCH_AGENT_DIR")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir).join("com.rustclip.app.plist"));
    }
    Some(
        dirs::home_dir()?
            .join("Library")
            .join("LaunchAgents")
            .join("com.rustclip.app.plist"),
    )
}

/// Enable or disable launching at login by installing a per-user LaunchAgent.
/// We deliberately do not `launchctl bootstrap` it: the app is already running
/// when the switch is flipped, and the plist is picked up at the next login.
pub fn apply_start_at_login(enabled: bool) -> Result<(), String> {
    let path = launch_agent_path().ok_or_else(|| "No home directory".to_string())?;
    if !enabled {
        if path.exists() {
            fs::remove_file(&path).map_err(|e| format!("Could not remove login item: {e}"))?;
        }
        return Ok(());
    }

    let exe = std::env::current_exe().map_err(|e| format!("Could not find executable: {e}"))?;
    let exe = xml_escape(&exe.to_string_lossy());
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.rustclip.app</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
</dict>
</plist>
"#
    );
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Could not create LaunchAgents: {e}"))?;
    }
    fs::write(&path, plist).map_err(|e| format!("Could not write login item: {e}"))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_nsevent_modifiers_are_migrated() {
        let migrated = migrate_modifiers(NS_COMMAND | NS_SHIFT);
        assert_eq!(migrated, Modifiers::COMMAND | Modifiers::SHIFT);
        assert_eq!(migrate_modifiers(0b1010), Modifiers(0b1010));
    }
}
