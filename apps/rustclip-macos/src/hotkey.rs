//! System-wide hotkey through the Carbon HotKey API.
//!
//! Carbon is deprecated for UI, but `RegisterEventHotKey` is still the only
//! non-Accessibility way to register a global shortcut, and the API is a
//! handful of C functions. It is called directly via FFI here.

use std::ffi::c_void;
use std::ptr::null_mut;

use rustclip_core::Modifiers;

type OSStatus = i32;
type EventRef = *mut c_void;
type EventHandlerRef = *mut c_void;
type EventHotKeyRef = *mut c_void;
type EventTargetRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;

#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

// SAFETY: these signatures mirror the Carbon Event Manager headers. The
// application event target and the event type describe a keyboard hot-key
// event; references written through the out-parameters are checked for null and
// released by `Hotkey::drop`.
#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn InstallEventHandler(
        target: EventTargetRef,
        handler: Option<
            unsafe extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OSStatus,
        >,
        num_types: usize,
        type_list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut EventHandlerRef,
    ) -> OSStatus;
    fn RemoveEventHandler(handler_ref: EventHandlerRef) -> OSStatus;
    fn RegisterEventHotKey(
        hot_key_code: u32,
        hot_key_modifiers: u32,
        hot_key_id: EventHotKeyID,
        target: EventTargetRef,
        options: u32,
        out_ref: *mut EventHotKeyRef,
    ) -> OSStatus;
    fn UnregisterEventHotKey(hot_key_ref: EventHotKeyRef) -> OSStatus;
}

const K_EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
const K_HOT_KEY_SIGNATURE: u32 = u32::from_be_bytes(*b"RCLP");

const CARBON_CMD: u32 = 1 << 8;
const CARBON_SHIFT: u32 = 1 << 9;
const CARBON_OPTION: u32 = 1 << 11;
const CARBON_CONTROL: u32 = 1 << 12;

unsafe extern "C" fn hotkey_handler(
    _call_ref: EventHandlerCallRef,
    _event: EventRef,
    _user_data: *mut c_void,
) -> OSStatus {
    // The handler runs on the main thread through Carbon's event loop. Guard it
    // like every other system callback: a panic must not cross this boundary.
    crate::app::guard("global_hotkey", crate::app::on_global_hotkey);
    0
}

pub struct Hotkey {
    hotkey: EventHotKeyRef,
    handler: EventHandlerRef,
}

impl Drop for Hotkey {
    fn drop(&mut self) {
        // SAFETY: both references came from the registration calls above and
        // are released exactly once; null is checked first.
        unsafe {
            if !self.hotkey.is_null() {
                UnregisterEventHotKey(self.hotkey);
            }
            if !self.handler.is_null() {
                RemoveEventHandler(self.handler);
            }
        }
    }
}

/// Convert platform-neutral modifiers to Carbon modifier flags.
pub fn carbon_modifiers(modifiers: Modifiers) -> u32 {
    let mut out = 0;
    if modifiers.contains(Modifiers::COMMAND) {
        out |= CARBON_CMD;
    }
    if modifiers.contains(Modifiers::SHIFT) {
        out |= CARBON_SHIFT;
    }
    if modifiers.contains(Modifiers::OPTION) {
        out |= CARBON_OPTION;
    }
    if modifiers.contains(Modifiers::CONTROL) {
        out |= CARBON_CONTROL;
    }
    out
}

/// Convert NSEvent modifier flags from the AppKit shell to core bits.
pub fn modifiers_from_nsevent(flags: usize) -> Modifiers {
    const NS_SHIFT: usize = 1 << 17;
    const NS_CONTROL: usize = 1 << 18;
    const NS_OPTION: usize = 1 << 19;
    const NS_COMMAND: usize = 1 << 20;

    let mut modifiers = Modifiers(0);
    if flags & NS_SHIFT != 0 {
        modifiers |= Modifiers::SHIFT;
    }
    if flags & NS_CONTROL != 0 {
        modifiers |= Modifiers::CONTROL;
    }
    if flags & NS_OPTION != 0 {
        modifiers |= Modifiers::OPTION;
    }
    if flags & NS_COMMAND != 0 {
        modifiers |= Modifiers::COMMAND;
    }
    modifiers
}

/// A human-readable shortcut such as `⇧⌘V`.
pub fn display_shortcut(keycode: u16, modifiers: Modifiers, chars: Option<&str>) -> String {
    let mut out = String::new();
    if modifiers.contains(Modifiers::CONTROL) {
        out.push('⌃');
    }
    if modifiers.contains(Modifiers::OPTION) {
        out.push('⌥');
    }
    if modifiers.contains(Modifiers::SHIFT) {
        out.push('⇧');
    }
    if modifiers.contains(Modifiers::COMMAND) {
        out.push('⌘');
    }
    let key = chars
        .filter(|c| !c.trim().is_empty())
        .map(|c| c.to_uppercase())
        .unwrap_or_else(|| keycode_name(keycode).to_string());
    out.push_str(&key);
    out
}

fn keycode_name(code: u16) -> &'static str {
    match code {
        0 => "A",
        1 => "S",
        2 => "D",
        3 => "F",
        4 => "H",
        5 => "G",
        6 => "Z",
        7 => "X",
        8 => "C",
        9 => "V",
        11 => "B",
        12 => "Q",
        13 => "W",
        14 => "E",
        15 => "R",
        16 => "Y",
        17 => "T",
        18 => "1",
        19 => "2",
        20 => "3",
        21 => "4",
        22 => "6",
        23 => "5",
        24 => "=",
        25 => "9",
        26 => "7",
        27 => "-",
        28 => "8",
        29 => "0",
        30 => "]",
        31 => "O",
        32 => "U",
        33 => "[",
        34 => "I",
        35 => "P",
        36 => "↩",
        37 => "L",
        38 => "J",
        39 => "'",
        40 => "K",
        41 => ";",
        42 => "\\",
        43 => ",",
        44 => "/",
        45 => "N",
        46 => "M",
        47 => ".",
        48 => "⇥",
        49 => "Space",
        50 => "`",
        51 => "⌫",
        53 => "⎋",
        96 => "F5",
        97 => "F6",
        98 => "F7",
        99 => "F3",
        100 => "F8",
        101 => "F9",
        103 => "F11",
        105 => "F13",
        107 => "F14",
        109 => "F10",
        111 => "F12",
        113 => "F15",
        114 => "Help",
        115 => "Home",
        116 => "PgUp",
        117 => "⌦",
        118 => "F4",
        119 => "End",
        120 => "F2",
        121 => "PgDn",
        122 => "F1",
        123 => "←",
        124 => "→",
        125 => "↓",
        126 => "↑",
        _ => "?",
    }
}

/// Register (or re-register) the global shortcut. The previous registration
/// must be dropped by the caller first.
pub fn register(keycode: u16, modifiers: Modifiers) -> Result<Hotkey, OSStatus> {
    let carbon = carbon_modifiers(modifiers);
    if carbon == 0 {
        return Err(-50);
    }

    let event_type = EventTypeSpec {
        event_class: K_EVENT_CLASS_KEYBOARD,
        event_kind: K_EVENT_HOT_KEY_PRESSED,
    };

    // SAFETY: the target and type list outlive the synchronous install call;
    // Carbon writes the out-parameters, whose values `Hotkey` later releases.
    unsafe {
        let target = GetApplicationEventTarget();
        let mut handler: EventHandlerRef = null_mut();
        let status = InstallEventHandler(
            target,
            Some(hotkey_handler),
            1,
            &event_type,
            null_mut(),
            &mut handler,
        );
        if status != 0 {
            return Err(status);
        }

        let id = EventHotKeyID {
            signature: K_HOT_KEY_SIGNATURE,
            id: 1,
        };
        let mut hotkey: EventHotKeyRef = null_mut();
        let status = RegisterEventHotKey(keycode as u32, carbon, id, target, 0, &mut hotkey);
        if status != 0 {
            RemoveEventHandler(handler);
            return Err(status);
        }

        Ok(Hotkey { hotkey, handler })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_shortcut_orders_modifiers_and_uppercases_the_key() {
        assert_eq!(
            display_shortcut(9, Modifiers::COMMAND | Modifiers::SHIFT, Some("v")),
            "⇧⌘V"
        );
        assert_eq!(
            display_shortcut(9, Modifiers::COMMAND | Modifiers::SHIFT, None),
            "⇧⌘V"
        );
        assert_eq!(
            display_shortcut(
                49,
                Modifiers::CONTROL | Modifiers::OPTION | Modifiers::COMMAND,
                Some(" ")
            ),
            "⌃⌥⌘Space"
        );
    }

    #[test]
    fn carbon_modifiers_map_one_to_one() {
        let modifiers =
            Modifiers::COMMAND | Modifiers::SHIFT | Modifiers::OPTION | Modifiers::CONTROL;
        assert_eq!(
            carbon_modifiers(modifiers),
            CARBON_CMD | CARBON_SHIFT | CARBON_OPTION | CARBON_CONTROL
        );
        assert_eq!(carbon_modifiers(Modifiers::SHIFT), CARBON_SHIFT);
    }
}
