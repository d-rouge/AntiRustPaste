//! Incremental reader for zsh and bash history files.
//!
//! Shell history is private data. This module reads at most one bounded window
//! per call, keeps commands in memory only and never logs command text.
//!
//! The shell owns the file paths; core owns parsing and the byte cursor. A
//! cursor is stored per shell name, so a shell must map to at most one file.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Longest single command kept. Long heredocs and binary junk are skipped.
pub const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_TAIL_BYTES: u64 = 1024 * 1024;
const MAX_DELTA_BYTES: u64 = 256 * 1024;
const FINGERPRINT_BYTES: u64 = 64;
const MAX_CURSOR_FILE_BYTES: u64 = 64 * 1024;
const MAX_CURSORS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shell {
    Zsh,
    Bash,
}

impl Shell {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Zsh => "zsh",
            Self::Bash => "bash",
        }
    }
}

/// One shell history file. The shell name is also the cursor key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellFile {
    pub shell: Shell,
    pub path: PathBuf,
}

/// One command read from a history file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellCommand {
    pub text: String,
    pub shell: Shell,
    pub date_ms: i64,
    /// True when a newer occurrence may move an existing entry to the top.
    /// Re-reads of a rewritten file without timestamps set this to false so a
    /// bulk import cannot reorder the whole list.
    pub may_move: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShellRead {
    pub commands: Vec<ShellCommand>,
    pub cursor_changed: bool,
}

impl ShellRead {
    fn empty() -> Self {
        Self::default()
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
struct Cursor {
    offset: u64,
    fingerprint: u64,
}

/// Byte offsets for the history files read so far. Missing or corrupt state
/// simply means "read the tail again"; it never fails the app.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ShellCursors {
    #[serde(default)]
    files: BTreeMap<String, Cursor>,
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// Load the cursor sidecar. Corrupt or oversized files are ignored.
pub fn load_cursors(path: &Path) -> ShellCursors {
    let Ok(meta) = fs::metadata(path) else {
        return ShellCursors::default();
    };
    if meta.len() > MAX_CURSOR_FILE_BYTES {
        return ShellCursors::default();
    }
    let Ok(raw) = fs::read(path) else {
        return ShellCursors::default();
    };
    let Ok(mut cursors) = serde_json::from_slice::<ShellCursors>(&raw) else {
        return ShellCursors::default();
    };
    if cursors.files.len() > MAX_CURSORS {
        cursors.files.clear();
    }
    cursors
}

/// Persist the cursor sidecar atomically. Failures are non-fatal: the next
/// refresh re-reads the tail.
pub fn save_cursors(path: &Path, cursors: &ShellCursors) {
    let Ok(bytes) = serde_json::to_vec(cursors) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if fs::write(&tmp, bytes)
        .and_then(|_| fs::rename(&tmp, path))
        .is_err()
    {
        eprintln!("antirustpaste: could not save the shell history cursor");
    }
}

/// Read commands appended since the last call, or the tail on a first read,
/// a truncation or a rewritten file.
pub fn read_new(
    file: &ShellFile,
    cursors: &mut ShellCursors,
    max_commands: usize,
    now_ms: i64,
) -> ShellRead {
    let limit = max_commands.max(1);
    let Ok(meta) = fs::metadata(&file.path) else {
        return ShellRead::empty();
    };
    if !meta.is_file() {
        return ShellRead::empty();
    }
    let len = meta.len();
    if len == 0 {
        return ShellRead::empty();
    }
    let fallback_ms = meta
        .modified()
        .ok()
        .and_then(system_time_ms)
        .unwrap_or(now_ms);
    let key = file.shell.label();
    let saved = cursors.files.get(key).copied();

    if let Some(cursor) = saved
        && cursor.offset > 0
        && cursor.offset <= len
        && fingerprint_matches(&file.path, cursor.offset, cursor.fingerprint)
    {
        if cursor.offset == len {
            return ShellRead::empty();
        }
        let (commands, new_offset) =
            read_delta(&file.path, cursor.offset, len, file.shell, limit, now_ms);
        if new_offset > cursor.offset {
            let fingerprint = fingerprint(&file.path, new_offset).unwrap_or(0);
            cursors.files.insert(
                key.to_string(),
                Cursor {
                    offset: new_offset,
                    fingerprint,
                },
            );
            return ShellRead {
                commands,
                cursor_changed: true,
            };
        }
        return ShellRead {
            commands,
            cursor_changed: false,
        };
    }

    // First read, truncation, rotation or an in-place rewrite: take the tail.
    let (commands, new_offset) = read_tail(&file.path, len, file.shell, limit, fallback_ms);
    let fingerprint = fingerprint(&file.path, new_offset).unwrap_or(0);
    cursors.files.insert(
        key.to_string(),
        Cursor {
            offset: new_offset,
            fingerprint,
        },
    );
    ShellRead {
        commands,
        cursor_changed: true,
    }
}

fn system_time_ms(time: SystemTime) -> Option<i64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
}

fn read_delta(
    path: &Path,
    offset: u64,
    len: u64,
    shell: Shell,
    limit: usize,
    now_ms: i64,
) -> (Vec<ShellCommand>, u64) {
    let want = len.saturating_sub(offset).min(MAX_DELTA_BYTES);
    if want == 0 {
        return (Vec::new(), offset);
    }
    let Ok(mut file) = File::open(path) else {
        return (Vec::new(), offset);
    };
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return (Vec::new(), offset);
    }
    let mut buffer = Vec::new();
    if file.take(want).read_to_end(&mut buffer).is_err() {
        return (Vec::new(), offset);
    }
    let Some(last_newline) = buffer.iter().rposition(|byte| *byte == b'\n') else {
        // A very long unfinished line must not stall every refresh. Once the
        // window is full, skip it; otherwise wait for the line to finish.
        let new_offset = if buffer.len() as u64 >= want {
            offset.saturating_add(buffer.len() as u64)
        } else {
            offset
        };
        return (Vec::new(), new_offset);
    };
    let complete = buffer.get(..=last_newline).unwrap_or(&[]);
    let (commands, consumed) = parse_delta(complete, shell, now_ms, limit);
    (commands, offset.saturating_add(consumed as u64))
}

fn read_tail(
    path: &Path,
    len: u64,
    shell: Shell,
    limit: usize,
    fallback_ms: i64,
) -> (Vec<ShellCommand>, u64) {
    let start = len.saturating_sub(MAX_TAIL_BYTES);
    let Ok(mut file) = File::open(path) else {
        return (Vec::new(), start);
    };
    if file.seek(SeekFrom::Start(start)).is_err() {
        return (Vec::new(), start);
    }
    let mut buffer = Vec::new();
    if file.take(MAX_TAIL_BYTES).read_to_end(&mut buffer).is_err() {
        return (Vec::new(), start);
    }
    let skip = if start == 0 {
        0
    } else {
        match buffer.iter().position(|byte| *byte == b'\n') {
            Some(index) => index + 1,
            // One enormous line: move past it instead of retrying forever.
            None => return (Vec::new(), len),
        }
    };
    let slice = buffer.get(skip..).unwrap_or(&[]);
    let Some(last_newline) = slice.iter().rposition(|byte| *byte == b'\n') else {
        // No complete line yet. Keep the cursor before this partial line.
        return (Vec::new(), start.saturating_add(skip as u64));
    };
    let complete = slice.get(..=last_newline).unwrap_or(&[]);
    let commands = parse_tail(complete, shell, fallback_ms, limit);
    let consumed = start
        .saturating_add(skip as u64)
        .saturating_add(complete.len() as u64);
    (commands, consumed)
}

fn parse_tail(bytes: &[u8], shell: Shell, fallback_ms: i64, limit: usize) -> Vec<ShellCommand> {
    let mut out: VecDeque<ShellCommand> = VecDeque::with_capacity(limit.min(256));
    let mut pending_ms = None;
    let mut start = 0usize;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let line = bytes.get(start..index).unwrap_or(&[]);
        start = index + 1;
        let Some(input) = std::str::from_utf8(line).ok() else {
            continue;
        };
        if let Some(command) = parse_entry(input, shell, &mut pending_ms, fallback_ms, false) {
            if out.len() >= limit {
                out.pop_front();
            }
            out.push_back(command);
        }
    }
    out.into_iter().collect()
}

fn parse_delta(
    bytes: &[u8],
    shell: Shell,
    fallback_ms: i64,
    limit: usize,
) -> (Vec<ShellCommand>, usize) {
    let mut out = Vec::new();
    let mut pending_ms = None;
    let mut start = 0usize;
    let mut consumed = 0usize;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        if out.len() >= limit {
            break;
        }
        let line = bytes.get(start..index).unwrap_or(&[]);
        start = index + 1;
        consumed = start;
        let Some(input) = std::str::from_utf8(line).ok() else {
            continue;
        };
        if let Some(command) = parse_entry(input, shell, &mut pending_ms, fallback_ms, true) {
            out.push(command);
        }
    }
    (out, consumed)
}

fn parse_entry(
    line: &str,
    shell: Shell,
    pending_ms: &mut Option<i64>,
    fallback_ms: i64,
    delta: bool,
) -> Option<ShellCommand> {
    let line = line.trim_end_matches('\r');
    if line.is_empty() || line.contains('\0') {
        return None;
    }

    if shell == Shell::Bash
        && let Some(rest) = line.strip_prefix('#')
        && !rest.is_empty()
        && rest.bytes().all(|byte| byte.is_ascii_digit())
    {
        if let Ok(seconds) = rest.parse::<i64>()
            && let Some(ms) = seconds.checked_mul(1000)
            && ms > 0
        {
            *pending_ms = Some(ms);
        }
        return None;
    }

    let (command, timed_ms) = match shell {
        Shell::Zsh => parse_zsh(line),
        Shell::Bash => (line.to_string(), pending_ms.take()),
    };
    *pending_ms = None;

    if command.len() > MAX_COMMAND_BYTES || command.trim().is_empty() || command.contains('\0') {
        return None;
    }

    Some(ShellCommand {
        text: command,
        shell,
        date_ms: timed_ms.unwrap_or(fallback_ms),
        may_move: delta || timed_ms.is_some(),
    })
}

/// zsh extended history: `: <epoch>:<elapsed>;<command>`.
fn parse_zsh(line: &str) -> (String, Option<i64>) {
    let Some(rest) = line.strip_prefix(": ") else {
        return (line.to_string(), None);
    };
    let Some((meta, command)) = rest.split_once(';') else {
        return (line.to_string(), None);
    };
    let Some(seconds) = meta
        .split(':')
        .next()
        .and_then(|value| value.parse::<i64>().ok())
    else {
        return (line.to_string(), None);
    };
    let Some(ms) = seconds.checked_mul(1000).filter(|value| *value > 0) else {
        return (line.to_string(), None);
    };
    (command.to_string(), Some(ms))
}

fn fingerprint(path: &Path, offset: u64) -> Option<u64> {
    if offset == 0 {
        return Some(0);
    }
    let from = offset.saturating_sub(FINGERPRINT_BYTES);
    let length = usize::try_from(offset - from).ok()?;
    let mut file = File::open(path).ok()?;
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut buffer = vec![0u8; length];
    file.read_exact(&mut buffer).ok()?;
    let mut hasher = DefaultHasher::new();
    buffer.hash(&mut hasher);
    Some(hasher.finish())
}

fn fingerprint_matches(path: &Path, offset: u64, expected: u64) -> bool {
    fingerprint(path, offset) == Some(expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("antirustpaste-shell-{}", Uuid::new_v4()));
            let _ = fs::create_dir_all(&path);
            Self(path)
        }

        fn history(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn read(
        path: &Path,
        shell: Shell,
        cursors: &mut ShellCursors,
        max: usize,
        now_ms: i64,
    ) -> ShellRead {
        read_new(
            &ShellFile {
                shell,
                path: path.to_path_buf(),
            },
            cursors,
            max,
            now_ms,
        )
    }

    #[test]
    fn zsh_extended_entries_keep_commands_and_timestamps() {
        let temp = TempDir::new();
        let path = temp.history(".zsh_history");
        fs::write(
            &path,
            ": 1700000000:0;echo antirustpaste-smoke-1\n\
             : 1700000005:0;printf 'a;b'\n\
             plain command\n",
        )
        .expect("write history");

        let mut cursors = ShellCursors::default();
        let read = read(&path, Shell::Zsh, &mut cursors, 200, 42);
        let file_ms = fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(system_time_ms)
            .expect("history mtime");

        assert_eq!(read.commands.len(), 3);
        assert_eq!(read.commands[0].text, "echo antirustpaste-smoke-1");
        assert_eq!(read.commands[0].date_ms, 1_700_000_000_000);
        assert_eq!(read.commands[1].text, "printf 'a;b'");
        assert_eq!(read.commands[1].date_ms, 1_700_000_005_000);
        assert_eq!(read.commands[2].text, "plain command");
        assert_eq!(read.commands[2].date_ms, file_ms);
        assert!(read.commands[0].may_move);
        assert!(!read.commands[2].may_move);
    }

    #[test]
    fn bash_timestamp_comments_apply_to_the_next_command() {
        let temp = TempDir::new();
        let path = temp.history(".bash_history");
        fs::write(&path, "#1700000000\necho one\necho two\n").expect("write history");

        let mut cursors = ShellCursors::default();
        let read = read(&path, Shell::Bash, &mut cursors, 200, 42);
        let file_ms = fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(system_time_ms)
            .expect("history mtime");

        assert_eq!(read.commands.len(), 2);
        assert_eq!(read.commands[0].text, "echo one");
        assert_eq!(read.commands[0].date_ms, 1_700_000_000_000);
        assert_eq!(read.commands[1].text, "echo two");
        assert_eq!(read.commands[1].date_ms, file_ms);
    }

    #[test]
    fn the_first_read_keeps_only_the_last_commands() {
        let temp = TempDir::new();
        let path = temp.history(".bash_history");
        fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").expect("write history");

        let mut cursors = ShellCursors::default();
        let read = read(&path, Shell::Bash, &mut cursors, 2, 42);

        let texts: Vec<&str> = read.commands.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["four", "five"]);
    }

    #[test]
    fn a_second_read_returns_only_appended_commands() {
        let temp = TempDir::new();
        let path = temp.history(".zsh_history");
        fs::write(&path, "ls\n").expect("write history");

        let mut cursors = ShellCursors::default();
        let first = read(&path, Shell::Zsh, &mut cursors, 200, 42);
        assert_eq!(first.commands.len(), 1);
        assert!(first.cursor_changed);

        let idle = read(&path, Shell::Zsh, &mut cursors, 200, 43);
        assert!(idle.commands.is_empty());
        assert!(!idle.cursor_changed);

        fs::write(&path, "ls\npwd\n").expect("append history");
        let second = read(&path, Shell::Zsh, &mut cursors, 200, 44);
        assert_eq!(second.commands.len(), 1);
        assert_eq!(second.commands[0].text, "pwd");
        assert_eq!(second.commands[0].date_ms, 44);
        assert!(second.commands[0].may_move);
    }

    #[test]
    fn a_rewritten_file_is_read_as_a_tail() {
        let temp = TempDir::new();
        let path = temp.history(".bash_history");
        fs::write(&path, "one\ntwo\n").expect("write history");

        let mut cursors = ShellCursors::default();
        let first = read(&path, Shell::Bash, &mut cursors, 200, 42);
        assert_eq!(first.commands.len(), 2);

        fs::write(&path, "three\nfour\n").expect("rewrite history");
        let second = read(&path, Shell::Bash, &mut cursors, 200, 43);

        let texts: Vec<&str> = second.commands.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["three", "four"]);
        assert!(second.commands.iter().all(|command| !command.may_move));
    }

    #[test]
    fn a_partial_last_line_waits_for_its_newline() {
        let temp = TempDir::new();
        let path = temp.history(".bash_history");
        fs::write(&path, "one\ntwo").expect("write history");

        let mut cursors = ShellCursors::default();
        let first = read(&path, Shell::Bash, &mut cursors, 200, 42);
        assert_eq!(first.commands.len(), 1);
        assert_eq!(first.commands[0].text, "one");

        fs::write(&path, "one\ntwo\n").expect("finish line");
        let second = read(&path, Shell::Bash, &mut cursors, 200, 43);
        assert_eq!(second.commands.len(), 1);
        assert_eq!(second.commands[0].text, "two");
    }

    #[test]
    fn oversized_and_invalid_lines_are_skipped() {
        let temp = TempDir::new();
        let path = temp.history(".bash_history");
        let mut bytes = Vec::new();
        bytes.extend(std::iter::repeat_n(b'x', MAX_COMMAND_BYTES + 10));
        bytes.push(b'\n');
        bytes.extend_from_slice(&[0xff, b'\n']);
        bytes.extend_from_slice(b"ok\n");
        fs::write(&path, bytes).expect("write history");

        let mut cursors = ShellCursors::default();
        let read = read(&path, Shell::Bash, &mut cursors, 200, 42);

        assert_eq!(read.commands.len(), 1);
        assert_eq!(read.commands[0].text, "ok");
    }

    #[test]
    fn a_corrupt_cursor_file_is_ignored() {
        let temp = TempDir::new();
        let path = temp.0.join("shell-history.json");
        fs::write(&path, b"{ not json").expect("write cursor");

        let cursors = load_cursors(&path);
        assert!(cursors.files.is_empty());
    }

    #[test]
    fn cursors_are_saved_atomically() {
        let temp = TempDir::new();
        let path = temp.0.join("shell-history.json");
        let mut cursors = ShellCursors::default();
        cursors.files.insert(
            "zsh".to_string(),
            Cursor {
                offset: 12,
                fingerprint: 34,
            },
        );

        save_cursors(&path, &cursors);

        assert!(path.is_file());
        assert!(!path.with_extension("json.tmp").exists());
        let loaded = load_cursors(&path);
        assert_eq!(loaded.files.len(), 1);
        assert_eq!(
            loaded.files.get("zsh").map(|cursor| cursor.offset),
            Some(12)
        );
    }

    #[test]
    fn a_missing_file_is_a_no_op() {
        let temp = TempDir::new();
        let mut cursors = ShellCursors::default();
        let read = read(
            &temp.history(".does-not-exist"),
            Shell::Zsh,
            &mut cursors,
            200,
            42,
        );

        assert!(read.commands.is_empty());
        assert!(!read.cursor_changed);
        assert!(cursors.files.is_empty());
    }
}
