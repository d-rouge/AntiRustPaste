//! Application coordinator for the Linux GTK4 + X11 shell.

use std::cell::RefCell;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use antirustpaste_core::{
    ClipKind, Section, Settings, Store, ThumbnailService, filter_indices, shell_history,
};
use gtk4::gdk;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::clipboard;
use crate::settings;
use crate::thumbnails;
use crate::tray::{self, TrayAction, TrayHandle};
use crate::ui::{self, PopupWidgets, PrefsWidgets};
use crate::x11::X11Helper;

struct AppState {
    store: Store,
    settings: Settings,
    thumbs: ThumbnailService,
    thumb_dir: PathBuf,
    section: Section,
    query: String,
    visible_indices: Vec<usize>,
    ignore_next_clipboard_change: bool,
    x11: Option<X11Helper>,
    popup: PopupWidgets,
    prefs: PrefsWidgets,
    _tray: Option<TrayHandle>,
    tray_rx: Receiver<TrayAction>,
    hold_guard: Option<gio::ApplicationHoldGuard>,
}

pub fn run() {
    let app = adw::Application::builder()
        .application_id("com.antirustpaste.app")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    app.connect_activate(|app| {
        setup(app);
    });

    // Pass no extra CLI args to GTK so --help/--version stay ours.
    let empty: [&str; 0] = [];
    app.run_with_args(&empty);
}

fn setup(app: &adw::Application) {
    ui::install_css();

    let loaded_settings = settings::load();
    let store = Store::open();
    let thumb_dir = thumbnails::cache_dir();
    let valid_hashes: HashSet<u64> = store
        .clips
        .iter()
        .filter(|c| c.kind == ClipKind::Image)
        .map(|c| c.hash)
        .collect();
    antirustpaste_core::thumbnails::prune_now(&thumb_dir, &valid_hashes);
    let thumbs = ThumbnailService::new(thumb_dir.clone());

    let mut x11 = X11Helper::connect();
    if let Some(helper) = x11.as_mut()
        && loaded_settings.hotkey_enabled
    {
        let _ = helper.register_hotkey(
            loaded_settings.hotkey_modifiers,
            loaded_settings.hotkey_keycode,
        );
    }

    let (tray_tx, tray_rx) = mpsc::channel();
    let tray_handle = tray::spawn_tray(tray_tx);

    let popup = ui::build_popup(app);
    let prefs = ui::build_prefs(app, &loaded_settings);

    let initial_section = if env_flag("ANTIRUSTPASTE_OPEN_IMAGES") {
        popup.tab_images.set_active(true);
        Section::Images
    } else if env_flag("ANTIRUSTPASTE_OPEN_COMMANDS") {
        popup.tab_commands.set_active(true);
        Section::Commands
    } else {
        Section::Text
    };

    let hold_guard = Some(app.hold());

    let state = Rc::new(RefCell::new(AppState {
        store,
        settings: loaded_settings,
        thumbs,
        thumb_dir,
        section: initial_section,
        query: String::new(),
        visible_indices: Vec::new(),
        ignore_next_clipboard_change: false,
        x11,
        popup,
        prefs,
        _tray: tray_handle,
        tray_rx,
        hold_guard,
    }));

    wire_popup(&state);
    wire_prefs(&state);
    wire_clipboard(&state);
    wire_timers(app, &state);

    refresh_list(&state);

    if env_flag("ANTIRUSTPASTE_OPEN_PREFS") {
        show_prefs(&state);
    }
    if env_flag("ANTIRUSTPASTE_OPEN_PANEL")
        || env_flag("ANTIRUSTPASTE_OPEN_IMAGES")
        || env_flag("ANTIRUSTPASTE_OPEN_COMMANDS")
    {
        if initial_section == Section::Commands {
            reload_shell_history(&state);
        }
        show_popup(&state);
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

fn refresh_list(state: &Rc<RefCell<AppState>>) {
    let mut st = state.borrow_mut();
    let visible = filter_indices(&st.store.clips, st.section, &st.query);
    st.visible_indices = visible.clone();
    let section = st.section;
    let popup = st.popup.clone();

    let state_pin = Rc::clone(state);
    let on_pin: Rc<dyn Fn(usize)> = Rc::new(move |idx| {
        toggle_pin(&state_pin, idx);
    });

    let state_del = Rc::clone(state);
    let on_del: Rc<dyn Fn(usize)> = Rc::new(move |idx| {
        request_delete(&state_del, idx);
    });

    let AppState { store, thumbs, .. } = &mut *st;
    ui::rebuild_list(&popup, store, thumbs, &visible, section, on_pin, on_del);
}

fn show_popup(state: &Rc<RefCell<AppState>>) {
    let (window, search) = {
        let st = state.borrow();
        (st.popup.window.clone(), st.popup.search.clone())
    };
    window.present();
    search.grab_focus();
}

fn toggle_popup(state: &Rc<RefCell<AppState>>) {
    let visible = state.borrow().popup.window.is_visible();
    if visible {
        let win = state.borrow().popup.window.clone();
        win.set_visible(false);
    } else {
        if state.borrow().section == Section::Commands {
            reload_shell_history(state);
        }
        show_popup(state);
    }
}

fn show_prefs(state: &Rc<RefCell<AppState>>) {
    let win = state.borrow().prefs.window.clone();
    win.present();
}

fn choose_clip(state: &Rc<RefCell<AppState>>, store_index: usize) {
    let (copied, paste_after, popup_win) = {
        let mut st = state.borrow_mut();
        let Some(clip) = st.store.clips.get(store_index).cloned() else {
            return;
        };
        let Some(display) = gdk::Display::default() else {
            return;
        };
        let cb = display.clipboard();
        st.ignore_next_clipboard_change = true;
        let ok = clipboard::write_clip(&cb, &st.store, &clip);
        (ok, st.settings.paste_after_select, st.popup.window.clone())
    };

    if !copied {
        return;
    }
    popup_win.set_visible(false);

    if paste_after {
        let state_paste = Rc::clone(state);
        glib::timeout_add_local_once(Duration::from_millis(80), move || {
            if let Some(x11) = state_paste.borrow().x11.as_ref() {
                let _ = x11.send_ctrl_v();
            }
        });
    }
}

fn toggle_pin(state: &Rc<RefCell<AppState>>, store_index: usize) {
    {
        let mut st = state.borrow_mut();
        let Some(current) = st.store.clips.get(store_index).map(|c| c.pinned) else {
            return;
        };
        if st.store.set_pinned(store_index, !current).is_some() {
            st.store.save();
        }
    }
    refresh_list(state);
}

fn request_delete(state: &Rc<RefCell<AppState>>, store_index: usize) {
    let (confirm, parent) = {
        let st = state.borrow();
        (st.settings.confirm_delete, st.popup.window.clone())
    };
    if !confirm {
        delete_clip(state, store_index);
        return;
    }
    let state_yes = Rc::clone(state);
    ui::confirm_dialog(&parent, "Delete this clipping?", move || {
        delete_clip(&state_yes, store_index);
    });
}

fn delete_clip(state: &Rc<RefCell<AppState>>, store_index: usize) {
    {
        let mut st = state.borrow_mut();
        if st.store.remove(store_index) {
            st.store.save();
        }
    }
    refresh_list(state);
}

fn request_clear_section(state: &Rc<RefCell<AppState>>) {
    let (section, confirm, parent) = {
        let st = state.borrow();
        (
            st.section,
            st.settings.confirm_delete,
            st.popup.window.clone(),
        )
    };
    let msg = match section {
        Section::Text => "Clear all Text clippings?",
        Section::Images => "Clear all Images clippings?",
        Section::Commands => return,
    };
    if !confirm {
        clear_section(state, section);
        return;
    }
    let state_yes = Rc::clone(state);
    ui::confirm_dialog(&parent, msg, move || {
        clear_section(&state_yes, section);
    });
}

fn clear_section(state: &Rc<RefCell<AppState>>, section: Section) {
    {
        let mut st = state.borrow_mut();
        match section {
            Section::Text => {
                st.store
                    .remove_where(|c| c.kind != ClipKind::Image && c.kind != ClipKind::Command);
            }
            Section::Images => {
                st.store.remove_where(|c| c.kind == ClipKind::Image);
                st.thumbs.clear();
            }
            Section::Commands => return,
        }
        st.store.save();
    }
    refresh_list(state);
}

fn request_clear_all(state: &Rc<RefCell<AppState>>) {
    let (confirm, parent) = {
        let st = state.borrow();
        (st.settings.confirm_delete, st.popup.window.clone())
    };
    if !confirm {
        clear_all_clippings(state);
        return;
    }
    let state_yes = Rc::clone(state);
    ui::confirm_dialog(
        &parent,
        "Clear all clippings? Terminal commands will be kept.",
        move || {
            clear_all_clippings(&state_yes);
        },
    );
}

fn clear_all_clippings(state: &Rc<RefCell<AppState>>) {
    {
        let mut st = state.borrow_mut();
        st.store.remove_where(|c| c.kind != ClipKind::Command);
        st.thumbs.clear();
        st.store.save();
    }
    refresh_list(state);
}

fn reload_shell_history(state: &Rc<RefCell<AppState>>) {
    let history_dir = if let Some(dir) = std::env::var_os("ANTIRUSTPASTE_SHELL_HISTORY_DIR")
        && !dir.is_empty()
    {
        Some(PathBuf::from(dir))
    } else if std::env::var_os("ANTIRUSTPASTE_DATA_DIR").is_some_and(|v| !v.is_empty()) {
        None
    } else {
        dirs::home_dir()
    };

    let Some(base) = history_dir else {
        return;
    };

    let files = [
        shell_history::ShellFile {
            shell: shell_history::Shell::Zsh,
            path: base.join(".zsh_history"),
        },
        shell_history::ShellFile {
            shell: shell_history::Shell::Bash,
            path: base.join(".bash_history"),
        },
    ];

    {
        let mut st = state.borrow_mut();
        let cursor_path = st.store.dir().join("shell-history.json");
        let mut cursors = shell_history::load_cursors(&cursor_path);
        let max_clips = st.settings.max_clips;
        let now = shell_history::now_ms();
        let mut changed = false;
        let mut cursor_changed = false;
        for file in &files {
            let read = shell_history::read_new(file, &mut cursors, max_clips, now);
            if read.cursor_changed {
                cursor_changed = true;
            }
            for cmd in read.commands {
                if st.store.upsert_command(
                    &cmd.text,
                    cmd.shell.label(),
                    cmd.date_ms,
                    max_clips,
                    cmd.may_move,
                ) {
                    changed = true;
                }
            }
        }
        if cursor_changed {
            shell_history::save_cursors(&cursor_path, &cursors);
        }
        if changed {
            st.store.save();
        }
    }
    refresh_list(state);
}

fn export_images_pdf(state: &Rc<RefCell<AppState>>) {
    let (parent, blobs) = {
        let st = state.borrow();
        let mut items = Vec::new();
        for clip in &st.store.clips {
            if clip.kind == ClipKind::Image
                && let Some(blob_name) = &clip.blob
                && let Some(path) = st.store.blob_file(blob_name)
                && let Ok(bytes) = fs::read(path)
            {
                items.push(bytes);
            }
        }
        (st.popup.window.clone(), items)
    };

    if blobs.is_empty() {
        let alert = adw::AlertDialog::builder()
            .heading("No images in history to export.")
            .build();
        alert.add_response("ok", "OK");
        alert.present(Some(&parent));
        return;
    }

    let dialog = gtk4::FileDialog::builder()
        .title("Export Images to PDF")
        .initial_name("AntiRustPaste-Images.pdf")
        .build();

    dialog.save(Some(&parent), gio::Cancellable::NONE, move |res| {
        let Ok(file) = res else {
            return;
        };
        let Some(dest) = file.path() else {
            return;
        };
        let pdf_images: Vec<antirustpaste_pdf::PdfImage> = blobs
            .into_iter()
            .map(|bytes| antirustpaste_pdf::PdfImage { bytes })
            .collect();
        match antirustpaste_pdf::write_pdf(&pdf_images) {
            Ok(pdf_bytes) => {
                let tmp = dest.with_extension("pdf.tmp");
                if let Err(err) = fs::write(&tmp, pdf_bytes).and_then(|_| fs::rename(&tmp, &dest)) {
                    eprintln!("antirustpaste-linux: failed to write PDF: {err}");
                }
            }
            Err(err) => {
                eprintln!("antirustpaste-linux: PDF generation failed: {err}");
            }
        }
    });
}

fn wire_popup(state: &Rc<RefCell<AppState>>) {
    let popup = state.borrow().popup.clone();

    let state_search = Rc::clone(state);
    popup.search.connect_search_changed(move |entry| {
        state_search.borrow_mut().query = entry.text().to_string();
        refresh_list(&state_search);
    });

    let state_text = Rc::clone(state);
    popup.tab_text.connect_toggled(move |btn| {
        if btn.is_active() {
            state_text.borrow_mut().section = Section::Text;
            refresh_list(&state_text);
        }
    });

    let state_img = Rc::clone(state);
    popup.tab_images.connect_toggled(move |btn| {
        if btn.is_active() {
            state_img.borrow_mut().section = Section::Images;
            refresh_list(&state_img);
        }
    });

    let state_cmd = Rc::clone(state);
    popup.tab_commands.connect_toggled(move |btn| {
        if btn.is_active() {
            state_cmd.borrow_mut().section = Section::Commands;
            reload_shell_history(&state_cmd);
        }
    });

    let state_act = Rc::clone(state);
    popup.list.connect_row_activated(move |list, _row| {
        if let Some(idx) = ui::selected_store_index(list) {
            choose_clip(&state_act, idx);
        }
    });

    let state_ref = Rc::clone(state);
    popup.refresh_btn.connect_clicked(move |_| {
        reload_shell_history(&state_ref);
    });

    let state_pdf = Rc::clone(state);
    popup.pdf_btn.connect_clicked(move |_| {
        export_images_pdf(&state_pdf);
    });

    let state_clr = Rc::clone(state);
    popup.clear_btn.connect_clicked(move |_| {
        request_clear_section(&state_clr);
    });

    let state_prefs = Rc::clone(state);
    popup.prefs_btn.connect_clicked(move |_| {
        show_prefs(&state_prefs);
    });

    let key_ctrl = gtk4::EventControllerKey::new();
    let state_key = Rc::clone(state);
    key_ctrl.connect_key_pressed(move |_, key, _code, mods| {
        let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
        match key {
            gdk::Key::Escape => {
                let win = state_key.borrow().popup.window.clone();
                win.set_visible(false);
                glib::Propagation::Stop
            }
            gdk::Key::Up => {
                let list = state_key.borrow().popup.list.clone();
                ui::move_selection(&list, -1);
                glib::Propagation::Stop
            }
            gdk::Key::Down => {
                let list = state_key.borrow().popup.list.clone();
                ui::move_selection(&list, 1);
                glib::Propagation::Stop
            }
            gdk::Key::Return | gdk::Key::KP_Enter => {
                let idx = {
                    let list = &state_key.borrow().popup.list;
                    ui::selected_store_index(list)
                };
                if let Some(i) = idx {
                    choose_clip(&state_key, i);
                }
                glib::Propagation::Stop
            }
            gdk::Key::p | gdk::Key::P if ctrl => {
                let idx = {
                    let list = &state_key.borrow().popup.list;
                    ui::selected_store_index(list)
                };
                if let Some(i) = idx {
                    toggle_pin(&state_key, i);
                }
                glib::Propagation::Stop
            }
            gdk::Key::r | gdk::Key::R if ctrl => {
                if state_key.borrow().section == Section::Commands {
                    reload_shell_history(&state_key);
                }
                glib::Propagation::Stop
            }
            gdk::Key::Delete | gdk::Key::BackSpace => {
                let (empty_query, idx) = {
                    let st = state_key.borrow();
                    (
                        st.query.is_empty(),
                        ui::selected_store_index(&st.popup.list),
                    )
                };
                if empty_query && let Some(i) = idx {
                    request_delete(&state_key, i);
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }
            _ => {
                if ctrl
                    && let Some(ch) = key.to_unicode()
                    && let Some(digit) = ch.to_digit(10)
                    && (1..=9).contains(&digit)
                {
                    let target = state_key
                        .borrow()
                        .visible_indices
                        .get((digit as usize).saturating_sub(1))
                        .copied();
                    if let Some(idx) = target {
                        choose_clip(&state_key, idx);
                    }
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }
        }
    });
    popup.window.add_controller(key_ctrl);
}

fn wire_prefs(state: &Rc<RefCell<AppState>>) {
    let prefs = state.borrow().prefs.clone();

    let state_max = Rc::clone(state);
    prefs.max_clips.connect_value_notify(move |spin| {
        let limit = spin.value().clamp(1.0, 9999.0) as usize;
        {
            let mut st = state_max.borrow_mut();
            st.settings.max_clips = limit;
            st.store.trim_sections(limit);
            st.store.save();
            settings::save(&st.settings);
        }
        refresh_list(&state_max);
    });

    let state_rec = Rc::clone(state);
    prefs.record.connect_active_notify(move |row| {
        let mut st = state_rec.borrow_mut();
        st.settings.record = row.is_active();
        settings::save(&st.settings);
    });

    let state_conf = Rc::clone(state);
    prefs.confirm_delete.connect_active_notify(move |row| {
        let mut st = state_conf.borrow_mut();
        st.settings.confirm_delete = row.is_active();
        settings::save(&st.settings);
    });

    let state_paste = Rc::clone(state);
    prefs.paste_after_select.connect_active_notify(move |row| {
        let mut st = state_paste.borrow_mut();
        st.settings.paste_after_select = row.is_active();
        settings::save(&st.settings);
    });

    let state_login = Rc::clone(state);
    prefs.start_at_login.connect_active_notify(move |row| {
        let active = row.is_active();
        let mut st = state_login.borrow_mut();
        st.settings.start_at_login = active;
        settings::save(&st.settings);
        settings::sync_autostart(active);
    });

    let state_hk = Rc::clone(state);
    prefs.hotkey_enabled.connect_active_notify(move |row| {
        let active = row.is_active();
        let mut st = state_hk.borrow_mut();
        st.settings.hotkey_enabled = active;
        let modifiers = st.settings.hotkey_modifiers;
        let keycode = st.settings.hotkey_keycode;
        if let Some(x11) = st.x11.as_mut() {
            if active {
                let _ = x11.register_hotkey(modifiers, keycode);
            } else {
                x11.ungrab_hotkey();
            }
        }
        settings::save(&st.settings);
    });

    let state_cache = Rc::clone(state);
    prefs.clear_cache_btn.connect_clicked(move |_| {
        {
            let mut st = state_cache.borrow_mut();
            st.thumbs.clear();
            let _ = fs::remove_dir_all(&st.thumb_dir);
        }
        refresh_list(&state_cache);
    });
}

fn wire_clipboard(state: &Rc<RefCell<AppState>>) {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let cb = display.clipboard();
    let state_cb = Rc::clone(state);
    cb.connect_changed(move |clipboard| {
        let (should_record, ignore_once, active) = {
            let mut st = state_cb.borrow_mut();
            if st.ignore_next_clipboard_change {
                st.ignore_next_clipboard_change = false;
                return;
            }
            let active = st.x11.as_ref().and_then(X11Helper::active_app);
            let ignored = active
                .as_ref()
                .is_some_and(|(_, bundle)| st.settings.is_ignored(Some(bundle)));
            (st.settings.record && !ignored, false, active)
        };
        if !should_record || ignore_once {
            return;
        }
        let (src_name, src_bundle) = match active {
            Some((n, b)) => (Some(n), Some(b)),
            None => (None, None),
        };
        let state_done = Rc::clone(&state_cb);
        clipboard::read_clipboard_async(clipboard, src_name, src_bundle, move |captured| {
            let Some(item) = captured else {
                return;
            };
            let changed = {
                let mut st = state_done.borrow_mut();
                let limit = st.settings.max_clips;
                let inserted = st.store.insert(item, limit);
                if inserted {
                    st.store.save();
                }
                inserted
            };
            if changed {
                refresh_list(&state_done);
            }
        });
    });
}

fn wire_timers(app: &adw::Application, state: &Rc<RefCell<AppState>>) {
    // Poll X11 hotkey & tray channel every 50 ms on the main thread.
    let state_poll = Rc::clone(state);
    let app_weak = app.downgrade();
    glib::timeout_add_local(Duration::from_millis(50), move || {
        let (hotkey_hit, actions) = {
            let st = state_poll.borrow();
            let hk = st.x11.as_ref().is_some_and(X11Helper::poll_hotkey);
            let mut acts = Vec::new();
            while let Ok(act) = st.tray_rx.try_recv() {
                acts.push(act);
            }
            (hk, acts)
        };

        if hotkey_hit {
            toggle_popup(&state_poll);
        }

        for act in actions {
            match act {
                TrayAction::TogglePopup => toggle_popup(&state_poll),
                TrayAction::OpenPrefs => show_prefs(&state_poll),
                TrayAction::ExportPdf => export_images_pdf(&state_poll),
                TrayAction::ClearAll => request_clear_all(&state_poll),
                TrayAction::Quit => {
                    state_poll.borrow_mut().hold_guard.take();
                    if let Some(a) = app_weak.upgrade() {
                        a.quit();
                    }
                    return glib::ControlFlow::Break;
                }
            }
        }
        glib::ControlFlow::Continue
    });

    // Poll finished background thumbnails every 150 ms while Images tab is active.
    let state_thumb = Rc::clone(state);
    glib::timeout_add_local(Duration::from_millis(150), move || {
        let should_refresh = {
            let mut st = state_thumb.borrow_mut();
            let mut any_ok = false;
            for res in st.thumbs.drain() {
                if res.ok {
                    any_ok = true;
                }
            }
            any_ok && st.section == Section::Images
        };
        if should_refresh {
            refresh_list(&state_thumb);
        }
        glib::ControlFlow::Continue
    });
}
