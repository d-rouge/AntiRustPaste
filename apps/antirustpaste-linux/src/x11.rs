//! X11 integration via pure-Rust `x11rb`: active window detection, global
//! hot key grabbing (`XGrabKey`) and synthetic `Ctrl+V` (`XTest`).
//!
//! When running under a pure Wayland session without XWayland, `X11Helper::connect`
//! returns `None` and callers degrade gracefully without crashing.

use std::collections::HashMap;

use antirustpaste_core::Modifiers;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt, GrabMode, KeyButMask, ModMask, Window,
};
use x11rb::protocol::xtest::ConnectionExt as XTestExt;
use x11rb::rust_connection::RustConnection;

const KEYCODE_ESCAPE: u8 = 9;
const KEY_PRESS_EVENT: u8 = 2;
const KEY_RELEASE_EVENT: u8 = 3;

pub struct X11Helper {
    conn: RustConnection,
    root: Window,
    atom_net_active_window: Atom,
    atom_net_wm_name: Atom,
    atom_utf8_string: Atom,
    atom_wm_class: Atom,
    keysym_to_keycode: HashMap<u32, u8>,
    grabbed: Option<(u8, u16)>,
}

impl X11Helper {
    pub fn connect() -> Option<Self> {
        let (conn, screen_num) = RustConnection::connect(None).ok()?;
        let screen = conn.setup().roots.get(screen_num)?;
        let root = screen.root;
        let atom_net_active_window = intern(&conn, b"_NET_ACTIVE_WINDOW")?;
        let atom_net_wm_name = intern(&conn, b"_NET_WM_NAME")?;
        let atom_utf8_string = intern(&conn, b"UTF8_STRING")?;
        let atom_wm_class = AtomEnum::WM_CLASS.into();
        let keysym_to_keycode = build_keymap(&conn);

        Some(Self {
            conn,
            root,
            atom_net_active_window,
            atom_net_wm_name,
            atom_utf8_string,
            atom_wm_class,
            keysym_to_keycode,
            grabbed: None,
        })
    }

    /// Return `(app_name, wm_class)` for the currently focused X11 window.
    pub fn active_app(&self) -> Option<(String, String)> {
        let reply = self
            .conn
            .get_property(
                false,
                self.root,
                self.atom_net_active_window,
                AtomEnum::WINDOW,
                0,
                1,
            )
            .ok()?
            .reply()
            .ok()?;
        let win = reply.value32()?.next()?;
        if win == 0 {
            return None;
        }

        let class_reply = self
            .conn
            .get_property(false, win, self.atom_wm_class, AtomEnum::STRING, 0, 256)
            .ok()?
            .reply()
            .ok();
        let (instance, class) = class_reply
            .as_ref()
            .map(|r| parse_wm_class(&r.value))
            .unwrap_or_default();

        let title = self
            .conn
            .get_property(
                false,
                win,
                self.atom_net_wm_name,
                self.atom_utf8_string,
                0,
                256,
            )
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .and_then(|r| String::from_utf8(r.value).ok())
            .unwrap_or_default();

        let name = if !class.is_empty() {
            class.clone()
        } else if !instance.is_empty() {
            instance.clone()
        } else if !title.is_empty() {
            title
        } else {
            return None;
        };

        let bundle = if !class.is_empty() {
            class
        } else {
            name.clone()
        };
        Some((name, bundle))
    }

    pub fn register_hotkey(&mut self, modifiers: Modifiers, keysym: u16) -> bool {
        self.ungrab_hotkey();
        let Some(&keycode) = self.keysym_to_keycode.get(&(keysym as u32)) else {
            eprintln!("antirustpaste-linux: no X11 keycode for keysym 0x{keysym:x}");
            return false;
        };
        let mask = to_mod_mask(modifiers);
        for extra in [
            0u16,
            u16::from(ModMask::LOCK),
            u16::from(ModMask::M2),
            u16::from(ModMask::LOCK | ModMask::M2),
        ] {
            let _ = self.conn.grab_key(
                true,
                self.root,
                ModMask::from(mask | extra),
                keycode,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            );
        }
        let _ = self.conn.flush();
        self.grabbed = Some((keycode, mask));
        true
    }

    pub fn ungrab_hotkey(&mut self) {
        let Some((keycode, mask)) = self.grabbed.take() else {
            return;
        };
        for extra in [
            0u16,
            u16::from(ModMask::LOCK),
            u16::from(ModMask::M2),
            u16::from(ModMask::LOCK | ModMask::M2),
        ] {
            let _ = self
                .conn
                .ungrab_key(keycode, self.root, ModMask::from(mask | extra));
        }
        let _ = self.conn.flush();
    }

    /// Non-blocking check for a grabbed hotkey press.
    pub fn poll_hotkey(&self) -> bool {
        let Some((target_code, target_mask)) = self.grabbed else {
            return false;
        };
        let mut triggered = false;
        while let Ok(Some(event)) = self.conn.poll_for_event() {
            if let Event::KeyPress(ev) = event {
                let state = u16::from(ev.state)
                    & !(u16::from(KeyButMask::LOCK) | u16::from(KeyButMask::MOD2));
                if ev.detail == target_code && state == target_mask {
                    triggered = true;
                }
            }
        }
        triggered
    }

    /// Synthesise `Ctrl+V` into the currently focused X11 window via XTest.
    pub fn send_ctrl_v(&self) -> bool {
        // Control_L = 0xffe3, 'v' = 0x0076
        let Some(&ctrl_code) = self.keysym_to_keycode.get(&0xffe3) else {
            return false;
        };
        let Some(&v_code) = self.keysym_to_keycode.get(&0x0076) else {
            return false;
        };
        let ok = self
            .conn
            .xtest_fake_input(KEY_PRESS_EVENT, ctrl_code, 0, self.root, 0, 0, 0)
            .is_ok()
            && self
                .conn
                .xtest_fake_input(KEY_PRESS_EVENT, v_code, 0, self.root, 0, 0, 0)
                .is_ok()
            && self
                .conn
                .xtest_fake_input(KEY_RELEASE_EVENT, v_code, 0, self.root, 0, 0, 0)
                .is_ok()
            && self
                .conn
                .xtest_fake_input(KEY_RELEASE_EVENT, ctrl_code, 0, self.root, 0, 0, 0)
                .is_ok();
        let _ = self.conn.flush();
        ok
    }
}

impl Drop for X11Helper {
    fn drop(&mut self) {
        self.ungrab_hotkey();
    }
}

fn intern(conn: &RustConnection, name: &[u8]) -> Option<Atom> {
    conn.intern_atom(false, name)
        .ok()?
        .reply()
        .ok()
        .map(|r| r.atom)
}

fn parse_wm_class(bytes: &[u8]) -> (String, String) {
    let mut parts = bytes.split(|&b| b == 0).filter(|s| !s.is_empty());
    let instance = parts
        .next()
        .and_then(|s| std::str::from_utf8(s).ok())
        .unwrap_or_default()
        .to_string();
    let class = parts
        .next()
        .and_then(|s| std::str::from_utf8(s).ok())
        .unwrap_or_default()
        .to_string();
    (instance, class)
}

fn build_keymap(conn: &RustConnection) -> HashMap<u32, u8> {
    let mut map = HashMap::new();
    let min_kc = conn.setup().min_keycode;
    let max_kc = conn.setup().max_keycode;
    let count = max_kc.saturating_sub(min_kc).saturating_add(1);
    let Ok(cookie) = conn.get_keyboard_mapping(min_kc, count) else {
        return map;
    };
    let Ok(reply) = cookie.reply() else {
        return map;
    };
    let per = usize::from(reply.keysyms_per_keycode);
    if per == 0 {
        return map;
    }
    for (offset, chunk) in reply.keysyms.chunks(per).enumerate() {
        let Some(keycode) = min_kc.checked_add(offset as u8) else {
            break;
        };
        if keycode < KEYCODE_ESCAPE {
            continue;
        }
        for &sym in chunk.iter().take(2) {
            if sym != 0 {
                map.entry(sym).or_insert(keycode);
            }
        }
    }
    map
}

fn to_mod_mask(modifiers: Modifiers) -> u16 {
    let mut mask = 0u16;
    if modifiers.contains(Modifiers::SHIFT) {
        mask |= u16::from(ModMask::SHIFT);
    }
    if modifiers.contains(Modifiers::CONTROL) {
        mask |= u16::from(ModMask::CONTROL);
    }
    if modifiers.contains(Modifiers::OPTION) {
        mask |= u16::from(ModMask::M1);
    }
    if modifiers.contains(Modifiers::COMMAND) {
        mask |= u16::from(ModMask::M4);
    }
    mask
}
