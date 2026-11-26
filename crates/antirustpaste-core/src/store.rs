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
/// Maximum stored length of a user-chosen clip title, in characters.
pub const MAX_TITLE_CHARS: usize = 160;
/// Maximum text length accepted by the in-app editor, in bytes. Larger clips
/// stay readable and copyable but cannot be edited in the text view.
pub const MAX_EDIT_TEXT_BYTES: usize = 1024 * 1024;

/// Why `Store::set_text` refused an edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditTextError {
    /// The clip no longer exists or its kind has no editable text.
    Unavailable,
    /// The new text is empty or only whitespace.
    Empty,
    /// The new text exceeds `MAX_EDIT_TEXT_BYTES`.
    TooLarge,
    /// Another clip already stores the same text.
    Duplicate,
}

/// Which history entries a clear action removes.
///
/// Terminal commands are never removed by these scopes: they come from the
/// user's own shell history and have their own deletion path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClearScope {
    /// Every clipping, including pinned rows.
    Everything,
    /// Every Text-section clipping (text, rich text, HTML and files).
    Text,
    /// Every image clipping.
    Images,
    /// Unpinned Text-section clippings only.
    UnpinnedText,
    /// Unpinned image clippings only.
    UnpinnedImages,
}

impl ClearScope {
    /// Whether this scope removes `clip`.
    pub fn matches(self, clip: &Clip) -> bool {
        if clip.kind == ClipKind::Command {
            return false;
        }
        let image = clip.kind == ClipKind::Image;
        match self {
            Self::Everything => true,
            Self::Text => !image,
            Self::Images => image,
            Self::UnpinnedText => !image && !clip.pinned,
            Self::UnpinnedImages => image && !clip.pinned,
        }
    }
}

/// Disk footprint of one history directory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StorageUsage {
    /// The JSON index (`history.json`).
    pub index_bytes: u64,
    /// Regular files in `blobs/`.
    pub blob_bytes: u64,
    /// Other files directly in the history directory, such as the shell cursor
    /// sidecar or a quarantined backup.
    pub other_bytes: u64,
}

impl StorageUsage {
    /// All history files together, saturating instead of overflowing.
    pub fn history_total(self) -> u64 {
        self.index_bytes
            .saturating_add(self.blob_bytes)
            .saturating_add(self.other_bytes)
    }
}

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
    /// A command read from a shell history file.
    Command,
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
    /// Optional user-chosen name. It replaces the preview in the list and is
    /// included in search, but never changes the clipboard content.
    #[serde(default)]
    pub title: Option<String>,
    /// Pinned clips stay above unpinned clips and are exempt from automatic
    /// trimming. They are removed only by an explicit delete/clear.
    #[serde(default)]
    pub pinned: bool,
}

impl Clip {
    /// The title shown in the list: the user title when one is set, otherwise
    /// the normal content summary.
    pub fn display_title(&self) -> &str {
        if let Some(title) = &self.title
            && !title.is_empty()
        {
            return title;
        }
        self.content_title()
    }

    /// The content summary without the user title. Used to show the underlying
    /// clipping under a custom name.
    pub fn content_title(&self) -> &str {
        if self.preview.is_empty() {
            match self.kind {
                ClipKind::Text => "Text",
                ClipKind::RichText => "Rich Text",
                ClipKind::Html => "HTML",
                ClipKind::Image => "Image",
                ClipKind::File => "File",
                ClipKind::Command => "Command",
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

    /// The plain-text representation stored next to a rich clip.
    ///
    /// Returns `None` when the clip has no richer text form, so a "plain text"
    /// action only appears when it would change the pasteboard content. The
    /// stored blob and UTI are never modified.
    pub fn plain_text(&self) -> Option<&str> {
        match self.kind {
            ClipKind::RichText | ClipKind::Html => {
                self.text.as_deref().filter(|text| !text.is_empty())
            }
            _ => None,
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
    /// App or shell that produced the item. Commands use the shell name.
    pub source_name: Option<String>,
    pub source_bundle: Option<String>,
}

pub struct Store {
    dir: PathBuf,
    pub clips: Vec<Clip>,
}

impl Store {
    /// Open the history at the user's data directory.
    ///
    /// `ANTIRUSTPASTE_DATA_DIR` overrides the location. Agents and tests MUST point
    /// it at a scratch directory so they never read or delete real clippings.
    pub fn open() -> Self {
        let dir = std::env::var_os("ANTIRUSTPASTE_DATA_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(default_dir);
        Self::open_at(dir)
    }

    /// Open a history in an explicit directory. Used by tests and the
    /// `ANTIRUSTPASTE_DATA_DIR` override.
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

    /// Current disk footprint of the history directory. Directory reads are
    /// bounded and never follow symlinks.
    pub fn storage_usage(&self) -> StorageUsage {
        StorageUsage {
            index_bytes: regular_file_len(&self.index_path()),
            blob_bytes: directory_file_bytes(&self.dir.join("blobs"), None),
            other_bytes: directory_file_bytes(&self.dir, Some("history.json")),
        }
    }

    fn load(&mut self) {
        let path = self.index_path();
        if let Ok(meta) = fs::metadata(&path)
            && meta.len() > MAX_INDEX_BYTES
        {
            eprintln!("antirustpaste: history index is too large; moving it aside");
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
                    clip.title = clip.title.as_deref().and_then(make_title);
                }
                self.clips = clips;
            }
            Err(err) => {
                eprintln!("antirustpaste: could not read history ({err}); starting fresh");
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
                    eprintln!("antirustpaste: could not save history: {err}");
                }
            }
            Err(err) => eprintln!("antirustpaste: could not encode history: {err}"),
        }
    }

    fn write_blob(&self, bytes: &[u8]) -> Option<String> {
        let name = format!("{}.bin", Uuid::new_v4());
        let path = self.blob_path(&name);
        match fs::write(&path, bytes) {
            Ok(()) => Some(name),
            Err(err) => {
                eprintln!("antirustpaste: could not write blob: {err}");
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
        let is_command = captured.kind == ClipKind::Command;

        // Re-copying something that is already in the history moves it to the
        // top instead of creating a duplicate. A pinned duplicate stays pinned
        // and keeps its user-chosen title.
        let (was_pinned, old_title) =
            if let Some(pos) = self.clips.iter().position(|c| c.hash == hash) {
                let old = self.clips.remove(pos);
                let pinned = old.pinned;
                let title = old.title;
                if let Some(blob) = old.blob {
                    self.delete_blob(&blob);
                }
                (pinned, title)
            } else {
                (false, None)
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
            source_name: captured.source_name,
            source_bundle: captured.source_bundle,
            date_ms: chrono::Utc::now().timestamp_millis(),
            hash,
            title: old_title,
            pinned: was_pinned,
        };
        let insert_at = if was_pinned {
            0
        } else {
            self.clips.iter().filter(|clip| clip.pinned).count()
        };
        self.clips.insert(insert_at, clip);
        // Clipboard items and shell commands have independent budgets, so a
        // busy shell cannot evict the clipboard history (or the reverse).
        self.trim_where(max, |clip| (clip.kind == ClipKind::Command) == is_command);
        true
    }

    /// Trim unpinned clips to `max`; pinned clips never count and never leave.
    pub fn trim(&mut self, max: usize) {
        self.trim_where(max, |_| true);
    }

    /// Trim both groups independently, keeping up to `max` of each.
    pub fn trim_sections(&mut self, max: usize) {
        self.trim_where(max, |clip| clip.kind == ClipKind::Command);
        self.trim_where(max, |clip| clip.kind != ClipKind::Command);
    }

    fn trim_where(&mut self, max: usize, group: impl Fn(&Clip) -> bool) {
        let max = max.max(1);
        while self
            .clips
            .iter()
            .filter(|clip| !clip.pinned && group(clip))
            .count()
            > max
        {
            let Some(index) = self
                .clips
                .iter()
                .rposition(|clip| !clip.pinned && group(clip))
            else {
                break;
            };
            let old = self.clips.remove(index);
            if let Some(blob) = old.blob {
                self.delete_blob(&blob);
            }
        }
    }

    /// Insert a command read from a shell history file, or refresh an existing
    /// entry. `may_move` is false when the caller re-read a rewritten history
    /// file and must not reorder commands that were already known.
    pub fn upsert_command(
        &mut self,
        text: &str,
        shell: &str,
        date_ms: i64,
        max: usize,
        may_move: bool,
    ) -> bool {
        if text.trim().is_empty() {
            return false;
        }
        let captured = Captured {
            kind: ClipKind::Command,
            preview: make_preview(text),
            text: Some(text.to_string()),
            data_type: None,
            data: None,
            byte_len: text.len(),
            width: None,
            height: None,
            source_name: Some(shell.to_string()),
            source_bundle: None,
        };
        let hash = hash_captured(&captured);

        if let Some(pos) = self.clips.iter().position(|clip| clip.hash == hash) {
            let Some(existing) = self.clips.get(pos) else {
                return false;
            };
            if !may_move || date_ms <= existing.date_ms {
                return false;
            }
            let pinned = existing.pinned;
            let mut clip = self.clips.remove(pos);
            clip.date_ms = date_ms;
            clip.source_name = captured.source_name;
            let insert_at = if pinned {
                0
            } else {
                self.clips.iter().filter(|clip| clip.pinned).count()
            };
            self.clips.insert(insert_at, clip);
            return true;
        }

        let clip = Clip {
            id: Uuid::new_v4().to_string(),
            kind: ClipKind::Command,
            preview: captured.preview,
            text: captured.text,
            data_type: None,
            blob: None,
            byte_len: captured.byte_len,
            width: None,
            height: None,
            source_name: captured.source_name,
            source_bundle: None,
            date_ms,
            hash,
            title: None,
            pinned: false,
        };
        let insert_at = self.clips.iter().filter(|clip| clip.pinned).count();
        self.clips.insert(insert_at, clip);
        self.trim_where(max, |clip| clip.kind == ClipKind::Command);
        true
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

    /// Replace the optional display title. An empty or whitespace-only string
    /// clears it. Returns true when the stored title changed.
    pub fn set_title(&mut self, index: usize, title: &str) -> bool {
        let title = make_title(title);
        let Some(clip) = self.clips.get_mut(index) else {
            return false;
        };
        if clip.title == title {
            return false;
        }
        clip.title = title;
        true
    }

    /// Replace the text of a plain-text or command clip. The source metadata,
    /// id, date, title and pin are preserved, while the preview, byte length and
    /// hash are recomputed. Returns `Ok(false)` when the text is unchanged.
    ///
    /// Duplicates are refused instead of merged so an edit cannot silently
    /// remove the metadata of another row.
    pub fn set_text(&mut self, index: usize, text: &str) -> Result<bool, EditTextError> {
        let Some(clip) = self.clips.get(index) else {
            return Err(EditTextError::Unavailable);
        };
        if !matches!(clip.kind, ClipKind::Text | ClipKind::Command) {
            return Err(EditTextError::Unavailable);
        }
        if text.trim().is_empty() {
            return Err(EditTextError::Empty);
        }
        if text.len() > MAX_EDIT_TEXT_BYTES {
            return Err(EditTextError::TooLarge);
        }
        if clip.text.as_deref() == Some(text) {
            return Ok(false);
        }

        let captured = Captured {
            kind: clip.kind,
            preview: make_preview(text),
            text: Some(text.to_string()),
            data_type: None,
            data: None,
            byte_len: text.len(),
            width: None,
            height: None,
            source_name: clip.source_name.clone(),
            source_bundle: clip.source_bundle.clone(),
        };
        let hash = hash_captured(&captured);
        if self
            .clips
            .iter()
            .enumerate()
            .any(|(other_index, other)| other_index != index && other.hash == hash)
        {
            return Err(EditTextError::Duplicate);
        }

        let Some(clip) = self.clips.get_mut(index) else {
            return Err(EditTextError::Unavailable);
        };
        clip.text = captured.text;
        clip.preview = captured.preview;
        clip.byte_len = captured.byte_len;
        clip.hash = hash;
        Ok(true)
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

    /// Number of clippings that [`Self::clear_scope`] would remove.
    pub fn count_scope(&self, scope: ClearScope) -> usize {
        self.clips.iter().filter(|clip| scope.matches(clip)).count()
    }

    /// Remove every clipping matching `scope`, deleting its blob. Commands and
    /// pinned rows outside the scope are kept. Returns the number removed.
    pub fn clear_scope(&mut self, scope: ClearScope) -> usize {
        self.remove_where(|clip| scope.matches(clip))
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

/// Bound on directory entries inspected for the disk-usage line. A history is
/// normally far below this; a hostile directory cannot stall the UI forever.
const MAX_USAGE_ENTRIES: usize = 100_000;

fn regular_file_len(path: &Path) -> u64 {
    fs::symlink_metadata(path)
        .ok()
        .filter(|meta| meta.file_type().is_file())
        .map(|meta| meta.len())
        .unwrap_or(0)
}

/// Sum regular files directly in `dir`, skipping `skip` when it names a file.
/// Symlinks and nested directories are not followed.
fn directory_file_bytes(dir: &Path, skip: Option<&str>) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for (index, entry) in entries.flatten().enumerate() {
        if index >= MAX_USAGE_ENTRIES {
            break;
        }
        if let Some(skip) = skip
            && entry.file_name().to_str() == Some(skip)
        {
            continue;
        }
        if let Ok(meta) = fs::symlink_metadata(entry.path())
            && meta.file_type().is_file()
        {
            total = total.saturating_add(meta.len());
        }
    }
    total
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

/// Normalise a user-chosen title: one line, collapsed whitespace, bounded
/// length. An empty result clears the title.
pub fn make_title(text: &str) -> Option<String> {
    let mut out = String::new();
    let mut last_space = false;
    for ch in text.chars() {
        let ch = if ch.is_whitespace() || ch.is_control() {
            ' '
        } else {
            ch
        };
        if ch == ' ' {
            if last_space || out.is_empty() {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        out.push(ch);
        if out.chars().count() >= MAX_TITLE_CHARS {
            break;
        }
    }
    let trimmed = out.trim_end();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
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

/// The application data directory, with a one-time rename from the directory
/// used before the product rename. Nothing is lost if the rename fails: the new
/// directory is simply created empty and the old history stays where it was.
fn default_dir() -> PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("AntiRustPaste");
    migrate_legacy_dir(&dir, &base.join("RustClip"));
    dir
}

fn migrate_legacy_dir(new: &Path, old: &Path) {
    if new.exists() || !old.exists() {
        return;
    }
    let _ = fs::rename(old, new);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("antirustpaste-test-{}", Uuid::new_v4()));
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
            source_name: None,
            source_bundle: None,
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
            source_name: None,
            source_bundle: None,
        }
    }

    fn rich_clip(kind: ClipKind, text: Option<&str>) -> Clip {
        Clip {
            id: "rich".to_string(),
            kind,
            preview: "Rich Text".to_string(),
            text: text.map(str::to_string),
            data_type: Some("public.rtf".to_string()),
            blob: None,
            byte_len: 0,
            width: None,
            height: None,
            source_name: None,
            source_bundle: None,
            date_ms: 0,
            hash: 0,
            title: None,
            pinned: false,
        }
    }

    #[test]
    fn plain_text_is_only_exposed_for_rich_clips() {
        let rich = rich_clip(ClipKind::RichText, Some("plain words"));
        assert_eq!(rich.plain_text(), Some("plain words"));

        let html = rich_clip(ClipKind::Html, Some("html words"));
        assert_eq!(html.plain_text(), Some("html words"));

        let empty = rich_clip(ClipKind::RichText, Some(""));
        assert_eq!(empty.plain_text(), None);

        let missing = rich_clip(ClipKind::Html, None);
        assert_eq!(missing.plain_text(), None);

        for kind in [
            ClipKind::Text,
            ClipKind::Command,
            ClipKind::File,
            ClipKind::Image,
        ] {
            assert_eq!(rich_clip(kind, Some("plain words")).plain_text(), None);
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
    fn title_normalisation_collapses_whitespace_and_bounds_length() {
        assert_eq!(
            make_title("  release \n\t checklist  ").as_deref(),
            Some("release checklist")
        );
        assert_eq!(make_title(" \n\t "), None);
        assert_eq!(make_title("a\0b"), Some("a b".to_string()));
        let long = "x".repeat(500);
        assert_eq!(
            make_title(&long).map(|title| title.chars().count()),
            Some(MAX_TITLE_CHARS)
        );
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
    fn a_title_survives_recopying_and_can_be_cleared() {
        let temp = TempDir::new();
        let mut store = temp.store();

        store.insert(text_clip("original"), 10);
        assert!(store.set_title(0, "  My   note "));
        assert_eq!(store.clips[0].title.as_deref(), Some("My note"));
        assert_eq!(store.clips[0].display_title(), "My note");
        assert_eq!(store.clips[0].content_title(), "original");

        store.insert(text_clip("another"), 10);
        store.insert(text_clip("original"), 10);
        assert_eq!(store.clips.len(), 2);
        assert_eq!(store.clips[0].title.as_deref(), Some("My note"));

        assert!(store.set_title(0, " \n "));
        assert_eq!(store.clips[0].title, None);
        assert_eq!(store.clips[0].display_title(), "original");
        assert!(!store.set_title(0, ""));
        assert!(!store.set_title(99, "ignored"));
    }

    #[test]
    fn editing_text_updates_preview_hash_and_keeps_metadata() {
        let temp = TempDir::new();
        let mut store = temp.store();

        store.insert(text_clip("old text"), 10);
        store.set_title(0, "note");
        store.set_pinned(0, true).expect("pin the clip");
        let original_id = store.clips[0].id.clone();
        let original_date = store.clips[0].date_ms;

        assert_eq!(store.set_text(0, "first line\nsecond line"), Ok(true));
        let clip = &store.clips[0];
        assert_eq!(clip.text.as_deref(), Some("first line\nsecond line"));
        assert_eq!(clip.preview, make_preview("first line\nsecond line"));
        assert_eq!(clip.byte_len, "first line\nsecond line".len());
        assert_eq!(
            clip.hash,
            hash_captured(&text_clip("first line\nsecond line"))
        );
        assert_eq!(clip.id, original_id);
        assert_eq!(clip.date_ms, original_date);
        assert_eq!(clip.title.as_deref(), Some("note"));
        assert!(clip.pinned);
        assert_eq!(store.set_text(0, "first line\nsecond line"), Ok(false));

        store.save();
        let reopened = temp.store();
        assert_eq!(
            reopened.clips[0].text.as_deref(),
            Some("first line\nsecond line")
        );
        assert_eq!(
            reopened.clips[0].hash,
            hash_captured(&text_clip("first line\nsecond line"))
        );
    }

    #[test]
    fn editing_text_rejects_invalid_input_and_duplicates() {
        let temp = TempDir::new();
        let mut store = temp.store();

        store.insert(text_clip("alpha"), 10);
        store.insert(text_clip("beta"), 10);

        assert_eq!(store.set_text(0, " \n\t "), Err(EditTextError::Empty));
        assert_eq!(
            store.set_text(0, &"x".repeat(MAX_EDIT_TEXT_BYTES + 1)),
            Err(EditTextError::TooLarge)
        );
        assert_eq!(store.set_text(0, "alpha"), Err(EditTextError::Duplicate));
        assert_eq!(store.set_text(99, "gamma"), Err(EditTextError::Unavailable));

        store.insert(image_clip(b"not an image"), 10);
        let image_index = store
            .clips
            .iter()
            .position(|clip| clip.kind == ClipKind::Image)
            .expect("image clip");
        assert_eq!(
            store.set_text(image_index, "gamma"),
            Err(EditTextError::Unavailable)
        );
    }

    #[test]
    fn editing_a_command_keeps_its_shell() {
        let temp = TempDir::new();
        let mut store = temp.store();

        store.upsert_command("echo old", "zsh", 1_700_000_000_000, 10, true);
        let index = store
            .clips
            .iter()
            .position(|clip| clip.kind == ClipKind::Command)
            .expect("command clip");

        assert_eq!(store.set_text(index, "echo new"), Ok(true));
        assert_eq!(store.clips[index].text.as_deref(), Some("echo new"));
        assert_eq!(store.clips[index].source_name.as_deref(), Some("zsh"));
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
    fn commands_use_their_own_trim_budget() {
        let temp = TempDir::new();
        let mut store = temp.store();

        store.insert(text_clip("clip one"), 1);
        store.upsert_command("echo one", "zsh", 1_700_000_000_000, 1, true);
        store.insert(text_clip("clip two"), 1);
        store.upsert_command("echo two", "zsh", 1_700_000_001_000, 1, true);

        let clips = store
            .clips
            .iter()
            .filter(|clip| clip.kind != ClipKind::Command)
            .count();
        let commands = store
            .clips
            .iter()
            .filter(|clip| clip.kind == ClipKind::Command)
            .count();
        assert_eq!(clips, 1);
        assert_eq!(commands, 1);
        assert!(
            store
                .clips
                .iter()
                .any(|clip| clip.kind == ClipKind::Command
                    && clip.text.as_deref() == Some("echo two"))
        );
    }

    #[test]
    fn trim_sections_applies_the_limit_to_each_group() {
        let temp = TempDir::new();
        let mut store = temp.store();

        store.insert(text_clip("one"), 10);
        store.insert(text_clip("two"), 10);
        store.upsert_command("echo one", "zsh", 1, 10, true);
        store.upsert_command("echo two", "zsh", 2, 10, true);

        store.trim_sections(1);

        assert_eq!(
            store
                .clips
                .iter()
                .filter(|clip| clip.kind != ClipKind::Command)
                .count(),
            1
        );
        assert_eq!(
            store
                .clips
                .iter()
                .filter(|clip| clip.kind == ClipKind::Command)
                .count(),
            1
        );
        assert!(
            store
                .clips
                .iter()
                .any(|clip| clip.text.as_deref() == Some("two"))
        );
        assert!(
            store
                .clips
                .iter()
                .any(|clip| clip.text.as_deref() == Some("echo two"))
        );
    }

    #[test]
    fn a_repeated_command_moves_only_when_allowed_and_newer() {
        let temp = TempDir::new();
        let mut store = temp.store();

        assert!(store.upsert_command("ls -la", "zsh", 1_000, 10, true));
        assert!(!store.upsert_command("ls -la", "zsh", 2_000, 10, false));
        assert_eq!(store.clips.len(), 1);
        assert_eq!(store.clips[0].date_ms, 1_000);

        assert!(store.upsert_command("ls -la", "zsh", 3_000, 10, true));
        assert_eq!(store.clips[0].date_ms, 3_000);
        assert!(!store.upsert_command("ls -la", "zsh", 2_500, 10, true));
        assert_eq!(store.clips[0].date_ms, 3_000);
    }

    #[test]
    fn a_pinned_command_is_exempt_from_the_command_budget() {
        let temp = TempDir::new();
        let mut store = temp.store();

        store.upsert_command("pinned command", "bash", 1, 1, true);
        store.set_pinned(0, true).expect("pin command");
        store.upsert_command("newer command", "bash", 2, 1, true);

        let commands = store
            .clips
            .iter()
            .filter(|clip| clip.kind == ClipKind::Command)
            .count();
        assert_eq!(commands, 2);
        assert!(store.clips[0].pinned);
    }

    #[test]
    fn insert_keeps_the_source_on_the_new_clip_even_with_pinned_entries() {
        let temp = TempDir::new();
        let mut store = temp.store();

        store.insert(text_clip("pinned"), 10);
        store.set_pinned(0, true).expect("pin first clip");

        let mut second = text_clip("second");
        second.source_name = Some("Safari".to_string());
        second.source_bundle = Some("com.apple.Safari".to_string());
        store.insert(second, 10);

        assert_eq!(store.clips[0].text.as_deref(), Some("pinned"));
        assert_eq!(store.clips[0].source_name, None);
        let inserted = store
            .clips
            .iter()
            .find(|clip| clip.text.as_deref() == Some("second"))
            .expect("second clip");
        assert_eq!(inserted.source_name.as_deref(), Some("Safari"));
        assert_eq!(inserted.source_bundle.as_deref(), Some("com.apple.Safari"));
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

    fn scoped_store() -> (TempDir, Store) {
        let temp = TempDir::new();
        let mut store = temp.store();
        store.insert(text_clip("text"), 10);
        store.insert(image_clip(b"image"), 10);
        store.insert(text_clip("pinned text"), 10);
        store.set_pinned(0, true).expect("pin text");
        store.insert(image_clip(b"pinned image"), 10);
        store.set_pinned(1, true).expect("pin image");
        store.upsert_command("echo hi", "zsh", 1_700_000_000_000, 10, true);
        (temp, store)
    }

    fn has_text(store: &Store, text: &str) -> bool {
        store
            .clips
            .iter()
            .any(|clip| clip.text.as_deref() == Some(text))
    }

    #[test]
    fn clear_text_scope_removes_text_and_keeps_images_and_commands() {
        let (_temp, mut store) = scoped_store();

        assert_eq!(store.count_scope(ClearScope::Text), 2);
        assert_eq!(store.clear_scope(ClearScope::Text), 2);

        assert!(!has_text(&store, "text"));
        assert!(!has_text(&store, "pinned text"));
        assert!(
            store
                .clips
                .iter()
                .filter(|clip| clip.kind == ClipKind::Image)
                .count()
                == 2
        );
        assert!(
            store
                .clips
                .iter()
                .any(|clip| clip.kind == ClipKind::Command)
        );
    }

    #[test]
    fn clear_images_scope_removes_images_and_their_blobs() {
        let (_temp, mut store) = scoped_store();
        let blobs: Vec<PathBuf> = store
            .clips
            .iter()
            .filter(|clip| clip.kind == ClipKind::Image)
            .filter_map(|clip| clip.blob.as_deref())
            .map(|name| store.blob_path(name))
            .collect();
        assert_eq!(blobs.len(), 2);

        assert_eq!(store.count_scope(ClearScope::Images), 2);
        assert_eq!(store.clear_scope(ClearScope::Images), 2);

        assert!(store.clips.iter().all(|clip| clip.kind != ClipKind::Image));
        for blob in blobs {
            assert!(!blob.exists(), "blob should be deleted: {}", blob.display());
        }
        assert!(has_text(&store, "text"));
        assert!(has_text(&store, "pinned text"));
    }

    #[test]
    fn unpinned_scopes_keep_pinned_clips_and_commands() {
        let (_temp, mut store) = scoped_store();

        assert_eq!(store.clear_scope(ClearScope::UnpinnedText), 1);
        assert!(!has_text(&store, "text"));
        assert!(has_text(&store, "pinned text"));
        assert_eq!(store.count_scope(ClearScope::UnpinnedImages), 1);
        assert_eq!(store.clear_scope(ClearScope::UnpinnedImages), 1);
        assert!(
            store
                .clips
                .iter()
                .filter(|clip| clip.kind == ClipKind::Image)
                .count()
                == 1
        );
        assert!(
            store
                .clips
                .iter()
                .filter(|clip| clip.kind == ClipKind::Image)
                .all(|clip| clip.pinned)
        );
        assert!(
            store
                .clips
                .iter()
                .any(|clip| clip.kind == ClipKind::Command)
        );
    }

    #[test]
    fn clear_everything_keeps_commands() {
        let (_temp, mut store) = scoped_store();

        assert_eq!(store.count_scope(ClearScope::Everything), 4);
        assert_eq!(store.clear_scope(ClearScope::Everything), 4);

        assert_eq!(store.clips.len(), 1);
        assert_eq!(store.clips[0].kind, ClipKind::Command);
    }

    #[test]
    fn storage_usage_counts_index_blobs_and_sidecars() {
        let temp = TempDir::new();
        let mut store = temp.store();
        store.insert(image_clip(b"12345"), 10);
        store.save();
        fs::write(store.dir().join("shell-history.json"), b"abcd").expect("write sidecar");

        let usage = store.storage_usage();
        assert!(usage.index_bytes > 0);
        assert_eq!(usage.blob_bytes, 5);
        assert_eq!(usage.other_bytes, 4);
        assert_eq!(
            usage.history_total(),
            usage.index_bytes + usage.blob_bytes + usage.other_bytes
        );

        store.clear_scope(ClearScope::Everything);
        store.save();
        assert_eq!(store.storage_usage().blob_bytes, 0);
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
        assert_eq!(clip.title, None);
        assert_eq!(clip.text.as_deref(), Some("legacy clipping"));
    }

    #[test]
    fn an_oversized_title_is_normalised_on_load() {
        let temp = TempDir::new();
        let dir = temp.0.join("store");
        let _ = fs::create_dir_all(dir.join("blobs"));
        let long_title = "x".repeat(500);
        let json = format!(
            r#"[{{
                "id": "titled",
                "kind": "text",
                "preview": "body",
                "text": "body",
                "data_type": null,
                "blob": null,
                "byte_len": 4,
                "source_name": null,
                "source_bundle": null,
                "date_ms": 1700000000000,
                "hash": 2,
                "title": "{long_title}"
            }}]"#
        );
        fs::write(dir.join("history.json"), json).expect("write titled index");

        let store = Store::open_at(dir);
        let Some(clip) = store.clips.first() else {
            panic!("record should load");
        };
        assert_eq!(
            clip.title.as_deref().map(|title| title.chars().count()),
            Some(MAX_TITLE_CHARS)
        );
        assert_eq!(clip.display_title().chars().count(), MAX_TITLE_CHARS);
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
    #[test]
    fn legacy_history_directory_is_migrated_only_once() {
        let root = TempDir::new();
        let old = root.0.join("RustClip");
        let new = root.0.join("AntiRustPaste");
        fs::create_dir_all(old.join("blobs")).expect("old dir");
        fs::write(old.join("history.json"), "[]").expect("old index");

        migrate_legacy_dir(&new, &old);
        assert!(new.join("history.json").is_file());
        assert!(!old.exists());

        fs::create_dir_all(&old).expect("old dir again");
        migrate_legacy_dir(&new, &old);
        assert!(old.exists(), "an existing new directory wins");
        assert!(new.join("history.json").is_file());
    }
}
