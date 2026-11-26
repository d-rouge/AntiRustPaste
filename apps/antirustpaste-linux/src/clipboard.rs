//! GDK4 clipboard reading and writing with bounded input and concealed-type
//! filtering.

use std::fs;

use antirustpaste_core::{Captured, Clip, ClipKind, Store, file_preview, make_preview};
use gtk4::gdk;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;

const MAX_BLOB_BYTES: usize = 50 * 1024 * 1024;

const CONCEALED_MIME_TYPES: &[&str] = &[
    "x-kde-passwordManagerHint",
    "application/x-nspasteboard-concealed",
    "org.nspasteboard.ConcealedType",
    "org.nspasteboard.TransientType",
];

pub fn is_concealed(formats: &gdk::ContentFormats) -> bool {
    CONCEALED_MIME_TYPES
        .iter()
        .any(|mime| formats.contain_mime_type(mime))
}

/// Read the current clipboard asynchronously and deliver a `Captured` item.
pub fn read_clipboard_async(
    clipboard: &gdk::Clipboard,
    source_name: Option<String>,
    source_bundle: Option<String>,
    on_done: impl FnOnce(Option<Captured>) + 'static,
) {
    let formats = clipboard.formats();
    if is_concealed(&formats) {
        on_done(None);
        return;
    }

    if formats.contain_mime_type("image/png")
        || formats.contain_mime_type("image/jpeg")
        || formats.contain_mime_type("image/tiff")
    {
        let mime = if formats.contain_mime_type("image/png") {
            "image/png"
        } else if formats.contain_mime_type("image/jpeg") {
            "image/jpeg"
        } else {
            "image/tiff"
        };
        let mime_owned = mime.to_string();
        clipboard.read_async(
            &[mime],
            glib::Priority::DEFAULT,
            gio::Cancellable::NONE,
            move |res| {
                let Ok((stream, _)) = res else {
                    on_done(None);
                    return;
                };
                let Some(bytes) = read_stream_bounded(&stream) else {
                    on_done(None);
                    return;
                };
                let Ok(info) = antirustpaste_imaging::info(&bytes) else {
                    on_done(None);
                    return;
                };
                let byte_len = bytes.len();
                on_done(Some(Captured {
                    kind: ClipKind::Image,
                    preview: format!("Image ({} × {})", info.width, info.height),
                    text: None,
                    data_type: Some(mime_owned),
                    data: Some(bytes),
                    byte_len,
                    width: Some(info.width),
                    height: Some(info.height),
                    source_name,
                    source_bundle,
                }));
            },
        );
        return;
    }

    if formats.contain_mime_type("text/uri-list") {
        clipboard.read_async(
            &["text/uri-list"],
            glib::Priority::DEFAULT,
            gio::Cancellable::NONE,
            move |res| {
                let Ok((stream, _)) = res else {
                    on_done(None);
                    return;
                };
                let Some(bytes) = read_stream_bounded(&stream) else {
                    on_done(None);
                    return;
                };
                let Ok(raw) = String::from_utf8(bytes) else {
                    on_done(None);
                    return;
                };
                let paths: Vec<String> = raw
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(|l| l.strip_prefix("file://").unwrap_or(l).to_string())
                    .collect();
                if paths.is_empty() {
                    on_done(None);
                    return;
                }
                let joined = paths.join("\n");
                let byte_len = joined.len();
                let preview = paths
                    .first()
                    .map(|p| {
                        let base = file_preview(p);
                        if paths.len() > 1 {
                            format!("{base} (+{})", paths.len().saturating_sub(1))
                        } else {
                            base
                        }
                    })
                    .unwrap_or_else(|| "File".to_string());
                on_done(Some(Captured {
                    kind: ClipKind::File,
                    preview,
                    text: Some(joined),
                    data_type: Some("text/uri-list".to_string()),
                    data: None,
                    byte_len,
                    width: None,
                    height: None,
                    source_name,
                    source_bundle,
                }));
            },
        );
        return;
    }

    if formats.contain_mime_type("text/plain;charset=utf-8")
        || formats.contain_mime_type("text/plain")
        || formats.contain_mime_type("UTF8_STRING")
    {
        clipboard.read_text_async(gio::Cancellable::NONE, move |res| {
            let Ok(Some(text)) = res else {
                on_done(None);
                return;
            };
            let text_str = text.to_string();
            if text_str.is_empty() || text_str.len() > MAX_BLOB_BYTES {
                on_done(None);
                return;
            }
            let byte_len = text_str.len();
            on_done(Some(Captured {
                kind: ClipKind::Text,
                preview: make_preview(&text_str),
                text: Some(text_str),
                data_type: Some("text/plain;charset=utf-8".to_string()),
                data: None,
                byte_len,
                width: None,
                height: None,
                source_name,
                source_bundle,
            }));
        });
        return;
    }

    on_done(None);
}

pub fn write_clip(clipboard: &gdk::Clipboard, store: &Store, clip: &Clip) -> bool {
    match clip.kind {
        ClipKind::Text | ClipKind::Command => {
            let Some(text) = &clip.text else {
                return false;
            };
            clipboard.set_text(text);
            true
        }
        ClipKind::File => {
            let Some(text) = &clip.text else {
                return false;
            };
            let uris: String = text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| {
                    if l.starts_with("file://") {
                        format!("{l}\r\n")
                    } else {
                        format!("file://{l}\r\n")
                    }
                })
                .collect();
            let provider = gdk::ContentProvider::for_bytes(
                "text/uri-list",
                &glib::Bytes::from(uris.as_bytes()),
            );
            clipboard.set_content(Some(&provider)).is_ok()
        }
        ClipKind::Image | ClipKind::RichText | ClipKind::Html | ClipKind::Other => {
            let Some(blob_name) = &clip.blob else {
                return false;
            };
            let Some(path) = store.blob_file(blob_name) else {
                return false;
            };
            let Ok(bytes) = fs::read(path) else {
                return false;
            };
            if bytes.len() > MAX_BLOB_BYTES {
                return false;
            }
            let mime = clip.data_type.as_deref().unwrap_or("image/png");
            let provider = gdk::ContentProvider::for_bytes(mime, &glib::Bytes::from(&bytes));
            clipboard.set_content(Some(&provider)).is_ok()
        }
    }
}

fn read_stream_bounded(stream: &gio::InputStream) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = stream.read(&mut buf, gio::Cancellable::NONE).ok()?;
        if n == 0 {
            break;
        }
        if out.len().saturating_add(n) > MAX_BLOB_BYTES {
            return None;
        }
        if let Some(slice) = buf.get(..n) {
            out.extend_from_slice(slice);
        }
    }
    Some(out)
}
