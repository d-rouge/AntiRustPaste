//! Application state, menu bar item, clipboard polling and all user actions.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::ptr::{NonNull, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

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
    ClearScope, Clip, ClipKind, DEFAULT_PINNED_BACKGROUND, DEFAULT_PINNED_INDICATOR, EditTextError,
    IgnoredApp, MAX_EDIT_TEXT_BYTES, MAX_LIMIT, Section, Settings, Shell, ShellFile, Store,
    ThumbnailService, filter_indices as core_filter_indices, human_size, load_cursors, now_ms,
    read_new, save_cursors,
};
use antirustpaste_pdf::PdfImage;

use crate::settings::{self, apply_start_at_login};
use crate::thumbnails;
use crate::update;
use crate::views::{self, ClipListView, ClipPanel, PanelDelegate, PdfExportParts, PrefsParts};

mod control;

pub(crate) const PANEL_W: f64 = 460.0;
pub(crate) const PANEL_H: f64 = 480.0;
pub(crate) const ROW_H: f64 = 54.0;
pub(crate) const LIST_HEADER_H: f64 = 26.0;
const LIST_BOTTOM_PAD: f64 = 4.0;
pub(crate) const LIST_BOTTOM: f64 = 32.0;
pub(crate) const SEARCH_H: f64 = 40.0;
/// Dwell time before the hover callout appears.
const HOVER_PREVIEW_DELAY: f64 = 0.6;

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
const MAX_APP_ICONS: usize = 64;
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

/// A temporary text preview that Quick Look may hand to TextEdit. The app polls
/// the file and imports saved changes back into the clip.
pub(crate) struct WatchedTextPreview {
    pub path: PathBuf,
    pub clip_id: String,
    pub modified: Option<SystemTime>,
    pub len: u64,
}

/// Bound on simultaneously watched text previews. Older files are removed when
/// the list grows past this, so an editing session cannot fill `/tmp`.
const MAX_WATCHED_TEXT_PREVIEWS: usize = 8;

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
    pub clear_all_button: Retained<NSButton>,
    pub clear_unpinned_button: Retained<NSButton>,
    pub export_pdf_button: Retained<NSButton>,
    pub refresh_commands_button: Retained<NSButton>,
    /// Status-menu item that pauses and resumes capture; kept in sync with the
    /// Preferences checkbox.
    pub recording_menu_item: Retained<NSMenuItem>,
    /// SF Symbol pencil for the Edited badge, in normal and emphasised states.
    pub title_symbol: Option<Retained<NSImage>>,
    pub title_symbol_selected: Option<Retained<NSImage>>,
    /// Floating callout shown while the pointer rests on a row.
    pub hover_preview: Option<views::HoverPreviewParts>,
    pub hover_preview_target: Option<String>,
    pub hover_preview_token: u64,
    /// Source-app icons keyed by bundle identifier; `None` means lookup failed.
    pub app_icons: HashMap<String, Option<Retained<NSImage>>>,
    pub section: ClipboardSection,
    pub thumbnails: HashMap<u64, Retained<NSImage>>,
    pub thumbnail_failed: HashSet<u64>,
    pub thumbnail_service: ThumbnailService,
    /// NSWindow's delegate is a weak reference; keep ours alive here.
    #[allow(dead_code)]
    pub panel_delegate: Retained<PanelDelegate>,
    pub prefs: Option<PrefsParts>,
    pub pdf_export: Option<PdfExportState>,
    /// Local JSON control socket used by the MCP bridge.
    pub control: Option<control::ControlServer>,
    /// File URL handed to the shared Quick Look panel; `None` when it is closed.
    pub preview_item: Option<Retained<NSURL>>,
    /// Temporary preview file written for Quick Look; deleted when it closes,
    /// unless it is a watched text file that an external editor may still hold.
    pub preview_file: Option<PathBuf>,
    /// Text previews whose temporary file is watched for edits made in TextEdit.
    pub text_previews: Vec<WatchedTextPreview>,
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
    /// True while a row context menu is being tracked. The panel must not hide
    /// on a temporary key-window change caused by the menu.
    pub context_menu_open: bool,
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

fn set_section(section: ClipboardSection) {
    hide_hover_preview();
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
    let (clear_all_count, clear_unpinned_count) = match s.section {
        ClipboardSection::Text => (
            s.store.count_scope(ClearScope::Text),
            s.store.count_scope(ClearScope::UnpinnedText),
        ),
        ClipboardSection::Images => (
            s.store.count_scope(ClearScope::Images),
            s.store.count_scope(ClearScope::UnpinnedImages),
        ),
        ClipboardSection::Commands => (0, 0),
    };
    let clear_hidden = s.section == ClipboardSection::Commands;
    s.clear_all_button.setHidden(clear_hidden);
    s.clear_unpinned_button.setHidden(clear_hidden);
    s.clear_all_button.setEnabled(clear_all_count > 0);
    s.clear_unpinned_button.setEnabled(clear_unpinned_count > 0);
}

fn refresh_ui() {
    let stale_preview = with_state(|s| {
        s.hover_preview_target
            .as_deref()
            .is_some_and(|id| !s.store.clips.iter().any(|clip| clip.id == id))
    });
    if stale_preview {
        hide_hover_preview();
    }
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
    update_history_controls();
    if with_state(|s| {
        s.prefs
            .as_ref()
            .is_some_and(|prefs| prefs.window.isVisible())
    }) {
        update_history_storage();
    }
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
                let control = with_state(|s| s.control.take());
                drop(control);
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

        #[unsafe(method(toggleRecording:))]
        fn toggle_recording(&self, _sender: &AnyObject) {
            guard("toggle_recording", toggle_recording_setting);
        }

        #[unsafe(method(clearCurrentSection:))]
        fn clear_current_section(&self, _sender: &AnyObject) {
            guard("clear_current_section", || {
                clear_current_scope(false);
            });
        }

        #[unsafe(method(clearCurrentUnpinned:))]
        fn clear_current_unpinned(&self, _sender: &AnyObject) {
            guard("clear_current_unpinned", || {
                clear_current_scope(true);
            });
        }

        #[unsafe(method(deleteAllClippings:))]
        fn delete_all_clippings(&self, _sender: &AnyObject) {
            guard("delete_all_clippings", || {
                clear_scope(ClearScope::Everything);
            });
        }

        #[unsafe(method(deleteAllImages:))]
        fn delete_all_images(&self, _sender: &AnyObject) {
            guard("delete_all_images", || {
                clear_scope(ClearScope::Images);
            });
        }

        #[unsafe(method(deleteAllText:))]
        fn delete_all_text(&self, _sender: &AnyObject) {
            guard("delete_all_text", || {
                clear_scope(ClearScope::Text);
            });
        }

        #[unsafe(method(deleteUnpinnedText:))]
        fn delete_unpinned_text(&self, _sender: &AnyObject) {
            guard("delete_unpinned_text", || {
                clear_scope(ClearScope::UnpinnedText);
            });
        }

        #[unsafe(method(deleteUnpinnedImages:))]
        fn delete_unpinned_images(&self, _sender: &AnyObject) {
            guard("delete_unpinned_images", || {
                clear_scope(ClearScope::UnpinnedImages);
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
                set_recording(on);
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

        #[unsafe(method(pinnedBackgroundChanged:))]
        fn pinned_background_changed(&self, sender: &NSColorWell) {
            guard("pinned_background_changed", || {
                set_pinned_color(sender, true);
            });
        }

        #[unsafe(method(pinnedIndicatorChanged:))]
        fn pinned_indicator_changed(&self, sender: &NSColorWell) {
            guard("pinned_indicator_changed", || {
                set_pinned_color(sender, false);
            });
        }

        #[unsafe(method(restorePinnedColors:))]
        fn restore_pinned_colors(&self, _sender: &AnyObject) {
            guard("restore_pinned_colors", || {
                reset_pinned_colors();
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

        #[unsafe(method(rowMenuPin:))]
        fn row_menu_pin(&self, sender: &NSMenuItem) {
            guard("row_menu_pin", || {
                if let Some(id) = menu_clip_id(sender) {
                    toggle_pin_by_id(&id);
                }
            });
        }

        #[unsafe(method(rowMenuCopy:))]
        fn row_menu_copy(&self, sender: &NSMenuItem) {
            guard("row_menu_copy", || {
                if let Some(id) = menu_clip_id(sender) {
                    paste_clip_by_id(&id, false);
                }
            });
        }

        #[unsafe(method(rowMenuCopyPlain:))]
        fn row_menu_copy_plain(&self, sender: &NSMenuItem) {
            guard("row_menu_copy_plain", || {
                if let Some(id) = menu_clip_id(sender) {
                    copy_clip_plain_by_id(&id);
                }
            });
        }

        #[unsafe(method(rowMenuPaste:))]
        fn row_menu_paste(&self, sender: &NSMenuItem) {
            guard("row_menu_paste", || {
                if let Some(id) = menu_clip_id(sender) {
                    paste_clip_by_id(&id, auto_paste());
                }
            });
        }

        #[unsafe(method(rowMenuEdit:))]
        fn row_menu_edit(&self, sender: &NSMenuItem) {
            guard("row_menu_edit", || {
                if let Some(id) = menu_clip_id(sender) {
                    edit_clip(&id);
                }
            });
        }

        #[unsafe(method(rowMenuRename:))]
        fn row_menu_rename(&self, sender: &NSMenuItem) {
            guard("row_menu_rename", || {
                if let Some(id) = menu_clip_id(sender) {
                    rename_clip(&id);
                }
            });
        }

        #[unsafe(method(rowMenuDelete:))]
        fn row_menu_delete(&self, sender: &NSMenuItem) {
            guard("row_menu_delete", || {
                if let Some(id) = menu_clip_id(sender) {
                    delete_clip(&id);
                }
            });
        }

        #[unsafe(method(rowMenuQuickLook:))]
        fn row_menu_quick_look(&self, sender: &NSMenuItem) {
            guard("row_menu_quick_look", || {
                if let Some(id) = menu_clip_id(sender) {
                    toggle_preview_for_clip(&id);
                }
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
                    close_quick_look();
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

    let (menu, recording_menu_item) = build_status_menu(mtm);
    let parts = views::build_panel(mtm, &delegate_ref());
    let panel_delegate = PanelDelegate::create(mtm);
    parts
        .panel
        .setDelegate(Some(ProtocolObject::from_ref(&*panel_delegate)));
    let control = control::ControlServer::start();

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
        clear_all_button: parts.clear_all_button,
        clear_unpinned_button: parts.clear_unpinned_button,
        export_pdf_button: parts.export_pdf_button,
        refresh_commands_button: parts.refresh_commands_button,
        recording_menu_item,
        title_symbol: parts.title_symbol,
        title_symbol_selected: parts.title_symbol_selected,
        hover_preview: None,
        hover_preview_target: None,
        hover_preview_token: 0,
        app_icons: HashMap::new(),
        section: ClipboardSection::Text,
        thumbnails: HashMap::new(),
        thumbnail_failed: HashSet::new(),
        thumbnail_service: thumbnails::new_service(),
        panel_delegate,
        prefs: None,
        pdf_export: None,
        control,
        preview_item: None,
        preview_file: None,
        text_previews: Vec::new(),
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
        context_menu_open: false,
        accessibility_asked: false,
    };
    STATE.with(|slot| *slot.borrow_mut() = Some(state));

    sync_recording_ui();
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

fn recording_menu_title(recording: bool) -> &'static str {
    if recording {
        "Pause Recording"
    } else {
        "Resume Recording"
    }
}

fn build_status_menu(mtm: MainThreadMarker) -> (Retained<NSMenu>, Retained<NSMenuItem>) {
    let target = delegate_any();
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);

    let recording_item = menu_item(
        recording_menu_title(true),
        sel!(toggleRecording:),
        &target,
        mtm,
    );
    // No checkmark: the title already says whether clicking will pause or
    // resume, and a check next to "Pause Recording" reads as if pause were on.
    recording_item.setState(NSControlStateValueOff);
    menu.addItem(&recording_item);
    menu.addItem(&NSMenuItem::separatorItem(mtm));

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

    (menu, recording_item)
}

/// Update the status-menu item and the Preferences checkbox from the single
/// stored recording flag. The state borrow is dropped before any AppKit call.
fn sync_recording_ui() {
    let (recording, menu_item, record_box) = with_state(|s| {
        (
            s.settings.record,
            s.recording_menu_item.clone(),
            s.prefs.as_ref().map(|prefs| prefs.record_box.clone()),
        )
    });
    menu_item.setTitle(&NSString::from_str(recording_menu_title(recording)));
    if let Some(record_box) = record_box {
        let value = if recording {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        };
        record_box.setState(value);
    }
}

fn set_recording(recording: bool) {
    with_state(|s| {
        s.settings.record = recording;
        settings::save(&s.settings);
    });
    if recording {
        // Resume from the pasteboard as it is now: a change made while paused
        // must not be captured on the next poll.
        let count = pasteboard::general().changeCount();
        with_state(|s| s.last_change_count = count);
    }
    sync_recording_ui();
}

fn toggle_recording_setting() {
    let recording = with_state(|s| s.settings.record);
    set_recording(!recording);
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
        guard("control_poll", control::poll);
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
    poll_edited_text_previews();
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
    if preview_is_open() || with_state(|s| s.context_menu_open) {
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
    hide_hover_preview();
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
    hide_hover_preview();
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
    hide_hover_preview();
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

pub(crate) fn handle_list_right_click(view: &ClipListView, event: &NSEvent) {
    hide_hover_preview();
    let point = view.convertPoint_fromView(event.locationInWindow(), None);
    let Some(clip) = with_state(|s| {
        let indices = filtered_indices(s);
        let layout = list_layout(s, &indices);
        let row = layout.row_at(point.y + s.scroll)?;
        let index = *indices.get(row)?;
        s.store.clips.get(index).cloned()
    }) else {
        return;
    };

    let mtm = main_thread();
    let target = delegate_any();
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);

    let pin_title = if clip.pinned { "Unpin" } else { "Pin" };
    let pin_item = context_menu_item(pin_title, sel!(rowMenuPin:), &clip.id, &target, mtm);
    set_menu_shortcut(&pin_item, "p", NSEventModifierFlags::Command);
    menu.addItem(&pin_item);
    menu.addItem(&NSMenuItem::separatorItem(mtm));

    let copy_item = context_menu_item("Copy", sel!(rowMenuCopy:), &clip.id, &target, mtm);
    set_menu_shortcut(&copy_item, "c", NSEventModifierFlags::Command);
    menu.addItem(&copy_item);

    // Only rich clips carry a plain-text fallback, so the item would be
    // meaningless for text, file, image and command rows.
    if clip.plain_text().is_some() {
        let plain_item = context_menu_item(
            "Copy as Plain Text",
            sel!(rowMenuCopyPlain:),
            &clip.id,
            &target,
            mtm,
        );
        menu.addItem(&plain_item);
    }

    // "Paste" only makes sense when choosing a row also synthesises ⌘V. With
    // the preference off it would do exactly what "Copy" does.
    if auto_paste() {
        let paste_item = context_menu_item("Paste", sel!(rowMenuPaste:), &clip.id, &target, mtm);
        set_menu_shortcut(&paste_item, "\r", NSEventModifierFlags::empty());
        menu.addItem(&paste_item);
    }

    let edit_item = context_menu_item("Edit Text…", sel!(rowMenuEdit:), &clip.id, &target, mtm);
    set_menu_shortcut(
        &edit_item,
        "e",
        NSEventModifierFlags::Command | NSEventModifierFlags::Shift,
    );
    edit_item.setEnabled(can_edit_text(&clip));
    menu.addItem(&edit_item);

    let rename_item = context_menu_item("Set Title…", sel!(rowMenuRename:), &clip.id, &target, mtm);
    set_menu_shortcut(
        &rename_item,
        "r",
        NSEventModifierFlags::Command | NSEventModifierFlags::Shift,
    );
    rename_item.setEnabled(clip.kind != ClipKind::Command);
    menu.addItem(&rename_item);

    menu.addItem(&NSMenuItem::separatorItem(mtm));

    let preview_item = context_menu_item(
        "Quick Look",
        sel!(rowMenuQuickLook:),
        &clip.id,
        &target,
        mtm,
    );
    set_menu_shortcut(&preview_item, "y", NSEventModifierFlags::Command);
    let previewable = with_state(|s| can_preview_clip(&clip, &s.store));
    preview_item.setEnabled(previewable);
    menu.addItem(&preview_item);

    let delete_item = context_menu_item("Delete", sel!(rowMenuDelete:), &clip.id, &target, mtm);
    set_menu_shortcut(&delete_item, "\u{8}", NSEventModifierFlags::empty());
    menu.addItem(&delete_item);

    // Menu tracking runs a nested run loop; the state borrow is released before
    // the menu is shown and every action looks the clip up by id again.
    with_state(|s| s.context_menu_open = true);
    NSMenu::popUpContextMenu_withEvent_forView(&menu, event, view);
    with_state(|s| s.context_menu_open = false);
    restore_panel_focus_after_menu();
}

fn context_menu_item(
    title: &str,
    action: Sel,
    clip_id: &str,
    target: &AnyObject,
    mtm: MainThreadMarker,
) -> Retained<NSMenuItem> {
    let item = menu_item(title, action, target, mtm);
    let id = NSString::from_str(clip_id);
    unsafe { item.setRepresentedObject(Some(as_any(&*id))) };
    item
}

fn set_menu_shortcut(item: &NSMenuItem, key: &str, mask: NSEventModifierFlags) {
    item.setKeyEquivalent(&NSString::from_str(key));
    item.setKeyEquivalentModifierMask(mask);
}

fn menu_clip_id(sender: &NSMenuItem) -> Option<String> {
    sender
        .representedObject()?
        .downcast::<NSString>()
        .ok()
        .map(|id| id.to_string())
}

fn restore_panel_focus_after_menu() {
    // Quick Look owns the keyboard while it is open; `finish_closed_preview`
    // gives the popup focus back when the preview closes.
    if preview_is_open() {
        return;
    }
    let mtm = main_thread();
    let active = NSApplication::sharedApplication(mtm).isActive();
    let (visible, panel, search) =
        with_state(|s| (s.panel.isVisible(), s.panel.clone(), s.search.clone()));
    if !visible || !active {
        return;
    }
    with_state(|s| s.hide_token = s.hide_token.wrapping_add(1));
    panel.makeKeyAndOrderFront(None);
    panel.makeFirstResponder(Some(&search));
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

fn toggle_pin_by_id(id: &str) {
    let index = with_state(|s| s.store.clips.iter().position(|clip| clip.id == id));
    let Some(index) = index else {
        NSBeep();
        return;
    };
    toggle_pin_store(index);
}

fn toggle_pin_at(filtered_index: usize) {
    let store_index = with_state(|s| filtered_indices(s).get(filtered_index).copied());
    let Some(store_index) = store_index else {
        NSBeep();
        return;
    };
    toggle_pin_store(store_index);
}

fn toggle_pin_store(store_index: usize) {
    hide_hover_preview();
    let pinned = with_state(|s| s.store.clips.get(store_index).map(|clip| clip.pinned));
    let Some(pinned) = pinned else {
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

fn paste_clip_by_id(id: &str, simulate: bool) {
    let index = with_state(|s| s.store.clips.iter().position(|clip| clip.id == id));
    let Some(index) = index else {
        NSBeep();
        return;
    };
    paste_store_index(index, simulate);
}

// ---------------------------------------------------------------------------
// Hover preview
// ---------------------------------------------------------------------------

/// Called by the list view when the row under the pointer changes. `None`
/// hides the callout immediately; a new clip opens after a short dwell so
/// sweeping across rows does not flash a preview for every one.
pub(crate) fn hover_preview_changed(id: Option<String>) {
    let (shown, panel) = with_state(|s| {
        (
            s.hover_preview_target.clone(),
            s.hover_preview.as_ref().map(|parts| parts.panel.clone()),
        )
    });
    let visible = panel.as_ref().is_some_and(|panel| panel.isVisible());
    if shown == id && visible {
        return;
    }
    if shown != id {
        hide_hover_preview();
    }
    let Some(id) = id else {
        return;
    };

    let needs_build = with_state(|s| s.hover_preview.is_none());
    if needs_build {
        let parts = views::build_hover_preview(main_thread());
        with_state(|s| s.hover_preview = Some(parts));
    }

    let token = with_state(|s| {
        s.hover_preview_token = s.hover_preview_token.wrapping_add(1);
        s.hover_preview_target = Some(id.clone());
        s.hover_preview_token
    });
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        guard("hover_preview", || {
            show_hover_preview_if_current(&id, token)
        });
    });
    unsafe {
        NSTimer::scheduledTimerWithTimeInterval_repeats_block(HOVER_PREVIEW_DELAY, false, &block);
    }
}

fn hide_hover_preview() {
    let panel = with_state(|s| {
        s.hover_preview_token = s.hover_preview_token.wrapping_add(1);
        s.hover_preview_target = None;
        s.hover_preview.as_ref().map(|parts| parts.panel.clone())
    });
    if let Some(panel) = panel {
        panel.orderOut(None);
    }
}

fn show_hover_preview_if_current(id: &str, token: u64) {
    let Some((clip, parts)) = with_state(|s| {
        if s.hover_preview_token != token || s.hover_preview_target.as_deref() != Some(id) {
            return None;
        }
        let clip = s.store.clips.iter().find(|clip| clip.id == id)?.clone();
        let parts = s.hover_preview.as_ref()?.clone();
        Some((clip, parts))
    }) else {
        return;
    };

    let body_text = hover_body(&clip);
    let meta_text = hover_meta(&clip);
    let icon = app_icon_for_bundle(clip.source_bundle.as_deref());
    views::present_hover_preview(
        main_thread(),
        &parts,
        body_text.as_deref(),
        &meta_text,
        icon.as_deref(),
    );
}

fn hover_body(clip: &Clip) -> Option<String> {
    if clip.kind == ClipKind::Image {
        return None;
    }
    let text = clip
        .text
        .as_deref()
        .filter(|text| !text.is_empty())
        .unwrap_or(&clip.preview);
    let mut chars = text.chars();
    let mut out: String = chars.by_ref().take(2000).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    Some(out)
}

pub(crate) fn app_icon_for_bundle(bundle_id: Option<&str>) -> Option<Retained<NSImage>> {
    let bundle = bundle_id?;
    if let Some(cached) = with_state(|s| s.app_icons.get(bundle).cloned()) {
        return cached;
    }
    let workspace = NSWorkspace::sharedWorkspace();
    let icon = workspace
        .URLForApplicationWithBundleIdentifier(&NSString::from_str(bundle))
        .and_then(|url| url.path())
        .map(|path| workspace.iconForFile(&path));
    with_state(|s| {
        if s.app_icons.len() >= MAX_APP_ICONS {
            s.app_icons.clear();
        }
        s.app_icons.insert(bundle.to_string(), icon.clone());
    });
    icon
}

fn hover_meta(clip: &Clip) -> String {
    let source = clip
        .source_name
        .as_deref()
        .filter(|source| !source.is_empty())
        .unwrap_or("Unknown app");
    let kind = match clip.kind {
        ClipKind::Text => "Plain Text",
        ClipKind::RichText => "Rich Text",
        ClipKind::Html => "HTML",
        ClipKind::Image => "Image",
        ClipKind::File => "File",
        ClipKind::Command => "Command",
        ClipKind::Other => "Clipboard item",
    };
    let size = if clip.kind == ClipKind::Image {
        match (clip.width, clip.height) {
            (Some(width), Some(height)) => {
                format!("{width}×{height} · {}", human_size(clip.byte_len))
            }
            _ => human_size(clip.byte_len),
        }
    } else if clip.byte_len <= 64 * 1024 {
        match clip.text.as_deref() {
            Some(text) => format!("{} chars", text.chars().count()),
            None => human_size(clip.byte_len),
        }
    } else {
        human_size(clip.byte_len)
    };
    format!("{source} · {kind} · {size}")
}

#[allow(deprecated)]
fn paste_store_index(index: usize, simulate: bool) {
    let Some(clip) = with_state(|s| s.store.clips.get(index).cloned()) else {
        return;
    };
    write_clip_to_clipboard(&clip, false, simulate);
}

fn copy_clip_plain_by_id(id: &str) {
    let clip = with_state(|s| s.store.clips.iter().find(|clip| clip.id == id).cloned());
    let Some(clip) = clip else {
        NSBeep();
        return;
    };
    write_clip_to_clipboard(&clip, true, false);
}

/// Write `clip` to the general pasteboard, hide the popup and reactivate the
/// previous app. `plain` writes only the stored plain text; `simulate` also
/// posts ⌘V once Accessibility allows it.
#[allow(deprecated)]
fn write_clip_to_pasteboard(clip: &Clip, plain: bool) -> bool {
    let pb = pasteboard::general();
    let written = if plain {
        pasteboard::write_clip_plain_text(&pb, clip)
    } else {
        with_state(|s| pasteboard::write_clip(&pb, &s.store, clip))
    };
    if written {
        let change_count = pb.changeCount();
        with_state(|s| s.last_change_count = change_count);
    }
    written
}

fn schedule_simulated_paste() {
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

#[allow(deprecated)]
fn write_clip_to_clipboard(clip: &Clip, plain: bool, simulate: bool) {
    let target = resolve_target_app();
    if !write_clip_to_pasteboard(clip, plain) {
        NSBeep();
        return;
    }
    hide_panel();

    if let Some(app) = target {
        app.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps);
    }

    if simulate {
        schedule_simulated_paste();
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
    // While the row context menu is open it owns the keyboard, including the
    // shortcut key equivalents shown in its items.
    if with_state(|s| s.context_menu_open) {
        return false;
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
            14 if flags & NS_SHIFT != 0 && flags & (NS_OPTION | NS_CONTROL) == 0 => {
                edit_selected();
                return true;
            }
            14 => {
                open_pdf_export();
                return true;
            }
            16 => {
                toggle_quick_look();
                return true;
            }
            15 if flags & NS_SHIFT != 0 && flags & (NS_OPTION | NS_CONTROL) == 0 => {
                rename_selected();
                return true;
            }
            15 if with_state(|s| s.section == ClipboardSection::Commands) => {
                refresh_shell_history();
                return true;
            }
            _ => {}
        }
    }

    // Space opens Quick Look in every tab while the search field is empty. A
    // leading space would otherwise be consumed instead of typed; the existing
    // image-tab behaviour already made that trade-off. Option/Control/Command
    // combinations keep their normal meaning (for example Option+Space).
    if key == 49
        && flags & (NS_OPTION | NS_CONTROL | NS_COMMAND) == 0
        && with_state(|s| s.query.is_empty())
    {
        toggle_quick_look();
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
// Titles and deletion
// ---------------------------------------------------------------------------

fn can_edit_text(clip: &Clip) -> bool {
    matches!(clip.kind, ClipKind::Text | ClipKind::Command)
        && clip
            .text
            .as_deref()
            .is_some_and(|text| text.len() <= MAX_EDIT_TEXT_BYTES)
}

fn edit_selected() {
    let id = with_state(|s| {
        let index = *filtered_indices(s).get(s.selected)?;
        s.store.clips.get(index).map(|clip| clip.id.clone())
    });
    let Some(id) = id else {
        NSBeep();
        return;
    };
    edit_clip(&id);
}

#[allow(deprecated)]
fn edit_clip(id: &str) {
    hide_hover_preview();
    let mtm = main_thread();
    let Some((current, kind, has_text)) = with_state(|s| {
        let clip = s.store.clips.iter().find(|clip| clip.id == id)?;
        Some((
            clip.text.clone().unwrap_or_default(),
            clip.kind,
            clip.text.is_some(),
        ))
    }) else {
        NSBeep();
        return;
    };
    if !matches!(kind, ClipKind::Text | ClipKind::Command) || !has_text {
        NSBeep();
        return;
    }
    if current.len() > MAX_EDIT_TEXT_BYTES {
        show_alert(
            "Clipping is too large to edit",
            "Only text up to 1 MiB can be edited in the app. Copy it and edit it elsewhere.",
        );
        return;
    }

    activate_app();
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Edit clipping text"));
    alert.setInformativeText(&NSString::from_str(
        "Saving updates this history entry, its preview and its size. The clipboard changes the next time you copy it. Empty text is not saved.",
    ));
    alert.addButtonWithTitle(&NSString::from_str("Save"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));

    let width = 440.0;
    let height = 240.0;
    let scroll = NSScrollView::new(mtm);
    scroll.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(width, height),
    ));
    scroll.setBorderType(NSBorderType::BezelBorder);
    scroll.setHasVerticalScroller(true);
    scroll.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);

    let text_view = NSTextView::new(mtm);
    text_view.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(width, height),
    ));
    text_view.setMinSize(NSSize::new(0.0, height));
    text_view.setMaxSize(NSSize::new(width, 1_000_000.0));
    text_view.setVerticallyResizable(true);
    text_view.setHorizontallyResizable(false);
    text_view.setRichText(false);
    text_view.setAllowsUndo(true);
    text_view.setFont(Some(&NSFont::systemFontOfSize(13.0)));
    text_view.setString(&NSString::from_str(&current));
    // SAFETY: an `NSTextView` created by `new` owns its text container for its
    // whole lifetime, so the returned reference is valid during this setup.
    if let Some(container) = unsafe { text_view.textContainer() } {
        container.setWidthTracksTextView(true);
        container.setContainerSize(NSSize::new(width, 1_000_000.0));
    }
    scroll.setDocumentView(Some(&text_view));

    alert.setAccessoryView(Some(&scroll));
    alert.window().makeFirstResponder(Some(&text_view));

    if alert.runModal() != NSAlertFirstButtonReturn {
        return;
    }

    let edited = text_view.string().to_string();
    let outcome = with_state(|s| {
        let Some(index) = s.store.clips.iter().position(|clip| clip.id == id) else {
            return Err(EditTextError::Unavailable);
        };
        s.store.set_text(index, &edited)
    });
    match outcome {
        Ok(true) => {
            with_state(|s| s.store.save());
            refresh_ui();
        }
        Ok(false) => {}
        Err(EditTextError::Empty) => show_alert(
            "Text not saved",
            "The clipping text cannot be empty or only whitespace.",
        ),
        Err(EditTextError::TooLarge) => show_alert(
            "Text not saved",
            "The text is larger than the 1 MiB editing limit.",
        ),
        Err(EditTextError::Duplicate) => show_alert(
            "Text not saved",
            "Another clipping already has this exact text.",
        ),
        Err(EditTextError::Unavailable) => NSBeep(),
    }
}

fn rename_selected() {
    let id = with_state(|s| {
        let index = *filtered_indices(s).get(s.selected)?;
        s.store.clips.get(index).map(|clip| clip.id.clone())
    });
    let Some(id) = id else {
        NSBeep();
        return;
    };
    rename_clip(&id);
}

#[allow(deprecated)]
fn rename_clip(id: &str) {
    hide_hover_preview();
    let mtm = main_thread();
    let Some((current, can_rename)) = with_state(|s| {
        let clip = s.store.clips.iter().find(|clip| clip.id == id)?;
        Some((
            clip.title.clone().unwrap_or_default(),
            clip.kind != ClipKind::Command,
        ))
    }) else {
        NSBeep();
        return;
    };
    if !can_rename {
        NSBeep();
        return;
    }

    activate_app();
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Set clipping title"));
    alert.setInformativeText(&NSString::from_str(
        "This title is shown above the copied text. Search still matches both the title and the copied text. Leave it empty to use the normal preview.",
    ));
    alert.addButtonWithTitle(&NSString::from_str("Save"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));

    let field = NSTextField::new(mtm);
    field.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(280.0, 24.0),
    ));
    field.setFont(Some(&NSFont::systemFontOfSize(13.0)));
    field.setEditable(true);
    field.setBezeled(true);
    field.setDrawsBackground(true);
    field.setStringValue(&NSString::from_str(&current));
    field.setPlaceholderString(Some(&NSString::from_str("Optional title")));
    alert.setAccessoryView(Some(&field));
    alert.window().makeFirstResponder(Some(&field));

    if alert.runModal() != NSAlertFirstButtonReturn {
        return;
    }

    let raw = field.stringValue().to_string();
    let changed = with_state(|s| {
        let Some(index) = s.store.clips.iter().position(|clip| clip.id == id) else {
            return false;
        };
        let changed = s.store.set_title(index, &raw);
        if changed {
            s.store.save();
        }
        changed
    });
    if changed {
        refresh_ui();
    }
}

fn delete_selected() {
    let id = with_state(|s| {
        let index = *filtered_indices(s).get(s.selected)?;
        s.store.clips.get(index).map(|clip| clip.id.clone())
    });
    let Some(id) = id else {
        return;
    };
    delete_clip(&id);
}

fn delete_clip(id: &str) {
    hide_hover_preview();
    let mtm = main_thread();
    let Some(confirm) = with_state(|s| {
        s.store
            .clips
            .iter()
            .any(|clip| clip.id == id)
            .then_some(s.settings.confirm_delete)
    }) else {
        NSBeep();
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

    // Resolve the id again: the 1 s pasteboard poll can insert a clip while the
    // confirmation alert is up, which would shift a previously captured index.
    with_state(|s| {
        let Some(index) = s.store.clips.iter().position(|clip| clip.id == id) else {
            return;
        };
        s.store.remove(index);
        s.store.save();
    });
    refresh_ui();
    prune_thumbnails();
}

fn clear_scope_messages(scope: ClearScope) -> (&'static str, &'static str) {
    match scope {
        ClearScope::Everything => (
            "Delete all clippings?",
            "This permanently removes every text and image clipping, including pinned ones. Terminal commands are kept. This action cannot be undone.",
        ),
        ClearScope::Text => (
            "Delete all text clippings?",
            "This removes every text, rich text, HTML and file clipping in the Text tab, including pinned ones. Images and terminal commands are kept. This action cannot be undone.",
        ),
        ClearScope::Images => (
            "Delete all images?",
            "This removes every image and its thumbnail, including pinned ones. Text clippings and terminal commands are kept. This action cannot be undone.",
        ),
        ClearScope::UnpinnedText => (
            "Delete unpinned text clippings?",
            "This removes every unpinned text, rich text, HTML and file clipping. Pinned text clippings, images and terminal commands are kept. This action cannot be undone.",
        ),
        ClearScope::UnpinnedImages => (
            "Delete unpinned images?",
            "This removes every unpinned image and its thumbnail. Pinned images, text clippings and terminal commands are kept. This action cannot be undone.",
        ),
    }
}

/// Delete every clipping in `scope`. Commands are never touched. The caller
/// does not hold a state borrow while the confirmation alert runs.
/// Delete a scope without asking. Used by the confirmation path and by MCP,
/// whose calls are already explicit. Returns the number of removed clippings.
fn perform_clear_scope(scope: ClearScope) -> usize {
    let (removed, search) = with_state(|s| {
        let removed = s.store.clear_scope(scope);
        if removed > 0 {
            s.store.save();
            s.selected = 0;
            s.scroll = 0.0;
            // A global delete should not leave a stale search behind the popup.
            if scope == ClearScope::Everything {
                s.query.clear();
            }
        }
        let search = if removed > 0 && scope == ClearScope::Everything {
            Some(s.search.clone())
        } else {
            None
        };
        (removed, search)
    });
    if let Some(search) = search {
        search.setStringValue(&NSString::from_str(""));
    }
    if removed > 0 {
        refresh_ui();
        if scope == ClearScope::Everything {
            clear_thumbnail_cache();
        } else {
            prune_thumbnails();
        }
    }
    removed
}

fn clear_scope(scope: ClearScope) {
    let mtm = main_thread();
    let (count, confirm) = with_state(|s| (s.store.count_scope(scope), s.settings.confirm_delete));
    if count == 0 {
        NSBeep();
        return;
    }

    if confirm {
        activate_app();
        let alert = NSAlert::new(mtm);
        let (message, info) = clear_scope_messages(scope);
        alert.setMessageText(&NSString::from_str(message));
        alert.setInformativeText(&NSString::from_str(info));
        alert.addButtonWithTitle(&NSString::from_str(if scope == ClearScope::Everything {
            "Delete All"
        } else {
            "Delete"
        }));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        if alert.runModal() != NSAlertFirstButtonReturn {
            return;
        }
    }

    perform_clear_scope(scope);
}

/// Footer buttons: clear the active tab, either every row or only unpinned
/// rows. The Commands tab has no clear action.
fn clear_current_scope(unpinned: bool) {
    let scope = with_state(|s| match (s.section, unpinned) {
        (ClipboardSection::Text, false) => Some(ClearScope::Text),
        (ClipboardSection::Images, false) => Some(ClearScope::Images),
        (ClipboardSection::Text, true) => Some(ClearScope::UnpinnedText),
        (ClipboardSection::Images, true) => Some(ClearScope::UnpinnedImages),
        (ClipboardSection::Commands, _) => None,
    });
    if let Some(scope) = scope {
        clear_scope(scope);
    }
}

/// Refresh the counts and enabled buttons in the Preferences **History** tab.
fn update_history_controls() {
    let (summary, enabled, prefs) = with_state(|s| {
        let all = s.store.count_scope(ClearScope::Everything);
        let text = s.store.count_scope(ClearScope::Text);
        let images = s.store.count_scope(ClearScope::Images);
        let unpinned_text = s.store.count_scope(ClearScope::UnpinnedText);
        let unpinned_images = s.store.count_scope(ClearScope::UnpinnedImages);
        let pinned = s
            .store
            .clips
            .iter()
            .filter(|clip| clip.pinned && clip.kind != ClipKind::Command)
            .count();
        let summary = format!("{text} text · {images} images · {pinned} pinned");
        let prefs = s.prefs.as_ref().map(|prefs| {
            (
                prefs.history_summary.clone(),
                prefs.delete_all_clippings.clone(),
                prefs.delete_all_images.clone(),
                prefs.delete_all_text.clone(),
                prefs.delete_unpinned_text.clone(),
                prefs.delete_unpinned_images.clone(),
            )
        });
        (
            summary,
            [
                all > 0,
                images > 0,
                text > 0,
                unpinned_text > 0,
                unpinned_images > 0,
            ],
            prefs,
        )
    });
    let Some((summary_field, all, images, text, unpinned_text, unpinned_images)) = prefs else {
        return;
    };
    summary_field.setStringValue(&NSString::from_str(&summary));
    all.setEnabled(enabled[0]);
    images.setEnabled(enabled[1]);
    text.setEnabled(enabled[2]);
    unpinned_text.setEnabled(enabled[3]);
    unpinned_images.setEnabled(enabled[4]);
}

fn disk_size(bytes: u64) -> String {
    human_size(usize::try_from(bytes).unwrap_or(usize::MAX))
}

/// Refresh the disk-usage line in the Preferences **History** tab. History
/// files and the temporary preview cache are measured separately.
fn update_history_storage() {
    let storage = with_state(|s| s.store.storage_usage());
    let cache = thumbnails::cache_usage_bytes();
    let label = with_state(|s| s.prefs.as_ref().map(|prefs| prefs.history_storage.clone()));
    let Some(label) = label else {
        return;
    };
    let history = storage.history_total();
    let total = history.saturating_add(cache);
    label.setStringValue(&NSString::from_str(&format!(
        "Disk usage: {} · history {} · cache {}",
        disk_size(total),
        disk_size(history),
        disk_size(cache)
    )));
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

fn search_changed() {
    hide_hover_preview();
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
    hide_hover_preview();
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
            prefs
                .pinned_background_well
                .setColor(&views::color_from_rgb(s.settings.pinned_background));
            prefs
                .pinned_indicator_well
                .setColor(&views::color_from_rgb(s.settings.pinned_indicator));
            prefs.ignored_view.clear_selection();
            prefs.ignored_view.setNeedsDisplay(true);
        }
    });
    update_history_controls();
    update_history_storage();

    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    let window = with_state(|s| s.prefs.as_ref().map(|p| p.window.clone()));
    if let Some(window) = window {
        window.makeKeyAndOrderFront(None);
    }
}

fn set_pinned_color(sender: &NSColorWell, background: bool) {
    let color = views::rgb_from_color(&sender.color());
    let (settings, list) = with_state(|s| {
        if background {
            s.settings.pinned_background = color;
        } else {
            s.settings.pinned_indicator = color;
        }
        (s.settings.clone(), s.list.clone())
    });
    settings::save(&settings);
    list.setNeedsDisplay(true);
}

fn reset_pinned_colors() {
    let (settings, background_well, indicator_well, list) = with_state(|s| {
        s.settings.pinned_background = DEFAULT_PINNED_BACKGROUND;
        s.settings.pinned_indicator = DEFAULT_PINNED_INDICATOR;
        let prefs = s.prefs.as_ref();
        (
            s.settings.clone(),
            prefs.map(|p| p.pinned_background_well.clone()),
            prefs.map(|p| p.pinned_indicator_well.clone()),
            s.list.clone(),
        )
    });
    settings::save(&settings);
    if let Some(well) = background_well {
        well.setColor(&views::color_from_rgb(DEFAULT_PINNED_BACKGROUND));
    }
    if let Some(well) = indicator_well {
        well.setColor(&views::color_from_rgb(DEFAULT_PINNED_INDICATOR));
    }
    list.setNeedsDisplay(true);
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
        s.text_previews.clear();
        s.list.setNeedsDisplay(true);
    });
    // The button also clears the disposable Quick Look preview directory.
    thumbnails::reset_previews();
    update_history_storage();
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
    hide_hover_preview();
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

/// Bound on the text written to a temporary preview file. The full clip stays
/// in the history; Quick Look only needs enough to show something useful.
const MAX_PREVIEW_TEXT_BYTES: usize = 8 * 1024 * 1024;

/// Where the selected clip can be read from for the shared Quick Look panel.
enum PreviewSource {
    /// A `File` clipping still points at an existing file or directory.
    ExistingFile { path: PathBuf, is_dir: bool },
    /// Editable text written to a stable per-clip preview file.
    TextBytes { bytes: Vec<u8> },
    /// Blob or fallback text written to a hash-named preview file.
    Bytes {
        bytes: Vec<u8>,
        extension: &'static str,
    },
}

/// Space (any tab, empty search) or ⌘Y: show the selected clip in the shared
/// Quick Look panel, or close it when it is already open.
fn toggle_quick_look() {
    let id = with_state(|s| {
        let index = *filtered_indices(s).get(s.selected)?;
        s.store.clips.get(index).map(|clip| clip.id.clone())
    });
    let Some(id) = id else {
        NSBeep();
        return;
    };
    toggle_preview_for_clip(&id);
}

/// True when the row menu may offer Quick Look. This only checks that the
/// stored source still exists; decoding happens when the panel is opened.
fn can_preview_clip(clip: &Clip, store: &Store) -> bool {
    match clip.kind {
        ClipKind::Text | ClipKind::Command => {
            clip.text.as_deref().is_some_and(|text| !text.is_empty())
        }
        ClipKind::RichText | ClipKind::Html | ClipKind::Image => clip
            .blob
            .as_deref()
            .and_then(|blob| store.blob_file(blob))
            .is_some_and(|path| path.is_file()),
        ClipKind::File => stored_file_path(clip).is_some(),
        ClipKind::Other => false,
    }
}

/// Resolve a `File` clipping to a path that still exists. Relative paths and
/// special files are refused so the panel never opens an arbitrary target.
fn stored_file_path(clip: &Clip) -> Option<(PathBuf, bool)> {
    let path = PathBuf::from(clip.text.as_deref()?);
    if !path.is_absolute() {
        return None;
    }
    let metadata = fs::metadata(&path).ok()?;
    if !metadata.is_file() && !metadata.is_dir() {
        return None;
    }
    Some((path, metadata.is_dir()))
}

fn preview_source_for_clip(clip: &Clip, store: &Store) -> Option<PreviewSource> {
    match clip.kind {
        ClipKind::Text | ClipKind::Command => {
            let text = clip.text.as_deref().filter(|text| !text.is_empty())?;
            let bytes = bounded_preview_text(text).as_bytes().to_vec();
            if can_edit_text(clip) {
                Some(PreviewSource::TextBytes { bytes })
            } else {
                // Too large for the editor: preview it read-only with a
                // hash-named file that is never watched for changes.
                Some(PreviewSource::Bytes {
                    bytes,
                    extension: "txt",
                })
            }
        }
        ClipKind::RichText => {
            let data_type = clip.data_type.as_deref().unwrap_or("public.rtf");
            // RTFD pasteboard data is a file wrapper, not a flat document, so
            // previewing the plain-text fallback is more reliable than writing
            // those bytes to a `.rtfd` file.
            if data_type == "public.rtfd"
                && let Some(text) = clip.text.as_deref().filter(|text| !text.is_empty())
            {
                return Some(PreviewSource::Bytes {
                    bytes: bounded_preview_text(text).as_bytes().to_vec(),
                    extension: "txt",
                });
            }
            Some(PreviewSource::Bytes {
                bytes: store.read_blob(clip.blob.as_deref()?)?,
                extension: if data_type == "public.rtfd" {
                    "rtfd"
                } else {
                    "rtf"
                },
            })
        }
        ClipKind::Html => Some(PreviewSource::Bytes {
            bytes: store.read_blob(clip.blob.as_deref()?)?,
            extension: "html",
        }),
        ClipKind::Image => {
            let bytes = store.read_blob(clip.blob.as_deref()?)?;
            let info = antirustpaste_imaging::info(&bytes).ok()?;
            let extension = match info.format {
                antirustpaste_imaging::Format::Png => "png",
                antirustpaste_imaging::Format::Tiff => "tiff",
                antirustpaste_imaging::Format::Jpeg => "jpg",
            };
            Some(PreviewSource::Bytes { bytes, extension })
        }
        ClipKind::File => {
            let (path, is_dir) = stored_file_path(clip)?;
            Some(PreviewSource::ExistingFile { path, is_dir })
        }
        ClipKind::Other => None,
    }
}

/// Cut a text preview on a UTF-8 boundary; the caller keeps the full clip.
fn bounded_preview_text(text: &str) -> &str {
    if text.len() <= MAX_PREVIEW_TEXT_BYTES {
        return text;
    }
    let mut end = MAX_PREVIEW_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    &text[..end]
}

fn toggle_preview_for_clip(id: &str) {
    let mtm = main_thread();
    let Some(panel) = (unsafe { QLPreviewPanel::sharedPreviewPanel(mtm) }) else {
        return;
    };
    if panel.isVisible() {
        close_quick_look();
        return;
    }

    let Some((hash, source)) = with_state(|s| {
        let clip = s.store.clips.iter().find(|clip| clip.id == id)?;
        let source = preview_source_for_clip(clip, &s.store)?;
        Some((clip.hash, source))
    }) else {
        NSBeep();
        return;
    };

    let (url, preview_file) = match source {
        PreviewSource::ExistingFile { path, is_dir } => (
            NSURL::fileURLWithPath_isDirectory(
                &NSString::from_str(&path.to_string_lossy()),
                is_dir,
            ),
            None,
        ),
        PreviewSource::TextBytes { bytes } => {
            let (name, watched) = match text_preview_file_name(id) {
                Some(name) => (name, true),
                None => (format!("preview-{hash}.txt"), false),
            };
            let Some(path) = write_preview_file(&name, &bytes) else {
                NSBeep();
                return;
            };
            if watched {
                watch_text_preview(&path, id);
            }
            let url = NSURL::fileURLWithPath_isDirectory(
                &NSString::from_str(&path.to_string_lossy()),
                false,
            );
            (url, Some(path))
        }
        PreviewSource::Bytes { bytes, extension } => {
            let name = format!("preview-{hash}.{extension}");
            let Some(path) = write_preview_file(&name, &bytes) else {
                NSBeep();
                return;
            };
            let url = NSURL::fileURLWithPath_isDirectory(
                &NSString::from_str(&path.to_string_lossy()),
                false,
            );
            (url, Some(path))
        }
    };

    let delegate = delegate_ref();
    let source = ProtocolObject::from_ref(&*delegate);
    unsafe {
        panel.setDataSource(Some(source));
        panel.setDelegate(Some(as_any(&*delegate)));
    }
    let old = with_state(|s| {
        let old = s.preview_file.take();
        s.preview_file = preview_file.clone();
        s.preview_item = Some(url);
        old
    });
    if let Some(old) = old
        && preview_file.as_deref() != Some(old.as_path())
    {
        let watched = with_state(|s| s.text_previews.iter().any(|watch| watch.path == old));
        if !watched {
            let _ = fs::remove_file(old);
        }
    }

    unsafe { panel.reloadData() };
    activate_app();
    panel.makeKeyAndOrderFront(None);
}

/// A stable file name per text clip keeps the Quick Look → TextEdit round-trip
/// on one path. Refuse ids that could escape the private preview directory.
fn text_preview_file_name(id: &str) -> Option<String> {
    if id.is_empty()
        || id.len() > 64
        || !id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return None;
    }
    Some(format!("preview-{id}.txt"))
}

fn write_preview_file(name: &str, bytes: &[u8]) -> Option<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if name.is_empty() || name.contains('/') || name.contains('\\') || name == ".." {
        return None;
    }
    let dir = thumbnails::ensure_preview_dir().ok()?;
    let path = dir.join(name);
    // A stale symlink must never redirect the write outside the private cache.
    if fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        let _ = fs::remove_file(&path);
    }
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

/// Remember a text preview so the 1 s poll can import edits saved in TextEdit.
fn watch_text_preview(path: &Path, clip_id: &str) {
    let metadata = fs::metadata(path).ok();
    let watched = WatchedTextPreview {
        path: path.to_path_buf(),
        clip_id: clip_id.to_string(),
        modified: metadata.as_ref().and_then(|meta| meta.modified().ok()),
        len: metadata.as_ref().map(|meta| meta.len()).unwrap_or(0),
    };
    let removed: Vec<PathBuf> = with_state(|s| {
        // A newer preview of the same clip supersedes its older documents.
        s.text_previews
            .retain(|entry| entry.clip_id != clip_id && entry.path != path);
        s.text_previews.push(watched);
        let mut removed = Vec::new();
        while s.text_previews.len() > MAX_WATCHED_TEXT_PREVIEWS {
            removed.push(s.text_previews.remove(0).path);
        }
        removed
    });
    for path in removed {
        let _ = fs::remove_file(path);
    }
}

/// Import a saved TextEdit change. Quick Look closes as soon as TextEdit opens,
/// so this cannot wait for the panel: it watches the temporary file while the
/// app runs and updates the clip when the file changes.
fn poll_edited_text_previews() {
    let watched: Vec<(PathBuf, String, Option<SystemTime>, u64)> = with_state(|s| {
        s.text_previews
            .iter()
            .map(|entry| {
                (
                    entry.path.clone(),
                    entry.clip_id.clone(),
                    entry.modified,
                    entry.len,
                )
            })
            .collect()
    });

    for (path, clip_id, last_modified, last_len) in watched {
        let Ok(symlink_metadata) = fs::symlink_metadata(&path) else {
            remove_text_preview_watch(&path);
            continue;
        };
        if symlink_metadata.file_type().is_symlink() || !symlink_metadata.is_file() {
            remove_text_preview_watch(&path);
            let _ = fs::remove_file(&path);
            continue;
        }
        let Ok(metadata) = fs::metadata(&path) else {
            remove_text_preview_watch(&path);
            continue;
        };
        let modified = metadata.modified().ok();
        let len = metadata.len();
        if modified == last_modified && len == last_len {
            continue;
        }
        with_state(|s| {
            if let Some(entry) = s.text_previews.iter_mut().find(|entry| entry.path == path) {
                entry.modified = modified;
                entry.len = len;
            }
        });
        if len > MAX_EDIT_TEXT_BYTES as u64 {
            continue;
        }
        let Some(text) = read_bounded_text_file(&path) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }

        let outcome = with_state(|s| {
            let Some(index) = s.store.clips.iter().position(|clip| clip.id == clip_id) else {
                return Err(EditTextError::Unavailable);
            };
            s.store.set_text(index, &text)
        });
        match outcome {
            Ok(true) => {
                with_state(|s| s.store.save());
                refresh_ui();
            }
            Ok(false) => {}
            Err(EditTextError::Unavailable) => {
                remove_text_preview_watch(&path);
                let _ = fs::remove_file(&path);
            }
            Err(EditTextError::Duplicate) => {
                // The user can resolve the duplicate in the editor; do not
                // overwrite the other row or steal focus from TextEdit.
                NSBeep();
            }
            Err(EditTextError::Empty | EditTextError::TooLarge) => {}
        }
    }
}

fn remove_text_preview_watch(path: &Path) {
    with_state(|s| s.text_previews.retain(|entry| entry.path != path));
}

fn read_bounded_text_file(path: &Path) -> Option<String> {
    use std::io::Read;

    let mut limited = fs::File::open(path)
        .ok()?
        .take(MAX_EDIT_TEXT_BYTES as u64 + 1);
    let mut bytes = Vec::new();
    limited.read_to_end(&mut bytes).ok()?;
    if bytes.len() > MAX_EDIT_TEXT_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn close_quick_look() {
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
    let (path, watched) = with_state(|s| {
        s.preview_item = None;
        let path = s.preview_file.take();
        let watched = path
            .as_ref()
            .is_some_and(|path| s.text_previews.iter().any(|entry| &entry.path == path));
        (path, watched)
    });
    // A watched text file may still be open in TextEdit, so it stays until the
    // editor saves, the watch is evicted, or the app quits.
    if let Some(path) = path
        && !watched
    {
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
    fn text_preview_is_cut_on_a_utf8_boundary() {
        let short = "antirustpaste-smoke-1";
        assert_eq!(bounded_preview_text(short), short);

        let mut long = String::with_capacity(MAX_PREVIEW_TEXT_BYTES + 4);
        long.push_str(&"a".repeat(MAX_PREVIEW_TEXT_BYTES - 1));
        long.push('€');
        let preview = bounded_preview_text(&long);
        assert_eq!(preview.len(), MAX_PREVIEW_TEXT_BYTES - 1);
        assert_eq!(preview.as_bytes().last(), Some(&b'a'));
    }

    #[test]
    fn text_preview_names_stay_inside_the_cache() {
        assert_eq!(
            text_preview_file_name("abc-123").as_deref(),
            Some("preview-abc-123.txt")
        );
        assert_eq!(text_preview_file_name("../secret"), None);
        assert_eq!(text_preview_file_name("a/b"), None);
        assert_eq!(text_preview_file_name(""), None);
        assert_eq!(text_preview_file_name(&"x".repeat(65)), None);
    }

    #[test]
    fn bounded_text_file_reads_respect_the_edit_limit() {
        let dir = scratch("bounded-text");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch directory");
        let path = dir.join("clip.txt");

        fs::write(&path, "antirustpaste-smoke-1").expect("seed text");
        assert_eq!(
            read_bounded_text_file(&path).as_deref(),
            Some("antirustpaste-smoke-1")
        );

        fs::write(&path, "x".repeat(MAX_EDIT_TEXT_BYTES + 1)).expect("oversized text");
        assert_eq!(read_bounded_text_file(&path), None);

        fs::write(&path, [0xff, 0xfe]).expect("invalid utf8");
        assert_eq!(read_bounded_text_file(&path), None);

        let _ = fs::remove_dir_all(&dir);
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
