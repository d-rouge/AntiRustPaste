//! Thumbnail cache directory policy and GTK texture loading for Linux.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use gtk4::gdk;
use gtk4::gdk_pixbuf;
use gtk4::gio;
use gtk4::glib;

pub fn cache_dir() -> PathBuf {
    if let Some(override_dir) = std::env::var_os("ANTIRUSTPASTE_THUMB_CACHE_DIR")
        && !override_dir.is_empty()
    {
        let candidate = PathBuf::from(override_dir);
        if is_safe_override(&candidate) {
            return candidate;
        }
        eprintln!(
            "antirustpaste-linux: ignoring unsafe ANTIRUSTPASTE_THUMB_CACHE_DIR ({})",
            candidate.display()
        );
    }
    if let Some(xdg_cache) = dirs::cache_dir() {
        return xdg_cache.join("antirustpaste").join("thumbnails");
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
    let mut hasher = DefaultHasher::new();
    user.hash(&mut hasher);
    let hash = hasher.finish();
    std::env::temp_dir()
        .join(format!("AntiRustPaste-{user}-{hash:016x}"))
        .join("thumbnails")
}

fn is_safe_override(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    if path == Path::new("/") || path == std::env::temp_dir() {
        return false;
    }
    if let Some(home) = dirs::home_dir()
        && path == home
    {
        return false;
    }
    true
}

pub fn load_texture(png_bytes: &[u8]) -> Option<gdk::Texture> {
    let bytes = glib::Bytes::from(png_bytes);
    let stream = gio::MemoryInputStream::from_bytes(&bytes);
    let pixbuf = gdk_pixbuf::Pixbuf::from_stream(&stream, gio::Cancellable::NONE).ok()?;
    Some(gdk::Texture::for_pixbuf(&pixbuf))
}
