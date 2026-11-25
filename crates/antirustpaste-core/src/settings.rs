//! Platform-neutral preferences model.
//!
//! Persistence lives in the platform shell: macOS uses `NSUserDefaults`, future
//! Windows/Linux shells can use a JSON file or their own store.

use serde::{Deserialize, Serialize};

pub const MAX_LIMIT: usize = 9999;
pub const DEFAULT_LIMIT: usize = 200;

/// Platform-neutral modifier bits. The macOS shell maps these to NSEvent flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Modifiers(pub u32);

impl Modifiers {
    pub const SHIFT: Self = Self(1 << 0);
    pub const CONTROL: Self = Self(1 << 1);
    pub const OPTION: Self = Self(1 << 2);
    pub const COMMAND: Self = Self(1 << 3);

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IgnoredApp {
    pub name: String,
    pub bundle_id: String,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub max_clips: usize,
    pub record: bool,
    pub confirm_delete: bool,
    pub start_at_login: bool,
    pub ignored_apps: Vec<IgnoredApp>,
    pub hotkey_keycode: u16,
    pub hotkey_modifiers: Modifiers,
    pub hotkey_display: String,
    pub hotkey_enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_clips: DEFAULT_LIMIT,
            record: true,
            confirm_delete: true,
            start_at_login: false,
            ignored_apps: Vec::new(),
            hotkey_keycode: 9, // V
            hotkey_modifiers: Modifiers::COMMAND | Modifiers::SHIFT,
            hotkey_display: "⇧⌘V".to_string(),
            hotkey_enabled: true,
        }
    }
}

impl Settings {
    pub fn is_ignored(&self, bundle_id: Option<&str>) -> bool {
        let Some(bundle) = bundle_id else {
            return false;
        };
        self.ignored_apps.iter().any(|a| a.bundle_id == bundle)
    }

    pub fn add_ignored(&mut self, app: IgnoredApp) {
        if app.bundle_id.is_empty() {
            return;
        }
        if !self
            .ignored_apps
            .iter()
            .any(|a| a.bundle_id == app.bundle_id)
        {
            self.ignored_apps.push(app);
            self.ignored_apps.sort_by_key(|a| a.name.to_lowercase());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, bundle_id: &str) -> IgnoredApp {
        IgnoredApp {
            name: name.to_string(),
            bundle_id: bundle_id.to_string(),
        }
    }

    #[test]
    fn ignored_apps_are_deduplicated_and_sorted() {
        let mut settings = Settings::default();
        settings.add_ignored(app("Zebra", "com.example.zebra"));
        settings.add_ignored(app("alpha", "com.example.alpha"));
        settings.add_ignored(app("alpha again", "com.example.alpha"));
        settings.add_ignored(app("No bundle", ""));

        let names: Vec<&str> = settings
            .ignored_apps
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "Zebra"]);
        assert!(settings.is_ignored(Some("com.example.alpha")));
        assert!(!settings.is_ignored(Some("com.example.other")));
        assert!(!settings.is_ignored(None));
    }

    #[test]
    fn modifiers_are_platform_neutral_bits() {
        let modifiers = Modifiers::COMMAND | Modifiers::SHIFT;
        assert_eq!(modifiers.bits(), 0b1001);
        assert!(modifiers.contains(Modifiers::SHIFT));
        assert!(!modifiers.contains(Modifiers::CONTROL));
    }
}
