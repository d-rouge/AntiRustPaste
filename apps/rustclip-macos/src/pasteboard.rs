//! Reading and writing the macOS general pasteboard.

use objc2::rc::Retained;
use objc2_app_kit::{
    NSPasteboard, NSPasteboardType, NSPasteboardTypeFileURL, NSPasteboardTypeHTML,
    NSPasteboardTypePNG, NSPasteboardTypeRTF, NSPasteboardTypeRTFD, NSPasteboardTypeString,
    NSPasteboardTypeTIFF,
};
use objc2_foundation::{NSArray, NSData, NSString, NSURL};

use rustclip_core::{Captured, Clip, ClipKind, Store, file_preview, human_size, make_preview};

const MAX_ITEM_BYTES: usize = 50 * 1024 * 1024;

// The pasteboard type constants are `extern static`s, so reading them is
// unsafe. Wrap them once instead of sprinkling `unsafe` through the logic.
fn t_string() -> &'static NSPasteboardType {
    unsafe { NSPasteboardTypeString }
}
fn t_file_url() -> &'static NSPasteboardType {
    unsafe { NSPasteboardTypeFileURL }
}
fn t_png() -> &'static NSPasteboardType {
    unsafe { NSPasteboardTypePNG }
}
fn t_tiff() -> &'static NSPasteboardType {
    unsafe { NSPasteboardTypeTIFF }
}
fn t_rtfd() -> &'static NSPasteboardType {
    unsafe { NSPasteboardTypeRTFD }
}
fn t_rtf() -> &'static NSPasteboardType {
    unsafe { NSPasteboardTypeRTF }
}
fn t_html() -> &'static NSPasteboardType {
    unsafe { NSPasteboardTypeHTML }
}

pub fn general() -> Retained<NSPasteboard> {
    NSPasteboard::generalPasteboard()
}

fn type_name(types: &NSArray<NSString>, needle: &str) -> bool {
    types.iter().any(|t| t.to_string() == needle)
}

pub fn is_concealed(pb: &NSPasteboard) -> bool {
    pb.types()
        .map(|types| {
            type_name(&types, "org.nspasteboard.ConcealedType")
                || type_name(&types, "org.nspasteboard.TransientType")
        })
        .unwrap_or(false)
}

/// Read every usable item currently on the pasteboard.
pub fn read_items(pb: &NSPasteboard) -> Vec<Captured> {
    if is_concealed(pb) {
        return Vec::new();
    }
    let Some(items) = pb.pasteboardItems() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for item in items.iter() {
        let types = item.types();
        let mut chosen: Option<Captured> = None;

        for uti in types.iter() {
            let uti = uti.to_string();
            match uti.as_str() {
                "public.file-url" => {
                    if let Some(url_str) = item.stringForType(t_file_url()) {
                        let raw = url_str.to_string();
                        let path = NSURL::URLWithString(&url_str)
                            .and_then(|u| u.path())
                            .map(|p| p.to_string())
                            .unwrap_or_else(|| raw.clone());
                        if path.is_empty() {
                            continue;
                        }
                        chosen = Some(Captured {
                            kind: ClipKind::File,
                            preview: file_preview(&path),
                            text: Some(path.clone()),
                            data_type: None,
                            data: None,
                            byte_len: path.len(),
                            width: None,
                            height: None,
                        });
                        break;
                    }
                }
                "public.png" | "public.tiff" | "public.jpeg" => {
                    let data = match uti.as_str() {
                        "public.png" => item.dataForType(t_png()),
                        "public.tiff" => item.dataForType(t_tiff()),
                        _ => {
                            let jpeg_type = NSString::from_str("public.jpeg");
                            item.dataForType(&jpeg_type)
                        }
                    };
                    if let Some(data) = data {
                        let bytes = data.to_vec();
                        if bytes.is_empty() || bytes.len() > MAX_ITEM_BYTES {
                            continue;
                        }
                        let preview = format!("Image · {}", human_size(bytes.len()));
                        let info = rustclip_imaging::info(&bytes).ok();
                        chosen = Some(Captured {
                            kind: ClipKind::Image,
                            preview,
                            text: None,
                            data_type: Some(uti.clone()),
                            data: Some(bytes.clone()),
                            byte_len: bytes.len(),
                            width: info.map(|info| info.width),
                            height: info.map(|info| info.height),
                        });
                        break;
                    }
                }
                "public.rtfd" | "public.rtf" | "public.html" => {
                    let pb_type = match uti.as_str() {
                        "public.rtfd" => t_rtfd(),
                        "public.rtf" => t_rtf(),
                        _ => t_html(),
                    };
                    if let Some(data) = item.dataForType(pb_type) {
                        let bytes = data.to_vec();
                        if bytes.is_empty() || bytes.len() > MAX_ITEM_BYTES {
                            continue;
                        }
                        let plain = item
                            .stringForType(t_string())
                            .map(|s| s.to_string())
                            .filter(|s| s.len() <= MAX_ITEM_BYTES);
                        let preview = plain.as_deref().map(make_preview).unwrap_or_else(|| {
                            if uti == "public.html" {
                                "HTML".to_string()
                            } else {
                                "Rich Text".to_string()
                            }
                        });
                        let kind = if uti == "public.html" {
                            ClipKind::Html
                        } else {
                            ClipKind::RichText
                        };
                        let len = bytes.len();
                        chosen = Some(Captured {
                            kind,
                            preview,
                            text: plain,
                            data_type: Some(uti.clone()),
                            data: Some(bytes),
                            byte_len: len,
                            width: None,
                            height: None,
                        });
                        break;
                    }
                }
                "public.utf8-plain-text" | "public.plain-text" | "NSStringPboardType" => {
                    if let Some(text) = item.stringForType(t_string()) {
                        let text = text.to_string();
                        if text.trim().is_empty() || text.len() > MAX_ITEM_BYTES {
                            continue;
                        }
                        chosen = Some(Captured {
                            kind: ClipKind::Text,
                            preview: make_preview(&text),
                            byte_len: text.len(),
                            text: Some(text),
                            data_type: None,
                            data: None,
                            width: None,
                            height: None,
                        });
                        break;
                    }
                }
                _ => {}
            }
        }

        if let Some(c) = chosen {
            out.push(c);
        }
        if out.len() >= 25 {
            break;
        }
    }
    out
}

/// Put a stored clipping back on the pasteboard.
pub fn write_clip(pb: &NSPasteboard, store: &Store, clip: &Clip) -> bool {
    pb.clearContents();
    match clip.kind {
        ClipKind::Text => {
            if let Some(text) = &clip.text {
                return pb.setString_forType(&NSString::from_str(text), t_string());
            }
            false
        }
        ClipKind::File => {
            let Some(path) = &clip.text else {
                return false;
            };
            let url = NSURL::fileURLWithPath(&NSString::from_str(path));
            let Some(url_string) = url.absoluteString() else {
                return false;
            };
            pb.setString_forType(&url_string, t_file_url())
        }
        _ => {
            let mut ok = false;
            if let (Some(name), Some(uti)) = (&clip.blob, &clip.data_type)
                && let Some(bytes) = store.read_blob(name)
            {
                let data = NSData::from_vec(bytes);
                ok = pb.setData_forType(Some(&data), &NSString::from_str(uti));
            }
            // Rich clips also carry a plain-text fallback so they paste into
            // simple text fields too.
            if matches!(clip.kind, ClipKind::RichText | ClipKind::Html)
                && let Some(text) = &clip.text
            {
                ok |= pb.setString_forType(&NSString::from_str(text), t_string());
            }
            ok
        }
    }
}
