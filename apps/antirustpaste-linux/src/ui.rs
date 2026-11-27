//! GTK4 + libadwaita UI windows: history popup, preferences and dialogs.

use std::cell::RefCell;
use std::rc::Rc;

use antirustpaste_core::{Clip, ClipKind, Section, Settings, Store, ThumbnailService, human_size};
use gtk4::gdk;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::thumbnails;

#[derive(Clone)]
pub struct PopupWidgets {
    pub window: adw::ApplicationWindow,
    pub search: gtk4::SearchEntry,
    pub tab_text: gtk4::ToggleButton,
    pub tab_images: gtk4::ToggleButton,
    pub tab_commands: gtk4::ToggleButton,
    pub list: gtk4::ListBox,
    pub status_label: gtk4::Label,
    pub refresh_btn: gtk4::Button,
    pub pdf_btn: gtk4::Button,
    pub clear_btn: gtk4::Button,
    pub prefs_btn: gtk4::Button,
}

#[derive(Clone)]
pub struct PrefsWidgets {
    pub window: adw::PreferencesWindow,
    pub max_text: adw::SpinRow,
    pub max_images: adw::SpinRow,
    pub max_commands: adw::SpinRow,
    pub record: adw::SwitchRow,
    pub confirm_delete: adw::SwitchRow,
    pub paste_after_select: adw::SwitchRow,
    pub start_at_login: adw::SwitchRow,
    pub hotkey_enabled: adw::SwitchRow,
    pub clear_cache_btn: gtk4::Button,
}

pub fn install_css() {
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(
        "
        .pinned-row {
            background-color: alpha(#fae637, 0.16);
            border-left: 3px solid #5e35b1;
        }
        .clip-title {
            font-weight: 600;
        }
        .clip-subtitle {
            font-size: 0.85em;
            opacity: 0.72;
        }
        .section-header {
            font-size: 0.78em;
            font-weight: 700;
            letter-spacing: 0.06em;
            opacity: 0.65;
            padding: 8px 6px 4px 6px;
        }
        .thumb-frame {
            border-radius: 6px;
            background-color: alpha(currentColor, 0.06);
        }
        ",
    );
    if let Some(display) = gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

pub fn build_popup(app: &adw::Application) -> PopupWidgets {
    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new("AntiRustPaste", "Clipboard history");
    header.set_title_widget(Some(&title));

    let prefs_btn = gtk4::Button::from_icon_name("preferences-system-symbolic");
    prefs_btn.set_tooltip_text(Some("Preferences…"));
    header.pack_end(&prefs_btn);

    let search = gtk4::SearchEntry::new();
    search.set_placeholder_text(Some("Search clipboard history…"));
    search.set_hexpand(true);

    let tabs_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    tabs_box.add_css_class("linked");
    tabs_box.set_halign(gtk4::Align::Center);

    let tab_text = tab_button("text-x-generic-symbolic", "Text");
    let tab_images = tab_button("image-x-generic-symbolic", "Images");
    let tab_commands = tab_button("utilities-terminal-symbolic", "Commands");
    tab_images.set_group(Some(&tab_text));
    tab_commands.set_group(Some(&tab_text));
    tab_text.set_active(true);

    tabs_box.append(&tab_text);
    tabs_box.append(&tab_images);
    tabs_box.append(&tab_commands);

    let list = gtk4::ListBox::new();
    list.set_selection_mode(gtk4::SelectionMode::Single);
    list.set_activate_on_single_click(false);
    list.add_css_class("boxed-list");

    let empty_status = adw::StatusPage::builder()
        .icon_name("edit-paste-symbolic")
        .title("No matching items")
        .description("Copy text or images, or switch tabs.")
        .build();
    list.set_placeholder(Some(&empty_status));

    let list_wrap = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    list_wrap.set_margin_top(4);
    list_wrap.set_margin_bottom(4);
    list_wrap.set_margin_start(12);
    list_wrap.set_margin_end(12);
    list_wrap.append(&list);

    let scroller = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&list_wrap)
        .build();

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    let top_controls = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    top_controls.set_margin_top(8);
    top_controls.set_margin_start(12);
    top_controls.set_margin_end(12);
    top_controls.append(&search);
    top_controls.append(&tabs_box);

    content.append(&top_controls);
    content.append(&scroller);

    let footer = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    footer.set_margin_top(6);
    footer.set_margin_bottom(8);
    footer.set_margin_start(12);
    footer.set_margin_end(12);

    let status_label = gtk4::Label::new(Some("0 items"));
    status_label.set_halign(gtk4::Align::Start);
    status_label.set_hexpand(true);
    status_label.add_css_class("clip-subtitle");

    let refresh_btn = gtk4::Button::with_label("Refresh");
    refresh_btn.set_icon_name("view-refresh-symbolic");
    refresh_btn.set_visible(false);

    let pdf_btn = gtk4::Button::with_label("PDF…");
    pdf_btn.set_icon_name("document-save-symbolic");
    pdf_btn.set_visible(false);

    let clear_btn = gtk4::Button::with_label("Clear");
    clear_btn.add_css_class("destructive-action");

    footer.append(&status_label);
    footer.append(&refresh_btn);
    footer.append(&pdf_btn);
    footer.append(&clear_btn);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    toolbar.add_bottom_bar(&footer);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("AntiRustPaste")
        .default_width(460)
        .default_height(560)
        .hide_on_close(true)
        .content(&toolbar)
        .build();

    PopupWidgets {
        window,
        search,
        tab_text,
        tab_images,
        tab_commands,
        list,
        status_label,
        refresh_btn,
        pdf_btn,
        clear_btn,
        prefs_btn,
    }
}

fn tab_button(icon_name: &str, label_text: &str) -> gtk4::ToggleButton {
    let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let icon = gtk4::Image::from_icon_name(icon_name);
    let label = gtk4::Label::new(Some(label_text));
    hbox.append(&icon);
    hbox.append(&label);
    let btn = gtk4::ToggleButton::new();
    btn.set_child(Some(&hbox));
    btn
}

pub fn build_prefs(app: &adw::Application, settings: &Settings) -> PrefsWidgets {
    let max_text = adw::SpinRow::with_range(1.0, 9999.0, 10.0);
    max_text.set_title("Text history limit");
    max_text.set_subtitle("Plain, rich text, HTML and file clippings");
    max_text.set_value(settings.max_text as f64);

    let max_images = adw::SpinRow::with_range(1.0, 9999.0, 10.0);
    max_images.set_title("Image history limit");
    max_images.set_subtitle("Images have their own budget");
    max_images.set_value(settings.max_images as f64);

    let max_commands = adw::SpinRow::with_range(1.0, 9999.0, 10.0);
    max_commands.set_title("Command history limit");
    max_commands.set_subtitle("500 matches the bash default; zsh usually keeps 1000");
    max_commands.set_value(settings.max_commands as f64);

    let record = adw::SwitchRow::builder()
        .title("Record clipboard changes")
        .active(settings.record)
        .build();

    let confirm_delete = adw::SwitchRow::builder()
        .title("Confirm before deleting")
        .active(settings.confirm_delete)
        .build();

    let history_group = adw::PreferencesGroup::builder()
        .title("History &amp; Recording")
        .build();
    history_group.add(&max_text);
    history_group.add(&max_images);
    history_group.add(&max_commands);
    history_group.add(&record);
    history_group.add(&confirm_delete);

    let paste_after_select = adw::SwitchRow::builder()
        .title("Paste after choosing")
        .subtitle("Synthesises Ctrl+V on X11 after copying")
        .active(settings.paste_after_select)
        .build();

    let start_at_login = adw::SwitchRow::builder()
        .title("Start at login")
        .subtitle("Writes an XDG autostart desktop entry")
        .active(settings.start_at_login)
        .build();

    let hotkey_enabled = adw::SwitchRow::builder()
        .title("Enable global shortcut")
        .subtitle("Requires an X11 session")
        .active(settings.hotkey_enabled)
        .build();

    let hotkey_row = adw::ActionRow::builder()
        .title("Global shortcut")
        .subtitle(&settings.hotkey_display)
        .build();

    let behavior_group = adw::PreferencesGroup::builder()
        .title("Behavior &amp; Shortcut")
        .build();
    behavior_group.add(&paste_after_select);
    behavior_group.add(&start_at_login);
    behavior_group.add(&hotkey_enabled);
    behavior_group.add(&hotkey_row);

    let clear_cache_btn = gtk4::Button::with_label("Clear");
    clear_cache_btn.set_valign(gtk4::Align::Center);
    clear_cache_btn.add_css_class("destructive-action");

    let cache_row = adw::ActionRow::builder()
        .title("Thumbnail cache")
        .subtitle("Remove generated preview images")
        .build();
    cache_row.add_suffix(&clear_cache_btn);

    let storage_group = adw::PreferencesGroup::builder().title("Storage").build();
    storage_group.add(&cache_row);

    let page = adw::PreferencesPage::new();
    page.add(&history_group);
    page.add(&behavior_group);
    page.add(&storage_group);

    let window = adw::PreferencesWindow::builder()
        .application(app)
        .title("Preferences")
        .default_width(440)
        .default_height(520)
        .search_enabled(false)
        .hide_on_close(true)
        .build();
    window.add(&page);

    PrefsWidgets {
        window,
        max_text,
        max_images,
        max_commands,
        record,
        confirm_delete,
        paste_after_select,
        start_at_login,
        hotkey_enabled,
        clear_cache_btn,
    }
}

pub fn rebuild_list(
    widgets: &PopupWidgets,
    store: &Store,
    thumbs: &mut ThumbnailService,
    visible_indices: &[usize],
    section: Section,
    on_pin: Rc<dyn Fn(usize)>,
    on_delete: Rc<dyn Fn(usize)>,
) {
    while let Some(child) = widgets.list.first_child() {
        widgets.list.remove(&child);
    }

    widgets
        .refresh_btn
        .set_visible(section == Section::Commands);
    widgets.pdf_btn.set_visible(section == Section::Images);
    widgets.clear_btn.set_visible(section != Section::Commands);

    let mut shown_pinned_header = false;
    let mut shown_recent_header = false;
    let mut first_selectable: Option<gtk4::ListBoxRow> = None;

    for &idx in visible_indices {
        let Some(clip) = store.clips.get(idx) else {
            continue;
        };

        if clip.pinned && !shown_pinned_header {
            shown_pinned_header = true;
            widgets.list.append(&section_header_row("PINNED"));
        } else if !clip.pinned && shown_pinned_header && !shown_recent_header {
            shown_recent_header = true;
            widgets.list.append(&section_header_row("RECENT"));
        }

        let row = build_clip_row(clip, idx, store, thumbs, &on_pin, &on_delete);
        if first_selectable.is_none() {
            first_selectable = Some(row.clone());
        }
        widgets.list.append(&row);
    }

    if let Some(row) = first_selectable {
        widgets.list.select_row(Some(&row));
    }

    let count = visible_indices.len();
    let noun = if count == 1 { "item" } else { "items" };
    widgets.status_label.set_text(&format!("{count} {noun}"));
}

fn section_header_row(title: &str) -> gtk4::ListBoxRow {
    let label = gtk4::Label::new(Some(title));
    label.set_halign(gtk4::Align::Start);
    label.add_css_class("section-header");
    let row = gtk4::ListBoxRow::new();
    row.set_selectable(false);
    row.set_activatable(false);
    row.set_child(Some(&label));
    row
}

fn build_clip_row(
    clip: &Clip,
    store_index: usize,
    store: &Store,
    thumbs: &mut ThumbnailService,
    on_pin: &Rc<dyn Fn(usize)>,
    on_delete: &Rc<dyn Fn(usize)>,
) -> gtk4::ListBoxRow {
    let row = gtk4::ListBoxRow::new();
    row.set_widget_name(&format!("clip-{store_index}"));
    if clip.pinned {
        row.add_css_class("pinned-row");
    }

    let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    hbox.set_margin_top(8);
    hbox.set_margin_bottom(8);
    hbox.set_margin_start(10);
    hbox.set_margin_end(10);

    if clip.kind == ClipKind::Image {
        let blob_path = clip.blob.as_deref().and_then(|b| store.blob_file(b));
        if let Some(png) = thumbs.image_for(clip.hash, blob_path)
            && let Some(tex) = thumbnails::load_texture(&png)
        {
            let pic = gtk4::Picture::for_paintable(&tex);
            pic.set_size_request(40, 40);
            pic.set_can_shrink(true);
            pic.add_css_class("thumb-frame");
            hbox.append(&pic);
        } else {
            let icon = gtk4::Image::from_icon_name("image-x-generic-symbolic");
            icon.set_size_request(40, 40);
            icon.add_css_class("thumb-frame");
            hbox.append(&icon);
        }
    }

    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    vbox.set_hexpand(true);

    let title = gtk4::Label::new(Some(clip.display_title()));
    title.set_halign(gtk4::Align::Start);
    title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    title.set_max_width_chars(44);
    title.add_css_class("clip-title");

    let sub_text = if clip.kind == ClipKind::Image {
        format!("{} · {}", clip.subtitle(), human_size(clip.byte_len))
    } else {
        clip.subtitle()
    };
    let subtitle = gtk4::Label::new(Some(&sub_text));
    subtitle.set_halign(gtk4::Align::Start);
    subtitle.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    subtitle.add_css_class("clip-subtitle");

    vbox.append(&title);
    vbox.append(&subtitle);

    let pin_btn = gtk4::Button::from_icon_name(if clip.pinned {
        "view-pin-symbolic"
    } else {
        "pin-symbolic"
    });
    if !clip.pinned {
        pin_btn.set_label("○");
    }
    pin_btn.add_css_class("flat");
    pin_btn.set_tooltip_text(Some("Pin / unpin (Ctrl+P)"));
    let pin_cb = Rc::clone(on_pin);
    pin_btn.connect_clicked(move |_| pin_cb(store_index));

    let del_btn = gtk4::Button::from_icon_name("edit-delete-symbolic");
    del_btn.add_css_class("flat");
    del_btn.set_tooltip_text(Some("Delete"));
    let del_cb = Rc::clone(on_delete);
    del_btn.connect_clicked(move |_| del_cb(store_index));

    hbox.append(&vbox);
    hbox.append(&pin_btn);
    hbox.append(&del_btn);

    row.set_child(Some(&hbox));
    row
}

pub fn selected_store_index(list: &gtk4::ListBox) -> Option<usize> {
    let row = list.selected_row()?;
    let name = row.widget_name();
    let num = name.strip_prefix("clip-")?;
    num.parse::<usize>().ok()
}

pub fn move_selection(list: &gtk4::ListBox, delta: i32) {
    let mut rows = Vec::new();
    let mut child = list.first_child();
    while let Some(widget) = child {
        let next = widget.next_sibling();
        if let Ok(row) = widget.downcast::<gtk4::ListBoxRow>()
            && row.is_selectable()
        {
            rows.push(row);
        }
        child = next;
    }
    if rows.is_empty() {
        return;
    }
    let current = list
        .selected_row()
        .and_then(|sel| rows.iter().position(|r| *r == sel))
        .unwrap_or(0);
    let next_idx = if delta < 0 {
        current.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        current
            .saturating_add(delta as usize)
            .min(rows.len().saturating_sub(1))
    };
    if let Some(target) = rows.get(next_idx) {
        list.select_row(Some(target));
        target.grab_focus();
    }
}

pub fn confirm_dialog(
    parent: &impl IsA<gtk4::Widget>,
    message: &str,
    on_confirm: impl FnOnce() + 'static,
) {
    let dialog = adw::AlertDialog::builder()
        .heading(message)
        .default_response("delete")
        .close_response("cancel")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("delete", "Delete");
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);

    let cb = RefCell::new(Some(on_confirm));
    dialog.choose(Some(parent), gtk4::gio::Cancellable::NONE, move |res| {
        if res == "delete"
            && let Some(f) = cb.borrow_mut().take()
        {
            f();
        }
    });
}
