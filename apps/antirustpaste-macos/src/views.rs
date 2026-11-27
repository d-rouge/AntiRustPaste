//! AppKit views: the popup history list, the ignored-apps list, the popup
//! panel itself and the preferences window.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::*;
use objc2_foundation::*;

use antirustpaste_core::{Clip, ClipKind, Rgb, human_size};

use crate::app::{
    self, AppState, ClipboardSection, LIST_BOTTOM, LIST_HEADER_H, ListLayout, PANEL_H, PANEL_W,
    ROW_H, SEARCH_H, SectionUi, as_any, filtered_indices, guard, list_layout, with_state,
};

// ---------------------------------------------------------------------------
// Small drawing helpers
// ---------------------------------------------------------------------------

fn draw_text(text: &str, rect: NSRect, font: &NSFont, color: &NSColor, align: NSTextAlignment) {
    if rect.size.width <= 1.0 || rect.size.height <= 1.0 {
        return;
    }
    let string = NSString::from_str(text);
    let style = NSMutableParagraphStyle::new();
    style.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    style.setAlignment(align);

    let keys: [&NSAttributedStringKey; 3] = unsafe {
        [
            NSFontAttributeName,
            NSForegroundColorAttributeName,
            NSParagraphStyleAttributeName,
        ]
    };
    let objects: [&AnyObject; 3] = [as_any(font), as_any(color), as_any(&*style)];
    let attrs: Retained<NSDictionary<NSAttributedStringKey, AnyObject>> =
        NSDictionary::from_slices(&keys, &objects);
    unsafe { string.drawInRect_withAttributes(rect, Some(&attrs)) };
}

fn text_width(text: &str, font: &NSFont) -> f64 {
    let string = NSString::from_str(text);
    let keys: [&NSAttributedStringKey; 1] = unsafe { [NSFontAttributeName] };
    let objects: [&AnyObject; 1] = [as_any(font)];
    let attrs: Retained<NSDictionary<NSAttributedStringKey, AnyObject>> =
        NSDictionary::from_slices(&keys, &objects);
    unsafe { string.sizeWithAttributes(Some(&attrs)).width }
}

fn text_height(text: &str, font: &NSFont) -> f64 {
    let string = NSString::from_str(text);
    let keys: [&NSAttributedStringKey; 1] = unsafe { [NSFontAttributeName] };
    let objects: [&AnyObject; 1] = [as_any(font)];
    let attrs: Retained<NSDictionary<NSAttributedStringKey, AnyObject>> =
        NSDictionary::from_slices(&keys, &objects);
    unsafe { string.sizeWithAttributes(Some(&attrs)).height }
}

fn fill_rounded(rect: NSRect, radius: f64, color: &NSColor) {
    let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, radius, radius);
    color.setFill();
    path.fill();
}

fn fill_rect(rect: NSRect, color: &NSColor) {
    color.setFill();
    NSBezierPath::fillRect(rect);
}

// ---------------------------------------------------------------------------
// Popup panel
// ---------------------------------------------------------------------------

define_class!(
    #[unsafe(super(NSPanel))]
    #[thread_kind = MainThreadOnly]
    #[name = "RCClipPanel"]
    #[ivars = ()]
    pub(crate) struct ClipPanel;

    unsafe impl NSObjectProtocol for ClipPanel {}

    impl ClipPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }
    }
);

impl ClipPanel {
    pub(crate) fn create(mtm: MainThreadMarker) -> Retained<Self> {
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PANEL_W, PANEL_H));
        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        unsafe {
            msg_send![
                Self::alloc(mtm),
                initWithContentRect: rect,
                styleMask: style,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        }
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RCPanelDelegate"]
    #[ivars = ()]
    pub(crate) struct PanelDelegate;

    unsafe impl NSObjectProtocol for PanelDelegate {}

    unsafe impl NSWindowDelegate for PanelDelegate {
        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            guard("window_did_resign_key", || {
                app::schedule_hide_from_resign();
            });
        }
    }
);

impl PanelDelegate {
    pub(crate) fn create(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

// ---------------------------------------------------------------------------
// History list
// ---------------------------------------------------------------------------

const NO_HOVER_ROW: usize = usize::MAX;

pub(crate) struct ClipListIvars {
    hover_row: Cell<usize>,
    hover_clip: RefCell<Option<String>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "RCClipListView"]
    #[ivars = ClipListIvars]
    pub(crate) struct ClipListView;

    unsafe impl NSObjectProtocol for ClipListView {}

    impl ClipListView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            guard("clip_list_update_tracking_areas", || {
                let areas = self.trackingAreas();
                for area in areas.iter() {
                    self.removeTrackingArea(&area);
                }
                let options = NSTrackingAreaOptions::MouseEnteredAndExited
                    | NSTrackingAreaOptions::MouseMoved
                    | NSTrackingAreaOptions::ActiveInKeyWindow
                    | NSTrackingAreaOptions::InVisibleRect;
                let area = unsafe {
                    NSTrackingArea::initWithRect_options_owner_userInfo(
                        NSTrackingArea::alloc(),
                        self.bounds(),
                        options,
                        Some(as_any(self)),
                        None,
                    )
                };
                self.addTrackingArea(&area);
            });
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            guard("draw_rect", || {
                draw_list(self);
            });
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, event: &NSEvent) {
            guard("clip_list_mouse_entered", || {
                self.update_hover_from_event(event);
            });
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            guard("clip_list_mouse_moved", || {
                self.update_hover_from_event(event);
            });
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            guard("clip_list_mouse_exited", || {
                let was_hovered = self.ivars().hover_row.replace(NO_HOVER_ROW) != NO_HOVER_ROW;
                let had_clip = self.ivars().hover_clip.borrow_mut().take().is_some();
                if was_hovered || had_clip {
                    self.setNeedsDisplay(true);
                    app::hover_preview_changed(None);
                }
            });
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            guard("mouse_down", || {
                app::handle_list_click(self, event);
            });
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            guard("right_mouse_down", || {
                app::handle_list_right_click(self, event);
            });
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            guard("scroll_wheel", || {
                app::handle_list_scroll(event.scrollingDeltaY());
                let point = self.convertPoint_fromView(event.locationInWindow(), None);
                self.update_hover_at(point);
            });
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            guard("key_down", || {
                // Unhandled keys are ignored: the search field keeps focus, so this
                // view only ever sees keys we deliberately route to it.
                let _ = app::handle_key_event(event);
            });
        }
    }
);

impl ClipListView {
    pub(crate) fn create(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ClipListIvars {
            hover_row: Cell::new(NO_HOVER_ROW),
            hover_clip: RefCell::new(None),
        });
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    pub(crate) fn hover_row(&self) -> usize {
        self.ivars().hover_row.get()
    }

    fn update_hover_from_event(&self, event: &NSEvent) {
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        self.update_hover_at(point);
    }

    fn update_hover_at(&self, point: NSPoint) {
        let (row, clip_id) = with_state(|s| {
            let indices = filtered_indices(s);
            let layout = list_layout(s, &indices);
            let row = layout.row_at(point.y + s.scroll).unwrap_or(NO_HOVER_ROW);
            let clip_id = if row == NO_HOVER_ROW {
                None
            } else {
                indices
                    .get(row)
                    .and_then(|index| s.store.clips.get(*index))
                    .map(|clip| clip.id.clone())
            };
            (row, clip_id)
        });
        let row_changed = self.ivars().hover_row.replace(row) != row;
        let clip_changed = {
            let mut hover_clip = self.ivars().hover_clip.borrow_mut();
            if *hover_clip == clip_id {
                false
            } else {
                *hover_clip = clip_id.clone();
                true
            }
        };
        if row_changed || clip_changed {
            self.setNeedsDisplay(true);
        }
        // Ask every time: if the callout was hidden by deactivating the app, the
        // same row must be able to show it again on the next mouse move.
        app::hover_preview_changed(clip_id);
    }
}

pub(crate) fn color_from_rgb(rgb: Rgb) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(
        f64::from(rgb.red) / 255.0,
        f64::from(rgb.green) / 255.0,
        f64::from(rgb.blue) / 255.0,
        1.0,
    )
}

pub(crate) fn rgb_from_color(color: &NSColor) -> Rgb {
    let Some(converted) = color.colorUsingColorSpace(&NSColorSpace::sRGBColorSpace()) else {
        return Rgb::new(0, 0, 0);
    };
    Rgb::new(
        component_to_u8(converted.redComponent()),
        component_to_u8(converted.greenComponent()),
        component_to_u8(converted.blueComponent()),
    )
}

fn component_to_u8(value: f64) -> u8 {
    if value.is_finite() {
        (value.clamp(0.0, 1.0) * 255.0).round() as u8
    } else {
        0
    }
}

fn draw_list(view: &ClipListView) {
    // AppKit hands this flipped view a dirty rect that reaches above its own
    // bounds while the panel is scrolled, so clip explicitly: rows, the card
    // and the group headers must never paint over the search field or footer.
    NSGraphicsContext::saveGraphicsState_class();
    NSBezierPath::bezierPathWithRect(view.bounds()).addClip();
    draw_list_contents(view);
    NSGraphicsContext::restoreGraphicsState_class();
}

fn draw_list_contents(view: &ClipListView) {
    let bounds = view.bounds();
    let width = bounds.size.width;
    let height = bounds.size.height;
    let hover_row = view.hover_row();

    let (
        indices,
        selected,
        scroll,
        query,
        section,
        layout,
        pinned_background_rgb,
        pinned_indicator_rgb,
        start,
        end,
        visible_bundles,
    ) = with_state(|s: &mut AppState| {
        let indices = filtered_indices(s);
        let layout = list_layout(s, &indices);
        let (start, end) = layout.visible_range(s.scroll, s.scroll + height);
        let visible_bundles = (start..end)
            .map(|visible| {
                indices
                    .get(visible)
                    .and_then(|index| s.store.clips.get(*index))
                    .and_then(|clip| clip.source_bundle.clone())
            })
            .collect::<Vec<_>>();
        (
            indices,
            s.selected,
            s.scroll,
            s.query.clone(),
            s.section,
            layout,
            s.settings.pinned_background,
            s.settings.pinned_indicator,
            start,
            end,
            visible_bundles,
        )
    });
    let pinned_background = color_from_rgb(pinned_background_rgb);
    let pinned_indicator = color_from_rgb(pinned_indicator_rgb);
    let row_icons = visible_bundles
        .iter()
        .map(|bundle| app::app_icon_for_bundle(bundle.as_deref()))
        .collect::<Vec<_>>();

    if indices.is_empty() {
        let message = if query.is_empty() {
            section.empty_message()
        } else {
            section.no_results_message()
        };
        let font = NSFont::systemFontOfSize(13.0);
        let rect = NSRect::new(
            NSPoint::new(0.0, height / 2.0 - 10.0),
            NSSize::new(width, 20.0),
        );
        draw_text(
            message,
            rect,
            &font,
            &NSColor::secondaryLabelColor(),
            NSTextAlignment::Center,
        );
        return;
    }

    let title_font = NSFont::systemFontOfSize(13.0);
    let subtitle_font = NSFont::systemFontOfSize(11.5);
    let subtitle_line_h = text_height("Xg", &subtitle_font).max(14.0);
    let label_color = NSColor::labelColor();
    let secondary = NSColor::secondaryLabelColor();
    let white = NSColor::whiteColor();
    let selected_bg = NSColor::selectedContentBackgroundColor();
    let hover_bg = selected_bg.colorWithAlphaComponent(0.12);
    let separator = NSColor::separatorColor().colorWithAlphaComponent(0.55);
    let placeholder = NSColor::separatorColor().colorWithAlphaComponent(0.35);

    with_state(|s: &mut AppState| {
        let title_symbol = s.title_symbol.clone();
        let title_symbol_selected = s.title_symbol_selected.clone();
        let badge_font = NSFont::systemFontOfSize(10.0);
        let badge_text = "Edited";
        let badge_text_width = text_width(badge_text, &badge_font);
        let badge_text_height = text_height("Xg", &badge_font).max(12.0);
        let badge_height = 16.0;
        let badge_padding = 7.0;
        let badge_pencil = 10.0;
        let badge_inner_gap = 4.0;
        let badge_width = badge_padding * 2.0 + badge_pencil + badge_inner_gap + badge_text_width;
        let badge_spacing = 6.0;
        let badge_fill = label_color.colorWithAlphaComponent(0.12);
        let badge_fill_emphasis = NSColor::blackColor().colorWithAlphaComponent(0.55);
        let badge_content = label_color.colorWithAlphaComponent(0.72);
        let badge_content_emphasis = white.colorWithAlphaComponent(0.95);
        draw_list_grouping(
            width,
            height,
            scroll,
            &layout,
            &secondary,
            &pinned_background,
        );

        for visible in start..end {
            let Some(&clip_index) = indices.get(visible) else {
                continue;
            };
            let (hash, blob, custom_title, content, subtitle, pinned) = {
                let Some(clip) = s.store.clips.get(clip_index) else {
                    continue;
                };
                (
                    clip.hash,
                    clip.blob.clone(),
                    clip.title.clone().filter(|title| !title.is_empty()),
                    content_summary(clip),
                    clip.subtitle(),
                    clip.pinned,
                )
            };
            let has_title = custom_title.is_some();
            let title = match custom_title {
                Some(title) => title,
                None => content,
            };
            let y = layout.row_top(visible) - scroll;
            let is_selected = visible == selected;
            let is_hovered = visible == hover_row;
            let in_pinned_group = visible < layout.pinned();

            if is_selected {
                fill_rounded(
                    NSRect::new(
                        NSPoint::new(6.0, y + 3.0),
                        NSSize::new(width - 12.0, ROW_H - 6.0),
                    ),
                    6.0,
                    &selected_bg,
                );
            } else if is_hovered {
                fill_rounded(
                    NSRect::new(
                        NSPoint::new(6.0, y + 3.0),
                        NSSize::new(width - 12.0, ROW_H - 6.0),
                    ),
                    6.0,
                    &hover_bg,
                );
            }

            let text_left = if section == ClipboardSection::Images {
                62.0
            } else {
                16.0
            };

            if section == ClipboardSection::Images {
                let thumb_rect = NSRect::new(NSPoint::new(13.0, y + 7.0), NSSize::new(40.0, 40.0));
                match app::thumbnail_for(s, hash, blob.as_deref()) {
                    Some(image) => draw_image_fit(thumb_rect, &image),
                    None => {
                        fill_rounded(thumb_rect, 4.0, &placeholder);
                        draw_text(
                            "IMG",
                            NSRect::new(
                                NSPoint::new(thumb_rect.origin.x, thumb_rect.origin.y + 14.0),
                                NSSize::new(thumb_rect.size.width, 12.0),
                            ),
                            &NSFont::systemFontOfSize(9.0),
                            &secondary,
                            NSTextAlignment::Center,
                        );
                    }
                }
            }

            let title_available = (width
                - text_left
                - 44.0
                - if has_title {
                    badge_width + badge_spacing
                } else {
                    0.0
                })
            .max(0.0);
            let title_rect = NSRect::new(
                NSPoint::new(text_left, y + 10.0),
                NSSize::new(title_available, 18.0),
            );
            let title_color = if is_selected { &white } else { &label_color };
            draw_text(
                &title,
                title_rect,
                &title_font,
                title_color,
                NSTextAlignment::Left,
            );
            if has_title {
                let measured = text_width(&title, &title_font).min(title_available);
                let badge_x = text_left + measured + badge_spacing;
                let badge_y = y + 19.0 - badge_height / 2.0;
                let badge_rect = NSRect::new(
                    NSPoint::new(badge_x, badge_y),
                    NSSize::new(badge_width, badge_height),
                );
                let badge_emphasis = pinned || is_selected;
                let badge_symbol = if badge_emphasis {
                    title_symbol_selected.as_ref()
                } else {
                    title_symbol.as_ref()
                };
                fill_rounded(
                    badge_rect,
                    badge_height / 2.0,
                    if badge_emphasis {
                        &badge_fill_emphasis
                    } else {
                        &badge_fill
                    },
                );
                let content_color = if badge_emphasis {
                    &badge_content_emphasis
                } else {
                    &badge_content
                };
                let mut content_x = badge_x + badge_padding;
                if let Some(symbol) = badge_symbol {
                    let pencil_y = badge_y + (badge_height - badge_pencil) / 2.0;
                    let pencil_rect = NSRect::new(
                        NSPoint::new(content_x, pencil_y),
                        NSSize::new(badge_pencil, badge_pencil),
                    );
                    draw_image_fit(pencil_rect, symbol);
                    content_x += badge_pencil + badge_inner_gap;
                }
                let badge_text_y = y + 19.0 - badge_text_height / 2.0;
                draw_text(
                    badge_text,
                    NSRect::new(
                        NSPoint::new(content_x, badge_text_y),
                        NSSize::new(badge_text_width, badge_text_height),
                    ),
                    &badge_font,
                    content_color,
                    NSTextAlignment::Left,
                );
            }

            let marker_rect = NSRect::new(
                NSPoint::new(width - 30.0, y + 21.0),
                NSSize::new(12.0, 12.0),
            );
            let marker_color = if is_selected {
                white.colorWithAlphaComponent(if pinned { 1.0 } else { 0.65 })
            } else if pinned {
                pinned_indicator.clone()
            } else {
                secondary.colorWithAlphaComponent(0.65)
            };
            draw_pin_marker(marker_rect, pinned, &marker_color);

            let row_icon = row_icons
                .get(visible.saturating_sub(start))
                .and_then(|icon| icon.as_ref());
            let subtitle_left = if row_icon.is_some() {
                text_left + 16.0
            } else {
                text_left
            };
            if let Some(icon) = row_icon {
                let icon_rect =
                    NSRect::new(NSPoint::new(text_left, y + 31.0), NSSize::new(13.0, 13.0));
                draw_image_fit_alpha(icon_rect, icon, 0.8);
            }
            let subtitle_y = y + 37.5 - subtitle_line_h / 2.0;
            let subtitle_rect = NSRect::new(
                NSPoint::new(subtitle_left, subtitle_y),
                NSSize::new(width - subtitle_left - 44.0, subtitle_line_h),
            );
            let subtitle_color = if is_selected {
                white.colorWithAlphaComponent(0.8)
            } else {
                secondary.clone()
            };
            draw_text(
                &subtitle,
                subtitle_rect,
                &subtitle_font,
                &subtitle_color,
                NSTextAlignment::Left,
            );

            if !in_pinned_group && visible + 1 != indices.len() {
                fill_rect(
                    NSRect::new(
                        NSPoint::new(text_left, y + ROW_H - 0.5),
                        NSSize::new(width - text_left - 16.0, 0.5),
                    ),
                    &separator,
                );
            }
        }
    });
}

fn draw_list_grouping(
    width: f64,
    height: f64,
    scroll: f64,
    layout: &ListLayout,
    secondary: &NSColor,
    pinned_background: &NSColor,
) {
    if let Some((top, card_height)) = layout.pinned_card() {
        let card = NSRect::new(
            NSPoint::new(4.0, top - scroll),
            NSSize::new(width - 8.0, card_height),
        );
        let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(card, 10.0, 10.0);
        NSColor::controlBackgroundColor()
            .colorWithAlphaComponent(0.45)
            .setFill();
        path.fill();
        pinned_background.colorWithAlphaComponent(0.18).setFill();
        path.fill();
        NSColor::separatorColor()
            .colorWithAlphaComponent(0.45)
            .setStroke();
        path.setLineWidth(1.0);
        path.stroke();
    }

    let font = NSFont::systemFontOfSize(10.0);
    if let Some(y) = layout.pinned_header_y() {
        draw_group_header("PINNED", y - scroll, width, height, &font, secondary);
    }
    if let Some(y) = layout.recent_header_y() {
        draw_group_header("RECENT", y - scroll, width, height, &font, secondary);
    }
}

fn draw_group_header(text: &str, y: f64, width: f64, height: f64, font: &NSFont, color: &NSColor) {
    if y + LIST_HEADER_H <= 0.0 || y >= height {
        return;
    }
    draw_text(
        text,
        NSRect::new(NSPoint::new(18.0, y + 7.0), NSSize::new(width - 36.0, 13.0)),
        font,
        color,
        NSTextAlignment::Left,
    );
}

fn draw_pin_marker(rect: NSRect, filled: bool, color: &NSColor) {
    let path = NSBezierPath::bezierPathWithOvalInRect(rect);
    if filled {
        color.setFill();
        path.fill();
    } else {
        color.setStroke();
        path.setLineWidth(1.5);
        path.stroke();
    }
}

fn content_summary(clip: &Clip) -> String {
    match (clip.width, clip.height) {
        (Some(width), Some(height)) if clip.kind == ClipKind::Image => {
            format!(
                "Image · {}×{} · {}",
                width,
                height,
                human_size(clip.byte_len)
            )
        }
        _ => clip.content_title().to_string(),
    }
}

fn image_title(clip: &Clip) -> String {
    if let Some(title) = &clip.title
        && !title.is_empty()
    {
        return title.clone();
    }
    match (clip.width, clip.height) {
        (Some(width), Some(height)) => {
            format!(
                "Image · {}×{} · {}",
                width,
                height,
                human_size(clip.byte_len)
            )
        }
        _ => clip.display_title().to_string(),
    }
}

fn draw_image_fit(rect: NSRect, image: &NSImage) {
    draw_image_fit_alpha(rect, image, 1.0);
}

fn draw_image_fit_alpha(rect: NSRect, image: &NSImage, alpha: f64) {
    let size = image.size();
    if size.width <= 0.0 || size.height <= 0.0 || rect.size.width <= 0.0 || rect.size.height <= 0.0
    {
        return;
    }
    let scale = (rect.size.width / size.width).min(rect.size.height / size.height);
    let width = size.width * scale;
    let height = size.height * scale;
    let fitted = NSRect::new(
        NSPoint::new(
            rect.origin.x + (rect.size.width - width) / 2.0,
            rect.origin.y + (rect.size.height - height) / 2.0,
        ),
        NSSize::new(width, height),
    );
    unsafe {
        image.drawInRect_fromRect_operation_fraction_respectFlipped_hints(
            fitted,
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
            NSCompositingOperation::SourceOver,
            alpha,
            true,
            None,
        );
    }
}

// ---------------------------------------------------------------------------
// Hover preview
// ---------------------------------------------------------------------------

const HOVER_PREVIEW_W: f64 = 380.0;
const HOVER_PADDING: f64 = 10.0;
const HOVER_META_H: f64 = 16.0;
const HOVER_GAP: f64 = 8.0;
const HOVER_BODY_MAX_H: f64 = 119.0;

#[derive(Clone)]
pub(crate) struct HoverPreviewParts {
    pub panel: Retained<NSPanel>,
    pub icon: Retained<NSImageView>,
    pub body: Retained<NSTextField>,
    pub meta: Retained<NSTextField>,
}

pub(crate) fn build_hover_preview(mtm: MainThreadMarker) -> HoverPreviewParts {
    let panel: Retained<NSPanel> = unsafe {
        msg_send![
            NSPanel::alloc(mtm),
            initWithContentRect: NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(HOVER_PREVIEW_W, 80.0)
            ),
            styleMask: NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            backing: NSBackingStoreType::Buffered,
            defer: false
        ]
    };
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setLevel(NSFloatingWindowLevel);
    panel.setHasShadow(true);
    panel.setIgnoresMouseEvents(true);
    panel.setHidesOnDeactivate(true);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );

    let bg = NSVisualEffectView::new(mtm);
    bg.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(HOVER_PREVIEW_W, 80.0),
    ));
    bg.setMaterial(NSVisualEffectMaterial::Popover);
    bg.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    bg.setState(NSVisualEffectState::Active);
    bg.setWantsLayer(true);
    if let Some(layer) = bg.layer() {
        layer.setCornerRadius(10.0);
        layer.setMasksToBounds(true);
    }
    panel.setContentView(Some(&bg));

    let body = NSTextField::wrappingLabelWithString(&NSString::from_str(""), mtm);
    body.setFrame(NSRect::new(
        NSPoint::new(HOVER_PADDING, HOVER_PADDING + HOVER_META_H + HOVER_GAP),
        NSSize::new(HOVER_PREVIEW_W - HOVER_PADDING * 2.0, HOVER_BODY_MAX_H),
    ));
    body.setFont(Some(&NSFont::systemFontOfSize(13.0)));
    body.setTextColor(Some(&NSColor::labelColor()));
    body.setMaximumNumberOfLines(7);
    bg.addSubview(&body);

    let icon = NSImageView::new(mtm);
    icon.setFrame(NSRect::new(
        NSPoint::new(HOVER_PADDING, HOVER_PADDING),
        NSSize::new(16.0, 16.0),
    ));
    icon.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
    icon.setImageFrameStyle(NSImageFrameStyle::None);
    icon.setAlphaValue(0.8);
    icon.setHidden(true);
    bg.addSubview(&icon);

    let meta = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    meta.setFrame(NSRect::new(
        NSPoint::new(HOVER_PADDING, HOVER_PADDING),
        NSSize::new(HOVER_PREVIEW_W - HOVER_PADDING * 2.0, HOVER_META_H),
    ));
    meta.setFont(Some(&NSFont::systemFontOfSize(11.0)));
    meta.setTextColor(Some(&NSColor::secondaryLabelColor()));
    bg.addSubview(&meta);

    HoverPreviewParts {
        panel,
        icon,
        body,
        meta,
    }
}

pub(crate) fn present_hover_preview(
    mtm: MainThreadMarker,
    parts: &HoverPreviewParts,
    body_text: Option<&str>,
    meta_text: &str,
    icon: Option<&NSImage>,
) {
    let panel = &parts.panel;
    let icon_view = &parts.icon;
    let body = &parts.body;
    let meta = &parts.meta;
    let inner_width = HOVER_PREVIEW_W - HOVER_PADDING * 2.0;
    icon_view.setImage(icon);
    icon_view.setHidden(icon.is_none());
    let meta_left = if icon.is_some() {
        HOVER_PADDING + 22.0
    } else {
        HOVER_PADDING
    };
    let meta_width = (HOVER_PREVIEW_W - HOVER_PADDING - meta_left).max(0.0);
    meta.setStringValue(&NSString::from_str(meta_text));

    let (height, body_height) = match body_text.filter(|text| !text.is_empty()) {
        Some(text) => {
            body.setHidden(false);
            body.setStringValue(&NSString::from_str(text));
            body.setFrameSize(NSSize::new(inner_width, HOVER_BODY_MAX_H));
            let measured = body
                .cell()
                .map(|cell| cell.cellSizeForBounds(body.bounds()).height)
                .unwrap_or(17.0);
            let body_height = if measured.is_finite() {
                measured.clamp(17.0, HOVER_BODY_MAX_H)
            } else {
                17.0
            };
            (
                HOVER_PADDING + body_height + HOVER_GAP + HOVER_META_H + HOVER_PADDING,
                body_height,
            )
        }
        None => {
            body.setHidden(true);
            (HOVER_PADDING + HOVER_META_H + HOVER_PADDING, 0.0)
        }
    };

    let origin = panel.frame().origin;
    panel.setFrame_display(
        NSRect::new(origin, NSSize::new(HOVER_PREVIEW_W, height)),
        true,
    );
    if let Some(content) = panel.contentView() {
        content.setFrame(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(HOVER_PREVIEW_W, height),
        ));
    }
    meta.setFrame(NSRect::new(
        NSPoint::new(meta_left, HOVER_PADDING),
        NSSize::new(meta_width, HOVER_META_H),
    ));
    body.setFrame(NSRect::new(
        NSPoint::new(HOVER_PADDING, HOVER_PADDING + HOVER_META_H + HOVER_GAP),
        NSSize::new(inner_width, body_height),
    ));

    let mouse = NSEvent::mouseLocation();
    let screen = NSScreen::screens(mtm)
        .iter()
        .find(|screen| {
            let frame = screen.frame();
            mouse.x >= frame.origin.x
                && mouse.x <= frame.origin.x + frame.size.width
                && mouse.y >= frame.origin.y
                && mouse.y <= frame.origin.y + frame.size.height
        })
        .or_else(|| NSScreen::mainScreen(mtm));
    if let Some(screen) = screen {
        let visible = screen.visibleFrame();
        // Stay next to the pointer, like a tooltip. Flip to the other side only
        // when the preferred side would leave the screen.
        let mut x = mouse.x + 18.0;
        if x + HOVER_PREVIEW_W > visible.origin.x + visible.size.width {
            x = mouse.x - HOVER_PREVIEW_W - 18.0;
        }
        let mut y = mouse.y - height / 2.0;
        x = x.max(visible.origin.x + 8.0);
        y = y
            .max(visible.origin.y + 8.0)
            .min(visible.origin.y + visible.size.height - height - 8.0);
        panel.setFrameOrigin(NSPoint::new(x, y));
    }
    panel.orderFrontRegardless();
}

// ---------------------------------------------------------------------------
// PDF image selection
// ---------------------------------------------------------------------------

pub(crate) const PDF_ROW_H: f64 = 64.0;
const NO_DRAG_ROW: usize = usize::MAX;

pub(crate) struct PdfSelectionIvars {
    drag_from: Cell<usize>,
    drag_boundary: Cell<usize>,
    press_y: Cell<f64>,
    dragging: Cell<bool>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "RCPdfSelectionView"]
    #[ivars = PdfSelectionIvars]
    pub(crate) struct PdfSelectionView;

    unsafe impl NSObjectProtocol for PdfSelectionView {}

    impl PdfSelectionView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            guard("pdf_selection_draw", || draw_pdf_selection(self));
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            guard("pdf_selection_mouse_down", || {
                let point = self.convertPoint_fromView(event.locationInWindow(), None);
                self.ivars().press_y.set(point.y);
                self.ivars().drag_from.set(pdf_row_at(point.y));
                self.ivars().drag_boundary.set(0);
                self.ivars().dragging.set(false);
            });
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            guard("pdf_selection_mouse_dragged", || {
                let from = self.ivars().drag_from.get();
                if from == NO_DRAG_ROW {
                    return;
                }
                let point = self.convertPoint_fromView(event.locationInWindow(), None);
                if !self.ivars().dragging.get() {
                    if (point.y - self.ivars().press_y.get()).abs() < 3.0 {
                        return;
                    }
                    self.ivars().dragging.set(true);
                }
                let boundary = pdf_boundary_at(point.y).min(app::pdf_row_count());
                self.ivars().drag_boundary.set(boundary);
                self.scrollRectToVisible(NSRect::new(
                    NSPoint::new(0.0, point.y),
                    NSSize::new(1.0, 1.0),
                ));
                self.setNeedsDisplay(true);
            });
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            guard("pdf_selection_mouse_up", || {
                let from = self.ivars().drag_from.get();
                let dragging = self.ivars().dragging.get();
                let boundary = self.ivars().drag_boundary.get();
                self.ivars().drag_from.set(NO_DRAG_ROW);
                self.ivars().dragging.set(false);
                self.setNeedsDisplay(true);
                if from == NO_DRAG_ROW {
                    return;
                }
                if dragging {
                    app::pdf_move_row(from, boundary);
                } else {
                    app::pdf_toggle_row(from);
                }
            });
        }
    }
);

impl PdfSelectionView {
    pub(crate) fn create(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(PdfSelectionIvars {
            drag_from: Cell::new(NO_DRAG_ROW),
            drag_boundary: Cell::new(0),
            press_y: Cell::new(0.0),
            dragging: Cell::new(false),
        });
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(528.0, 1.0));
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

fn pdf_row_at(y: f64) -> usize {
    let row = (y / PDF_ROW_H).floor();
    if row < 0.0 { NO_DRAG_ROW } else { row as usize }
}

fn pdf_boundary_at(y: f64) -> usize {
    let boundary = (y / PDF_ROW_H).round();
    if boundary <= 0.0 {
        0
    } else {
        boundary as usize
    }
}

fn draw_pdf_selection(view: &PdfSelectionView) {
    let bounds = view.bounds();
    let width = bounds.size.width;
    let height = bounds.size.height;
    let rows = with_state(|s: &mut AppState| {
        let Some(pdf) = &s.pdf_export else {
            return Vec::new();
        };
        pdf.order
            .iter()
            .filter_map(|id| {
                let clip = s
                    .store
                    .clips
                    .iter()
                    .find(|clip| clip.id == *id && clip.kind == ClipKind::Image)?;
                Some((
                    clip.hash,
                    clip.blob.clone(),
                    image_title(clip),
                    clip.subtitle(),
                    pdf.selected.contains(&clip.id),
                ))
            })
            .collect::<Vec<_>>()
    });

    if rows.is_empty() {
        draw_text(
            "No images to export",
            NSRect::new(
                NSPoint::new(0.0, height / 2.0 - 10.0),
                NSSize::new(width, 20.0),
            ),
            &NSFont::systemFontOfSize(13.0),
            &NSColor::secondaryLabelColor(),
            NSTextAlignment::Center,
        );
        return;
    }

    let dragging = view.ivars().dragging.get();
    let drag_from = view.ivars().drag_from.get();
    let drag_boundary = view.ivars().drag_boundary.get();

    let title_font = NSFont::systemFontOfSize(13.0);
    let subtitle_font = NSFont::systemFontOfSize(11.0);
    let label = NSColor::labelColor();
    let secondary = NSColor::secondaryLabelColor();
    let accent = NSColor::selectedContentBackgroundColor();
    let border = NSColor::separatorColor();
    let white = NSColor::whiteColor();

    with_state(|s: &mut AppState| {
        for (index, (hash, blob, title, subtitle, selected)) in rows.iter().enumerate() {
            let y = index as f64 * PDF_ROW_H;
            let row_rect = NSRect::new(NSPoint::new(0.0, y), NSSize::new(width, PDF_ROW_H));
            if *selected {
                fill_rounded(row_rect, 0.0, &accent.colorWithAlphaComponent(0.08));
            }
            if dragging && index == drag_from {
                fill_rounded(row_rect, 0.0, &accent.colorWithAlphaComponent(0.16));
            }

            let checkbox = NSRect::new(NSPoint::new(14.0, y + 24.0), NSSize::new(16.0, 16.0));
            let box_path =
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(checkbox, 4.0, 4.0);
            if *selected {
                accent.setFill();
                box_path.fill();
                // The view is flipped, so y grows downwards: draw the check
                // down from the left arm to the bottom vertex, then up-right.
                let check = NSBezierPath::new();
                check.moveToPoint(NSPoint::new(
                    checkbox.origin.x + 4.0,
                    checkbox.origin.y + 8.0,
                ));
                check.lineToPoint(NSPoint::new(
                    checkbox.origin.x + 7.0,
                    checkbox.origin.y + 11.5,
                ));
                check.lineToPoint(NSPoint::new(
                    checkbox.origin.x + 12.0,
                    checkbox.origin.y + 5.0,
                ));
                white.setStroke();
                check.setLineWidth(1.6);
                check.stroke();
            } else {
                NSColor::controlBackgroundColor().setFill();
                box_path.fill();
                border.setStroke();
                box_path.setLineWidth(1.0);
                box_path.stroke();
            }

            let thumb_rect = NSRect::new(NSPoint::new(42.0, y + 10.0), NSSize::new(44.0, 44.0));
            match app::thumbnail_for(s, *hash, blob.as_deref()) {
                Some(image) => draw_image_fit(thumb_rect, &image),
                None => {
                    fill_rounded(thumb_rect, 4.0, &border.colorWithAlphaComponent(0.35));
                    draw_text(
                        "IMG",
                        NSRect::new(
                            NSPoint::new(thumb_rect.origin.x, thumb_rect.origin.y + 15.0),
                            NSSize::new(thumb_rect.size.width, 12.0),
                        ),
                        &NSFont::systemFontOfSize(9.0),
                        &secondary,
                        NSTextAlignment::Center,
                    );
                }
            }

            draw_text(
                title,
                NSRect::new(
                    NSPoint::new(96.0, y + 13.0),
                    NSSize::new(width - 112.0, 18.0),
                ),
                &title_font,
                &label,
                NSTextAlignment::Left,
            );
            draw_text(
                subtitle,
                NSRect::new(
                    NSPoint::new(96.0, y + 34.0),
                    NSSize::new(width - 112.0, 15.0),
                ),
                &subtitle_font,
                &secondary,
                NSTextAlignment::Left,
            );

            if index + 1 != rows.len() {
                fill_rect(
                    NSRect::new(
                        NSPoint::new(96.0, y + PDF_ROW_H - 0.5),
                        NSSize::new(width - 112.0, 0.5),
                    ),
                    &border.colorWithAlphaComponent(0.5),
                );
            }
        }

        if dragging {
            let line_y = (drag_boundary as f64 * PDF_ROW_H).clamp(1.0, height - 1.0);
            fill_rect(
                NSRect::new(
                    NSPoint::new(8.0, line_y - 1.0),
                    NSSize::new(width - 16.0, 2.0),
                ),
                &accent,
            );
        }
    });
}

pub(crate) struct PdfExportParts {
    pub window: Retained<NSWindow>,
    pub view: Retained<PdfSelectionView>,
    pub status: Retained<NSTextField>,
    pub select_all: Retained<NSButton>,
    pub select_none: Retained<NSButton>,
    pub save: Retained<NSButton>,
}

impl PdfExportParts {
    /// Hide the export window. The parts keep the window alive; a later export
    /// replaces them.
    pub(crate) fn close(&self) {
        self.window.orderOut(None);
    }
}

pub(crate) fn build_pdf_export(
    mtm: MainThreadMarker,
    delegate: &app::AppDelegate,
    image_count: usize,
) -> Option<PdfExportParts> {
    let target = as_any(delegate);
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(560.0, 560.0)),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(&NSString::from_str("Export Images to PDF"));
    unsafe { window.setReleasedWhenClosed(false) };
    window.center();
    window.setBackgroundColor(Some(&NSColor::windowBackgroundColor()));

    let content = window.contentView()?;
    let header = NSTextField::labelWithString(
        &NSString::from_str("Select images. Each selected image is one A4 page."),
        mtm,
    );
    header.setFrame(NSRect::new(
        NSPoint::new(16.0, 524.0),
        NSSize::new(528.0, 18.0),
    ));
    header.setFont(Some(&NSFont::systemFontOfSize(12.0)));
    header.setTextColor(Some(&NSColor::secondaryLabelColor()));
    content.addSubview(&header);

    let view = PdfSelectionView::create(mtm);
    let document_height = (image_count as f64 * PDF_ROW_H + 16.0).max(448.0);
    view.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(528.0, document_height),
    ));

    let scroll = NSScrollView::new(mtm);
    scroll.setFrame(NSRect::new(
        NSPoint::new(16.0, 64.0),
        NSSize::new(528.0, 448.0),
    ));
    scroll.setBorderType(NSBorderType::BezelBorder);
    scroll.setDrawsBackground(true);
    scroll.setHasVerticalScroller(true);
    scroll.setAutohidesScrollers(true);
    scroll.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
    scroll.setDocumentView(Some(&view));
    content.addSubview(&scroll);

    let status = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    status.setFrame(NSRect::new(
        NSPoint::new(16.0, 24.0),
        NSSize::new(160.0, 18.0),
    ));
    status.setFont(Some(&NSFont::systemFontOfSize(11.0)));
    status.setTextColor(Some(&NSColor::secondaryLabelColor()));
    content.addSubview(&status);

    let select_all = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Select All"),
            Some(target),
            Some(sel!(pdfSelectAll:)),
            mtm,
        )
    };
    select_all.setFrame(NSRect::new(
        NSPoint::new(184.0, 16.0),
        NSSize::new(84.0, 26.0),
    ));
    style_pdf_button(&select_all);
    select_all.setToolTip(Some(&NSString::from_str("Select every image")));
    content.addSubview(&select_all);

    let select_none = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Select None"),
            Some(target),
            Some(sel!(pdfSelectNone:)),
            mtm,
        )
    };
    select_none.setFrame(NSRect::new(
        NSPoint::new(272.0, 16.0),
        NSSize::new(92.0, 26.0),
    ));
    style_pdf_button(&select_none);
    select_none.setToolTip(Some(&NSString::from_str("Deselect every image")));
    content.addSubview(&select_none);

    let save = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Save PDF…"),
            Some(target),
            Some(sel!(pdfSave:)),
            mtm,
        )
    };
    save.setFrame(NSRect::new(
        NSPoint::new(368.0, 16.0),
        NSSize::new(100.0, 26.0),
    ));
    style_pdf_button(&save);
    save.setKeyEquivalent(&NSString::from_str("s"));
    save.setToolTip(Some(&NSString::from_str(
        "Choose where to save the PDF (⌘S)",
    )));
    content.addSubview(&save);

    let cancel = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Cancel"),
            Some(target),
            Some(sel!(pdfCancel:)),
            mtm,
        )
    };
    cancel.setFrame(NSRect::new(
        NSPoint::new(472.0, 16.0),
        NSSize::new(72.0, 26.0),
    ));
    style_pdf_button(&cancel);
    cancel.setKeyEquivalent(&NSString::from_str("\u{1b}"));
    cancel.setToolTip(Some(&NSString::from_str("Close this window (esc)")));
    content.addSubview(&cancel);

    Some(PdfExportParts {
        window,
        view,
        status,
        select_all,
        select_none,
        save,
    })
}

fn style_pdf_button(button: &NSButton) {
    button.setBezelStyle(NSBezelStyle::Automatic);
    button.setControlSize(NSControlSize::Small);
    button.setFont(Some(&NSFont::systemFontOfSize(11.0)));
}

// ---------------------------------------------------------------------------
// Ignored apps list
// ---------------------------------------------------------------------------

pub(crate) struct IgnoredIvars {
    selected: Cell<usize>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "RCIgnoredAppsView"]
    #[ivars = IgnoredIvars]
    pub(crate) struct IgnoredAppsView;

    unsafe impl NSObjectProtocol for IgnoredAppsView {}

    impl IgnoredAppsView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            guard("draw_rect", || {
                draw_ignored(self);
            });
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            guard("mouse_down", || {
                let point = self.convertPoint_fromView(event.locationInWindow(), None);
                let row = ((point.y - 8.0) / 26.0).floor() as isize;
                let count = with_state(|s: &mut AppState| s.settings.ignored_apps.len());
                let value = if row >= 0 && (row as usize) < count {
                    row as usize
                } else {
                    usize::MAX
                };
                self.ivars().selected.set(value);
                self.setNeedsDisplay(true);
            });
        }
    }
);

impl IgnoredAppsView {
    pub(crate) fn create(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(IgnoredIvars {
            selected: Cell::new(usize::MAX),
        });
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    pub(crate) fn selected(&self) -> usize {
        self.ivars().selected.get()
    }

    pub(crate) fn clear_selection(&self) {
        self.ivars().selected.set(usize::MAX);
    }
}

fn draw_ignored(view: &IgnoredAppsView) {
    let bounds = view.bounds();
    let width = bounds.size.width;
    let height = bounds.size.height;
    let outer = NSRect::new(
        NSPoint::new(0.5, 0.5),
        NSSize::new((width - 1.0).max(1.0), (height - 1.0).max(1.0)),
    );
    let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(outer, 6.0, 6.0);
    NSColor::controlBackgroundColor().setFill();
    path.fill();
    NSColor::separatorColor().setStroke();
    path.setLineWidth(1.0);
    path.stroke();

    let apps = with_state(|s: &mut AppState| s.settings.ignored_apps.clone());
    let selected = view.selected();

    if apps.is_empty() {
        let font = NSFont::systemFontOfSize(12.0);
        let rect = NSRect::new(
            NSPoint::new(8.0, height / 2.0 - 9.0),
            NSSize::new(width - 16.0, 18.0),
        );
        draw_text(
            "No ignored apps",
            rect,
            &font,
            &NSColor::secondaryLabelColor(),
            NSTextAlignment::Center,
        );
        return;
    }

    let name_font = NSFont::systemFontOfSize(12.0);
    let bundle_font = NSFont::systemFontOfSize(10.0);
    let label = NSColor::labelColor();
    let secondary = NSColor::secondaryLabelColor();
    let white = NSColor::whiteColor();
    let selected_bg = NSColor::selectedContentBackgroundColor();

    for (i, ignored) in apps.iter().enumerate() {
        let y = 8.0 + i as f64 * 26.0;
        if y + 24.0 > height {
            break;
        }
        let is_selected = i == selected;
        if is_selected {
            fill_rounded(
                NSRect::new(NSPoint::new(5.0, y), NSSize::new(width - 10.0, 24.0)),
                4.0,
                &selected_bg,
            );
        }
        let text = format!("{}  —  {}", ignored.name, ignored.bundle_id);
        let rect = NSRect::new(NSPoint::new(14.0, y + 5.0), NSSize::new(width - 28.0, 15.0));
        draw_text(
            &text,
            rect,
            &name_font,
            if is_selected { &white } else { &label },
            NSTextAlignment::Left,
        );
        let _ = (&bundle_font, &secondary);
    }
}

// ---------------------------------------------------------------------------
// Building the popup
// ---------------------------------------------------------------------------

pub(crate) struct PanelParts {
    pub panel: Retained<ClipPanel>,
    pub search: Retained<NSSearchField>,
    pub list: Retained<ClipListView>,
    pub footer: Retained<NSTextField>,
    pub section_control: Retained<NSSegmentedControl>,
    pub clear_all_button: Retained<NSButton>,
    pub clear_unpinned_button: Retained<NSButton>,
    pub export_pdf_button: Retained<NSButton>,
    pub refresh_commands_button: Retained<NSButton>,
    pub title_symbol: Option<Retained<NSImage>>,
    pub title_symbol_selected: Option<Retained<NSImage>>,
}

pub(crate) fn build_panel(mtm: MainThreadMarker, delegate: &app::AppDelegate) -> PanelParts {
    let panel = ClipPanel::create(mtm);
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setLevel(NSFloatingWindowLevel);
    panel.setBecomesKeyOnlyIfNeeded(false);
    panel.setAcceptsMouseMovedEvents(true);
    panel.setHasShadow(true);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );

    let bg = NSVisualEffectView::new(mtm);
    bg.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(PANEL_W, PANEL_H),
    ));
    bg.setMaterial(NSVisualEffectMaterial::Popover);
    bg.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    bg.setState(NSVisualEffectState::Active);
    bg.setWantsLayer(true);
    if let Some(layer) = bg.layer() {
        layer.setCornerRadius(12.0);
        layer.setMasksToBounds(true);
    }
    panel.setContentView(Some(&bg));

    let search = NSSearchField::new(mtm);
    search.setFrame(NSRect::new(
        NSPoint::new(12.0, PANEL_H - SEARCH_H),
        NSSize::new(PANEL_W - 24.0 - 140.0, 28.0),
    ));
    search.setPlaceholderString(Some(&NSString::from_str("Search clippings")));
    search.setFont(Some(&NSFont::systemFontOfSize(13.0)));
    unsafe { search.setDelegate(Some(ProtocolObject::from_ref(delegate))) };
    bg.addSubview(&search);

    let text_label = NSString::from_str("Text");
    let images_label = NSString::from_str("Images");
    let commands_label = NSString::from_str("Commands");
    let labels = NSArray::from_slice(&[&*text_label, &*images_label, &*commands_label]);
    let section_control = unsafe {
        NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
            &labels,
            NSSegmentSwitchTracking::SelectOne,
            Some(as_any(delegate)),
            Some(sel!(sectionChanged:)),
            mtm,
        )
    };
    section_control.setFrame(NSRect::new(
        NSPoint::new(PANEL_W - 12.0 - 126.0, PANEL_H - SEARCH_H + 2.0),
        NSSize::new(126.0, 24.0),
    ));
    section_control.setSelectedSegment(0);
    for (index, symbol, description) in [
        (
            0,
            "text.alignleft",
            "Text, rich text, HTML and file clippings",
        ),
        (1, "photo", "Copied images"),
        (2, "terminal", "Terminal commands from zsh and bash"),
    ] {
        if let Some(image) = segment_symbol(symbol, description) {
            section_control.setImage_forSegment(Some(&image), index);
            section_control.setLabel_forSegment(&NSString::from_str(""), index);
        }
        section_control.setWidth_forSegment(42.0, index);
        section_control.setToolTip_forSegment(Some(&NSString::from_str(description)), index);
    }
    section_control.setAccessibilityLabel(Some(&NSString::from_str("Sections")));
    bg.addSubview(&section_control);

    let list = ClipListView::create(mtm);
    list.setFrame(NSRect::new(
        NSPoint::new(0.0, LIST_BOTTOM),
        NSSize::new(PANEL_W, PANEL_H - SEARCH_H - 8.0 - LIST_BOTTOM),
    ));
    bg.addSubview(&list);

    let footer = NSTextField::labelWithString(&NSString::from_str("0 clippings"), mtm);
    footer.setFrame(NSRect::new(
        NSPoint::new(14.0, 9.0),
        NSSize::new(148.0, 16.0),
    ));
    footer.setFont(Some(&NSFont::systemFontOfSize(10.0)));
    footer.setTextColor(Some(&NSColor::secondaryLabelColor()));
    bg.addSubview(&footer);

    let clear_all_button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Clear All"),
            Some(as_any(delegate)),
            Some(sel!(clearCurrentSection:)),
            mtm,
        )
    };
    clear_all_button.setFrame(NSRect::new(
        NSPoint::new(248.0, 6.0),
        NSSize::new(72.0, 22.0),
    ));
    clear_all_button.setBezelStyle(NSBezelStyle::Automatic);
    clear_all_button.setControlSize(NSControlSize::Small);
    clear_all_button.setFont(Some(&NSFont::systemFontOfSize(10.0)));
    clear_all_button.setToolTip(Some(&NSString::from_str(
        "Delete every clipping in this tab, including pinned rows",
    )));
    bg.addSubview(&clear_all_button);

    let clear_unpinned_button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Clear Unpinned"),
            Some(as_any(delegate)),
            Some(sel!(clearCurrentUnpinned:)),
            mtm,
        )
    };
    clear_unpinned_button.setFrame(NSRect::new(
        NSPoint::new(328.0, 6.0),
        NSSize::new(118.0, 22.0),
    ));
    clear_unpinned_button.setBezelStyle(NSBezelStyle::Automatic);
    clear_unpinned_button.setControlSize(NSControlSize::Small);
    clear_unpinned_button.setFont(Some(&NSFont::systemFontOfSize(10.0)));
    clear_unpinned_button.setToolTip(Some(&NSString::from_str(
        "Delete unpinned clippings in this tab; pinned rows are kept",
    )));
    bg.addSubview(&clear_unpinned_button);

    let export_pdf_button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("PDF…"),
            Some(as_any(delegate)),
            Some(sel!(exportImagesPdf:)),
            mtm,
        )
    };
    export_pdf_button.setFrame(NSRect::new(
        NSPoint::new(178.0, 6.0),
        NSSize::new(62.0, 22.0),
    ));
    export_pdf_button.setBezelStyle(NSBezelStyle::Automatic);
    export_pdf_button.setControlSize(NSControlSize::Small);
    export_pdf_button.setFont(Some(&NSFont::systemFontOfSize(10.0)));
    export_pdf_button.setToolTip(Some(&NSString::from_str(
        "Choose images and export them to a PDF (⌘E)",
    )));
    export_pdf_button.setHidden(true);
    bg.addSubview(&export_pdf_button);

    let refresh_commands_button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Refresh"),
            Some(as_any(delegate)),
            Some(sel!(refreshCommands:)),
            mtm,
        )
    };
    refresh_commands_button.setFrame(NSRect::new(
        NSPoint::new(384.0, 6.0),
        NSSize::new(62.0, 22.0),
    ));
    refresh_commands_button.setBezelStyle(NSBezelStyle::Automatic);
    refresh_commands_button.setControlSize(NSControlSize::Small);
    refresh_commands_button.setFont(Some(&NSFont::systemFontOfSize(10.0)));
    refresh_commands_button.setToolTip(Some(&NSString::from_str(
        "Read new commands from your zsh and bash history (⌘R)",
    )));
    refresh_commands_button.setHidden(true);
    bg.addSubview(&refresh_commands_button);

    let title_symbol = tinted_symbol(
        "pencil",
        "Custom title",
        10.0,
        &NSColor::labelColor().colorWithAlphaComponent(0.72),
    );
    let title_symbol_selected = tinted_symbol(
        "pencil",
        "Custom title",
        10.0,
        &NSColor::whiteColor().colorWithAlphaComponent(0.95),
    );

    PanelParts {
        panel,
        search,
        list,
        footer,
        section_control,
        clear_all_button,
        clear_unpinned_button,
        export_pdf_button,
        refresh_commands_button,
        title_symbol,
        title_symbol_selected,
    }
}

fn tinted_symbol(
    name: &str,
    description: &str,
    size: f64,
    color: &NSColor,
) -> Option<Retained<NSImage>> {
    let base = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(name),
        Some(&NSString::from_str(description)),
    )?;
    let size_config = NSImageSymbolConfiguration::configurationWithPointSize_weight(size, 0.0);
    let tint = NSImageSymbolConfiguration::configurationWithHierarchicalColor(color);
    let config = size_config.configurationByApplyingConfiguration(&tint);
    base.imageWithSymbolConfiguration(&config)
}

fn segment_symbol(name: &str, description: &str) -> Option<Retained<NSImage>> {
    let base = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(name),
        Some(&NSString::from_str(description)),
    )?;
    let config = NSImageSymbolConfiguration::configurationWithPointSize_weight(13.0, 0.0);
    let image = base.imageWithSymbolConfiguration(&config)?;
    image.setTemplate(true);
    Some(image)
}

// ---------------------------------------------------------------------------
// Preferences
// ---------------------------------------------------------------------------

pub(crate) struct PrefsParts {
    pub window: Retained<NSWindow>,
    pub text_max_field: Retained<NSTextField>,
    pub images_max_field: Retained<NSTextField>,
    pub commands_max_field: Retained<NSTextField>,
    pub record_box: Retained<NSButton>,
    pub confirm_box: Retained<NSButton>,
    pub paste_box: Retained<NSButton>,
    pub login_box: Retained<NSButton>,
    pub shortcut_button: Retained<NSButton>,
    pub pinned_background_well: Retained<NSColorWell>,
    pub pinned_indicator_well: Retained<NSColorWell>,
    pub ignored_view: Retained<IgnoredAppsView>,
    pub history_summary: Retained<NSTextField>,
    pub history_storage: Retained<NSTextField>,
    pub delete_all_clippings: Retained<NSButton>,
    pub delete_all_images: Retained<NSButton>,
    pub delete_all_text: Retained<NSButton>,
    pub delete_unpinned_text: Retained<NSButton>,
    pub delete_unpinned_images: Retained<NSButton>,
}

fn label(text: &str, frame: NSRect, size: f64, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    field.setFrame(frame);
    field.setFont(Some(&NSFont::systemFontOfSize(size)));
    field.setTextColor(Some(&NSColor::labelColor()));
    field
}

fn heading(text: &str, frame: NSRect, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    field.setFrame(frame);
    field.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
    field.setTextColor(Some(&NSColor::labelColor()));
    field
}

fn secondary_label(
    text: &str,
    frame: NSRect,
    size: f64,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    field.setFrame(frame);
    field.setFont(Some(&NSFont::systemFontOfSize(size)));
    field.setTextColor(Some(&NSColor::secondaryLabelColor()));
    field
}

fn history_limit_field(
    frame: NSRect,
    action: Sel,
    label: &str,
    tooltip: &str,
    target: &AnyObject,
    delegate: &app::AppDelegate,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    let field = NSTextField::new(mtm);
    field.setFrame(frame);
    field.setFont(Some(&NSFont::systemFontOfSize(13.0)));
    field.setEditable(true);
    field.setBezeled(true);
    field.setDrawsBackground(true);
    field.setToolTip(Some(&NSString::from_str(tooltip)));
    field.setAccessibilityLabel(Some(&NSString::from_str(label)));
    unsafe {
        field.setTarget(Some(target));
        field.setAction(Some(action));
        field.setDelegate(Some(ProtocolObject::from_ref(delegate)));
    }
    field
}

pub(crate) fn build_preferences(mtm: MainThreadMarker, delegate: &app::AppDelegate) -> PrefsParts {
    let target = as_any(delegate);

    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(500.0, 478.0)),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(&NSString::from_str("AntiRustPaste Preferences"));
    unsafe { window.setReleasedWhenClosed(false) };
    window.center();
    window.setBackgroundColor(Some(&NSColor::windowBackgroundColor()));

    let tabs = NSTabView::new(mtm);
    tabs.setFrame(NSRect::new(
        NSPoint::new(16.0, 16.0),
        NSSize::new(468.0, 446.0),
    ));

    // ---- General ----
    let general = NSView::new(mtm);
    general.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(468.0, 408.0),
    ));

    general.addSubview(&heading(
        "Clipboard History",
        NSRect::new(NSPoint::new(16.0, 370.0), NSSize::new(400.0, 18.0)),
        mtm,
    ));

    general.addSubview(&label(
        "Remember last",
        NSRect::new(NSPoint::new(16.0, 336.0), NSSize::new(100.0, 20.0)),
        13.0,
        mtm,
    ));

    let text_max_field = history_limit_field(
        NSRect::new(NSPoint::new(118.0, 332.0), NSSize::new(50.0, 24.0)),
        sel!(textLimitChanged:),
        "Text history limit",
        "How many text clippings to keep. Includes rich text, HTML and file rows.",
        target,
        delegate,
        mtm,
    );
    general.addSubview(&text_max_field);
    general.addSubview(&label(
        "text",
        NSRect::new(NSPoint::new(172.0, 336.0), NSSize::new(30.0, 20.0)),
        13.0,
        mtm,
    ));

    let images_max_field = history_limit_field(
        NSRect::new(NSPoint::new(208.0, 332.0), NSSize::new(50.0, 24.0)),
        sel!(imagesLimitChanged:),
        "Image history limit",
        "How many images to keep.",
        target,
        delegate,
        mtm,
    );
    general.addSubview(&images_max_field);
    general.addSubview(&label(
        "images",
        NSRect::new(NSPoint::new(262.0, 336.0), NSSize::new(42.0, 20.0)),
        13.0,
        mtm,
    ));

    let commands_max_field = history_limit_field(
        NSRect::new(NSPoint::new(310.0, 332.0), NSSize::new(50.0, 24.0)),
        sel!(commandsLimitChanged:),
        "Command history limit",
        "How many shell commands to keep. The bash default is 500.",
        target,
        delegate,
        mtm,
    );
    general.addSubview(&commands_max_field);
    general.addSubview(&label(
        "commands",
        NSRect::new(NSPoint::new(364.0, 336.0), NSSize::new(72.0, 20.0)),
        13.0,
        mtm,
    ));

    let record_box = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str("Record clipboard history"),
            Some(target),
            Some(sel!(recordToggled:)),
            mtm,
        )
    };
    record_box.setFrame(NSRect::new(
        NSPoint::new(16.0, 300.0),
        NSSize::new(420.0, 20.0),
    ));
    general.addSubview(&record_box);

    let confirm_box = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str("Confirm before deleting"),
            Some(target),
            Some(sel!(confirmToggled:)),
            mtm,
        )
    };
    confirm_box.setFrame(NSRect::new(
        NSPoint::new(16.0, 272.0),
        NSSize::new(420.0, 20.0),
    ));
    general.addSubview(&confirm_box);

    let paste_box = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str("Paste after choosing a clipping (requires Accessibility)"),
            Some(target),
            Some(sel!(pasteToggled:)),
            mtm,
        )
    };
    paste_box.setFrame(NSRect::new(
        NSPoint::new(16.0, 244.0),
        NSSize::new(436.0, 20.0),
    ));
    general.addSubview(&paste_box);

    let login_box = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str("Start AntiRustPaste at login"),
            Some(target),
            Some(sel!(startAtLoginToggled:)),
            mtm,
        )
    };
    login_box.setFrame(NSRect::new(
        NSPoint::new(16.0, 216.0),
        NSSize::new(420.0, 20.0),
    ));
    general.addSubview(&login_box);

    let separator = NSBox::new(mtm);
    separator.setBoxType(NSBoxType::Separator);
    separator.setFrame(NSRect::new(
        NSPoint::new(16.0, 196.0),
        NSSize::new(436.0, 1.0),
    ));
    general.addSubview(&separator);

    general.addSubview(&heading(
        "Global Shortcut",
        NSRect::new(NSPoint::new(16.0, 166.0), NSSize::new(300.0, 18.0)),
        mtm,
    ));

    let shortcut_button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("⇧⌘V"),
            Some(target),
            Some(sel!(shortcutButtonClicked:)),
            mtm,
        )
    };
    shortcut_button.setFrame(NSRect::new(
        NSPoint::new(16.0, 126.0),
        NSSize::new(160.0, 30.0),
    ));
    general.addSubview(&shortcut_button);

    general.addSubview(&secondary_label(
        "Click to record a new shortcut",
        NSRect::new(NSPoint::new(188.0, 132.0), NSSize::new(260.0, 18.0)),
        11.0,
        mtm,
    ));

    general.addSubview(&secondary_label(
        "History is stored in ~/Library/Application Support/AntiRustPaste",
        NSRect::new(NSPoint::new(16.0, 88.0), NSSize::new(436.0, 18.0)),
        11.0,
        mtm,
    ));

    general.addSubview(&secondary_label(
        "Click the menu bar icon or use the shortcut to open the history.",
        NSRect::new(NSPoint::new(16.0, 64.0), NSSize::new(436.0, 18.0)),
        11.0,
        mtm,
    ));

    let cache_button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Clear Thumbnail Cache"),
            Some(target),
            Some(sel!(clearThumbnailCache:)),
            mtm,
        )
    };
    cache_button.setFrame(NSRect::new(
        NSPoint::new(16.0, 24.0),
        NSSize::new(176.0, 30.0),
    ));
    general.addSubview(&cache_button);

    general.addSubview(&secondary_label(
        "Temporary previews; safe to delete.",
        NSRect::new(NSPoint::new(202.0, 30.0), NSSize::new(250.0, 18.0)),
        11.0,
        mtm,
    ));

    // ---- Appearance ----
    let appearance = NSView::new(mtm);
    appearance.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(468.0, 408.0),
    ));

    appearance.addSubview(&heading(
        "Pinned Clips",
        NSRect::new(NSPoint::new(16.0, 370.0), NSSize::new(400.0, 18.0)),
        mtm,
    ));
    appearance.addSubview(&secondary_label(
        "Pinned rows keep a tinted background and a filled marker.",
        NSRect::new(NSPoint::new(16.0, 340.0), NSSize::new(436.0, 18.0)),
        11.0,
        mtm,
    ));

    let (pinned_background, pinned_indicator) =
        with_state(|s: &mut AppState| (s.settings.pinned_background, s.settings.pinned_indicator));

    appearance.addSubview(&label(
        "Row background",
        NSRect::new(NSPoint::new(16.0, 298.0), NSSize::new(140.0, 20.0)),
        13.0,
        mtm,
    ));

    let pinned_background_well = NSColorWell::colorWellWithStyle(NSColorWellStyle::Default, mtm);
    pinned_background_well.setFrame(NSRect::new(
        NSPoint::new(168.0, 290.0),
        NSSize::new(44.0, 28.0),
    ));
    pinned_background_well.setSupportsAlpha(false);
    pinned_background_well.setColor(&color_from_rgb(pinned_background));
    pinned_background_well
        .setAccessibilityLabel(Some(&NSString::from_str("Pinned row background color")));
    unsafe {
        pinned_background_well.setTarget(Some(target));
        pinned_background_well.setAction(Some(sel!(pinnedBackgroundChanged:)));
    }
    appearance.addSubview(&pinned_background_well);

    appearance.addSubview(&label(
        "Marker",
        NSRect::new(NSPoint::new(16.0, 258.0), NSSize::new(140.0, 20.0)),
        13.0,
        mtm,
    ));

    let pinned_indicator_well = NSColorWell::colorWellWithStyle(NSColorWellStyle::Default, mtm);
    pinned_indicator_well.setFrame(NSRect::new(
        NSPoint::new(168.0, 250.0),
        NSSize::new(44.0, 28.0),
    ));
    pinned_indicator_well.setSupportsAlpha(false);
    pinned_indicator_well.setColor(&color_from_rgb(pinned_indicator));
    pinned_indicator_well.setAccessibilityLabel(Some(&NSString::from_str("Pinned marker color")));
    unsafe {
        pinned_indicator_well.setTarget(Some(target));
        pinned_indicator_well.setAction(Some(sel!(pinnedIndicatorChanged:)));
    }
    appearance.addSubview(&pinned_indicator_well);

    appearance.addSubview(&secondary_label(
        "The background is a soft tint so text stays readable.",
        NSRect::new(NSPoint::new(16.0, 214.0), NSSize::new(436.0, 18.0)),
        11.0,
        mtm,
    ));
    appearance.addSubview(&secondary_label(
        "Changes apply when you reopen the history list.",
        NSRect::new(NSPoint::new(16.0, 190.0), NSSize::new(436.0, 18.0)),
        11.0,
        mtm,
    ));

    let restore_button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Restore Defaults"),
            Some(target),
            Some(sel!(restorePinnedColors:)),
            mtm,
        )
    };
    restore_button.setFrame(NSRect::new(
        NSPoint::new(16.0, 140.0),
        NSSize::new(150.0, 30.0),
    ));
    restore_button.setBezelStyle(NSBezelStyle::Automatic);
    restore_button.setControlSize(NSControlSize::Small);
    restore_button.setFont(Some(&NSFont::systemFontOfSize(11.0)));
    restore_button.setToolTip(Some(&NSString::from_str(
        "Restore the original yellow background and violet marker",
    )));
    appearance.addSubview(&restore_button);

    appearance.addSubview(&secondary_label(
        "Restores the original yellow card and violet marker.",
        NSRect::new(NSPoint::new(180.0, 146.0), NSSize::new(272.0, 18.0)),
        11.0,
        mtm,
    ));

    // ---- Exceptions ----
    let exceptions = NSView::new(mtm);
    exceptions.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(468.0, 408.0),
    ));

    exceptions.addSubview(&heading(
        "Ignored Apps",
        NSRect::new(NSPoint::new(16.0, 370.0), NSSize::new(400.0, 18.0)),
        mtm,
    ));
    exceptions.addSubview(&secondary_label(
        "Clipboard changes copied from these apps are not recorded.",
        NSRect::new(NSPoint::new(16.0, 344.0), NSSize::new(436.0, 18.0)),
        11.0,
        mtm,
    ));

    let ignored_view = IgnoredAppsView::create(mtm);
    let ignored_height = with_state(|s: &mut AppState| {
        (16.0 + s.settings.ignored_apps.len() as f64 * 26.0).max(216.0)
    });
    ignored_view.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(436.0, ignored_height),
    ));

    let ignored_scroll = NSScrollView::new(mtm);
    ignored_scroll.setFrame(NSRect::new(
        NSPoint::new(16.0, 116.0),
        NSSize::new(436.0, 216.0),
    ));
    ignored_scroll.setBorderType(NSBorderType::NoBorder);
    ignored_scroll.setDrawsBackground(false);
    ignored_scroll.setHasVerticalScroller(true);
    ignored_scroll.setAutohidesScrollers(true);
    ignored_scroll.setDocumentView(Some(&ignored_view));
    exceptions.addSubview(&ignored_scroll);

    let add_button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Add App…"),
            Some(target),
            Some(sel!(addIgnoredApp:)),
            mtm,
        )
    };
    add_button.setFrame(NSRect::new(
        NSPoint::new(16.0, 76.0),
        NSSize::new(110.0, 30.0),
    ));
    exceptions.addSubview(&add_button);

    let remove_button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Remove"),
            Some(target),
            Some(sel!(removeIgnoredApp:)),
            mtm,
        )
    };
    remove_button.setFrame(NSRect::new(
        NSPoint::new(134.0, 76.0),
        NSSize::new(100.0, 30.0),
    ));
    exceptions.addSubview(&remove_button);

    exceptions.addSubview(&secondary_label(
        "Use Add App… to choose an application.",
        NSRect::new(NSPoint::new(16.0, 46.0), NSSize::new(436.0, 18.0)),
        11.0,
        mtm,
    ));

    // ---- History ----
    let history = NSView::new(mtm);
    history.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(468.0, 408.0),
    ));

    history.addSubview(&heading(
        "Delete History",
        NSRect::new(NSPoint::new(16.0, 370.0), NSSize::new(400.0, 18.0)),
        mtm,
    ));
    history.addSubview(&secondary_label(
        "These actions delete clippings only. Terminal commands are kept.",
        NSRect::new(NSPoint::new(16.0, 342.0), NSSize::new(436.0, 18.0)),
        11.0,
        mtm,
    ));
    let history_summary = secondary_label(
        "0 text · 0 images · 0 pinned",
        NSRect::new(NSPoint::new(16.0, 322.0), NSSize::new(436.0, 18.0)),
        11.0,
        mtm,
    );
    history.addSubview(&history_summary);

    let history_storage = secondary_label(
        "Disk usage: calculating…",
        NSRect::new(NSPoint::new(16.0, 298.0), NSSize::new(436.0, 18.0)),
        11.0,
        mtm,
    );
    history_storage.setToolTip(Some(&NSString::from_str(
        "History includes the index and stored blobs. Cache holds thumbnails and Quick Look previews; macOS clears it at boot.",
    )));
    history.addSubview(&history_storage);

    history.addSubview(&label(
        "All Clippings",
        NSRect::new(NSPoint::new(16.0, 270.0), NSSize::new(200.0, 20.0)),
        13.0,
        mtm,
    ));
    let delete_all_clippings = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Delete All Clippings…"),
            Some(target),
            Some(sel!(deleteAllClippings:)),
            mtm,
        )
    };
    delete_all_clippings.setFrame(NSRect::new(
        NSPoint::new(16.0, 234.0),
        NSSize::new(190.0, 30.0),
    ));
    delete_all_clippings.setToolTip(Some(&NSString::from_str(
        "Delete every text and image clipping, including pinned ones",
    )));
    history.addSubview(&delete_all_clippings);
    history.addSubview(&secondary_label(
        "Text and images, pinned included.",
        NSRect::new(NSPoint::new(216.0, 240.0), NSSize::new(236.0, 18.0)),
        11.0,
        mtm,
    ));

    history.addSubview(&label(
        "Images",
        NSRect::new(NSPoint::new(16.0, 202.0), NSSize::new(200.0, 20.0)),
        13.0,
        mtm,
    ));
    let delete_all_images = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Delete All Images…"),
            Some(target),
            Some(sel!(deleteAllImages:)),
            mtm,
        )
    };
    delete_all_images.setFrame(NSRect::new(
        NSPoint::new(16.0, 166.0),
        NSSize::new(170.0, 30.0),
    ));
    delete_all_images.setToolTip(Some(&NSString::from_str(
        "Delete every image clipping and its thumbnail, including pinned ones",
    )));
    history.addSubview(&delete_all_images);

    let delete_unpinned_images = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Delete Unpinned Images…"),
            Some(target),
            Some(sel!(deleteUnpinnedImages:)),
            mtm,
        )
    };
    delete_unpinned_images.setFrame(NSRect::new(
        NSPoint::new(194.0, 166.0),
        NSSize::new(190.0, 30.0),
    ));
    delete_unpinned_images.setToolTip(Some(&NSString::from_str(
        "Delete every unpinned image clipping; pinned images are kept",
    )));
    history.addSubview(&delete_unpinned_images);
    history.addSubview(&secondary_label(
        "The unpinned action keeps pinned images.",
        NSRect::new(NSPoint::new(16.0, 138.0), NSSize::new(436.0, 18.0)),
        11.0,
        mtm,
    ));

    history.addSubview(&label(
        "Text",
        NSRect::new(NSPoint::new(16.0, 110.0), NSSize::new(200.0, 20.0)),
        13.0,
        mtm,
    ));
    let delete_all_text = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Delete All Text…"),
            Some(target),
            Some(sel!(deleteAllText:)),
            mtm,
        )
    };
    delete_all_text.setFrame(NSRect::new(
        NSPoint::new(16.0, 74.0),
        NSSize::new(170.0, 30.0),
    ));
    delete_all_text.setToolTip(Some(&NSString::from_str(
        "Delete every text, rich text, HTML and file clipping, including pinned ones",
    )));
    history.addSubview(&delete_all_text);

    let delete_unpinned_text = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Delete Unpinned Text…"),
            Some(target),
            Some(sel!(deleteUnpinnedText:)),
            mtm,
        )
    };
    delete_unpinned_text.setFrame(NSRect::new(
        NSPoint::new(194.0, 74.0),
        NSSize::new(190.0, 30.0),
    ));
    delete_unpinned_text.setToolTip(Some(&NSString::from_str(
        "Delete unpinned text, rich text, HTML and file clippings; pinned text is kept",
    )));
    history.addSubview(&delete_unpinned_text);
    history.addSubview(&secondary_label(
        "The unpinned action keeps pinned text, rich text, HTML and files.",
        NSRect::new(NSPoint::new(16.0, 46.0), NSSize::new(436.0, 18.0)),
        11.0,
        mtm,
    ));

    let general_item = NSTabViewItem::new();
    general_item.setLabel(&NSString::from_str("General"));
    general_item.setView(Some(&general));
    tabs.addTabViewItem(&general_item);

    let appearance_item = NSTabViewItem::new();
    appearance_item.setLabel(&NSString::from_str("Appearance"));
    appearance_item.setView(Some(&appearance));
    tabs.addTabViewItem(&appearance_item);

    let exceptions_item = NSTabViewItem::new();
    exceptions_item.setLabel(&NSString::from_str("Exceptions"));
    exceptions_item.setView(Some(&exceptions));
    tabs.addTabViewItem(&exceptions_item);

    let history_item = NSTabViewItem::new();
    history_item.setLabel(&NSString::from_str("History"));
    history_item.setView(Some(&history));
    tabs.addTabViewItem(&history_item);

    if let Some(content) = window.contentView() {
        content.addSubview(&tabs);
    }

    PrefsParts {
        window,
        text_max_field,
        images_max_field,
        commands_max_field,
        record_box,
        confirm_box,
        paste_box,
        login_box,
        shortcut_button,
        pinned_background_well,
        pinned_indicator_well,
        ignored_view,
        history_summary,
        history_storage,
        delete_all_clippings,
        delete_all_images,
        delete_all_text,
        delete_unpinned_text,
        delete_unpinned_images,
    }
}

// Keep the compiler aware that these modifier flags are used by the shortcut
// recorder in the app module through this module's imports.
