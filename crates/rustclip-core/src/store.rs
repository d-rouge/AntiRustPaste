//! On-disk clipboard history: a JSON index plus a directory of binary blobs.
//!
//! Everything here is plain Rust (serde + std::fs), so the app has no database
//! or Objective-C persistence dependency.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

const MAX_INDEX_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_BLOB_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PREVIEW_CHARS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum ClipKind {
    Text,
    RichText,
    Html,
    Image,
    File,
    Other,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Clip {
    pub id: String,
    pub kind: ClipKind,
    /// Short single-line description shown in the list.
    pub preview: String,
    /// Full text for text clips, or the path for file clips.
    pub text: Option<String>,
    /// Pasteboard UTI for blob clips (image / rtf / html).
    pub data_type: Option<String>,
    /// Blob file name inside `<store>/blobs`.
    pub blob: Option<String>,
    pub byte_len: usize,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    pub source_name: Option<String>,
    pub source_bundle: Option<String>,
    pub date_ms: i64,
    pub hash: u64,
    /// Pinned clips stay above unpinned clips and are exempt from automatic
    /// trimming. They are removed only by an explicit delete/clear.
    #[serde(default)]
    pub pinned: bool,
}

impl Clip {
    pub fn display_title(&self) -> &str {
        if self.preview.is_empty() {
            match self.kind {
                ClipKind::Text => "Text",
                ClipKind::RichText => "Rich Text",
                ClipKind::Html => "HTML",
                ClipKind::Image => "Image",
                ClipKind::File => "File",
                ClipKind::Other => "Clipboard item",
            }
        } else {
            &self.preview
        }
    }

    pub fn subtitle(&self) -> String {
        let when = relative_time(self.date_ms);
        match &self.source_name {
            Some(app) if !app.is_empty() => format!("{app} · {when}"),
            _ => when,
        }
    }
}

/// Raw item captured from the pasteboard, before it is assigned an id/date.
#[derive(Clone, Debug)]
pub struct Captured {
    pub kind: ClipKind,
    pub preview: String,
    pub text: Option<String>,
    pub data_type: Option<String>,
    pub data: Option<Vec<u8>>,
    pub byte_len: usize,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

pub struct Store {
    dir: PathBuf,
    pub clips: Vec<Clip>,
}

impl Store {
    /// Open the history at the user's data directory.
    ///
    /// `RUSTCLIP_DATA_DIR` overrides the location. Agents and tests MUST point
    /// it at a scratch directory so they never read or delete real clippings.
    pub fn open() -> Self {
        let dir = std::env::var_os("RUSTCLIP_DATA_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::data_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("RustClip")
            });
        Self::open_at(dir)
    }

    /// Open a history in an explicit directory. Used by tests and the
    /// `RUSTCLIP_DATA_DIR` override.
    pub fn open_at(dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(dir.join("blobs"));
        let mut store = Self {
            dir,
            clips: Vec::new(),
        };
        store.load();
        store
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join("history.json")
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn blob_path(&self, name: &str) -> PathBuf {
        self.dir.join("blobs").join(name)
    }

    /// Validated path to a stored blob, or `None` for an unsafe name.
    pub fn blob_file(&self, name: &str) -> Option<PathBuf> {
        if !valid_blob_name(name) {
            return None;
        }
        Some(self.blob_path(name))
    }

    fn load(&mut self) {
        let path = self.index_path();
        if let Ok(meta) = fs::metadata(&path)
            && meta.len() > MAX_INDEX_BYTES
        {
            eprintln!("rustclip: history index is too large; moving it aside");
            let _ = fs::rename(&path, path.with_extension("json.bak"));
            return;
        }
        let Ok(raw) = fs::read(&path) else {
            return;
        };
        match serde_json::from_slice::<Vec<Clip>>(&raw) {
            Ok(mut clips) => {
                // Drop entries whose blob disappeared or whose name could point
                // outside the blob directory.
                clips.retain(|c| match &c.blob {
                    Some(name) => valid_blob_name(name) && self.blob_path(name).exists(),
                    None => true,
                });
                for clip in &mut clips {
                    truncate_preview(&mut clip.preview);
                }
                self.clips = clips;
            }
            Err(err) => {
                eprintln!("rustclip: could not read history ({err}); starting fresh");
                let _ = fs::rename(&path, path.with_extension("json.bak"));
            }
        }
    }

    pub fn save(&self) {
        let path = self.index_path();
        let tmp = path.with_extension("json.tmp");
        match serde_json::to_vec(&self.clips) {
            Ok(bytes) => {
                if let Err(err) = fs::write(&tmp, bytes).and_then(|_| fs::rename(&tmp, &path)) {
                    eprintln!("rustclip: could not save history: {err}");
                }
            }
            Err(err) => eprintln!("rustclip: could not encode history: {err}"),
        }
    }

    fn write_blob(&self, bytes: &[u8]) -> Option<String> {
        let name = format!("{}.bin", Uuid::new_v4());
        let path = self.blob_path(&name);
        match fs::write(&path, bytes) {
            Ok(()) => Some(name),
            Err(err) => {
                eprintln!("rustclip: could not write blob: {err}");
                None
            }
        }
    }

    fn delete_blob(&self, name: &str) {
        if valid_blob_name(name) {
            let _ = fs::remove_file(self.blob_path(name));
        }
    }

    /// Insert a captured item. Returns true when the history changed.
    pub fn insert(&mut self, captured: Captured, max: usize) -> bool {
        let hash = hash_captured(&captured);

        // Re-copying something that is already in the history moves it to the
        // top instead of creating a duplicate. A pinned duplicate stays pinned.
        let was_pinned = if let Some(pos) = self.clips.iter().position(|c| c.hash == hash) {
            let old = self.clips.remove(pos);
            let pinned = old.pinned;
            if let Some(blob) = old.blob {
                self.delete_blob(&blob);
            }
            pinned
        } else {
            false
        };

        let blob = captured.data.as_deref().and_then(|d| self.write_blob(d));
        let clip = Clip {
            id: Uuid::new_v4().to_string(),
            kind: captured.kind,
            preview: captured.preview,
            text: captured.text,
            data_type: captured.data_type,
            blob,
            byte_len: captured.byte_len,
            width: captured.width,
            height: captured.height,
            source_name: None,
            source_bundle: None,
            date_ms: chrono::Utc::now().timestamp_millis(),
            hash,
            pinned: was_pinned,
        };
        let insert_at = if was_pinned {
            0
        } else {
            self.clips.iter().filter(|clip| clip.pinned).count()
        };
        self.clips.insert(insert_at, clip);
        self.trim(max);
        true
    }

    /// Trim unpinned clips to `max`; pinned clips never count and never leave.
    pub fn trim(&mut self, max: usize) {
        let max = max.max(1);
        while self.clips.iter().filter(|clip| !clip.pinned).count() > max {
            let Some(index) = self.clips.iter().rposition(|clip| !clip.pinned) else {
                break;
            };
            let old = self.clips.remove(index);
            if let Some(blob) = old.blob {
                self.delete_blob(&blob);
            }
        }
    }

    /// Pin or unpin a clip and move it into the matching group. Returns its new
    /// index, or `None` for an invalid index.
    pub fn set_pinned(&mut self, index: usize, pinned: bool) -> Option<usize> {
        if index >= self.clips.len() {
            return None;
        }
        if self.clips[index].pinned == pinned {
            return Some(index);
        }

        let mut clip = self.clips.remove(index);
        clip.pinned = pinned;
        let insert_at = if pinned {
            0
        } else {
            self.clips.iter().filter(|clip| clip.pinned).count()
        };
        self.clips.insert(insert_at, clip);
        Some(insert_at)
    }

    pub fn remove(&mut self, index: usize) -> bool {
        if index >= self.clips.len() {
            return false;
        }
        let clip = self.clips.remove(index);
        if let Some(blob) = clip.blob {
            self.delete_blob(&blob);
        }
        true
    }

    /// Remove every clip matching `remove`, deleting its blob. Returns the
    /// number of removed clips.
    pub fn remove_where(&mut self, mut remove: impl FnMut(&Clip) -> bool) -> usize {
        let mut kept = Vec::with_capacity(self.clips.len());
        let mut removed = Vec::new();
        for clip in self.clips.drain(..) {
            if remove(&clip) {
                removed.push(clip);
            } else {
                kept.push(clip);
            }
        }
        self.clips = kept;
        for clip in &removed {
            if let Some(blob) = &clip.blob {
                self.delete_blob(blob);
            }
        }
        removed.len()
    }

    pub fn clear(&mut self) {
        let blobs: Vec<String> = self.clips.drain(..).filter_map(|clip| clip.blob).collect();
        for blob in blobs {
            self.delete_blob(&blob);
        }
    }

    pub fn read_blob(&self, name: &str) -> Option<Vec<u8>> {
        if !valid_blob_name(name) {
            return None;
        }
        let path = self.blob_path(name);
        if fs::metadata(&path).ok()?.len() > MAX_BLOB_BYTES {
            return None;
        }
        fs::read(path).ok()
    }
}

/// Blob names are generated as `<uuid>.bin`. Reject anything that could escape
/// the `blobs/` directory if a history file is edited by hand or by malware.
fn valid_blob_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 80
        && name.ends_with(".bin")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

fn truncate_preview(preview: &mut String) {
    if preview.chars().count() > MAX_PREVIEW_CHARS {
        *preview = preview.chars().take(MAX_PREVIEW_CHARS).collect();
    }
}

fn hash_captured(captured: &Captured) -> u64 {
    let mut hasher = DefaultHasher::new();
    (captured.kind as u8).hash(&mut hasher);
    captured.text.hash(&mut hasher);
    captured.data_type.hash(&mut hasher);
    captured.data.hash(&mut hasher);
    hasher.finish()
}

pub fn relative_time(ms: i64) -> String {
    let then = chrono::DateTime::from_timestamp_millis(ms).unwrap_or(chrono::DateTime::UNIX_EPOCH);
    let now = chrono::Utc::now();
    let secs = (now - then).num_seconds().max(0);
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 7 * 86_400 {
        format!("{}d ago", secs / 86_400)
    } else {
        then.with_timezone(&chrono::Local)
            .format("%b %-d")
            .to_string()
    }
}

pub fn human_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Turn arbitrary clipboard text into a compact single-line preview.
pub fn make_preview(text: &str) -> String {
    let mut out = String::new();
    let mut last_space = false;
    for ch in text.chars() {
        let ch = if ch.is_whitespace() { ' ' } else { ch };
        if ch == ' ' {
            if last_space || out.is_empty() {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        out.push(ch);
        if out.chars().count() >= 160 {
            break;
        }
    }
    let trimmed = out.trim_end().to_string();
    if trimmed.is_empty() {
        "Text".to_string()
    } else {
        trimmed
    }
}

pub fn file_preview(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("rustclip-test-{}", Uuid::new_v4()));
            let _ = fs::create_dir_all(&path);
            Self(path)
        }

        fn store(&self) -> Store {
            Store::open_at(self.0.join("store"))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn text_clip(text: &str) -> Captured {
        Captured {
            kind: ClipKind::Text,
            preview: make_preview(text),
            text: Some(text.to_string()),
            data_type: None,
            data: None,
            byte_len: text.len(),
            width: None,
            height: None,
        }
    }

    fn image_clip(bytes: &[u8]) -> Captured {
        Captured {
            kind: ClipKind::Image,
            preview: "Image".to_string(),
            text: None,
            data_type: Some("public.png".to_string()),
            data: Some(bytes.to_vec()),
            byte_len: bytes.len(),
            width: None,
            height: None,
        }
    }

    #[test]
    fn preview_collapses_whitespace_and_bounds_length() {
        assert_eq!(make_preview("  hello \n\t world  "), "hello world");
        assert_eq!(make_preview(" \n\t "), "Text");
        let long = "x".repeat(500);
        assert_eq!(make_preview(&long).chars().count(), 160);
    }

    #[test]
    fn inserting_a_duplicate_moves_it_to_the_top() {
        let temp = TempDir::new();
        let mut store = temp.store();

        assert!(store.insert(text_clip("first"), 10));
        assert!(store.insert(text_clip("second"), 10));
        assert_eq!(store.clips[0].text.as_deref(), Some("second"));

        assert!(store.insert(text_clip("first"), 10));
        assert_eq!(store.clips.len(), 2);
        assert_eq!(store.clips[0].text.as_deref(), Some("first"));
        assert_eq!(store.clips[1].text.as_deref(), Some("second"));
    }

    #[test]
    fn pinning_keeps_a_clip_above_the_limit() {
        let temp = TempDir::new();
        let mut store = temp.store();

        store.insert(text_clip("pinned"), 1);
        store.set_pinned(0, true).expect("pin first clip");
        store.insert(text_clip("second"), 1);
        store.insert(text_clip("third"), 1);

        assert_eq!(store.clips.len(), 2);
        assert_eq!(store.clips[0].text.as_deref(), Some("pinned"));
        assert!(store.clips[0].pinned);
        assert_eq!(store.clips[1].text.as_deref(), Some("third"));
    }

    #[test]
    fn duplicate_of_pinned_clip_stays_pinned_at_the_top() {
        let temp = TempDir::new();
        let mut store = temp.store();

        store.insert(text_clip("first"), 10);
        store.insert(text_clip("second"), 10);
        store.set_pinned(1, true).expect("pin first clip");
        store.insert(text_clip("first"), 10);

        assert_eq!(store.clips.len(), 2);
        assert!(store.clips[0].pinned);
        assert_eq!(store.clips[0].text.as_deref(), Some("first"));
        assert!(!store.clips[1].pinned);
    }

    #[test]
    fn unpinning_moves_a_clip_into_the_unpinned_group() {
        let temp = TempDir::new();
        let mut store = temp.store();

        store.insert(text_clip("one"), 10);
        store.insert(text_clip("two"), 10);
        store.set_pinned(1, true).expect("pin one");
        let new_index = store.set_pinned(0, false).expect("unpin one");

        assert_eq!(new_index, 0);
        assert!(!store.clips[0].pinned);
        assert_eq!(store.clips[0].text.as_deref(), Some("one"));
        assert!(!store.clips[1].pinned);
    }

    #[test]
    fn trimming_removes_the_oldest_blob() {
        let temp = TempDir::new();
        let mut store = temp.store();

        store.insert(image_clip(b"not really a png"), 10);
        let blob = store.clips[0].blob.clone().expect("image blob name");
        let blob_path = store.blob_path(&blob);
        assert!(blob_path.exists());

        store.insert(text_clip("newest"), 10);
        store.trim(1);

        assert_eq!(store.clips.len(), 1);
        assert_eq!(store.clips[0].text.as_deref(), Some("newest"));
        assert!(!blob_path.exists());
    }

    #[test]
    fn remove_where_keeps_other_kinds_and_deletes_removed_blobs() {
        let temp = TempDir::new();
        let mut store = temp.store();

        store.insert(image_clip(b"keep only text"), 10);
        let image_blob = store.clips[0].blob.clone().expect("image blob");
        let image_path = store.blob_path(&image_blob);
        store.insert(text_clip("keep me"), 10);

        let removed = store.remove_where(|clip| clip.kind == ClipKind::Image);

        assert_eq!(removed, 1);
        assert_eq!(store.clips.len(), 1);
        assert_eq!(store.clips[0].text.as_deref(), Some("keep me"));
        assert!(!image_path.exists());
    }

    #[test]
    fn clearing_removes_every_blob() {
        let temp = TempDir::new();
        let mut store = temp.store();

        store.insert(image_clip(b"one"), 10);
        store.insert(image_clip(b"two"), 10);
        let blobs: Vec<PathBuf> = store
            .clips
            .iter()
            .filter_map(|clip| clip.blob.as_deref())
            .map(|name| store.blob_path(name))
            .collect();
        assert_eq!(blobs.len(), 2);

        store.clear();

        assert!(store.clips.is_empty());
        for path in blobs {
            assert!(!path.exists(), "blob should be deleted: {}", path.display());
        }
    }

    #[test]
    fn a_corrupt_index_is_quarantined_instead_of_crashing() {
        let temp = TempDir::new();
        let dir = temp.0.join("store");
        let _ = fs::create_dir_all(dir.join("blobs"));
        let index = dir.join("history.json");
        fs::write(&index, b"{ this is not valid json").expect("write corrupt index");

        let store = Store::open_at(dir.clone());

        assert!(store.clips.is_empty());
        assert!(!index.exists());
        assert!(dir.join("history.json.bak").exists());
    }

    #[test]
    fn old_records_without_image_dimensions_still_load() {
        let temp = TempDir::new();
        let dir = temp.0.join("store");
        let _ = fs::create_dir_all(dir.join("blobs"));
        let json = r#"[{
            "id": "legacy",
            "kind": "text",
            "preview": "legacy clipping",
            "text": "legacy clipping",
            "data_type": null,
            "blob": null,
            "byte_len": 15,
            "source_name": null,
            "source_bundle": null,
            "date_ms": 1700000000000,
            "hash": 1
        }]"#;
        fs::write(dir.join("history.json"), json).expect("write legacy index");

        let store = Store::open_at(dir);
        assert_eq!(store.clips.len(), 1);
        let clip = &store.clips[0];
        assert_eq!(clip.width, None);
        assert_eq!(clip.height, None);
        assert_eq!(clip.text.as_deref(), Some("legacy clipping"));
    }

    #[test]
    fn blob_names_that_escape_the_store_are_rejected() {
        assert!(valid_blob_name("0b0e4c1a-0000-0000-0000-000000000000.bin"));
        assert!(!valid_blob_name("../outside.bin"));
        assert!(!valid_blob_name("nested/inside.bin"));
        assert!(!valid_blob_name("no-extension"));
        assert!(!valid_blob_name(""));
    }

    #[test]
    fn entries_with_missing_blobs_are_dropped_on_load() {
        let temp = TempDir::new();
        let mut store = temp.store();
        store.insert(image_clip(b"orphan"), 10);
        let blob = store.clips[0].blob.clone().expect("blob name");
        store.save();
        fs::remove_file(store.blob_path(&blob)).expect("remove blob");

        let reopened = Store::open_at(store.dir().to_path_buf());
        assert!(reopened.clips.is_empty());
    }
}
