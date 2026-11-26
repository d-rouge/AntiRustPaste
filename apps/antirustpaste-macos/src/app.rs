//! Application state, menu bar item, clipboard polling and all user actions.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::ptr::{NonNull, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::*;
use objc2_foundation::*;
use objc2_quick_look_ui::{
    QLPreviewItem, QLPreviewPanel, QLPreviewPanelDataSource, QLPreviewPanelDelegate,
};

use crate::hotkey::{self, Hotkey};
use crate::paste;
use crate::pasteboard;
use antirustpaste_core::{
    Clip, ClipKind, IgnoredApp, MAX_LIMIT, Section, Settings, Shell, ShellFile, Store,
    ThumbnailService, filter_indices as core_filter_indices, load_cursors, now_ms, read_new,
    save_cursors,
};
use antirustpaste_pdf::PdfImage;

use crate::settings::{self, apply_start_at_login};
use crate::thumbnails;
use crate::update;
use crate::views::{self, ClipListView, ClipPanel, PanelDelegate, PdfExportParts, PrefsParts};

pub(crate) const PANEL_W: f64 = 460.0;
pub(crate) const PANEL_H: f64 = 480.0;
pub(crate) const ROW_H: f64 = 54.0;
pub(crate) const LIST_HEADER_H: f64 = 26.0;
const LIST_BOTTOM_PAD: f64 = 4.0;
pub(crate) const LIST_BOTTOM: f64 = 32.0;
pub(crate) const SEARCH_H: f64 = 40.0;

/// Vertical layout of the history list: a **PINNED** header and card for the
/// pinned group, a **RECENT** header, then the unpinned rows. Headers are only
/// present when they separate something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ListLayout {
    pinned: usize,
    total: usize,
    pinned_header: bool,
    recent_header: bool,
}

impl ListLayout {
    pub(crate) fn new(pinned: usize, total: usize) -> Self {
        let pinned = pinned.min(total);
        Self {
            pinned,
            total,
            pinned_header: pinned > 0,
            recent_header: pinned > 0 && pinned < total,
        }
    }

    pub(crate) fn pinned(&self) -> usize {
        self.pinned
    }

    fn rows_top(&self) -> f64 {
        if self.pinned_header {
            LIST_HEADER_H
        } else {
            0.0
        }
    }

    pub(crate) fn pinned_header_y(&self) -> Option<f64> {
        if self.pinned_header { Some(0.0) } else { None }
    }

    pub(crate) fn recent_header_y(&self) -> Option<f64> {
        if self.recent_header {
            Some(self.rows_top() + self.pinned as f64 * ROW_H)
        } else {
            None
        }
    }

    pub(crate) fn row_top(&self, index: usize) -> f64 {
        if index < self.pinned {
            return self.rows_top() + index as f64 * ROW_H;
        }
        let start = match self.recent_header_y() {
            Some(header_y) => header_y + LIST_HEADER_H,
            None => self.rows_top() + self.pinned as f64 * ROW_H,
        };
        start + index.saturating_sub(self.pinned) as f64 * ROW_H
    }

    pub(crate) fn content_height(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.row_top(self.total - 1) + ROW_H + LIST_BOTTOM_PAD
    }

    /// Top and height of the rounded card behind the pinned rows.
    pub(crate) fn pinned_card(&self) -> Option<(f64, f64)> {
        if !self.pinned_header {
            return None;
        }
        Some((self.rows_top() - 3.0, self.pinned as f64 * ROW_H + 6.0))
    }

    pub(crate) fn row_at(&self, y: f64) -> Option<usize> {
        if y < 0.0 || self.total == 0 {
            return None;
        }
        let mut low = 0;
        let mut high = self.total;
        while low < high {
            let mid = low + (high - low) / 2;
            let top = self.row_top(mid);
            if y < top {
                high = mid;
            } else if y >= top + ROW_H {
                low = mid + 1;
            } else {
                return Some(mid);
            }
        }
        None
    }

    /// Half-open row range intersecting `top..bottom` in document coordinates.
    pub(crate) fn visible_range(&self, top: f64, bottom: f64) -> (usize, usize) {
        if self.total == 0 || bottom <= top {
            return (0, 0);
        }
        let mut low = 0;
        let mut high = self.total;
        while low < high {
            let mid = low + (high - low) / 2;
            if self.row_top(mid) + ROW_H <= top {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        let start = low;
        let mut high = self.total;
        while low < high {
            let mid = low + (high - low) / 2;
            if self.row_top(mid) < bottom {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        (start, low)
    }
}

/// Count the pinned prefix of the filtered rows. The store keeps pinned clips
/// at the front, so the prefix is the pinned group.
pub(crate) fn list_layout(s: &AppState, indices: &[usize]) -> ListLayout {
    let pinned = indices
        .iter()
        .take_while(|&&index| s.store.clips.get(index).is_some_and(|clip| clip.pinned))
        .count();
    ListLayout::new(pinned, indices.len())
}
const MAX_IN_MEMORY_THUMBNAILS: usize = 256;
const STATUS_ICON_PNG: &[u8] = include_bytes!("../../../assets/status-icon.png");

// NSEventModifierFlags values used only for local key handling and recording.
const NS_SHIFT: usize = 1 << 17;
const NS_CONTROL: usize = 1 << 18;
const NS_OPTION: usize = 1 << 19;
const NS_COMMAND: usize = 1 << 20;

pub(crate) use antirustpaste_core::Section as ClipboardSection;

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
            (Self::Commands, 1) => "1 command".to_string(),
            (Self::Text, count) => format!("{count} clippings"),
            (Self::Images, count) => format!("{count} images"),
            (Self::Commands, count) => format!("{count} commands"),
        }
    }

    fn empty_message(self) -> &'static str {
        match self {
            Self::Text => "Your clippings will appear here…",
            Self::Images => "Your images will appear here…",
            Self::Commands => "Your terminal commands will appear here…",
        }
    }

    fn no_results_message(self) -> &'static str {
        match self {
            Self::Text => "No clippings found",
            Self::Images => "No images found",
            Self::Commands => "No commands found",
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            Self::Text => "Search clippings",
            Self::Images => "Search images",
            Self::Commands => "Search commands",
        }
    }
}

pub(crate) struct PdfExportState {
    pub parts: PdfExportParts,
    pub order: Vec<String>,
    pub selected: HashSet<String>,
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
    pub export_pdf_button: Retained<NSButton>,
    pub refresh_commands_button: Retained<NSButton>,
    pub pin_outline: Option<Retained<NSImage>>,
    pub pin_filled: Option<Retained<NSImage>>,
    pub pin_outline_selected: Option<Retained<NSImage>>,
    pub pin_filled_selected: Option<Retained<NSImage>>,
    pub section: ClipboardSection,
    pub thumbnails: HashMap<u64, Retained<NSImage>>,
    pub thumbnail_failed: HashSet<u64>,
    pub thumbnail_service: ThumbnailService,
    /// NSWindow's delegate is a weak reference; keep ours alive here.
    #[allow(dead_code)]
    pub panel_delegate: Retained<PanelDelegate>,
    pub prefs: Option<PrefsParts>,
    pub pdf_export: Option<PdfExportState>,
    /// File URL handed to the shared Quick Look panel; `None` when it is closed.
    pub preview_item: Option<Retained<NSURL>>,
    /// Full-size image written for Quick Look; deleted when the preview closes.
    pub preview_file: Option<PathBuf>,
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
        let state = guard.as_mut().expect("AntiRustPaste state not initialised");
        f(state)
    })
}

/// Run an AppKit, Carbon or timer callback without letting a panic unwind into
/// Objective-C. A Rust panic crossing an `extern "C"` boundary aborts the
/// process, so every entry point that the system calls goes through here.
pub(crate) fn guard(name: &str, f: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
        eprintln!("antirustpaste: recovered from a panic in {name}");
    }
}

/// The value-returning form of [`guard`]. Callers choose a safe fallback when
/// it returns `None`.
pub(crate) fn guard_value<R>(name: &str, f: impl FnOnce() -> R) -> Option<R> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(value) => Some(value),
        Err(_) => {
            eprintln!("antirustpaste: recovered from a panic in {name}");
            None
        }
    }
}

pub(crate) fn as_any<T: Message>(obj: &T) -> &AnyObject {
    unsafe { &*(obj as *const T as *const AnyObject) }
}

/// AntiRustPaste never spawns a thread: startup, AppKit delegate callbacks, block
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

pub(crate) fn section_matches(section: ClipboardSection, clip: &Clip) -> bool {
    match section {
        ClipboardSection::Text => clip.kind != ClipKind::Image && clip.kind != ClipKind::Command,
        ClipboardSection::Images => clip.kind == ClipKind::Image,
        ClipboardSection::Commands => clip.kind == ClipKind::Command,
    }
}

fn set_section(section: ClipboardSection) {
    let changed = with_state(|s| {
        let changed = s.section != section;
        s.section = section;
        s.selected = 0;
        s.scroll = 0.0;
        s.section_control.setSelectedSegment(match section {
            ClipboardSection::Text => 0,
            ClipboardSection::Images => 1,
            ClipboardSection::Commands => 2,
        });
        s.search
            .setPlaceholderString(Some(&NSString::from_str(section.placeholder())));
        update_footer(s);
        s.list.setNeedsDisplay(true);
        changed
    });
    if changed && section == ClipboardSection::Commands {
        // Commands are read only when the user opens the tab; there is no
        // background poll of shell history files.
        refresh_shell_history();
    }
}

fn update_footer(s: &AppState) {
    let count = filtered_indices(s).len();
    let label = s.section.footer_label(count);
    let hint = match s.section {
        ClipboardSection::Text => format!("{label} · ⌘P pin"),
        ClipboardSection::Images => format!("{label} · ⌘E PDF"),
        ClipboardSection::Commands => format!("{label} · ⌘R refresh"),
    };
    s.footer.setStringValue(&NSString::from_str(&hint));
    s.export_pdf_button
        .setHidden(s.section != ClipboardSection::Images);
    s.export_pdf_button.setEnabled(
        s.store
            .clips
            .iter()
            .any(|clip| clip.kind == ClipKind::Image),
    );
    s.refresh_commands_button
        .setHidden(s.section != ClipboardSection::Commands);
    s.clear_text_button.setEnabled(
        s.store
            .clips
            .iter()
            .any(|clip| section_matches(ClipboardSection::Text, clip)),
    );
    s.clear_images_button.setEnabled(
        s.store
            .clips
            .iter()
            .any(|clip| section_matches(ClipboardSection::Images, clip)),
    );
}

fn refresh_ui() {
    with_state(|s| {
        let indices = filtered_indices(s);
        let layout = list_layout(s, &indices);
        let len = indices.len();
        if s.selected >= len {
            s.selected = len.saturating_sub(1);
        }
        let height = s.list.bounds().size.height;
        let content = layout.content_height();
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

        #[unsafe(method(applicationWillTerminate:))]
        fn application_will_terminate(&self, _notification: &NSNotification) {
            guard("application_will_terminate", || {
                if let Some(preview) = preview_panel() {
                    preview.orderOut(None);
                }
                cleanup_preview();
                thumbnails::reset_previews();
            });
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
                let section = match sender.selectedSegment() {
                    1 => ClipboardSection::Images,
                    2 => ClipboardSection::Commands,
                    _ => ClipboardSection::Text,
                };
                set_section(section);
                let (panel, search) = with_state(|s| (s.panel.clone(), s.search.clone()));
                panel.makeFirstResponder(Some(&search));
            });
        }

        #[unsafe(method(refreshCommands:))]
        fn refresh_commands(&self, _sender: &AnyObject) {
            guard("refresh_commands", refresh_shell_history);
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

        #[unsafe(method(exportImagesPdf:))]
        fn export_images_pdf(&self, _sender: &AnyObject) {
            guard("export_images_pdf", open_pdf_export);
        }

        #[unsafe(method(pdfSelectAll:))]
        fn pdf_select_all(&self, _sender: &AnyObject) {
            guard("pdf_select_all", || pdf_select(true));
        }

        #[unsafe(method(pdfSelectNone:))]
        fn pdf_select_none(&self, _sender: &AnyObject) {
            guard("pdf_select_none", || pdf_select(false));
        }

        #[unsafe(method(pdfSave:))]
        fn pdf_save(&self, _sender: &AnyObject) {
            guard("pdf_save", save_pdf);
        }

        #[unsafe(method(pdfCancel:))]
        fn pdf_cancel(&self, _sender: &AnyObject) {
            guard("pdf_cancel", close_pdf_export);
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

        #[unsafe(method(checkForUpdates:))]
        fn check_for_updates(&self, _sender: &AnyObject) {
            guard("check_for_updates", || {
                check_for_updates(true);
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

        #[unsafe(method(pasteToggled:))]
        fn paste_toggled(&self, sender: &NSButton) {
            guard("paste_toggled", || {
                let on = sender.state() == NSControlStateValueOn;
                with_state(|s| {
                    s.settings.paste_after_select = on;
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

    unsafe impl QLPreviewPanelDataSource for AppDelegate {
        #[unsafe(method(numberOfPreviewItemsInPreviewPanel:))]
        fn preview_item_count(&self, _panel: Option<&QLPreviewPanel>) -> NSInteger {
            guard_value("preview_item_count", || {
                with_state(|s| if s.preview_item.is_some() { 1 } else { 0 })
            })
            .unwrap_or(0)
        }

        #[unsafe(method_id(previewPanel:previewItemAtIndex:))]
        fn preview_item_at_index(
            &self,
            _panel: Option<&QLPreviewPanel>,
            index: NSInteger,
        ) -> Option<Retained<ProtocolObject<dyn QLPreviewItem>>> {
            guard_value("preview_item_at_index", || {
                if index != 0 {
                    return None;
                }
                let url = with_state(|s| s.preview_item.clone())?;
                Some(ProtocolObject::from_retained(url))
            })
            .flatten()
        }

    }

    unsafe impl NSWindowDelegate for AppDelegate {}

    unsafe impl QLPreviewPanelDelegate for AppDelegate {
        #[unsafe(method(previewPanel:handleEvent:))]
        fn preview_panel_handle_event(
            &self,
            _panel: Option<&QLPreviewPanel>,
            event: Option<&NSEvent>,
        ) -> bool {
            guard_value("preview_panel_handle_event", || {
                let Some(event) = event else {
                    return false;
                };
                if event.r#type() != NSEventType::KeyDown {
                    return false;
                }
                if event.keyCode() == 49 || event.keyCode() == 53 {
                    close_image_preview();
                    return true;
                }
                false
            })
            .unwrap_or(false)
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
        eprintln!("antirustpaste: must start on the main thread");
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
    store.trim_sections(settings.max_clips);
    store.save();
    thumbnails::cleanup_legacy_cache();
    thumbnails::reset_previews();
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
        button.setToolTip(Some(&NSString::from_str(
            "AntiRustPaste — clipboard history",
        )));
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
        export_pdf_button: parts.export_pdf_button,
        refresh_commands_button: parts.refresh_commands_button,
        pin_outline: parts.pin_outline,
        pin_filled: parts.pin_filled,
        pin_outline_selected: parts.pin_outline_selected,
        pin_filled_selected: parts.pin_filled_selected,
        section: ClipboardSection::Text,
        thumbnails: HashMap::new(),
        thumbnail_failed: HashSet::new(),
        thumbnail_service: thumbnails::new_service(),
        panel_delegate,
        prefs: None,
        pdf_export: None,
        preview_item: None,
        preview_file: None,
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

    // Only a packaged app can replace itself; a `target/` build and a
    // Gatekeeper-translocated copy fall back to the manual release page.
    if !update_check_disabled() && update_bundle().is_some() {
        check_for_updates(false);
    }

    // Automation hooks used by docs/testing.md and scripts/smoke.sh. They are
    // inert unless an agent or test sets them explicitly.
    if std::env::var_os("ANTIRUSTPASTE_OPEN_PREFS").is_some() {
        open_preferences();
    }
    let open_images = std::env::var_os("ANTIRUSTPASTE_OPEN_IMAGES").is_some();
    if open_images {
        set_section(ClipboardSection::Images);
    }
    let open_commands = std::env::var_os("ANTIRUSTPASTE_OPEN_COMMANDS").is_some();
    if open_commands {
        set_section(ClipboardSection::Commands);
    }
    if open_images || open_commands || std::env::var_os("ANTIRUSTPASTE_OPEN_PANEL").is_some() {
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
        "Export Images to PDF…",
        sel!(exportImagesPdf:),
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
    menu.addItem(&menu_item(
        "About AntiRustPaste",
        sel!(showAbout:),
        &target,
        mtm,
    ));
    menu.addItem(&menu_item(
        "Check for Updates…",
        sel!(checkForUpdates:),
        &target,
        mtm,
    ));
    menu.addItem(&menu_item(
        "Quit AntiRustPaste",
        sel!(quitApp:),
        &target,
        mtm,
    ));

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
    finish_closed_preview();
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
        for mut item in items {
            item.source_name = source_name.clone();
            item.source_bundle = source_bundle.clone();
            let max = s.settings.max_clips;
            if s.store.insert(item, max) {
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
// Shell history
// ---------------------------------------------------------------------------

/// The history files belong to the shell, so paths stay in the macOS shell.
/// `ANTIRUSTPASTE_SHELL_HISTORY_DIR` keeps tests away from the operator's real
/// command history.
fn shell_history_files() -> Vec<ShellFile> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let base = match std::env::var_os("ANTIRUSTPASTE_SHELL_HISTORY_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        Some(dir) => dir,
        None => {
            // Safety net for agents and tests: an isolated data directory
            // without an explicit shell-history override must not fall back to
            // the operator's real history files.
            if std::env::var_os("ANTIRUSTPASTE_DATA_DIR").is_some_and(|value| !value.is_empty()) {
                return Vec::new();
            }
            home
        }
    };
    vec![
        ShellFile {
            shell: Shell::Zsh,
            path: base.join(".zsh_history"),
        },
        ShellFile {
            shell: Shell::Bash,
            path: base.join(".bash_history"),
        },
    ]
}

/// Read new commands on demand, when the Commands tab is opened or the user
/// asks for a refresh. This is the only place that touches shell history.
fn refresh_shell_history() {
    let (dir, max) = with_state(|s| (s.store.dir().to_path_buf(), s.settings.max_clips));
    let cursor_path = dir.join("shell-history.json");
    let mut cursors = load_cursors(&cursor_path);
    let files = shell_history_files();
    let now = now_ms();
    let mut reads = Vec::with_capacity(files.len());
    for file in &files {
        reads.push(read_new(file, &mut cursors, max, now));
    }
    let cursor_changed = reads.iter().any(|read| read.cursor_changed);
    // zsh and bash are read separately; merge them by timestamp so the
    // Commands list is newest-first across both files.
    let mut commands: Vec<_> = reads.into_iter().flat_map(|read| read.commands).collect();
    commands.sort_by_key(|command| command.date_ms);

    let mut changed = false;
    with_state(|s| {
        for command in &commands {
            if s.store.upsert_command(
                &command.text,
                command.shell.label(),
                command.date_ms,
                max,
                command.may_move,
            ) {
                changed = true;
            }
        }
        if changed {
            s.store.save();
        }
    });
    if cursor_changed {
        save_cursors(&cursor_path, &cursors);
    }
    if changed {
        refresh_ui();
    }
}

// ---------------------------------------------------------------------------
// Popup
// ---------------------------------------------------------------------------

/// A click outside the popup makes it resign key. Delay the hide briefly so a
/// click on the status item can cancel it and toggle the popup instead.
pub(crate) fn schedule_hide_from_resign() {
    if preview_is_open() {
        return;
    }
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
    if with_state(|s| s.preview_item.is_some()) {
        if let Some(preview) = preview_panel() {
            preview.orderOut(None);
        }
        cleanup_preview();
    }
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
        guard("present_panel", || {
            if with_state(|s| s.pending_show) {
                present_panel();
            }
        });
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
    if with_state(|s| s.section == ClipboardSection::Commands) {
        refresh_shell_history();
    }
}

pub(crate) fn handle_list_click(view: &ClipListView, event: &NSEvent) {
    let point = view.convertPoint_fromView(event.locationInWindow(), None);
    let width = view.bounds().size.width;
    let (scroll, layout) = with_state(|s| {
        let indices = filtered_indices(s);
        let layout = list_layout(s, &indices);
        (s.scroll, layout)
    });
    let Some(row) = layout.row_at(point.y + scroll) else {
        return;
    };
    with_state(|s| s.selected = row);
    if point.x >= width - 36.0 {
        toggle_pin_at(row);
    } else {
        paste_filtered(row, auto_paste());
    }
}

pub(crate) fn handle_list_scroll(delta: f64) {
    with_state(|s| {
        let indices = filtered_indices(s);
        let layout = list_layout(s, &indices);
        let height = s.list.bounds().size.height;
        let max = (layout.content_height() - height).max(0.0);
        s.scroll = (s.scroll - delta).clamp(0.0, max);
        s.list.setNeedsDisplay(true);
    });
}

fn move_selection(delta: isize) {
    with_state(|s| {
        let indices = filtered_indices(s);
        let layout = list_layout(s, &indices);
        let len = indices.len();
        if len == 0 {
            return;
        }
        let current = s.selected.min(len - 1) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        s.selected = next;
        let height = s.list.bounds().size.height;
        let top = layout.row_top(next);
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
    paste_filtered(selected, auto_paste());
}

/// Whether choosing a clipping should synthesise ⌘V after copying it.
fn auto_paste() -> bool {
    with_state(|s| s.settings.paste_after_select)
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

    if let Some(app) = target {
        app.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps);
    }

    if !simulate {
        return;
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
        if flags & NS_SHIFT != 0 && flags & (NS_OPTION | NS_CONTROL) == 0 {
            match key {
                18 => {
                    set_section(ClipboardSection::Text);
                    return true;
                }
                19 => {
                    set_section(ClipboardSection::Images);
                    return true;
                }
                20 => {
                    set_section(ClipboardSection::Commands);
                    return true;
                }
                _ => {}
            }
        }

        // ⌘1…⌘9 paste the Nth visible row of the active tab, after filtering.
        if flags & (NS_SHIFT | NS_OPTION | NS_CONTROL) == 0
            && let Some(character) = event
                .charactersIgnoringModifiers()
                .and_then(|value| value.to_string().chars().next())
            && let Some(digit) = character
                .to_digit(10)
                .filter(|digit| (1..=9).contains(digit))
        {
            paste_filtered(digit as usize - 1, auto_paste());
            return true;
        }

        match key {
            35 => {
                toggle_pin_selected();
                return true;
            }
            14 => {
                open_pdf_export();
                return true;
            }
            16 => {
                toggle_image_preview();
                return true;
            }
            15 if with_state(|s| s.section == ClipboardSection::Commands) => {
                refresh_shell_history();
                return true;
            }
            _ => {}
        }
    }

    if key == 49 && with_state(|s| s.section == ClipboardSection::Images && s.query.is_empty()) {
        toggle_image_preview();
        return true;
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
            .filter(|clip| section_matches(section, clip))
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
            ClipboardSection::Commands => (
                "Delete all terminal commands?",
                "This removes every command read from your shell history. Clippings are kept. This action cannot be undone.",
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
        s.store.remove_where(|clip| section_matches(section, clip));
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
            "This permanently removes your clipboard history. Terminal commands are kept. This action cannot be undone.",
        ));
        alert.addButtonWithTitle(&NSString::from_str("Delete All"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        if alert.runModal() != NSAlertFirstButtonReturn {
            return;
        }
    }

    let search = with_state(|s| {
        // "Clear All Clippings" leaves shell commands alone: they are read from
        // the user's own history file, not captured from the pasteboard.
        s.store.remove_where(|clip| clip.kind != ClipKind::Command);
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
                "Maximum: {} per section",
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
            prefs.paste_box.setState(if s.settings.paste_after_select {
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
    let (old, clips, commands, confirm) = with_state(|s| {
        let clips = s
            .store
            .clips
            .iter()
            .filter(|clip| clip.kind != ClipKind::Command)
            .count();
        let commands = s
            .store
            .clips
            .iter()
            .filter(|clip| clip.kind == ClipKind::Command)
            .count();
        (
            s.settings.max_clips,
            clips,
            commands,
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

    if (new_limit < clips || new_limit < commands) && confirm {
        activate_app();
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("Reduce the history limit?"));
        alert.setInformativeText(&NSString::from_str(&format!(
            "You currently have {clips} clippings and {commands} commands stored. Reducing the limit to {new_limit} will permanently remove the oldest items from each section that exceeds it."
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
        s.store.trim_sections(new_limit);
        s.store.save();
        settings::save(&s.settings);
    });
    sender.setStringValue(&NSString::from_str(&new_limit.to_string()));
    with_state(|s| {
        if let Some(prefs) = &s.prefs {
            prefs.max_label.setStringValue(&NSString::from_str(&format!(
                "Maximum: {new_limit} per section"
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

fn pdf_image_ids(s: &AppState) -> Vec<String> {
    s.store
        .clips
        .iter()
        .filter(|clip| clip.kind == ClipKind::Image)
        .map(|clip| clip.id.clone())
        .collect()
}

fn pdf_refresh() {
    with_state(|s| {
        let available = pdf_image_ids(s);
        let Some(pdf) = &mut s.pdf_export else {
            return;
        };
        pdf_sync_order(&mut pdf.order, &available);
        let total = pdf.order.len();
        let selected = pdf
            .order
            .iter()
            .filter(|id| pdf.selected.contains(*id))
            .count();
        let status = format!("{selected} of {total} images selected");
        pdf.parts
            .status
            .setStringValue(&NSString::from_str(&status));
        pdf.parts.save.setEnabled(selected > 0);
        pdf.parts.select_all.setEnabled(selected < total);
        pdf.parts.select_none.setEnabled(selected > 0);
        let height = (total as f64 * views::PDF_ROW_H + 16.0).max(448.0);
        pdf.parts.view.setFrameSize(NSSize::new(528.0, height));
        pdf.parts.view.setNeedsDisplay(true);
    });
}

fn open_pdf_export() {
    let mtm = main_thread();
    let count = with_state(|s| {
        s.store
            .clips
            .iter()
            .filter(|clip| clip.kind == ClipKind::Image)
            .count()
    });
    if count == 0 {
        NSBeep();
        show_alert(
            "No images",
            "Copy at least one image before exporting a PDF.",
        );
        return;
    }

    if let Some(old) = with_state(|s| s.pdf_export.take()) {
        old.parts.close();
    }

    let Some(parts) = views::build_pdf_export(mtm, &delegate_ref(), count) else {
        NSBeep();
        show_alert(
            "Could not open the export window",
            "The export window could not be created. Nothing was changed.",
        );
        return;
    };
    let (order, selected) = with_state(|s| {
        let order = pdf_image_ids(s);
        let selected = order.iter().cloned().collect();
        (order, selected)
    });
    with_state(|s| {
        s.pdf_export = Some(PdfExportState {
            parts,
            order,
            selected,
        });
    });

    pdf_refresh();
    activate_app();
    let window = with_state(|s| s.pdf_export.as_ref().map(|pdf| pdf.parts.window.clone()));
    if let Some(window) = window {
        window.makeKeyAndOrderFront(None);
    }
}

pub(crate) fn pdf_row_count() -> usize {
    with_state(|s| {
        s.pdf_export
            .as_ref()
            .map(|pdf| pdf.order.len())
            .unwrap_or(0)
    })
}

pub(crate) fn pdf_toggle_row(row: usize) {
    let id = with_state(|s| {
        let pdf = s.pdf_export.as_ref()?;
        pdf.order.get(row).cloned()
    });
    let Some(id) = id else {
        return;
    };

    with_state(|s| {
        let Some(pdf) = &mut s.pdf_export else {
            return;
        };
        if !pdf.selected.remove(&id) {
            pdf.selected.insert(id);
        }
    });
    pdf_refresh();
}

pub(crate) fn pdf_move_row(from: usize, boundary: usize) {
    with_state(|s| {
        let Some(pdf) = &mut s.pdf_export else {
            return;
        };
        let count = pdf.order.len();
        if from >= count {
            return;
        }
        let mut destination = boundary.min(count);
        if destination > from {
            destination -= 1;
        }
        if let Some(order) = moved_order(&pdf.order, from, destination) {
            pdf.order = order;
        }
    });
    pdf_refresh();
}

/// Move `from` to `to`, shifting the rows in between. Returns `None` for an
/// out-of-range request and a copy for a no-op move.
fn moved_order(order: &[String], from: usize, to: usize) -> Option<Vec<String>> {
    let moved = order.get(from)?.clone();
    if to >= order.len() {
        return None;
    }
    if from == to {
        return Some(order.to_vec());
    }

    let mut result = Vec::with_capacity(order.len());
    for (index, item) in order.iter().enumerate() {
        if index == from {
            continue;
        }
        if result.len() == to {
            result.push(moved.clone());
        }
        result.push(item.clone());
    }
    if result.len() < order.len() {
        result.push(moved);
    }
    Some(result)
}

/// Keep the operator's drag order and append images captured since the window
/// opened. Ids removed from history are dropped.
fn pdf_sync_order(order: &mut Vec<String>, available: &[String]) -> bool {
    let mut changed = false;
    order.retain(|id| {
        let keep = available.iter().any(|candidate| candidate == id);
        if !keep {
            changed = true;
        }
        keep
    });
    for id in available {
        if !order.iter().any(|candidate| candidate == id) {
            order.push(id.clone());
            changed = true;
        }
    }
    changed
}

fn pdf_select(selected: bool) {
    with_state(|s| {
        let ids = pdf_image_ids(s);
        let Some(pdf) = &mut s.pdf_export else {
            return;
        };
        pdf.selected = if selected {
            ids.into_iter().collect()
        } else {
            HashSet::new()
        };
    });
    pdf_refresh();
}

fn save_pdf() {
    let mtm = main_thread();
    let selected_count = with_state(|s| {
        let Some(pdf) = &s.pdf_export else {
            return 0;
        };
        s.store
            .clips
            .iter()
            .filter(|clip| clip.kind == ClipKind::Image && pdf.selected.contains(&clip.id))
            .count()
    });
    if selected_count == 0 {
        NSBeep();
        return;
    }

    activate_app();
    let panel = NSSavePanel::savePanel(mtm);
    panel.setNameFieldStringValue(&NSString::from_str("Clipboard Images.pdf"));
    panel.setCanCreateDirectories(true);
    panel.setExtensionHidden(false);
    panel.setMessage(Some(&NSString::from_str(
        "Choose where to save the selected images as a PDF",
    )));
    if panel.runModal() != NSModalResponseOK {
        return;
    }
    let Some(url) = panel.URL() else {
        return;
    };
    let Some(path) = url.path().map(|path| PathBuf::from(path.to_string())) else {
        return;
    };
    let path = if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
    {
        path
    } else {
        path.with_extension("pdf")
    };

    let (images, missing) = with_state(|s| {
        let Some(pdf) = &s.pdf_export else {
            return (Vec::new(), false);
        };
        let mut images = Vec::new();
        let mut missing = false;
        for id in &pdf.order {
            if !pdf.selected.contains(id) {
                continue;
            }
            let clip = s
                .store
                .clips
                .iter()
                .find(|clip| clip.id == *id && clip.kind == ClipKind::Image);
            match clip
                .and_then(|clip| clip.blob.as_deref())
                .and_then(|blob| s.store.read_blob(blob))
            {
                Some(bytes) => images.push(PdfImage { bytes }),
                None => missing = true,
            }
        }
        (images, missing)
    });
    if missing || images.len() != selected_count {
        show_alert(
            "Some images are missing",
            "One or more selected images could not be read. Nothing was saved.",
        );
        return;
    }

    // Encoding runs on the main thread so the app keeps its single AppKit
    // thread; the selection is bounded by the history limit and the save
    // panel has already closed.
    let result = antirustpaste_pdf::write_pdf(&images)
        .map_err(|error| error.to_string())
        .and_then(|bytes| write_pdf_file(&path, &bytes));
    match result {
        Ok(path) => {
            close_pdf_export();
            show_alert("PDF saved", &path.to_string_lossy());
        }
        Err(error) => show_alert("Could not export PDF", &error),
    }
}

fn write_pdf_file(path: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    let temporary = path.with_extension("pdf.tmp");
    fs::write(&temporary, bytes).map_err(|error| format!("Could not write PDF: {error}"))?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("Could not save PDF: {error}"));
    }
    Ok(path.to_path_buf())
}

fn close_pdf_export() {
    // Clone the handle, drop the state borrow, then call AppKit. The parts stay
    // stored so the window is not deallocated from inside its own event; the
    // next export replaces them.
    let window = with_state(|s| s.pdf_export.as_ref().map(|pdf| pdf.parts.window.clone()));
    if let Some(window) = window {
        window.orderOut(None);
    }
}

// ---------------------------------------------------------------------------
// Quick Look preview
// ---------------------------------------------------------------------------

fn preview_panel() -> Option<Retained<QLPreviewPanel>> {
    let mtm = main_thread();
    if unsafe { QLPreviewPanel::sharedPreviewPanelExists(mtm) } {
        unsafe { QLPreviewPanel::sharedPreviewPanel(mtm) }
    } else {
        None
    }
}

fn preview_is_open() -> bool {
    with_state(|s| s.preview_item.is_some())
        && preview_panel().is_some_and(|panel| panel.isVisible())
}

/// Space (Images tab) or ⌘Y: show the selected image in the shared Quick Look
/// panel, or close it when it is already open.
fn toggle_image_preview() {
    let mtm = main_thread();
    let Some(panel) = (unsafe { QLPreviewPanel::sharedPreviewPanel(mtm) }) else {
        return;
    };
    if panel.isVisible() {
        close_image_preview();
        return;
    }

    let Some((hash, bytes)) = with_state(|s| {
        let index = *filtered_indices(s).get(s.selected)?;
        let clip = s.store.clips.get(index)?;
        if clip.kind != ClipKind::Image {
            return None;
        }
        let blob = clip.blob.as_deref()?;
        let bytes = s.store.read_blob(blob)?;
        Some((clip.hash, bytes))
    }) else {
        NSBeep();
        return;
    };
    let Ok(info) = antirustpaste_imaging::info(&bytes) else {
        NSBeep();
        return;
    };
    let extension = match info.format {
        antirustpaste_imaging::Format::Png => "png",
        antirustpaste_imaging::Format::Tiff => "tiff",
        antirustpaste_imaging::Format::Jpeg => "jpg",
    };
    let Some(path) = write_preview_file(hash, &bytes, extension) else {
        NSBeep();
        return;
    };

    let url =
        NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&path.to_string_lossy()), false);
    let delegate = delegate_ref();
    let source = ProtocolObject::from_ref(&*delegate);
    unsafe {
        panel.setDataSource(Some(source));
        panel.setDelegate(Some(as_any(&*delegate)));
    }
    let old = with_state(|s| {
        let old = s.preview_file.take();
        s.preview_file = Some(path.clone());
        s.preview_item = Some(url);
        old
    });
    if let Some(old) = old
        && old != path
    {
        let _ = fs::remove_file(old);
    }

    unsafe { panel.reloadData() };
    activate_app();
    panel.makeKeyAndOrderFront(None);
}

fn write_preview_file(hash: u64, bytes: &[u8], extension: &str) -> Option<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = thumbnails::ensure_preview_dir().ok()?;
    let path = dir.join(format!("preview-{hash}.{extension}"));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .ok()?;
    file.write_all(bytes).ok()?;
    Some(path)
}

fn close_image_preview() {
    if let Some(panel) = preview_panel() {
        panel.orderOut(None);
    }
    finish_closed_preview();
}

/// Called from the 1 s poll and after ordering the panel out: clean up once the
/// preview is gone and give the popup its keyboard focus back.
fn finish_closed_preview() {
    if !with_state(|s| s.preview_item.is_some()) || preview_is_open() {
        return;
    }
    let mtm = main_thread();
    let active = NSApplication::sharedApplication(mtm).isActive();
    let popup_visible = with_state(|s| s.panel.isVisible());
    if popup_visible {
        if active {
            let (panel, search) = with_state(|s| {
                // Cancel a hide scheduled while Quick Look was taking key.
                s.hide_token = s.hide_token.wrapping_add(1);
                (s.panel.clone(), s.search.clone())
            });
            panel.makeKeyAndOrderFront(None);
            panel.makeFirstResponder(Some(&search));
        } else {
            hide_panel();
        }
    }
    cleanup_preview();
}

fn cleanup_preview() {
    let path = with_state(|s| {
        s.preview_item = None;
        s.preview_file.take()
    });
    if let Some(path) = path {
        let _ = fs::remove_file(path);
    }
    if let Some(panel) = preview_panel() {
        unsafe {
            panel.setDataSource(None);
            panel.setDelegate(None);
        }
    }
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
    alert.setMessageText(&NSString::from_str("AntiRustPaste"));
    alert.setInformativeText(&NSString::from_str(&format!(
        "Version {}\n\nA pure-Rust clipboard history manager\n\nShortcut: {}\nHistory: {}",
        env!("CARGO_PKG_VERSION"),
        shortcut,
        dir
    )));
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    alert.addButtonWithTitle(&NSString::from_str("Check for Updates…"));
    if alert.runModal() == NSAlertSecondButtonReturn {
        check_for_updates(true);
    }
}

// ---------------------------------------------------------------------------
// Update checking
// ---------------------------------------------------------------------------

thread_local! {
    /// One session for all release requests. Its delegate queue is the main
    /// queue, so completion handlers run on the AppKit thread.
    static UPDATE_SESSION: RefCell<Option<Retained<NSURLSession>>> = const { RefCell::new(None) };
    static UPDATE_BUSY: Cell<bool> = const { Cell::new(false) };
}

/// Keeps staged-update directories distinct when two downloads finish in the
/// same millisecond (and when tests run in parallel).
static UPDATE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn update_check_disabled() -> bool {
    std::env::var_os("ANTIRUSTPASTE_DISABLE_UPDATE_CHECK").is_some()
}

/// The bundle to replace, when the app was launched from a normal `.app`.
fn update_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let bundle = update::bundle_root(&exe)?;
    if update::is_translocated(&bundle) {
        None
    } else {
        Some(bundle)
    }
}

fn update_session() -> Retained<NSURLSession> {
    UPDATE_SESSION.with(|cell| {
        if let Some(session) = cell.borrow().as_ref() {
            return session.clone();
        }
        let configuration = NSURLSessionConfiguration::ephemeralSessionConfiguration();
        configuration.setTimeoutIntervalForRequest(20.0);
        configuration.setTimeoutIntervalForResource(300.0);
        let queue = NSOperationQueue::mainQueue();
        // SAFETY: the delegate queue is the main operation queue, so callbacks
        // stay on the AppKit thread; no delegate object is installed.
        let session = unsafe {
            NSURLSession::sessionWithConfiguration_delegate_delegateQueue(
                &configuration,
                None,
                Some(&queue),
            )
        };
        *cell.borrow_mut() = Some(session.clone());
        session
    })
}

fn fetch_update_bytes(url: &str, limit: usize, done: impl Fn(Result<Vec<u8>, String>) + 'static) {
    let Some(ns_url) = NSURL::URLWithString(&NSString::from_str(url)) else {
        done(Err("The update address is not valid.".to_string()));
        return;
    };
    let request = NSMutableURLRequest::requestWithURL(&ns_url);
    request.setHTTPMethod(&NSString::from_str("GET"));
    request.setValue_forHTTPHeaderField(
        Some(&NSString::from_str(&format!(
            "AntiRustPaste/{}",
            env!("CARGO_PKG_VERSION")
        ))),
        &NSString::from_str("User-Agent"),
    );
    request.setValue_forHTTPHeaderField(
        Some(&NSString::from_str("application/vnd.github+json")),
        &NSString::from_str("Accept"),
    );

    let session = update_session();
    let block = RcBlock::new(
        move |data: *mut NSData, response: *mut NSURLResponse, error: *mut NSError| {
            guard("update_http", || {
                done(read_update_response(data, response, error, limit));
            });
        },
    );
    // SAFETY: the block matches the selector's declared signature and only
    // reads the pointers inside the callback, where they are still live.
    let task = unsafe { session.dataTaskWithRequest_completionHandler(&request, &block) };
    task.resume();
}

fn read_update_response(
    data: *mut NSData,
    response: *mut NSURLResponse,
    error: *mut NSError,
    limit: usize,
) -> Result<Vec<u8>, String> {
    // SAFETY: NSURLSession passes live objects to the completion handler; every
    // pointer is null-checked before it is dereferenced.
    unsafe {
        if !error.is_null() {
            return Err((*error).localizedDescription().to_string());
        }
        if data.is_null() {
            return Err("The server sent no data.".to_string());
        }
        if !response.is_null()
            && let Some(http) = as_any(&*response).downcast_ref::<NSHTTPURLResponse>()
        {
            let status = http.statusCode();
            if !(200..300).contains(&status) {
                return Err(format!("The server answered with HTTP {status}."));
            }
        }
        let bytes = (*data).to_vec();
        if bytes.len() > limit {
            return Err("The server sent more data than expected.".to_string());
        }
        Ok(bytes)
    }
}

fn check_for_updates(manual: bool) {
    if !manual && update_check_disabled() {
        return;
    }
    if UPDATE_BUSY.replace(true) {
        if manual {
            show_alert(
                "Check for Updates",
                "An update check is already in progress.",
            );
        }
        return;
    }

    fetch_update_bytes(
        update::RELEASES_API,
        update::RELEASE_JSON_LIMIT,
        move |result| {
            UPDATE_BUSY.set(false);
            let release =
                result.and_then(|bytes| update::latest_release(&bytes, env!("CARGO_PKG_VERSION")));
            match release {
                Ok(Some(available)) => offer_update(available),
                Ok(None) => {
                    if manual {
                        show_alert(
                            "Check for Updates",
                            &format!(
                                "You're up to date. AntiRustPaste {} is the latest version.",
                                env!("CARGO_PKG_VERSION")
                            ),
                        );
                    }
                }
                Err(err) => {
                    if manual {
                        show_alert("Could not check for updates", &err);
                    } else {
                        eprintln!("antirustpaste: update check failed: {err}");
                    }
                }
            }
        },
    );
}

fn offer_update(available: update::AvailableUpdate) {
    let bundle = update_bundle();
    let can_install = bundle.is_some();
    let mtm = main_thread();
    activate_app();
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(&format!(
        "AntiRustPaste {} is available",
        available.version
    )));
    alert.setInformativeText(&NSString::from_str(&format!(
        "You have version {}.{}",
        env!("CARGO_PKG_VERSION"),
        if can_install {
            " Download and install the update now?\n\nAntiRustPaste restarts to finish the update."
        } else {
            " This build cannot update itself; open the release page to download the new version."
        }
    )));
    alert.addButtonWithTitle(&NSString::from_str(if can_install {
        "Update Now"
    } else {
        "Open Release Page"
    }));
    alert.addButtonWithTitle(&NSString::from_str("Later"));
    if alert.runModal() != NSAlertFirstButtonReturn {
        return;
    }

    if can_install {
        download_update(available);
    } else {
        open_release_page(&available.page_url);
    }
}

fn open_release_page(url: &str) {
    let opened = NSURL::URLWithString(&NSString::from_str(url))
        .is_some_and(|url| NSWorkspace::sharedWorkspace().openURL(&url));
    if !opened {
        show_alert("Could not open the release page", url);
    }
}

fn download_update(available: update::AvailableUpdate) {
    if UPDATE_BUSY.replace(true) {
        return;
    }
    let Some(sums) = available.sums.clone() else {
        fetch_update_zip(available, None);
        return;
    };
    let for_zip = available.clone();
    fetch_update_bytes(&sums.url, update::CHECKSUMS_LIMIT, move |result| {
        let sums = result.and_then(|bytes| {
            String::from_utf8(bytes).map_err(|_| "The checksum file is not text.".to_string())
        });
        match sums {
            Ok(sums) => fetch_update_zip(for_zip.clone(), Some(sums)),
            Err(err) => finish_update_error(&err),
        }
    });
}

fn finish_update_error(message: &str) {
    UPDATE_BUSY.set(false);
    show_alert("Could not install the update", message);
}

fn fetch_update_zip(available: update::AvailableUpdate, sums: Option<String>) {
    let zip = available.zip.clone();
    let version = available.version;
    let zip_name = zip.name.clone();
    fetch_update_bytes(&zip.url, update::ZIP_LIMIT, move |result| {
        let bytes = match result {
            Ok(bytes) => bytes,
            Err(err) => {
                finish_update_error(&err);
                return;
            }
        };
        let work = match stage_update(&bytes, &zip_name, version, sums.as_deref()) {
            Ok(work) => work,
            Err(err) => {
                finish_update_error(&err);
                return;
            }
        };
        UPDATE_BUSY.set(false);
        prompt_restart(version, work);
    });
}

/// Write, verify and unpack the update. Returns the work directory that holds
/// `extract/AntiRustPaste.app`.
fn stage_update(
    zip: &[u8],
    zip_name: &str,
    version: update::Version,
    sums: Option<&str>,
) -> Result<PathBuf, String> {
    let work = std::env::temp_dir().join(format!(
        "antirustpaste-update-{}-{}-{}",
        std::process::id(),
        now_ms(),
        UPDATE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&work)
        .map_err(|err| format!("Could not create a temporary folder: {err}"))?;
    match stage_update_in(&work, zip, zip_name, version, sums) {
        Ok(()) => Ok(work),
        Err(err) => {
            let _ = fs::remove_dir_all(&work);
            Err(err)
        }
    }
}

fn stage_update_in(
    work: &Path,
    zip: &[u8],
    zip_name: &str,
    version: update::Version,
    sums: Option<&str>,
) -> Result<(), String> {
    let zip_path = work.join("update.zip");
    fs::write(&zip_path, zip)
        .map_err(|err| format!("Could not save the downloaded update: {err}"))?;

    if let Some(sums) = sums {
        let expected = update::checksum_for(sums, zip_name)
            .ok_or_else(|| format!("The checksum file has no entry for {zip_name}."))?;
        let actual = sha256_of(&zip_path)?;
        if actual != expected {
            return Err("The downloaded update failed its checksum check.".to_string());
        }
    }

    let extract = work.join("extract");
    fs::create_dir_all(&extract)
        .map_err(|err| format!("Could not create a temporary folder: {err}"))?;
    let status = Command::new("/usr/bin/ditto")
        .args(["-x", "-k"])
        .arg(&zip_path)
        .arg(&extract)
        .status()
        .map_err(|err| format!("Could not unpack the update: {err}"))?;
    if !status.success() {
        return Err("The downloaded update could not be unpacked.".to_string());
    }

    let staged = extract.join("AntiRustPaste.app");
    if !staged.join("Contents/MacOS/AntiRustPaste").is_file() {
        return Err("The downloaded archive does not contain AntiRustPaste.app.".to_string());
    }
    let info = staged.join("Contents/Info.plist");
    if plist_value(&info, "CFBundleIdentifier").as_deref()
        != Some(update::EXPECTED_BUNDLE_IDENTIFIER)
    {
        return Err("The downloaded app has a different bundle identifier.".to_string());
    }
    let staged_version = version.to_string();
    if plist_value(&info, "CFBundleShortVersionString").as_deref() != Some(staged_version.as_str())
    {
        return Err("The downloaded app has a different version.".to_string());
    }
    Ok(())
}

fn sha256_of(path: &Path) -> Result<String, String> {
    let output = Command::new("/usr/bin/shasum")
        .arg("-a")
        .arg("256")
        .arg(path)
        .output()
        .map_err(|err| format!("Could not run shasum: {err}"))?;
    if !output.status.success() {
        return Err("Could not read the downloaded update.".to_string());
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| "shasum printed unreadable text.".to_string())?;
    let hash = text.split_whitespace().next().unwrap_or_default();
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("shasum printed an unexpected result.".to_string());
    }
    Ok(hash.to_ascii_lowercase())
}

fn plist_value(plist: &Path, key: &str) -> Option<String> {
    let output = Command::new("/usr/libexec/PlistBuddy")
        .arg("-c")
        .arg(format!("Print :{key}"))
        .arg(plist)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

fn prompt_restart(version: update::Version, work: PathBuf) {
    let staged = work.join("extract").join("AntiRustPaste.app");
    let Some(target) = update_bundle() else {
        let _ = fs::remove_dir_all(&work);
        show_alert(
            "Could not install the update",
            "AntiRustPaste is not running from an app bundle.",
        );
        return;
    };
    if !can_replace_bundle(&target) {
        let _ = fs::remove_dir_all(&work);
        show_alert(
            "Could not install the update",
            "AntiRustPaste.app is not writable. Move it to Applications or install the new version manually.",
        );
        return;
    }

    let mtm = main_thread();
    activate_app();
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(&format!(
        "AntiRustPaste {version} is ready"
    )));
    alert.setInformativeText(&NSString::from_str(
        "Restart to finish installing the update. Clipboard history and settings are kept.",
    ));
    alert.addButtonWithTitle(&NSString::from_str("Restart Now"));
    alert.addButtonWithTitle(&NSString::from_str("Later"));
    if alert.runModal() != NSAlertFirstButtonReturn {
        let _ = fs::remove_dir_all(&work);
        return;
    }

    if let Err(err) = launch_installer(&work, &staged, &target) {
        let _ = fs::remove_dir_all(&work);
        show_alert("Could not install the update", &err);
        return;
    }
    NSApplication::sharedApplication(mtm).terminate(None);
}

fn can_replace_bundle(bundle: &Path) -> bool {
    let Some(parent) = bundle.parent() else {
        return false;
    };
    let probe = parent.join(format!(
        ".antirustpaste-update-probe-{}",
        std::process::id()
    ));
    match fs::write(&probe, b"probe") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

fn launch_installer(work: &Path, staged: &Path, target: &Path) -> Result<(), String> {
    let script = work.join("install.sh");
    fs::write(&script, update::INSTALL_SCRIPT)
        .map_err(|err| format!("Could not write the install script: {err}"))?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
        .map_err(|err| format!("Could not prepare the install script: {err}"))?;

    Command::new("/bin/sh")
        .arg(&script)
        .arg(std::process::id().to_string())
        .arg(target)
        .arg(staged)
        .arg(work)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("Could not start the installer: {err}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "antirustpaste-app-test-{}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn write_pdf_file_replaces_the_destination() {
        let dir = scratch("pdf-write");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch directory");
        let target = dir.join("out.pdf");
        fs::write(&target, b"old").expect("seed destination");

        let written = write_pdf_file(&target, b"%PDF-1.7 test").expect("write pdf");
        assert_eq!(written, target);
        assert_eq!(fs::read(&target).expect("read pdf"), b"%PDF-1.7 test");
        assert!(!dir.join("out.pdf.tmp").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn list_layout_without_pinned_rows_has_no_headers() {
        let layout = ListLayout::new(0, 3);
        assert_eq!(layout.row_top(0), 0.0);
        assert_eq!(layout.row_top(2), 2.0 * ROW_H);
        assert_eq!(layout.content_height(), 3.0 * ROW_H + 4.0);
        assert!(layout.pinned_header_y().is_none());
        assert!(layout.recent_header_y().is_none());
        assert!(layout.pinned_card().is_none());
        assert_eq!(layout.row_at(ROW_H + 1.0), Some(1));
        assert_eq!(layout.row_at(-1.0), None);
        assert_eq!(layout.visible_range(ROW_H, ROW_H * 2.0), (1, 2));
    }

    #[test]
    fn list_layout_separates_pinned_and_recent_groups() {
        let layout = ListLayout::new(2, 5);
        assert_eq!(layout.pinned_header_y(), Some(0.0));
        assert_eq!(layout.row_top(0), LIST_HEADER_H);
        assert_eq!(layout.row_top(1), LIST_HEADER_H + ROW_H);

        let recent_header = LIST_HEADER_H + 2.0 * ROW_H;
        assert_eq!(layout.recent_header_y(), Some(recent_header));
        assert_eq!(layout.row_top(2), recent_header + LIST_HEADER_H);
        assert_eq!(layout.row_at(recent_header + 2.0), None);
        assert_eq!(layout.row_at(LIST_HEADER_H + 5.0), Some(0));
        assert_eq!(layout.visible_range(0.0, LIST_HEADER_H), (0, 0));
        assert_eq!(
            layout.visible_range(recent_header, recent_header + LIST_HEADER_H),
            (2, 2)
        );
    }

    #[test]
    fn list_layout_with_only_pinned_rows_has_one_header() {
        let layout = ListLayout::new(2, 2);
        assert_eq!(layout.pinned_header_y(), Some(0.0));
        assert!(layout.recent_header_y().is_none());
        assert_eq!(layout.row_top(1), LIST_HEADER_H + ROW_H);
        assert_eq!(layout.content_height(), LIST_HEADER_H + 2.0 * ROW_H + 4.0);
        assert_eq!(layout.row_at(2.0), None);
    }

    #[test]
    fn moved_order_shifts_rows_in_both_directions() {
        let order = ids(&["a", "b", "c", "d"]);
        assert_eq!(moved_order(&order, 0, 2), Some(ids(&["b", "c", "a", "d"])));
        assert_eq!(moved_order(&order, 3, 1), Some(ids(&["a", "d", "b", "c"])));
        assert_eq!(moved_order(&order, 2, 2), Some(order.clone()));
        assert_eq!(moved_order(&order, 4, 0), None);
        assert_eq!(moved_order(&order, 0, 4), None);
    }

    #[test]
    fn pdf_order_keeps_dragged_order_and_appends_new_images() {
        let mut order = ids(&["b", "a"]);
        assert!(pdf_sync_order(&mut order, &ids(&["a", "b", "c"])));
        assert_eq!(order, ids(&["b", "a", "c"]));
        assert!(!pdf_sync_order(&mut order, &ids(&["a", "b", "c"])));
        assert!(pdf_sync_order(&mut order, &ids(&["b", "c"])));
        assert_eq!(order, ids(&["b", "c"]));
    }

    #[test]
    fn write_pdf_file_reports_a_missing_directory() {
        let dir = scratch("pdf-missing");
        let _ = fs::remove_dir_all(&dir);
        let target = dir.join("nested/out.pdf");

        assert!(write_pdf_file(&target, b"%PDF").is_err());
        assert!(!target.exists());
    }

    fn fake_app(dir: &Path, version: &str, identifier: &str) -> PathBuf {
        let app = dir.join("AntiRustPaste.app");
        fs::create_dir_all(app.join("Contents/MacOS")).expect("app directories");
        fs::write(app.join("Contents/MacOS/AntiRustPaste"), b"fake").expect("executable");
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <plist version=\"1.0\"><dict>\
             <key>CFBundleIdentifier</key><string>{identifier}</string>\
             <key>CFBundleShortVersionString</key><string>{version}</string>\
             </dict></plist>\n"
        );
        fs::write(app.join("Contents/Info.plist"), plist).expect("Info.plist");
        app
    }

    fn zipped_app(dir: &Path, app: &Path) -> Vec<u8> {
        let zip = dir.join("update.zip");
        let status = Command::new("/usr/bin/ditto")
            .args(["-c", "-k", "--keepParent"])
            .arg(app)
            .arg(&zip)
            .status()
            .expect("run ditto");
        assert!(status.success());
        fs::read(&zip).expect("zip bytes")
    }

    #[test]
    fn staged_update_is_verified_and_unpacked() {
        let dir = scratch("update-stage");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch directory");
        let app = fake_app(&dir, "0.9.0", update::EXPECTED_BUNDLE_IDENTIFIER);
        let bytes = zipped_app(&dir, &app);

        let zip_path = dir.join("update.zip");
        let hash = sha256_of(&zip_path).expect("sha256");
        let sums = format!("{hash}  ./AntiRustPaste-0.9.0-universal.zip\n");
        let version = update::Version::parse("0.9.0").expect("version");

        let work = stage_update(
            &bytes,
            "AntiRustPaste-0.9.0-universal.zip",
            version,
            Some(&sums),
        )
        .expect("stage update");
        assert!(
            work.join("extract/AntiRustPaste.app/Contents/MacOS/AntiRustPaste")
                .is_file()
        );

        let _ = fs::remove_dir_all(&work);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn staged_update_rejects_a_bad_checksum_or_version() {
        let dir = scratch("update-reject");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch directory");
        let app = fake_app(&dir, "0.8.0", update::EXPECTED_BUNDLE_IDENTIFIER);
        let bytes = zipped_app(&dir, &app);
        let version = update::Version::parse("0.9.0").expect("version");
        let zeros = "0".repeat(64);
        let sums = format!("{zeros}  ./AntiRustPaste-0.9.0-universal.zip\n");

        assert!(
            stage_update(
                &bytes,
                "AntiRustPaste-0.9.0-universal.zip",
                version,
                Some(&sums)
            )
            .is_err()
        );
        assert!(stage_update(&bytes, "AntiRustPaste-0.9.0-universal.zip", version, None).is_err());

        let _ = fs::remove_dir_all(&dir);
    }
}
