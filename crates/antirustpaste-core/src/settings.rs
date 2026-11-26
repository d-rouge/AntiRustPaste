//! Platform-neutral preferences model.
//!
//! Persistence lives in the platform shell: macOS uses `NSUserDefaults`, future
//! Windows/Linux shells can use a JSON file or their own store.

use serde::{Deserialize, Serialize};

pub const MAX_LIMIT: usize = 9999;
pub const DEFAULT_LIMIT: usize = 200;

/// A platform-neutral 8-bit sRGB colour. The macOS shell converts it to
/// `NSColor`; other shells can use it directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl Rgb {
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    pub const fn to_u32(self) -> u32 {
        ((self.red as u32) << 16) | ((self.green as u32) << 8) | self.blue as u32
    }

    pub const fn from_u32(value: u32) -> Self {
        Self::new(
            ((value >> 16) & 0xff) as u8,
            ((value >> 8) & 0xff) as u8,
            (value & 0xff) as u8,
        )
    }
}

/// Default pinned-row tint. The app renders it at low opacity, so this warm
/// yellow becomes a soft paper-like card rather than a saturated block.
pub const DEFAULT_PINNED_BACKGROUND: Rgb = Rgb::new(250, 230, 55);
/// Default pinned marker colour: a deep violet, the complementary hue of the
/// warm-yellow card. It is distinct from the blue selection and avoids the
/// recording/error association of red.
pub const DEFAULT_PINNED_INDICATOR: Rgb = Rgb::new(94, 53, 177);

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
    pub paste_after_select: bool,
    pub start_at_login: bool,
    pub ignored_apps: Vec<IgnoredApp>,
    pub hotkey_keycode: u16,
    pub hotkey_modifiers: Modifiers,
    pub hotkey_display: String,
    pub hotkey_enabled: bool,
    pub pinned_background: Rgb,
    pub pinned_indicator: Rgb,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_clips: DEFAULT_LIMIT,
            record: true,
            confirm_delete: true,
            paste_after_select: false,
            start_at_login: false,
            ignored_apps: Vec::new(),
            hotkey_keycode: 9, // V
            hotkey_modifiers: Modifiers::COMMAND | Modifiers::SHIFT,
            hotkey_display: "⇧⌘V".to_string(),
            hotkey_enabled: true,
            pinned_background: DEFAULT_PINNED_BACKGROUND,
            pinned_indicator: DEFAULT_PINNED_INDICATOR,
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
    fn selecting_a_clipping_copies_by_default() {
        assert!(!Settings::default().paste_after_select);
    }

    #[test]
    fn modifiers_are_platform_neutral_bits() {
        let modifiers = Modifiers::COMMAND | Modifiers::SHIFT;
        assert_eq!(modifiers.bits(), 0b1001);
        assert!(modifiers.contains(Modifiers::SHIFT));
        assert!(!modifiers.contains(Modifiers::CONTROL));
    }

    #[test]
    fn pinned_colours_have_defaults_and_round_trip() {
        let settings = Settings::default();
        assert_eq!(DEFAULT_PINNED_BACKGROUND, Rgb::new(250, 230, 55));
        assert_eq!(DEFAULT_PINNED_INDICATOR, Rgb::new(94, 53, 177));
        assert_eq!(settings.pinned_background, DEFAULT_PINNED_BACKGROUND);
        assert_eq!(settings.pinned_indicator, DEFAULT_PINNED_INDICATOR);
        assert_eq!(
            Rgb::from_u32(settings.pinned_background.to_u32()),
            settings.pinned_background
        );
    }

    #[test]
    fn rgb_packing_is_bounded_to_24_bits() {
        assert_eq!(Rgb::from_u32(u32::MAX), Rgb::new(255, 255, 255));
        assert_eq!(Rgb::new(1, 2, 3).to_u32(), 0x010203);
    }
}
