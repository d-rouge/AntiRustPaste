//! Application state, menu bar item, clipboard polling and all user actions.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ptr::{NonNull, null_mut};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::*;
use objc2_foundation::*;

use crate::hotkey::{self, Hotkey};
use crate::paste;
use crate::pasteboard;
use rustclip_core::{
    ClipKind, IgnoredApp, MAX_LIMIT, Section, Settings, Store, ThumbnailService,
    filter_indices as core_filter_indices,
};

use crate::settings::{self, apply_start_at_login};
use crate::thumbnails;
use crate::views::{self, ClipListView, ClipPanel, PanelDelegate, PrefsParts};

pub(crate) const PANEL_W: f64 = 460.0;
pub(crate) const PANEL_H: f64 = 480.0;
pub(crate) const ROW_H: f64 = 54.0;
pub(crate) const LIST_BOTTOM: f64 = 32.0;
pub(crate) const SEARCH_H: f64 = 40.0;
const MAX_IN_MEMORY_THUMBNAILS: usize = 256;
const STATUS_ICON_PNG: &[u8] = include_bytes!("../../../assets/status-icon.png");

// NSEventModifierFlags values used only for local key handling and recording.
const NS_SHIFT: usize = 1 << 17;
const NS_CONTROL: usize = 1 << 18;
const NS_OPTION: usize = 1 << 19;
const NS_COMMAND: usize = 1 << 20;

pub(crate) use rustclip_core::Section as ClipboardSection;

pub(crate) trait SectionUi {
    fn footer_label(self, count: usize) -> String;
    fn empty_message(self) -> &'static str;
    fn no_results_message(self) -> &'static str;
    fn placeholder(self) -> &'static str;
}

impl SectionUi for Section {
    fn footer_label(self, count: usize) -> String {
        match (self, count) {
            (Self::Text, 1) => "1 clipping".to_string(),
            (Self::Images, 1) => "1 image".to_string(),
            (Self::Text, count) => format!("{count} clippings"),
            (Self::Images, count) => format!("{count} images"),
        }
    }

    fn empty_message(self) -> &'static str {
        match self {
            Self::Text => "Your clippings will appear here…",
            Self::Images => "Your images will appear here…",
        }
    }

    fn no_results_message(self) -> &'static str {
        match self {
            Self::Text => "No clippings found",
            Self::Images => "No images found",
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            Self::Text => "Search clippings",
            Self::Images => "Search images",
        }
    }
}

pub(crate) struct AppState {
    pub store: Store,
    pub settings: Settings,
    pub status_item: Retained<NSStatusItem>,
    pub menu: Retained<NSMenu>,
    pub panel: Retained<ClipPanel>,
    pub search: Retained<NSSearchField>,
    pub list: Retained<ClipListView>,
    pub footer: Retained<NSTextField>,
    pub section_control: Retained<NSSegmentedControl>,
    pub clear_text_button: Retained<NSButton>,
    pub clear_images_button: Retained<NSButton>,
    pub section: ClipboardSection,
    pub thumbnails: HashMap<u64, Retained<NSImage>>,
    pub thumbnail_failed: HashSet<u64>,
    pub thumbnail_service: ThumbnailService,
    /// NSWindow's delegate is a weak reference; keep ours alive here.
    #[allow(dead_code)]
    pub panel_delegate: Retained<PanelDelegate>,
    pub prefs: Option<PrefsParts>,
    pub selected: usize,
    pub scroll: f64,
    pub query: String,
    pub last_change_count: isize,
    pub last_external: Option<(String, Option<String>)>,
    pub target_app: Option<Retained<NSRunningApplication>>,
    pub recording: bool,
    pub hotkey: Option<Hotkey>,
    pub key_monitor: Option<Retained<AnyObject>>,
    pub last_status_action: Option<Instant>,
    /// Screen rectangle of the status item that was actually clicked. Multi-
    /// display menu bars can have per-display status windows.
    pub panel_anchor: Option<(NSRect, Option<Retained<NSScreen>>)>,
    pub pending_show: bool,
    /// Invalidates a scheduled hide when the status item itself was clicked.
    pub hide_token: u64,
    pub accessibility_asked: bool,
}

thread_local! {
    static STATE: RefCell<Option<AppState>> = const { RefCell::new(None) };
    static DELEGATE: RefCell<Option<Retained<AppDelegate>>> = const { RefCell::new(None) };
}

pub(crate) fn with_state<R>(f: impl FnOnce(&mut AppState) -> R) -> R {
    STATE.with(|cell| {
        let mut guard = cell.borrow_mut();
        let state = guard.as_mut().expect("RustClip state not initialised");
        f(state)
    })
}

/// Run an AppKit, Carbon or timer callback without letting a panic unwind into
/// Objective-C. A Rust panic crossing an `extern "C"` boundary aborts the
/// process, so every entry point that the system calls goes through here.
pub(crate) fn guard(name: &str, f: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
        eprintln!("rustclip: recovered from a panic in {name}");
    }
}

/// The value-returning form of [`guard`]. Callers choose a safe fallback when
/// it returns `None`.
pub(crate) fn guard_value<R>(name: &str, f: impl FnOnce() -> R) -> Option<R> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(value) => Some(value),
        Err(_) => {
            eprintln!("rustclip: recovered from a panic in {name}");
            None
        }
    }
}

pub(crate) fn as_any<T: Message>(obj: &T) -> &AnyObject {
    unsafe { &*(obj as *const T as *const AnyObject) }
}

/// RustClip never spawns a thread: startup, AppKit delegate callbacks, block
/// timers and the Carbon hot-key handler all run on the main thread.
fn main_thread() -> MainThreadMarker {
    // SAFETY: every caller is one of the main-thread entry points listed above.
    // `run()` keeps the checked constructor so a wrong entry point cannot reach
    // AppKit by accident.
    unsafe { MainThreadMarker::new_unchecked() }
}

fn delegate_ref() -> Retained<AppDelegate> {
    DELEGATE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(delegate) = slot.as_ref() {
            return delegate.clone();
        }
        // Defensive self-heal: a target is needed for menu actions even if an
        // early callback ran before `run()` stored the delegate.
        let delegate = AppDelegate::new(main_thread());
        *slot = Some(delegate.clone());
        delegate
    })
}

fn delegate_any() -> Retained<AnyObject> {
    Retained::from(delegate_ref())
}

pub(crate) fn thumbnail_for(
    s: &mut AppState,
    hash: u64,
    blob: Option<&str>,
) -> Option<Retained<NSImage>> {
    if let Some(image) = s.thumbnails.get(&hash) {
        return Some(image.clone());
    }
    if s.thumbnail_failed.contains(&hash) {
        return None;
    }
    let blob_path = blob.and_then(|name| s.store.blob_file(name));
    let bytes = s.thumbnail_service.image_for(hash, blob_path)?;
    let image = thumbnails::nsimage_from_png(bytes)?;
    if s.thumbnails.len() >= MAX_IN_MEMORY_THUMBNAILS {
        s.thumbnails.clear();
    }
    s.thumbnails.insert(hash, image.clone());
    Some(image)
}

pub(crate) fn filtered_indices(s: &AppState) -> Vec<usize> {
    core_filter_indices(&s.store.clips, s.section, &s.query)
}

fn set_section(section: ClipboardSection) {
    with_state(|s| {
        s.section = section;
        s.selected = 0;
        s.scroll = 0.0;
        s.section_control.setSelectedSegment(match section {
            ClipboardSection::Text => 0,
            ClipboardSection::Images => 1,
        });
        s.search
            .setPlaceholderString(Some(&NSString::from_str(section.placeholder())));
        update_footer(s);
        s.list.setNeedsDisplay(true);
    });
}

fn update_footer(s: &AppState) {
    let count = filtered_indices(s).len();
    let label = s.section.footer_label(count);
    s.footer
        .setStringValue(&NSString::from_str(&format!("{label} · ⏎ · ⌫ · ⌘P pin")));
    s.clear_text_button.setEnabled(
        s.store
            .clips
            .iter()
            .any(|clip| clip.kind != ClipKind::Image),
    );
    s.clear_images_button.setEnabled(
        s.store
            .clips
            .iter()
            .any(|clip| clip.kind == ClipKind::Image),
    );
}

fn refresh_ui() {
    with_state(|s| {
        let len = filtered_indices(s).len();
        if s.selected >= len {
            s.selected = len.saturating_sub(1);
        }
        let height = s.list.bounds().size.height;
        let content = len as f64 * ROW_H;
        s.scroll = s.scroll.clamp(0.0, (content - height).max(0.0));
        update_footer(s);
        s.list.setNeedsDisplay(true);
    });
}

// ---------------------------------------------------------------------------
// Objective-C delegate
// ---------------------------------------------------------------------------

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RCAppDelegate"]
    #[ivars = ()]
    pub(crate) struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn application_did_finish_launching(&self, _notification: &NSNotification) {
            guard("application_did_finish_launching", || {
                setup_app();
            });
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn application_should_terminate_after_last_window_closed(
            &self,
            _sender: &NSApplication,
        ) -> bool {
            false
        }
    }

    unsafe impl NSControlTextEditingDelegate for AppDelegate {
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, notification: &NSNotification) {
            guard("control_text_did_change", || {
                let is_search = with_state(|s| {
                    notification
                        .object()
                        .map(|obj| {
                            std::ptr::eq(
                                Retained::as_ptr(&obj),
                                Retained::as_ptr(&s.search) as *const AnyObject,
                            )
                        })
                        .unwrap_or(false)
                });
                if is_search {
                    search_changed();
                }
            });
        }
    }

    unsafe impl NSSearchFieldDelegate for AppDelegate {}

    unsafe impl NSTextFieldDelegate for AppDelegate {}

    impl AppDelegate {
        #[allow(deprecated)]
        #[unsafe(method(statusItemClicked:))]
        fn status_item_clicked(&self, _sender: &AnyObject) {
            guard("status_item_clicked", || {
                let now = Instant::now();
                let duplicate = with_state(|s| {
                    let duplicate = s
                        .last_status_action
                        .is_some_and(|last| now.duration_since(last) < Duration::from_millis(250));
                    if !duplicate {
                        s.last_status_action = Some(now);
                    }
                    duplicate
                });
                if duplicate {
                    return;
                }

                let mtm = main_thread();
                let app = NSApplication::sharedApplication(mtm);
                let event = app.currentEvent();
                let anchor = event.as_ref().and_then(|event| {
                    let window = event.window(mtm)?;
                    Some((window.frame(), window.screen()))
                });
                let right_click = event
                    .as_ref()
                    .map(|event| {
                        let kind = event.r#type();
                        kind == NSEventType::RightMouseUp || kind == NSEventType::RightMouseDown
                    })
                    .unwrap_or(false);
                with_state(|s| s.panel_anchor = if right_click { None } else { anchor });
                if right_click {
                    capture_target_app();
                    // Menu tracking runs a nested run loop; never hold the state
                    // borrow across it or the polling timer would re-enter.
                    let (status_item, menu) = with_state(|s| (s.status_item.clone(), s.menu.clone()));
                    status_item.popUpStatusItemMenu(&menu);
                } else {
                    toggle_panel();
                }
            });
        }

        #[unsafe(method(sectionChanged:))]
        fn section_changed(&self, sender: &NSSegmentedControl) {
            guard("section_changed", || {
                let section = if sender.selectedSegment() == 1 {
                    ClipboardSection::Images
                } else {
                    ClipboardSection::Text
                };
                set_section(section);
                let (panel, search) = with_state(|s| (s.panel.clone(), s.search.clone()));
                panel.makeFirstResponder(Some(&search));
            });
        }

        #[unsafe(method(clearThumbnailCache:))]
        fn clear_thumbnail_cache(&self, _sender: &AnyObject) {
            guard("clear_thumbnail_cache", clear_thumbnail_cache);
        }

        #[unsafe(method(clearHistory:))]
        fn clear_history(&self, _sender: &AnyObject) {
            guard("clear_history", || {
                clear_all();
            });
        }

        #[unsafe(method(clearTextSection:))]
        fn clear_text_section(&self, _sender: &AnyObject) {
            guard("clear_text_section", || {
                clear_section(ClipboardSection::Text);
            });
        }

        #[unsafe(method(clearImagesSection:))]
        fn clear_images_section(&self, _sender: &AnyObject) {
            guard("clear_images_section", || {
                clear_section(ClipboardSection::Images);
            });
        }

        #[unsafe(method(showPreferences:))]
        fn show_preferences(&self, _sender: &AnyObject) {
            guard("show_preferences", || {
                open_preferences();
            });
        }

        #[unsafe(method(showAbout:))]
        fn show_about(&self, _sender: &AnyObject) {
            guard("show_about", || {
                show_about();
            });
        }

        #[unsafe(method(quitApp:))]
        fn quit_app(&self, _sender: &AnyObject) {
            guard("quit_app", || {
                let mtm = main_thread();
                NSApplication::sharedApplication(mtm).terminate(None);
            });
        }

        #[unsafe(method(maxClipsChanged:))]
        fn max_clips_changed(&self, sender: &NSTextField) {
            guard("max_clips_changed", || {
                max_clips_changed(sender);
            });
        }

        #[unsafe(method(recordToggled:))]
        fn record_toggled(&self, sender: &NSButton) {
            guard("record_toggled", || {
                let on = sender.state() == NSControlStateValueOn;
                with_state(|s| {
                    s.settings.record = on;
                    settings::save(&s.settings);
                });
                if on {
                    let pb = pasteboard::general();
                    let count = pb.changeCount();
                    with_state(|s| s.last_change_count = count);
                }
            });
        }

        #[unsafe(method(confirmToggled:))]
        fn confirm_toggled(&self, sender: &NSButton) {
            guard("confirm_toggled", || {
                let on = sender.state() == NSControlStateValueOn;
                with_state(|s| {
                    s.settings.confirm_delete = on;
                    settings::save(&s.settings);
                });
            });
        }

        #[unsafe(method(startAtLoginToggled:))]
        fn start_at_login_toggled(&self, sender: &NSButton) {
            guard("start_at_login_toggled", || {
                let on = sender.state() == NSControlStateValueOn;
                match apply_start_at_login(on) {
                    Ok(()) => with_state(|s| {
                        s.settings.start_at_login = on;
                        settings::save(&s.settings);
                    }),
                    Err(err) => {
                        sender.setState(if on {
                            NSControlStateValueOff
                        } else {
                            NSControlStateValueOn
                        });
                        show_alert("Could not update the login item", &err);
                    }
                }
            });
        }

        #[unsafe(method(shortcutButtonClicked:))]
        fn shortcut_button_clicked(&self, sender: &NSButton) {
            guard("shortcut_button_clicked", || {
                // Suspend the old shortcut while recording so it cannot fire.
                let old = with_state(|s| s.hotkey.take());
                drop(old);
                with_state(|s| {
                    s.recording = true;
                    sender.setTitle(&NSString::from_str("Type shortcut…"));
                });
            });
        }

        #[unsafe(method(addIgnoredApp:))]
        fn add_ignored_app(&self, _sender: &AnyObject) {
            guard("add_ignored_app", || {
                add_ignored_app();
            });
        }

        #[unsafe(method(removeIgnoredApp:))]
        fn remove_ignored_app(&self, _sender: &AnyObject) {
            guard("remove_ignored_app", || {
                remove_ignored_app();
            });
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

pub fn run() {
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("rustclip: must start on the main thread");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let delegate = AppDelegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    DELEGATE.with(|slot| *slot.borrow_mut() = Some(delegate));
    app.run();
}

fn setup_app() {
    let mtm = main_thread();
    let settings = settings::load();
    let mut store = Store::open();
    store.trim(settings.max_clips);
    store.save();
    thumbnails::cleanup_legacy_cache();
    let valid: HashSet<u64> = store.clips.iter().map(|clip| clip.hash).collect();
    thumbnails::prune_now(&valid);
    if settings.start_at_login {
        let _ = apply_start_at_login(true);
    }

    let pb = pasteboard::general();
    let last_change_count = pb.changeCount();

    let status_item =
        NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = status_item.button(mtm) {
        let data = NSData::from_vec(STATUS_ICON_PNG.to_vec());
        if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
            image.setTemplate(true);
            image.setSize(NSSize::new(18.0, 18.0));
            button.setImage(Some(&image));
        }
        button.setToolTip(Some(&NSString::from_str("RustClip — clipboard history")));
        button.sendActionOn(NSEventMask::LeftMouseUp | NSEventMask::RightMouseUp);
        let target = delegate_any();
        unsafe {
            button.setTarget(Some(&target));
            button.setAction(Some(sel!(statusItemClicked:)));
        }
    }

    let menu = build_status_menu(mtm);
    let parts = views::build_panel(mtm, &delegate_ref());
    let panel_delegate = PanelDelegate::create(mtm);
    parts
        .panel
        .setDelegate(Some(ProtocolObject::from_ref(&*panel_delegate)));

    let state = AppState {
        store,
        settings,
        status_item,
        menu,
        panel: parts.panel,
        search: parts.search,
        list: parts.list,
        footer: parts.footer,
        section_control: parts.section_control,
        clear_text_button: parts.clear_text_button,
        clear_images_button: parts.clear_images_button,
        section: ClipboardSection::Text,
        thumbnails: HashMap::new(),
        thumbnail_failed: HashSet::new(),
        thumbnail_service: thumbnails::new_service(),
        panel_delegate,
        prefs: None,
        selected: 0,
        scroll: 0.0,
        query: String::new(),
        last_change_count,
        last_external: None,
        target_app: None,
        recording: false,
        hotkey: None,
        key_monitor: None,
        last_status_action: None,
        panel_anchor: None,
        pending_show: false,
        hide_token: 0,
        accessibility_asked: false,
    };
    STATE.with(|slot| *slot.borrow_mut() = Some(state));

    install_key_monitor();
    install_poll_timer();
    install_thumbnail_timer();
    refresh_ui();
    register_hotkey_from_settings();
    update_frontmost_app();

    // Automation hooks used by docs/testing.md and scripts/smoke.sh. They are
    // inert unless an agent or test sets them explicitly.
    if std::env::var_os("RUSTCLIP_OPEN_PREFS").is_some() {
        open_preferences();
    }
    let open_images = std::env::var_os("RUSTCLIP_OPEN_IMAGES").is_some();
    if open_images {
        set_section(ClipboardSection::Images);
    }
    if open_images || std::env::var_os("RUSTCLIP_OPEN_PANEL").is_some() {
        show_panel();
    }
}

fn menu_item(
    title: &str,
    action: Sel,
    target: &AnyObject,
    mtm: MainThreadMarker,
) -> Retained<NSMenuItem> {
    let item = NSMenuItem::new(mtm);
    item.setTitle(&NSString::from_str(title));
    unsafe {
        item.setAction(Some(action));
        item.setTarget(Some(target));
    }
    item.setEnabled(true);
    item
}

fn build_status_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let target = delegate_any();
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);

    menu.addItem(&menu_item(
        "Preferences…",
        sel!(showPreferences:),
        &target,
        mtm,
    ));
    menu.addItem(&menu_item(
        "Clear All Clippings",
        sel!(clearHistory:),
        &target,
        mtm,
    ));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&menu_item("About RustClip", sel!(showAbout:), &target, mtm));
    menu.addItem(&menu_item("Quit RustClip", sel!(quitApp:), &target, mtm));

    menu
}

fn install_poll_timer() {
    let block = RcBlock::new(|_timer: NonNull<NSTimer>| {
        guard("pasteboard_poll", poll_pasteboard);
    });
    unsafe {
        NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0, true, &block);
    }
}

fn install_thumbnail_timer() {
    let block = RcBlock::new(|_timer: NonNull<NSTimer>| {
        guard("thumbnail_results", drain_thumbnail_results);
    });
    unsafe {
        NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.1, true, &block);
    }
}

fn install_key_monitor() {
    let block = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
        let event = unsafe { event.as_ref() };
        let handled = guard_value("key_monitor", || handle_key_event(event)).unwrap_or(false);
        if handled {
            null_mut()
        } else {
            event as *const NSEvent as *mut NSEvent
        }
    });
    let monitor = unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &block)
    };
    with_state(|s| s.key_monitor = monitor);
}

// ---------------------------------------------------------------------------
// Clipboard polling
// ---------------------------------------------------------------------------

fn update_frontmost_app() {
    let workspace = NSWorkspace::sharedWorkspace();
    if let Some(app) = workspace.frontmostApplication()
        && app.processIdentifier() != std::process::id() as i32
    {
        let name = app
            .localizedName()
            .map(|n| n.to_string())
            .unwrap_or_default();
        let bundle = app.bundleIdentifier().map(|b| b.to_string());
        with_state(|s| s.last_external = Some((name, bundle)));
    }
}

fn poll_pasteboard() {
    let pb = pasteboard::general();
    let current = pb.changeCount();
    update_frontmost_app();

    let (changed, recording) = with_state(|s| {
        let changed = current != s.last_change_count;
        s.last_change_count = current;
        (changed, s.settings.record)
    });
    if !changed || !recording {
        return;
    }

    let items = pasteboard::read_items(&pb);
    if items.is_empty() {
        return;
    }

    let (source_name, source_bundle) = with_state(|s| {
        s.last_external
            .clone()
            .map(|(name, bundle)| (Some(name), bundle))
            .unwrap_or((None, None))
    });
    if with_state(|s| s.settings.is_ignored(source_bundle.as_deref())) {
        return;
    }

    let mut changed_history = false;
    with_state(|s| {
        for item in items {
            let max = s.settings.max_clips;
            if s.store.insert(item, max) {
                if let Some(clip) = s.store.clips.first_mut() {
                    clip.source_name = source_name.clone();
                    clip.source_bundle = source_bundle.clone();
                }
                changed_history = true;
            }
        }
        if changed_history {
            s.store.save();
        }
    });

    if changed_history {
        refresh_ui();
        // Trimming may have removed an old clip; drop its cached thumbnail
        // without clearing the in-memory images that are still valid.
        prune_thumbnail_files();
    }
}

// ---------------------------------------------------------------------------
// Popup
// ---------------------------------------------------------------------------

/// A click outside the popup makes it resign key. Delay the hide briefly so a
/// click on the status item can cancel it and toggle the popup instead.
pub(crate) fn schedule_hide_from_resign() {
    let token = with_state(|s| {
        s.hide_token = s.hide_token.wrapping_add(1);
        s.hide_token
    });
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        let should_hide = with_state(|s| s.hide_token == token && s.panel.isVisible());
        if should_hide {
            hide_panel();
        }
    });
    unsafe {
        NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.15, false, &block);
    }
}

pub(crate) fn hide_panel() {
    // `orderOut` can synchronously deliver windowDidResignKey, which calls back
    // into hide_panel; take the panel out of the state borrow first.
    let panel = with_state(|s| {
        s.panel_anchor = None;
        s.pending_show = false;
        s.hide_token = s.hide_token.wrapping_add(1);
        s.panel.clone()
    });
    panel.orderOut(None);
}

fn toggle_panel() {
    let visible = with_state(|s| s.panel.isVisible());
    if visible {
        hide_panel();
    } else {
        show_panel();
    }
}

fn position_panel() {
    let mtm = main_thread();
    let anchor = with_state(|s| s.panel_anchor.take());
    let fallback = with_state(|s| {
        s.status_item
            .button(mtm)
            .and_then(|b| b.window())
            .map(|w| (w.frame(), w.screen()))
    });
    let Some((bf, screen)) = anchor.or(fallback) else {
        return;
    };
    let visible = screen
        .or_else(|| NSScreen::mainScreen(mtm))
        .map(|screen| screen.visibleFrame());
    let mut x = bf.origin.x + bf.size.width / 2.0 - PANEL_W / 2.0;
    let mut y = bf.origin.y - PANEL_H - 6.0;
    if let Some(vf) = visible {
        x = x
            .max(vf.origin.x + 8.0)
            .min(vf.origin.x + vf.size.width - PANEL_W - 8.0);
        y = y.max(vf.origin.y + 8.0);
    }
    with_state(|s| s.panel.setFrameOrigin(NSPoint::new(x, y)));
}

/// Remember which app should receive the paste. Called before the popup or the
/// status menu takes focus, while the previous app is still frontmost.
fn capture_target_app() {
    let frontmost = NSWorkspace::sharedWorkspace().frontmostApplication();
    let our_pid = std::process::id() as i32;
    if let Some(app) = frontmost
        && app.processIdentifier() != our_pid
    {
        with_state(|s| s.target_app = Some(app));
    }
}

/// The app that should receive the simulated ⌘V: the remembered target, or, if
/// that app quit meanwhile, the last app seen in front of us.
fn resolve_target_app() -> Option<Retained<NSRunningApplication>> {
    with_state(|s| {
        if let Some(app) = &s.target_app
            && !app.isTerminated()
        {
            return Some(app.clone());
        }
        let bundle = s
            .last_external
            .as_ref()
            .and_then(|(_, bundle)| bundle.clone())?;
        let identifier = NSString::from_str(&bundle);
        NSRunningApplication::runningApplicationsWithBundleIdentifier(&identifier)
            .iter()
            .next()
    })
}

fn show_panel() {
    capture_target_app();

    let search = with_state(|s| {
        s.query.clear();
        s.selected = 0;
        s.scroll = 0.0;
        s.search
            .setPlaceholderString(Some(&NSString::from_str(s.section.placeholder())));
        s.list.setNeedsDisplay(true);
        update_footer(s);
        s.search.clone()
    });
    search.setStringValue(&NSString::from_str(""));

    position_panel();
    with_state(|s| s.pending_show = true);
    let block = RcBlock::new(|_timer: NonNull<NSTimer>| {
        if with_state(|s| s.pending_show) {
            present_panel();
        }
    });
    unsafe {
        // Present after the status-item mouse event has finished dispatching.
        NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.05, false, &block);
    }
}

/// Present the prepared panel and make it key without a separate app-activation step.
#[allow(deprecated)]
fn present_panel() {
    let mtm = main_thread();
    let (panel, search) = with_state(|s| {
        s.pending_show = false;
        (s.panel.clone(), s.search.clone())
    });
    panel.setAlphaValue(1.0);
    panel.orderFrontRegardless();
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    panel.makeKeyAndOrderFront(None);
    panel.makeFirstResponder(Some(&search));
}

pub(crate) fn handle_list_click(view: &ClipListView, event: &NSEvent) {
    let point = view.convertPoint_fromView(event.locationInWindow(), None);
    let width = view.bounds().size.width;
    let (scroll, count) = with_state(|s| (s.scroll, filtered_indices(s).len()));
    if count == 0 {
        return;
    }
    let row = (point.y + scroll) / ROW_H;
    if row < 0.0 {
        return;
    }
    let row = row.floor() as usize;
    if row >= count {
        return;
    }
    with_state(|s| s.selected = row);
    if point.x >= width - 36.0 {
        toggle_pin_at(row);
    } else {
        paste_filtered(row, true);
    }
}

pub(crate) fn handle_list_scroll(delta: f64) {
    with_state(|s| {
        let height = s.list.bounds().size.height;
        let count = filtered_indices(s).len();
        let content = count as f64 * ROW_H;
        let max = (content - height).max(0.0);
        s.scroll = (s.scroll - delta).clamp(0.0, max);
        s.list.setNeedsDisplay(true);
    });
}

fn move_selection(delta: isize) {
    with_state(|s| {
        let len = filtered_indices(s).len();
        if len == 0 {
            return;
        }
        let current = s.selected.min(len - 1) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        s.selected = next;
        let height = s.list.bounds().size.height;
        let top = next as f64 * ROW_H;
        if top < s.scroll {
            s.scroll = top;
        }
        if top + ROW_H > s.scroll + height {
            s.scroll = (top + ROW_H - height).max(0.0);
        }
        s.list.setNeedsDisplay(true);
    });
}

fn paste_selected() {
    let selected = with_state(|s| s.selected);
    paste_filtered(selected, true);
}

fn toggle_pin_selected() {
    let selected = with_state(|s| s.selected);
    toggle_pin_at(selected);
}

fn toggle_pin_at(filtered_index: usize) {
    let snapshot = with_state(|s| {
        let store_index = *filtered_indices(s).get(filtered_index)?;
        let clip = s.store.clips.get(store_index)?;
        Some((store_index, clip.pinned))
    });
    let Some((store_index, pinned)) = snapshot else {
        NSBeep();
        return;
    };

    let new_index = with_state(|s| {
        let new_index = s.store.set_pinned(store_index, !pinned)?;
        s.store.save();
        Some(new_index)
    });
    let Some(new_index) = new_index else {
        return;
    };

    with_state(|s| {
        let filtered = filtered_indices(s);
        s.selected = filtered
            .iter()
            .position(|index| *index == new_index)
            .unwrap_or(0);
        s.list.setNeedsDisplay(true);
    });
    refresh_ui();
    move_selection(0);
}

fn copy_selected() {
    let selected = with_state(|s| s.selected);
    paste_filtered(selected, false);
}

fn paste_filtered(filtered_index: usize, simulate: bool) {
    let Some(index) = with_state(|s| filtered_indices(s).get(filtered_index).copied()) else {
        return;
    };
    paste_store_index(index, simulate);
}

#[allow(deprecated)]
fn paste_store_index(index: usize, simulate: bool) {
    let Some(clip) = with_state(|s| s.store.clips.get(index).cloned()) else {
        return;
    };
    let target = resolve_target_app();

    let pb = pasteboard::general();
    if !with_state(|s| pasteboard::write_clip(&pb, &s.store, &clip)) {
        NSBeep();
        return;
    }
    let change_count = pb.changeCount();
    with_state(|s| s.last_change_count = change_count);
    hide_panel();

    if !simulate {
        return;
    }

    if let Some(app) = target {
        app.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps);
    }

    let first_time = with_state(|s| {
        let first = !s.accessibility_asked;
        s.accessibility_asked = true;
        first
    });
    if first_time {
        paste::ensure_accessibility();
    }

    let block = RcBlock::new(|_timer: NonNull<NSTimer>| {
        guard("simulate_paste", paste::simulate_paste);
    });
    unsafe {
        NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.25, false, &block);
    }
}

// ---------------------------------------------------------------------------
// Keyboard
// ---------------------------------------------------------------------------

pub(crate) fn handle_key_event(event: &NSEvent) -> bool {
    let recording = with_state(|s| s.recording);
    if recording {
        return capture_shortcut(event);
    }

    let active = with_state(|s| {
        let panel = s.panel.isVisible() && s.panel.isKeyWindow();
        let prefs = s
            .prefs
            .as_ref()
            .map(|p| p.window.isKeyWindow())
            .unwrap_or(false);
        panel && !prefs
    });
    if !active {
        return false;
    }

    let key = event.keyCode();
    let flags = event.modifierFlags().0;

    if flags & NS_COMMAND != 0 {
        match key {
            18 => {
                set_section(ClipboardSection::Text);
                return true;
            }
            19 => {
                set_section(ClipboardSection::Images);
                return true;
            }
            35 => {
                toggle_pin_selected();
                return true;
            }
            _ => {}
        }
    }

    match key {
        126 => {
            move_selection(-1);
            true
        }
        125 => {
            move_selection(1);
            true
        }
        115 => {
            move_selection(-1000);
            true
        }
        119 => {
            move_selection(1000);
            true
        }
        36 | 76 => {
            paste_selected();
            true
        }
        53 => {
            hide_panel();
            true
        }
        51 | 117 => {
            let search_empty = with_state(|s| s.search.stringValue().to_string().trim().is_empty());
            if search_empty || flags & NS_COMMAND != 0 {
                delete_selected();
                true
            } else {
                false
            }
        }
        8 if flags & NS_COMMAND != 0 => {
            copy_selected();
            true
        }
        _ => false,
    }
}

fn capture_shortcut(event: &NSEvent) -> bool {
    let mtm = main_thread();
    let keycode = event.keyCode();
    let flags = event.modifierFlags().0 & (NS_COMMAND | NS_SHIFT | NS_OPTION | NS_CONTROL);
    let modifiers = hotkey::modifiers_from_nsevent(flags);

    if keycode == 53 && flags == 0 {
        cancel_recording();
        return true;
    }
    // Ignore presses of the modifier keys themselves.
    if (54..=58).contains(&keycode) {
        return true;
    }
    if flags & (NS_COMMAND | NS_OPTION | NS_CONTROL) == 0 {
        NSBeep();
        return true;
    }

    let chars = event.charactersIgnoringModifiers().map(|c| c.to_string());
    let display = hotkey::display_shortcut(keycode, modifiers, chars.as_deref());

    match hotkey::register(keycode, modifiers) {
        Ok(hotkey) => {
            with_state(|s| {
                s.hotkey = Some(hotkey);
                s.recording = false;
                s.settings.hotkey_keycode = keycode;
                s.settings.hotkey_modifiers = modifiers;
                s.settings.hotkey_display = display.clone();
                s.settings.hotkey_enabled = true;
                settings::save(&s.settings);
                if let Some(prefs) = &s.prefs {
                    prefs
                        .shortcut_button
                        .setTitle(&NSString::from_str(&display));
                }
            });
        }
        Err(_) => {
            NSBeep();
            let (keycode, modifiers, current_display) = with_state(|s| {
                (
                    s.settings.hotkey_keycode,
                    s.settings.hotkey_modifiers,
                    s.settings.hotkey_display.clone(),
                )
            });
            if let Ok(hotkey) = hotkey::register(keycode, modifiers) {
                with_state(|s| s.hotkey = Some(hotkey));
            }
            with_state(|s| {
                s.recording = false;
                if let Some(prefs) = &s.prefs {
                    prefs
                        .shortcut_button
                        .setTitle(&NSString::from_str(&current_display));
                }
            });
            activate_app();
            let alert = NSAlert::new(mtm);
            alert.setMessageText(&NSString::from_str("The key combination cannot be used"));
            alert.setInformativeText(&NSString::from_str(
                "It may already be used by a system-wide shortcut or by another app.",
            ));
            alert.addButtonWithTitle(&NSString::from_str("OK"));
            alert.runModal();
        }
    }
    true
}

fn cancel_recording() {
    let (keycode, modifiers, display) = with_state(|s| {
        (
            s.settings.hotkey_keycode,
            s.settings.hotkey_modifiers,
            s.settings.hotkey_display.clone(),
        )
    });
    if let Ok(hotkey) = hotkey::register(keycode, modifiers) {
        with_state(|s| s.hotkey = Some(hotkey));
    }
    with_state(|s| {
        s.recording = false;
        if let Some(prefs) = &s.prefs {
            prefs
                .shortcut_button
                .setTitle(&NSString::from_str(&display));
        }
    });
}

fn register_hotkey_from_settings() {
    let (enabled, keycode, modifiers) = with_state(|s| {
        (
            s.settings.hotkey_enabled,
            s.settings.hotkey_keycode,
            s.settings.hotkey_modifiers,
        )
    });
    if !enabled {
        return;
    }
    if let Ok(hotkey) = hotkey::register(keycode, modifiers) {
        with_state(|s| s.hotkey = Some(hotkey));
    }
}

pub fn on_global_hotkey() {
    let ready = STATE.with(|s| s.borrow().is_some());
    if ready {
        toggle_panel();
    }
}

// ---------------------------------------------------------------------------
// Deletion
// ---------------------------------------------------------------------------

fn delete_selected() {
    let mtm = main_thread();
    let (index, confirm) = with_state(|s| {
        (
            filtered_indices(s).get(s.selected).copied(),
            s.settings.confirm_delete,
        )
    });
    let Some(index) = index else {
        return;
    };

    if confirm {
        activate_app();
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("Delete the selected clipping?"));
        alert.setInformativeText(&NSString::from_str("This action cannot be undone."));
        alert.addButtonWithTitle(&NSString::from_str("Delete"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        if alert.runModal() != NSAlertFirstButtonReturn {
            return;
        }
    }

    with_state(|s| {
        s.store.remove(index);
        s.store.save();
    });
    refresh_ui();
    prune_thumbnails();
}

fn clear_section(section: ClipboardSection) {
    let mtm = main_thread();
    let (count, confirm) = with_state(|s| {
        let count = s
            .store
            .clips
            .iter()
            .filter(|clip| match section {
                ClipboardSection::Images => clip.kind == ClipKind::Image,
                ClipboardSection::Text => clip.kind != ClipKind::Image,
            })
            .count();
        (count, s.settings.confirm_delete)
    });
    if count == 0 {
        NSBeep();
        return;
    }

    if confirm {
        activate_app();
        let alert = NSAlert::new(mtm);
        let (message, info) = match section {
            ClipboardSection::Text => (
                "Delete all text clippings?",
                "This removes every text, rich text, HTML and file clipping. Images are kept. This action cannot be undone.",
            ),
            ClipboardSection::Images => (
                "Delete all images?",
                "This removes every copied image and its thumbnail. Text clippings are kept. This action cannot be undone.",
            ),
        };
        alert.setMessageText(&NSString::from_str(message));
        alert.setInformativeText(&NSString::from_str(info));
        alert.addButtonWithTitle(&NSString::from_str("Delete"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        if alert.runModal() != NSAlertFirstButtonReturn {
            return;
        }
    }

    with_state(|s| {
        s.store.remove_where(|clip| match section {
            ClipboardSection::Images => clip.kind == ClipKind::Image,
            ClipboardSection::Text => clip.kind != ClipKind::Image,
        });
        s.store.save();
        s.selected = 0;
        s.scroll = 0.0;
    });
    refresh_ui();
    prune_thumbnails();
}

fn clear_all() {
    let mtm = main_thread();
    let confirm = with_state(|s| s.settings.confirm_delete);
    if confirm {
        activate_app();
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("Delete all clippings?"));
        alert.setInformativeText(&NSString::from_str(
            "This permanently removes your clipboard history. This action cannot be undone.",
        ));
        alert.addButtonWithTitle(&NSString::from_str("Delete All"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        if alert.runModal() != NSAlertFirstButtonReturn {
            return;
        }
    }

    let search = with_state(|s| {
        s.store.clear();
        s.store.save();
        s.selected = 0;
        s.scroll = 0.0;
        s.query.clear();
        s.search.clone()
    });
    search.setStringValue(&NSString::from_str(""));
    refresh_ui();
    // Clear All Clippings removes the thumbnail cache too, including pending
    // jobs for clips that no longer exist.
    clear_thumbnail_cache();
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

fn search_changed() {
    with_state(|s| {
        s.query = s.search.stringValue().to_string();
        s.selected = 0;
        s.scroll = 0.0;
        update_footer(s);
        s.list.setNeedsDisplay(true);
    });
}

// ---------------------------------------------------------------------------
// Preferences
// ---------------------------------------------------------------------------

#[allow(deprecated)]
fn open_preferences() {
    let mtm = main_thread();
    hide_panel();

    let needs_build = with_state(|s| s.prefs.is_none());
    if needs_build {
        let parts = views::build_preferences(mtm, &delegate_ref());
        with_state(|s| s.prefs = Some(parts));
    }

    with_state(|s| {
        if let Some(prefs) = &s.prefs {
            prefs
                .max_field
                .setStringValue(&NSString::from_str(&s.settings.max_clips.to_string()));
            prefs.max_label.setStringValue(&NSString::from_str(&format!(
                "Maximum: {} clippings",
                s.settings.max_clips
            )));
            prefs.record_box.setState(if s.settings.record {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            prefs.confirm_box.setState(if s.settings.confirm_delete {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            prefs.login_box.setState(if s.settings.start_at_login {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            prefs
                .shortcut_button
                .setTitle(&NSString::from_str(&s.settings.hotkey_display));
            prefs.ignored_view.clear_selection();
            prefs.ignored_view.setNeedsDisplay(true);
        }
    });

    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    let window = with_state(|s| s.prefs.as_ref().map(|p| p.window.clone()));
    if let Some(window) = window {
        window.makeKeyAndOrderFront(None);
    }
}

fn max_clips_changed(sender: &NSTextField) {
    let mtm = main_thread();
    let (old, count, confirm) = with_state(|s| {
        (
            s.settings.max_clips,
            s.store.clips.len(),
            s.settings.confirm_delete,
        )
    });

    let raw = sender.stringValue().to_string();
    let new_limit = raw
        .trim()
        .parse::<usize>()
        .unwrap_or(old)
        .clamp(1, MAX_LIMIT);
    if new_limit == old {
        sender.setStringValue(&NSString::from_str(&old.to_string()));
        return;
    }

    if new_limit < count && confirm {
        activate_app();
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("Reduce the clipping limit?"));
        alert.setInformativeText(&NSString::from_str(&format!(
            "You currently have {count} clippings stored. Reducing the limit to {new_limit} will permanently remove the oldest clippings."
        )));
        alert.addButtonWithTitle(&NSString::from_str("Continue"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        if alert.runModal() != NSAlertFirstButtonReturn {
            sender.setStringValue(&NSString::from_str(&old.to_string()));
            return;
        }
    }

    with_state(|s| {
        s.settings.max_clips = new_limit;
        s.store.trim(new_limit);
        s.store.save();
        settings::save(&s.settings);
    });
    sender.setStringValue(&NSString::from_str(&new_limit.to_string()));
    with_state(|s| {
        if let Some(prefs) = &s.prefs {
            prefs.max_label.setStringValue(&NSString::from_str(&format!(
                "Maximum: {new_limit} clippings"
            )));
        }
    });
    refresh_ui();
    prune_thumbnails();
}

#[allow(deprecated)]
fn add_ignored_app() {
    let mtm = main_thread();
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(false);
    let extension = NSString::from_str("app");
    let types = NSArray::from_slice(&[&*extension]);
    panel.setAllowedFileTypes(Some(&types));
    panel.setMessage(Some(&NSString::from_str("Choose an application to ignore")));
    panel.setPrompt(Some(&NSString::from_str("Add")));

    if panel.runModal() != NSModalResponseOK {
        return;
    }
    let urls = panel.URLs();
    let Some(url) = urls.iter().next() else {
        return;
    };

    let name = url
        .lastPathComponent()
        .map(|n| n.to_string())
        .map(|n| n.trim_end_matches(".app").to_string())
        .unwrap_or_else(|| "Application".to_string());
    let bundle = NSBundle::bundleWithURL(&url)
        .and_then(|b| b.bundleIdentifier())
        .map(|b| b.to_string());
    let Some(bundle_id) = bundle else {
        show_alert(
            "Not an application",
            "The selected item has no bundle identifier.",
        );
        return;
    };

    with_state(|s| {
        s.settings.add_ignored(IgnoredApp { name, bundle_id });
        settings::save(&s.settings);
        if let Some(prefs) = &s.prefs {
            prefs.ignored_view.clear_selection();
            prefs.ignored_view.setNeedsDisplay(true);
        }
    });
    resize_ignored_view();
}

/// Keep the ignored-apps document view tall enough for its rows so the
/// enclosing scroll view can show a scroller when the list grows.
fn resize_ignored_view() {
    let height = with_state(|s| (16.0 + s.settings.ignored_apps.len() as f64 * 26.0).max(216.0));
    with_state(|s| {
        if let Some(prefs) = &s.prefs {
            prefs.ignored_view.setFrameSize(NSSize::new(436.0, height));
            prefs.ignored_view.setNeedsDisplay(true);
        }
    });
}

fn remove_ignored_app() {
    let selected = with_state(|s| {
        s.prefs
            .as_ref()
            .map(|p| p.ignored_view.selected())
            .unwrap_or(usize::MAX)
    });
    if selected == usize::MAX {
        NSBeep();
        return;
    }
    with_state(|s| {
        if selected < s.settings.ignored_apps.len() {
            s.settings.ignored_apps.remove(selected);
            settings::save(&s.settings);
        }
        if let Some(prefs) = &s.prefs {
            prefs.ignored_view.clear_selection();
            prefs.ignored_view.setNeedsDisplay(true);
        }
    });
    resize_ignored_view();
}

fn prune_thumbnail_files() {
    with_state(|s| {
        let valid: HashSet<u64> = s.store.clips.iter().map(|clip| clip.hash).collect();
        s.thumbnail_service.prune(valid);
    });
}

fn prune_thumbnails() {
    with_state(|s| {
        let valid: HashSet<u64> = s.store.clips.iter().map(|clip| clip.hash).collect();
        s.thumbnail_service.prune(valid.clone());
        s.thumbnails.retain(|hash, _| valid.contains(hash));
        s.thumbnail_failed.retain(|hash| valid.contains(hash));
    });
}

fn clear_thumbnail_cache() {
    with_state(|s| {
        s.thumbnail_service.clear();
        s.thumbnail_service.clear_now();
        s.thumbnails.clear();
        s.thumbnail_failed.clear();
        s.list.setNeedsDisplay(true);
    });
}

/// Called from the 0.1 s timer on the main thread. The worker only writes PNG
/// bytes; `NSImage` creation and insertion stay here.
fn drain_thumbnail_results() {
    with_state(|s| {
        let finished = s.thumbnail_service.drain();
        if finished.is_empty() {
            return;
        }
        let mut redraw = false;
        for result in finished {
            // A delete/trim may have removed the clip while its job was in
            // flight; do not insert a thumbnail for a clip that is gone.
            if !s.store.clips.iter().any(|clip| clip.hash == result.hash) {
                continue;
            }
            if result.ok
                && let Some(bytes) = s.thumbnail_service.load_cached(result.hash)
                && let Some(image) = thumbnails::nsimage_from_png(bytes)
            {
                if s.thumbnails.len() >= MAX_IN_MEMORY_THUMBNAILS {
                    s.thumbnails.clear();
                }
                s.thumbnails.insert(result.hash, image);
                redraw = true;
                continue;
            }
            s.thumbnail_failed.insert(result.hash);
        }
        if redraw {
            s.list.setNeedsDisplay(true);
        }
    });
}

#[allow(deprecated)]
fn activate_app() {
    let mtm = main_thread();
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}

fn show_alert(message: &str, info: &str) {
    let mtm = main_thread();
    activate_app();
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(message));
    alert.setInformativeText(&NSString::from_str(info));
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    alert.runModal();
}

fn show_about() {
    let mtm = main_thread();
    let (shortcut, dir) = with_state(|s| {
        (
            s.settings.hotkey_display.clone(),
            s.store.dir().display().to_string(),
        )
    });
    activate_app();
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("RustClip"));
    alert.setInformativeText(&NSString::from_str(&format!(
        "Version {}\n\nA pure-Rust clipboard history manager for macOS, inspired by CopyClip.\n\nShortcut: {}\nHistory: {}",
        env!("CARGO_PKG_VERSION"),
        shortcut,
        dir
    )));
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    alert.runModal();
}
