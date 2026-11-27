//! macOS persistence for the platform-neutral settings model.

use std::fs;
use std::path::PathBuf;

use antirustpaste_core::{
    DEFAULT_COMMAND_LIMIT, DEFAULT_IMAGE_LIMIT, DEFAULT_TEXT_LIMIT, IgnoredApp, MAX_LIMIT,
    Modifiers, Rgb, Settings,
};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSString, NSUserDefaults};

const K_MAX_TEXT: &str = "maxTextClips";
const K_MAX_IMAGES: &str = "maxImageClips";
const K_MAX_COMMANDS: &str = "maxCommandClips";
/// Earlier builds stored one shared limit for clippings and commands.
const K_MAX_LEGACY: &str = "maxClips";
const K_RECORD: &str = "recordClips";
const K_CONFIRM: &str = "confirmDelete";
const K_PASTE_AFTER_SELECT: &str = "pasteAfterSelect";
const K_LOGIN: &str = "startAtLogin";
const K_IGNORED: &str = "ignoredApps";
const K_KEYCODE: &str = "hotkeyKeyCode";
const K_MODIFIERS: &str = "hotkeyModifiers";
const K_DISPLAY: &str = "hotkeyDisplay";
const K_HOTKEY_ENABLED: &str = "hotkeyEnabled";
const K_PINNED_BACKGROUND: &str = "pinnedBackgroundColor";
const K_PINNED_INDICATOR: &str = "pinnedIndicatorColor";

// Legacy NSEventModifierFlags stored by earlier builds. New values use
// `antirustpaste_core::Modifiers` bits (shift/control/option/command = 1/2/4/8).
const NS_SHIFT: u32 = 1 << 17;
const NS_CONTROL: u32 = 1 << 18;
const NS_OPTION: u32 = 1 << 19;
const NS_COMMAND: u32 = 1 << 20;

pub fn load() -> Settings {
    let defaults = defaults();
    let mut settings = Settings::default();

    let legacy_limit = legacy_limit(&defaults);
    settings.max_text = limit_default(&defaults, K_MAX_TEXT)
        .or(legacy_limit)
        .unwrap_or(DEFAULT_TEXT_LIMIT);
    settings.max_images = limit_default(&defaults, K_MAX_IMAGES)
        .or(legacy_limit)
        .unwrap_or(DEFAULT_IMAGE_LIMIT);
    // The old shared `maxClips` covered clippings and commands. Commands now
    // start at the shell-oriented default, but never below a value the user
    // already chose, so a migration cannot silently delete command history.
    settings.max_commands = match limit_default(&defaults, K_MAX_COMMANDS) {
        Some(value) => value,
        None => legacy_limit
            .map(|value| value.max(DEFAULT_COMMAND_LIMIT))
            .unwrap_or(DEFAULT_COMMAND_LIMIT),
    };

    settings.record = bool_default(&defaults, K_RECORD, settings.record);
    settings.confirm_delete = bool_default(&defaults, K_CONFIRM, settings.confirm_delete);
    settings.paste_after_select =
        bool_default(&defaults, K_PASTE_AFTER_SELECT, settings.paste_after_select);
    settings.start_at_login = bool_default(&defaults, K_LOGIN, settings.start_at_login);
    settings.hotkey_enabled = bool_default(&defaults, K_HOTKEY_ENABLED, settings.hotkey_enabled);
    settings.pinned_background =
        rgb_default(&defaults, K_PINNED_BACKGROUND, settings.pinned_background);
    settings.pinned_indicator =
        rgb_default(&defaults, K_PINNED_INDICATOR, settings.pinned_indicator);

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
    defaults.setInteger_forKey(settings.max_text as isize, &NSString::from_str(K_MAX_TEXT));
    defaults.setInteger_forKey(
        settings.max_images as isize,
        &NSString::from_str(K_MAX_IMAGES),
    );
    defaults.setInteger_forKey(
        settings.max_commands as isize,
        &NSString::from_str(K_MAX_COMMANDS),
    );
    defaults.setBool_forKey(settings.record, &NSString::from_str(K_RECORD));
    defaults.setBool_forKey(settings.confirm_delete, &NSString::from_str(K_CONFIRM));
    defaults.setBool_forKey(
        settings.paste_after_select,
        &NSString::from_str(K_PASTE_AFTER_SELECT),
    );
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
    defaults.setInteger_forKey(
        settings.pinned_background.to_u32() as isize,
        &NSString::from_str(K_PINNED_BACKGROUND),
    );
    defaults.setInteger_forKey(
        settings.pinned_indicator.to_u32() as isize,
        &NSString::from_str(K_PINNED_INDICATOR),
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
    if let Ok(suite) = std::env::var("ANTIRUSTPASTE_DEFAULTS_SUITE")
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

fn limit_default(defaults: &NSUserDefaults, key: &str) -> Option<usize> {
    let key = NSString::from_str(key);
    defaults.objectForKey(&key)?;
    let value = defaults.integerForKey(&key);
    if value > 0 {
        Some((value as usize).min(MAX_LIMIT))
    } else {
        None
    }
}

fn legacy_limit(defaults: &NSUserDefaults) -> Option<usize> {
    limit_default(defaults, K_MAX_LEGACY)
}

fn bool_default(defaults: &NSUserDefaults, key: &str, fallback: bool) -> bool {
    let key = NSString::from_str(key);
    if defaults.objectForKey(&key).is_some() {
        defaults.boolForKey(&key)
    } else {
        fallback
    }
}

fn rgb_default(defaults: &NSUserDefaults, key: &str, fallback: Rgb) -> Rgb {
    let key = NSString::from_str(key);
    if defaults.objectForKey(&key).is_some() {
        Rgb::from_u32(defaults.integerForKey(&key).max(0) as u32)
    } else {
        fallback
    }
}

/// `ANTIRUSTPASTE_LAUNCH_AGENT_DIR` redirects the login item to a scratch
/// directory. Agents and tests MUST set it: changing the real LaunchAgents
/// folder affects the operator's next login.
fn launch_agent_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("ANTIRUSTPASTE_LAUNCH_AGENT_DIR")
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
