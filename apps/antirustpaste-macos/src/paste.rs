//! Synthesising the paste keystroke after the user picks a clipping.

use std::ffi::c_void;
use std::ptr::null_mut;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSDictionary, NSNumber, NSString};

// SAFETY: these are the public CoreGraphics event APIs. The pointers are
// created and released in `simulate_paste`; the tap and key-code constants match
// the SDK headers (`kCGHIDEventTap` and kVK_ANSI_V).
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreateKeyboardEvent(
        source: *mut c_void,
        virtual_key: u16,
        key_down: bool,
    ) -> *mut c_void;
    fn CGEventSetFlags(event: *mut c_void, flags: u64);
    fn CGEventPost(tap: u32, event: *mut c_void);
}

// SAFETY: `CFRelease` accepts an owned CoreFoundation object; callers pass only
// pointers returned by a Create function and never release them twice.
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

// SAFETY: both functions take no borrowed pointers except an optional
// CFDictionary for the options variant; the object is a live NSDictionary for
// the duration of the call.
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
}

const K_CG_EVENT_FLAG_MASK_COMMAND: u64 = 1 << 20;
const K_CGHID_EVENT_TAP: u32 = 0;
const KEY_V: u16 = 9;

/// Ask macOS for Accessibility access the first time we need to send ⌘V.
/// Returns whether the process is currently trusted.
pub fn ensure_accessibility() -> bool {
    // SAFETY: the dictionary is a live Foundation object for the duration of
    // the call; the API only reads it and returns a trust flag.
    unsafe {
        if AXIsProcessTrusted() {
            return true;
        }
        let key = NSString::from_str("AXTrustedCheckOptionPrompt");
        let value = NSNumber::numberWithBool(true);
        let value_obj: Retained<AnyObject> = Retained::from(value);
        let keys: [&NSString; 1] = [&key];
        let objects: [&AnyObject; 1] = [&value_obj];
        let options: Retained<NSDictionary<NSString, AnyObject>> =
            NSDictionary::from_slices(&keys, &objects);
        AXIsProcessTrustedWithOptions(&*options as *const _ as *const c_void);
        AXIsProcessTrusted()
    }
}

pub fn simulate_paste() {
    // SAFETY: each event is created by CoreGraphics, posted once and released
    // exactly once. A failed Create returns null and is handled below.
    unsafe {
        let down = CGEventCreateKeyboardEvent(null_mut(), KEY_V, true);
        if down.is_null() {
            return;
        }
        CGEventSetFlags(down, K_CG_EVENT_FLAG_MASK_COMMAND);
        CGEventPost(K_CGHID_EVENT_TAP, down);
        CFRelease(down as *const c_void);

        let up = CGEventCreateKeyboardEvent(null_mut(), KEY_V, false);
        if up.is_null() {
            return;
        }
        CGEventSetFlags(up, K_CG_EVENT_FLAG_MASK_COMMAND);
        CGEventPost(K_CGHID_EVENT_TAP, up);
        CFRelease(up as *const c_void);
    }
}
