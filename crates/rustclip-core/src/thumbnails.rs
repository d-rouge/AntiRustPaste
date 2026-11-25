//! Platform-neutral thumbnail cache and background generation.
//!
//! Decoding lives in `rustclip-imaging`. This module owns the temporary cache,
//! the single generation worker and the job/result channels. A platform shell
//! passes the cache directory (and can apply its own boot-clearing policy) and
//! converts the returned PNG bytes into its native image type.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};

use crate::store::MAX_BLOB_BYTES;

/// 64 pt at 2x. Large enough for a 40 pt row thumbnail.
pub const MAX_EDGE: u32 = 128;

/// A thumbnail PNG at the supported edge should be far below this. Refuse a
/// cache file larger than this instead of reading it into memory.
const MAX_CACHE_FILE_BYTES: u64 = 4 * 1024 * 1024;

fn file_name(hash: u64) -> String {
    format!("thumb-{hash:016x}.png")
}

/// Remove cached files whose hash is no longer in the history.
pub fn prune_now(dir: &Path, valid: &HashSet<u64>) {
    prune_in(dir, valid);
}

/// Load a finished thumbnail from the cache as PNG bytes.
pub fn load_cached(dir: &Path, hash: u64) -> Option<Vec<u8>> {
    read_cache(&dir.join(file_name(hash)))
}

/// Background generation service. Holds the job/result channels and the hashes
/// currently being generated.
pub struct ThumbnailService {
    dir: PathBuf,
    jobs: Sender<Command>,
    results: Receiver<JobResult>,
    pending: HashSet<(u64, u64)>,
    generation: u64,
}

enum Command {
    Generate(Job),
    Prune(HashSet<u64>),
    Clear,
}

struct Job {
    hash: u64,
    blob_path: PathBuf,
    generation: u64,
}

/// Result of a background generation. The platform shell loads the PNG on `ok`.
pub struct JobResult {
    pub hash: u64,
    pub ok: bool,
    generation: u64,
}

impl ThumbnailService {
    /// Start the single `rustclip-thumbnails` worker for `dir`.
    pub fn new(dir: PathBuf) -> Self {
        let (job_tx, job_rx) = channel();
        let (result_tx, result_rx) = channel();
        let worker_dir = dir.clone();
        if let Err(err) = std::thread::Builder::new()
            .name("rustclip-thumbnails".to_string())
            .spawn(move || worker(job_rx, result_tx, worker_dir))
        {
            eprintln!("rustclip: could not start thumbnail worker: {err}");
        }
        Self {
            dir,
            jobs: job_tx,
            results: result_rx,
            pending: HashSet::new(),
            generation: 0,
        }
    }

    /// Return cached PNG bytes, or queue generation and return `None`.
    ///
    /// `blob_path` is `None` when the clip has no usable blob; in that case a
    /// cache miss cannot be recovered.
    pub fn image_for(&mut self, hash: u64, blob_path: Option<PathBuf>) -> Option<Vec<u8>> {
        let path = self.dir.join(file_name(hash));
        if let Some(bytes) = read_cache(&path) {
            return Some(bytes);
        }
        if path.exists() {
            let _ = fs::remove_file(&path);
        }

        let blob_path = blob_path?;
        let key = (hash, self.generation);
        if self.pending.contains(&key) {
            return None;
        }
        if self
            .jobs
            .send(Command::Generate(Job {
                hash,
                blob_path,
                generation: self.generation,
            }))
            .is_ok()
        {
            self.pending.insert(key);
        }
        None
    }

    /// Load a finished thumbnail for this service's cache directory.
    pub fn load_cached(&self, hash: u64) -> Option<Vec<u8>> {
        load_cached(&self.dir, hash)
    }

    /// Take finished jobs without blocking. The caller loads the PNG and
    /// redraws on its UI thread.
    pub fn drain(&mut self) -> Vec<JobResult> {
        let mut finished = Vec::new();
        while let Ok(result) = self.results.try_recv() {
            self.pending.remove(&(result.hash, result.generation));
            if result.generation == self.generation {
                finished.push(result);
            }
        }
        finished
    }

    /// Queue a prune after the jobs already in flight.
    pub fn prune(&self, valid: HashSet<u64>) {
        let _ = self.jobs.send(Command::Prune(valid));
    }

    /// Delete cached preview files after the jobs already in flight. Pending
    /// results from before the clear are discarded.
    pub fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.pending.clear();
        let _ = self.jobs.send(Command::Clear);
    }

    /// Remove existing preview files immediately. A bounded directory scan.
    pub fn clear_now(&self) {
        prune_in(&self.dir, &HashSet::new());
    }
}

fn worker(jobs: Receiver<Command>, results: Sender<JobResult>, dir: PathBuf) {
    while let Ok(command) = jobs.recv() {
        match command {
            Command::Generate(job) => {
                // The imaging crate returns errors, but guard the worker anyway:
                // a panic here must not kill the only generator thread.
                let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    generate_from_blob(&job.blob_path, job.hash, &dir)
                }))
                .unwrap_or(false);
                if !ok {
                    eprintln!(
                        "rustclip: thumbnail generation failed for {:016x}",
                        job.hash
                    );
                }
                let _ = results.send(JobResult {
                    hash: job.hash,
                    ok,
                    generation: job.generation,
                });
            }
            Command::Prune(valid) => prune_in(&dir, &valid),
            Command::Clear => prune_in(&dir, &HashSet::new()),
        }
    }
}

fn generate_from_blob(blob_path: &Path, hash: u64, dir: &Path) -> bool {
    let Ok(metadata) = fs::metadata(blob_path) else {
        return false;
    };
    if metadata.len() > MAX_BLOB_BYTES {
        return false;
    }
    let Ok(bytes) = fs::read(blob_path) else {
        return false;
    };
    let Ok(thumb) = rustclip_imaging::thumbnail_png(&bytes, MAX_EDGE) else {
        return false;
    };
    write_atomic(dir, &dir.join(file_name(hash)), &thumb.png)
}

fn read_cache(path: &Path) -> Option<Vec<u8>> {
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() > MAX_CACHE_FILE_BYTES {
        let _ = fs::remove_file(path);
        return None;
    }
    fs::read(path).ok()
}

fn write_atomic(dir: &Path, path: &Path, bytes: &[u8]) -> bool {
    let created = !dir.exists();
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    if created {
        private_dir(dir);
    }

    let tmp = dir.join(format!(".thumb-{}.tmp", std::process::id()));
    if fs::write(&tmp, bytes).is_err() {
        let _ = fs::remove_file(&tmp);
        return false;
    }
    private_file(&tmp);
    match fs::rename(&tmp, path) {
        Ok(()) => true,
        Err(_) => {
            let _ = fs::remove_file(&tmp);
            false
        }
    }
}

fn prune_in(dir: &Path, valid: &HashSet<u64>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(hash) = parse_file_name(name) else {
            continue;
        };
        if !valid.contains(&hash) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn parse_file_name(name: &str) -> Option<u64> {
    let hex = name.strip_prefix("thumb-")?.strip_suffix(".png")?;
    if hex.len() != 16 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(hex, 16).ok()
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

#[cfg(unix)]
fn private_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        let _ = fs::set_permissions(path, permissions);
    }
}

#[cfg(not(unix))]
fn private_file(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x72,
        0xb6, 0x0d, 0x24, 0x00, 0x00, 0x00, 0x11, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x84, 0x19, 0x60, 0x0c, 0x00, 0x47, 0xca, 0x07, 0xf9, 0x1a, 0xb6,
        0xf1, 0xa9, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("rustclip-thumbs-{}", Uuid::new_v4()));
            let _ = fs::create_dir_all(&path);
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn file_names_round_trip_and_reject_junk() {
        assert_eq!(parse_file_name(&file_name(42)), Some(42));
        assert_eq!(parse_file_name("thumb-000000000000002a.png"), Some(42));
        assert_eq!(parse_file_name("thumb-42.png"), None);
        assert_eq!(parse_file_name("../thumb-0000000000000001.png"), None);
        assert_eq!(parse_file_name("thumb-zzzzzzzzzzzzzzzz.png"), None);
        assert_eq!(parse_file_name("other-0000000000000001.png"), None);
    }

    #[test]
    fn prune_keeps_valid_and_removes_stale_entries() {
        let temp = TempDir::new();
        let keep = temp.0.join(file_name(7));
        let drop = temp.0.join(file_name(9));
        let unrelated = temp.0.join("notes.txt");
        fs::write(&keep, b"keep").expect("write keep");
        fs::write(&drop, b"drop").expect("write drop");
        fs::write(&unrelated, b"unrelated").expect("write unrelated");

        let mut valid = HashSet::new();
        valid.insert(7);
        prune_in(&temp.0, &valid);

        assert!(keep.exists());
        assert!(!drop.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn generating_from_a_blob_writes_a_cache_file() {
        let temp = TempDir::new();
        let blob = temp.0.join("source.bin");
        fs::write(&blob, PNG).expect("write png");

        assert!(generate_from_blob(&blob, 11, &temp.0));
        let cached = temp.0.join(file_name(11));
        assert!(cached.exists());
        let bytes = fs::read(cached).expect("read cache");
        assert!(bytes.starts_with(b"\x89PNG"));
    }

    #[test]
    fn worker_generates_in_the_background() {
        let temp = TempDir::new();
        let blob = temp.0.join("source.bin");
        fs::write(&blob, PNG).expect("write png");

        let mut service = ThumbnailService::new(temp.0.clone());
        assert!(service.image_for(21, Some(blob)).is_none());

        let mut finished = false;
        for _ in 0..100 {
            for result in service.drain() {
                if result.hash == 21 && result.ok {
                    finished = true;
                }
            }
            if finished {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(finished, "worker did not finish within one second");
        assert!(temp.0.join(file_name(21)).exists());
    }

    #[test]
    fn clear_removes_generated_files() {
        let temp = TempDir::new();
        let blob = temp.0.join("source.bin");
        fs::write(&blob, PNG).expect("write png");

        let mut service = ThumbnailService::new(temp.0.clone());
        assert!(service.image_for(31, Some(blob)).is_none());
        let cached = temp.0.join(file_name(31));
        for _ in 0..100 {
            let _ = service.drain();
            if cached.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(cached.exists());

        service.clear();
        for _ in 0..100 {
            if !cached.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!cached.exists());
    }
}
