//! AppKit views: the popup history list, the ignored-apps list, the popup
//! panel itself and the preferences window.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::*;
use objc2_foundation::*;

use rustclip_core::{Clip, ClipKind, human_size};

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

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "RCClipListView"]
    #[ivars = ()]
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

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            guard("draw_rect", || {
                draw_list(self);
            });
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            guard("mouse_down", || {
                app::handle_list_click(self, event);
            });
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            guard("scroll_wheel", || {
                app::handle_list_scroll(event.scrollingDeltaY());
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
        let this = Self::alloc(mtm).set_ivars(());
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

fn dot_color(kind: ClipKind) -> Retained<NSColor> {
    match kind {
        ClipKind::Text => NSColor::systemBlueColor(),
        ClipKind::RichText => NSColor::systemPurpleColor(),
        ClipKind::Html => NSColor::systemIndigoColor(),
        ClipKind::Image => NSColor::systemGreenColor(),
        ClipKind::File => NSColor::systemOrangeColor(),
        ClipKind::Other => NSColor::systemGrayColor(),
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

    let (indices, selected, scroll, query, section, layout) = with_state(|s: &mut AppState| {
        let indices = filtered_indices(s);
        let layout = list_layout(s, &indices);
        (
            indices,
            s.selected,
            s.scroll,
            s.query.clone(),
            s.section,
            layout,
        )
    });

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
    let subtitle_font = NSFont::systemFontOfSize(11.0);
    let label_color = NSColor::labelColor();
    let secondary = NSColor::secondaryLabelColor();
    let white = NSColor::whiteColor();
    let selected_bg = NSColor::selectedContentBackgroundColor();
    let separator = NSColor::separatorColor().colorWithAlphaComponent(0.55);
    let placeholder = NSColor::separatorColor().colorWithAlphaComponent(0.35);

    let (start, end) = layout.visible_range(scroll, scroll + height);

    with_state(|s: &mut AppState| {
        draw_list_grouping(width, height, scroll, &layout, &secondary);

        for visible in start..end {
            let Some(&clip_index) = indices.get(visible) else {
                continue;
            };
            let (kind, hash, blob, title, subtitle, pinned) = {
                let Some(clip) = s.store.clips.get(clip_index) else {
                    continue;
                };
                (
                    clip.kind,
                    clip.hash,
                    clip.blob.clone(),
                    image_title(clip),
                    clip.subtitle(),
                    clip.pinned,
                )
            };
            let y = layout.row_top(visible) - scroll;
            let is_selected = visible == selected;
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
            }

            let text_left = if section == ClipboardSection::Images {
                62.0
            } else {
                36.0
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
            } else {
                let dot = NSRect::new(NSPoint::new(18.0, y + 22.0), NSSize::new(8.0, 8.0));
                let path = NSBezierPath::bezierPathWithOvalInRect(dot);
                if is_selected {
                    white.setFill();
                } else {
                    dot_color(kind).setFill();
                }
                path.fill();
            }

            let title_rect = NSRect::new(
                NSPoint::new(text_left, y + 10.0),
                NSSize::new(width - text_left - 44.0, 18.0),
            );
            let title_color = if is_selected { &white } else { &label_color };
            draw_text(
                &title,
                title_rect,
                &title_font,
                title_color,
                NSTextAlignment::Left,
            );

            let pin_rect = NSRect::new(
                NSPoint::new(width - 32.0, y + 18.0),
                NSSize::new(16.0, 18.0),
            );
            let pin_image = if is_selected {
                if pinned {
                    s.pin_filled_selected.as_ref()
                } else {
                    s.pin_outline_selected.as_ref()
                }
            } else if pinned {
                s.pin_filled.as_ref()
            } else {
                s.pin_outline.as_ref()
            };
            if let Some(image) = pin_image {
                if pinned {
                    draw_image_fit_rotated(pin_rect, image, -45.0);
                } else {
                    draw_image_fit(pin_rect, image);
                }
            } else {
                let pin_color = if is_selected {
                    white.colorWithAlphaComponent(if pinned { 1.0 } else { 0.65 })
                } else if pinned {
                    NSColor::controlAccentColor()
                } else {
                    secondary.colorWithAlphaComponent(0.72)
                };
                draw_pin(pin_rect, pinned, &pin_color);
            }

            let subtitle_rect = NSRect::new(
                NSPoint::new(text_left, y + 30.0),
                NSSize::new(width - text_left - 44.0, 15.0),
            );
            let subtitle_color = if is_selected {
                white.colorWithAlphaComponent(0.85)
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

fn draw_image_fit_rotated(rect: NSRect, image: &NSImage, degrees: f64) {
    let size = image.size();
    if size.width <= 0.0 || size.height <= 0.0 || rect.size.width <= 0.0 || rect.size.height <= 0.0
    {
        return;
    }
    let Some(context) = NSGraphicsContext::currentContext() else {
        return;
    };
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
    let center = NSPoint::new(
        fitted.origin.x + fitted.size.width / 2.0,
        fitted.origin.y + fitted.size.height / 2.0,
    );
    let transform = NSAffineTransform::new();
    transform.translateXBy_yBy(center.x, center.y);
    transform.rotateByDegrees(degrees);
    transform.translateXBy_yBy(-center.x, -center.y);

    context.saveGraphicsState();
    transform.concat();
    image.drawInRect(fitted);
    context.restoreGraphicsState();
}

fn draw_pin(rect: NSRect, filled: bool, color: &NSColor) {
    let head = NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
        NSPoint::new(rect.origin.x + 2.0, rect.origin.y + 1.0),
        NSSize::new(10.0, 8.0),
    ));
    let body = NSBezierPath::new();
    body.moveToPoint(NSPoint::new(rect.origin.x + 1.0, rect.origin.y + 9.0));
    body.lineToPoint(NSPoint::new(rect.origin.x + 13.0, rect.origin.y + 9.0));
    body.lineToPoint(NSPoint::new(rect.origin.x + 7.0, rect.origin.y + 16.0));
    body.closePath();

    if filled {
        color.setFill();
        head.fill();
        body.fill();
    } else {
        color.setStroke();
        head.setLineWidth(1.0);
        head.stroke();
        body.setLineWidth(1.0);
        body.stroke();
    }
}

fn image_title(clip: &Clip) -> String {
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
    image.drawInRect(fitted);
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
    pub clear_text_button: Retained<NSButton>,
    pub clear_images_button: Retained<NSButton>,
    pub export_pdf_button: Retained<NSButton>,
    pub pin_outline: Option<Retained<NSImage>>,
    pub pin_filled: Option<Retained<NSImage>>,
    pub pin_outline_selected: Option<Retained<NSImage>>,
    pub pin_filled_selected: Option<Retained<NSImage>>,
}

pub(crate) fn build_panel(mtm: MainThreadMarker, delegate: &app::AppDelegate) -> PanelParts {
    let panel = ClipPanel::create(mtm);
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setLevel(NSFloatingWindowLevel);
    panel.setBecomesKeyOnlyIfNeeded(false);
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
    let labels = NSArray::from_slice(&[&*text_label, &*images_label]);
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
        NSPoint::new(PANEL_W - 12.0 - 128.0, PANEL_H - SEARCH_H + 2.0),
        NSSize::new(128.0, 24.0),
    ));
    section_control.setSelectedSegment(0);
    section_control.setToolTip_forSegment(
        Some(&NSString::from_str(
            "Text, rich text, HTML and file clippings",
        )),
        0,
    );
    section_control.setToolTip_forSegment(Some(&NSString::from_str("Copied images")), 1);
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

    let clear_text_button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Clear Text"),
            Some(as_any(delegate)),
            Some(sel!(clearTextSection:)),
            mtm,
        )
    };
    clear_text_button.setFrame(NSRect::new(
        NSPoint::new(232.0, 6.0),
        NSSize::new(94.0, 22.0),
    ));
    clear_text_button.setBezelStyle(NSBezelStyle::Automatic);
    clear_text_button.setControlSize(NSControlSize::Small);
    clear_text_button.setFont(Some(&NSFont::systemFontOfSize(10.0)));
    clear_text_button.setToolTip(Some(&NSString::from_str(
        "Delete all text, rich text, HTML and file clippings",
    )));
    bg.addSubview(&clear_text_button);

    let clear_images_button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Clear Images"),
            Some(as_any(delegate)),
            Some(sel!(clearImagesSection:)),
            mtm,
        )
    };
    clear_images_button.setFrame(NSRect::new(
        NSPoint::new(330.0, 6.0),
        NSSize::new(116.0, 22.0),
    ));
    clear_images_button.setBezelStyle(NSBezelStyle::Automatic);
    clear_images_button.setControlSize(NSControlSize::Small);
    clear_images_button.setFont(Some(&NSFont::systemFontOfSize(10.0)));
    clear_images_button.setToolTip(Some(&NSString::from_str(
        "Delete all copied images and their thumbnails",
    )));
    bg.addSubview(&clear_images_button);

    let export_pdf_button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("PDF…"),
            Some(as_any(delegate)),
            Some(sel!(exportImagesPdf:)),
            mtm,
        )
    };
    export_pdf_button.setFrame(NSRect::new(
        NSPoint::new(166.0, 6.0),
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

    let pin_outline = pin_symbol(
        "pin",
        &NSColor::secondaryLabelColor().colorWithAlphaComponent(0.8),
    );
    let pin_filled = pin_symbol("pin.fill", &NSColor::controlAccentColor());
    let pin_outline_selected =
        pin_symbol("pin", &NSColor::whiteColor().colorWithAlphaComponent(0.8));
    let pin_filled_selected = pin_symbol("pin.fill", &NSColor::whiteColor());

    PanelParts {
        panel,
        search,
        list,
        footer,
        section_control,
        clear_text_button,
        clear_images_button,
        export_pdf_button,
        pin_outline,
        pin_filled,
        pin_outline_selected,
        pin_filled_selected,
    }
}

fn pin_symbol(name: &str, color: &NSColor) -> Option<Retained<NSImage>> {
    let base = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(name),
        Some(&NSString::from_str("Pin clipping")),
    )?;
    let size = NSImageSymbolConfiguration::configurationWithPointSize_weight(13.0, 0.0);
    let tint = NSImageSymbolConfiguration::configurationWithHierarchicalColor(color);
    let config = size.configurationByApplyingConfiguration(&tint);
    base.imageWithSymbolConfiguration(&config)
}

// ---------------------------------------------------------------------------
// Preferences
// ---------------------------------------------------------------------------

pub(crate) struct PrefsParts {
    pub window: Retained<NSWindow>,
    pub max_field: Retained<NSTextField>,
    pub max_label: Retained<NSTextField>,
    pub record_box: Retained<NSButton>,
    pub confirm_box: Retained<NSButton>,
    pub login_box: Retained<NSButton>,
    pub shortcut_button: Retained<NSButton>,
    pub ignored_view: Retained<IgnoredAppsView>,
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

pub(crate) fn build_preferences(mtm: MainThreadMarker, delegate: &app::AppDelegate) -> PrefsParts {
    let target = as_any(delegate);

    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(500.0, 450.0)),
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
        NSSize::new(468.0, 418.0),
    ));

    // ---- General ----
    let general = NSView::new(mtm);
    general.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(468.0, 380.0),
    ));

    general.addSubview(&heading(
        "Clipboard History",
        NSRect::new(NSPoint::new(16.0, 342.0), NSSize::new(400.0, 18.0)),
        mtm,
    ));

    general.addSubview(&label(
        "Remember last",
        NSRect::new(NSPoint::new(16.0, 308.0), NSSize::new(100.0, 20.0)),
        13.0,
        mtm,
    ));

    let max_field = NSTextField::new(mtm);
    max_field.setFrame(NSRect::new(
        NSPoint::new(120.0, 304.0),
        NSSize::new(64.0, 24.0),
    ));
    max_field.setFont(Some(&NSFont::systemFontOfSize(13.0)));
    max_field.setEditable(true);
    max_field.setBezeled(true);
    max_field.setDrawsBackground(true);
    unsafe {
        max_field.setTarget(Some(target));
        max_field.setAction(Some(sel!(maxClipsChanged:)));
    }
    general.addSubview(&max_field);

    general.addSubview(&label(
        "clippings",
        NSRect::new(NSPoint::new(190.0, 308.0), NSSize::new(80.0, 20.0)),
        13.0,
        mtm,
    ));

    let max_label = secondary_label(
        "Maximum: 200 clippings",
        NSRect::new(NSPoint::new(270.0, 308.0), NSSize::new(190.0, 20.0)),
        11.0,
        mtm,
    );
    general.addSubview(&max_label);

    let record_box = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str("Record clipboard history"),
            Some(target),
            Some(sel!(recordToggled:)),
            mtm,
        )
    };
    record_box.setFrame(NSRect::new(
        NSPoint::new(16.0, 272.0),
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
        NSPoint::new(16.0, 244.0),
        NSSize::new(420.0, 20.0),
    ));
    general.addSubview(&confirm_box);

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
        "History is stored in ~/Library/Application Support/RustClip",
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

    // ---- Exceptions ----
    let exceptions = NSView::new(mtm);
    exceptions.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(468.0, 380.0),
    ));

    exceptions.addSubview(&heading(
        "Ignored Apps",
        NSRect::new(NSPoint::new(16.0, 342.0), NSSize::new(400.0, 18.0)),
        mtm,
    ));
    exceptions.addSubview(&secondary_label(
        "Clipboard changes copied from these apps are not recorded.",
        NSRect::new(NSPoint::new(16.0, 316.0), NSSize::new(436.0, 18.0)),
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
        NSPoint::new(16.0, 88.0),
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
        NSPoint::new(16.0, 48.0),
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
        NSPoint::new(134.0, 48.0),
        NSSize::new(100.0, 30.0),
    ));
    exceptions.addSubview(&remove_button);

    exceptions.addSubview(&secondary_label(
        "Use Add App… to choose an application.",
        NSRect::new(NSPoint::new(16.0, 18.0), NSSize::new(436.0, 18.0)),
        11.0,
        mtm,
    ));

    let general_item = NSTabViewItem::new();
    general_item.setLabel(&NSString::from_str("General"));
    general_item.setView(Some(&general));
    tabs.addTabViewItem(&general_item);

    let exceptions_item = NSTabViewItem::new();
    exceptions_item.setLabel(&NSString::from_str("Exceptions"));
    exceptions_item.setView(Some(&exceptions));
    tabs.addTabViewItem(&exceptions_item);

    if let Some(content) = window.contentView() {
        content.addSubview(&tabs);
    }

    PrefsParts {
        window,
        max_field,
        max_label,
        record_box,
        confirm_box,
        login_box,
        shortcut_button,
        ignored_view,
    }
}

// Keep the compiler aware that these modifier flags are used by the shortcut
// recorder in the app module through this module's imports.
