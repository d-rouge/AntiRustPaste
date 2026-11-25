//! macOS thumbnail policy and AppKit image loading.
//!
//! The generation worker and file cache live in `rustclip_core::thumbnails`;
//! this module only chooses the boot-cleared cache directory, validates an
//! override, cleans the legacy location and turns PNG bytes into `NSImage`.

use std::fs;
use std::path::{Path, PathBuf};

use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_app_kit::NSImage;
use objc2_foundation::NSData;
use rustclip_core::ThumbnailService;

/// `RUSTCLIP_THUMB_CACHE_DIR` isolates the cache for agents and tests.
/// Without it, macOS uses `/tmp/AntiRustPaste-<user>-<hash>/thumbnails`, which the
/// OS clears at boot. Other platforms use the OS temporary directory.
pub fn cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("RUSTCLIP_THUMB_CACHE_DIR")
        && !dir.is_empty()
    {
        let path = PathBuf::from(dir);
        if is_safe_cache_dir(&path) {
            return path;
        }
        eprintln!(
            "rustclip: ignoring RUSTCLIP_THUMB_CACHE_DIR; it must be an absolute, dedicated directory"
        );
    }
    default_cache_dir()
}

fn default_cache_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/tmp")
            .join(format!("AntiRustPaste-{}", user_key()))
            .join("thumbnails")
    }

    #[cfg(not(target_os = "macos"))]
    {
        std::env::temp_dir()
            .join("AntiRustPaste")
            .join("thumbnails")
    }
}

/// Start the shared worker with the macOS cache directory and tighten the
/// default per-user container.
pub fn new_service() -> ThumbnailService {
    let dir = cache_dir();
    if dir == default_cache_dir()
        && let Some(parent) = dir.parent()
    {
        let _ = fs::create_dir_all(parent);
        private_dir(parent);
    }
    ThumbnailService::new(dir)
}

/// Remove the legacy cache under the per-user temp directory. It is ours, so
/// deleting it is safe.
pub fn cleanup_legacy_cache() {
    #[cfg(target_os = "macos")]
    {
        let legacy = std::env::temp_dir().join("RustClip").join("thumbnails");
        if legacy != cache_dir() {
            let _ = fs::remove_dir_all(legacy);
        }
    }
}

pub fn prune_now(valid: &std::collections::HashSet<u64>) {
    rustclip_core::thumbnails::prune_now(&cache_dir(), valid);
}

/// Quick Look preview files live beside the thumbnails so they are boot-cleared
/// with the cache. They hold full images, so the directory is private.
pub fn preview_dir() -> PathBuf {
    cache_dir().join("previews")
}

pub fn reset_previews() {
    let _ = fs::remove_dir_all(preview_dir());
}

pub fn ensure_preview_dir() -> std::io::Result<PathBuf> {
    let dir = preview_dir();
    fs::create_dir_all(&dir)?;
    private_dir(&dir);
    Ok(dir)
}

pub fn nsimage_from_png(bytes: Vec<u8>) -> Option<Retained<NSImage>> {
    let data = NSData::from_vec(bytes);
    let image = NSImage::initWithData(NSImage::alloc(), &data)?;
    let size = image.size();
    if size.width <= 0.0 || size.height <= 0.0 {
        return None;
    }
    Some(image)
}

/// Refuse a cache directory that could be a system root, the home directory or
/// a relative path. Clearing runs a directory scan, so a bad override such as
/// `/tmp` must never be used.
fn is_safe_cache_dir(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let forbidden = [
        Path::new("/"),
        Path::new("/tmp"),
        Path::new("/private/tmp"),
        Path::new("/var"),
        Path::new("/var/tmp"),
        Path::new("/Users"),
        Path::new("/Applications"),
        Path::new("/System"),
        Path::new("/Library"),
    ];
    if forbidden.contains(&path) {
        return false;
    }
    if dirs::home_dir().is_some_and(|home| path == home) {
        return false;
    }
    path.components().count() >= 3
}

fn user_key() -> String {
    let user = std::env::var("USER")
        .ok()
        .or_else(|| std::env::var("LOGNAME").ok());
    let home = dirs::home_dir();

    let source = user
        .clone()
        .or_else(|| {
            home.as_ref().and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
        })
        .unwrap_or_else(|| "user".to_string());

    let mut name: String = source
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .take(24)
        .collect();
    if name.is_empty() {
        name.push_str("user");
    }

    let hash_source = home
        .as_ref()
        .map(|path| path.as_os_str().as_encoded_bytes().to_vec())
        .unwrap_or_else(|| source.as_bytes().to_vec());
    format!("{name}-{:08x}", fnv1a(&hash_source) as u32)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(unix)]
fn private_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);
        let _ = fs::set_permissions(path, permissions);
    }
}

#[cfg(not(unix))]
fn private_dir(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_cache_roots_are_rejected() {
        assert!(!is_safe_cache_dir(Path::new("/")));
        assert!(!is_safe_cache_dir(Path::new("/tmp")));
        assert!(!is_safe_cache_dir(Path::new("/private/tmp")));
        assert!(!is_safe_cache_dir(Path::new("relative/thumbnails")));
        assert!(is_safe_cache_dir(Path::new("/tmp/AntiRustPaste-user-hash")));
    }

    #[test]
    fn user_key_is_filesystem_safe() {
        let key = user_key();
        assert!(!key.is_empty());
        assert!(key.len() <= 40);
        assert!(
            key.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        );
    }
}
