//! Private local control socket for the MCP bridge.
//!
//! The listener is non-blocking and is serviced by the existing 0.1 s timer on
//! the main thread. No new thread, queue or blocking wait is introduced.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use antirustpaste_control::{
    ControlError, ControlRequest, ControlResponse, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES,
    base64_encode, socket_path,
};
use serde_json::{Value, json};

use super::*;

const MAX_CONNECTIONS: usize = 8;
const MAX_REQUESTS_PER_TICK: usize = 16;
const CONNECTION_IDLE: Duration = Duration::from_secs(300);

pub(crate) struct ControlServer {
    listener: UnixListener,
    path: PathBuf,
    next_id: u64,
    connections: Vec<ControlConnection>,
}

struct ControlConnection {
    id: u64,
    stream: UnixStream,
    input: Vec<u8>,
    output: Vec<u8>,
    output_offset: usize,
    last_active: Instant,
    dead: bool,
}

impl ControlServer {
    /// Bind the per-user control socket. Failure only disables the MCP bridge;
    /// the menu-bar app keeps working.
    pub(crate) fn start() -> Option<Self> {
        let path = socket_path()?;
        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            eprintln!("antirustpaste: could not create the control directory: {error}");
            return None;
        }

        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_socket() => {
                if let Err(error) = fs::remove_file(&path) {
                    eprintln!("antirustpaste: could not remove the stale control socket: {error}");
                    return None;
                }
            }
            Ok(_) => {
                eprintln!("antirustpaste: refusing to replace a non-socket control path");
                return None;
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                eprintln!("antirustpaste: could not inspect the control socket: {error}");
                return None;
            }
        }

        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("antirustpaste: could not bind the control socket: {error}");
                return None;
            }
        };
        if let Err(error) = fs::set_permissions(&path, fs::Permissions::from_mode(0o600)) {
            eprintln!("antirustpaste: could not protect the control socket: {error}");
            let _ = fs::remove_file(&path);
            return None;
        }
        if let Err(error) = listener.set_nonblocking(true) {
            eprintln!("antirustpaste: could not make the control socket non-blocking: {error}");
            let _ = fs::remove_file(&path);
            return None;
        }

        Some(Self {
            listener,
            path,
            next_id: 1,
            connections: Vec::new(),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    fn poll(&mut self) -> Vec<(u64, ControlRequest)> {
        let now = Instant::now();
        self.connections
            .retain(|connection| now.duration_since(connection.last_active) < CONNECTION_IDLE);
        for connection in &mut self.connections {
            flush_connection(connection);
        }

        while self.connections.len() < MAX_CONNECTIONS {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let id = self.next_id;
                    self.next_id = self.next_id.wrapping_add(1);
                    self.connections.push(ControlConnection {
                        id,
                        stream,
                        input: Vec::new(),
                        output: Vec::new(),
                        output_offset: 0,
                        last_active: now,
                        dead: false,
                    });
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        let mut requests = Vec::new();
        for connection in &mut self.connections {
            if connection.dead || !connection.output.is_empty() {
                continue;
            }
            read_connection(connection, &mut requests);
        }
        self.connections.retain(|connection| !connection.dead);
        requests
    }

    fn queue_response(&mut self, connection_id: u64, response: ControlResponse) {
        if let Some(connection) = self
            .connections
            .iter_mut()
            .find(|connection| connection.id == connection_id)
        {
            enqueue_response(connection, response);
        }
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|meta| meta.file_type().is_socket()) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn flush_connection(connection: &mut ControlConnection) {
    while connection.output_offset < connection.output.len() {
        let remaining = &connection.output[connection.output_offset..];
        match connection.stream.write(remaining) {
            Ok(0) => {
                connection.dead = true;
                break;
            }
            Ok(written) => {
                connection.output_offset = connection.output_offset.saturating_add(written);
                connection.last_active = Instant::now();
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => {
                connection.dead = true;
                break;
            }
        }
    }
    if connection.output_offset >= connection.output.len() {
        connection.output.clear();
        connection.output_offset = 0;
    }
}

fn read_connection(connection: &mut ControlConnection, requests: &mut Vec<(u64, ControlRequest)>) {
    let mut buffer = [0u8; 4096];
    loop {
        match connection.stream.read(&mut buffer) {
            Ok(0) => {
                connection.dead = true;
                break;
            }
            Ok(read) => {
                connection.last_active = Instant::now();
                connection.input.extend_from_slice(&buffer[..read]);
                if connection.input.len() > MAX_REQUEST_BYTES {
                    enqueue_response(
                        connection,
                        ControlResponse::error(ControlError::new(
                            "request_too_large",
                            "The control request is too large.",
                        )),
                    );
                    connection.input.clear();
                    break;
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => {
                connection.dead = true;
                break;
            }
        }
    }

    while connection.output.is_empty() && requests.len() < MAX_REQUESTS_PER_TICK {
        let Some(end) = connection.input.iter().position(|byte| *byte == b'\n') else {
            break;
        };
        let line: Vec<u8> = connection.input.drain(..=end).collect();
        let Some(line) = line.get(..line.len().saturating_sub(1)) else {
            continue;
        };
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<ControlRequest>(line) {
            Ok(request) => requests.push((connection.id, request)),
            Err(_) => {
                enqueue_response(
                    connection,
                    ControlResponse::error(ControlError::new(
                        "bad_request",
                        "The control request is not valid JSON.",
                    )),
                );
                break;
            }
        }
    }
}

fn enqueue_response(connection: &mut ControlConnection, response: ControlResponse) {
    if connection.dead {
        return;
    }
    let mut bytes = match serde_json::to_vec(&response) {
        Ok(bytes) => bytes,
        Err(_) => {
            connection.dead = true;
            return;
        }
    };
    bytes.push(b'\n');
    if connection.output.len().saturating_add(bytes.len()) > MAX_RESPONSE_BYTES {
        let fallback = ControlResponse::error(ControlError::new(
            "response_too_large",
            "The response is too large for the control socket.",
        ));
        if let Ok(mut fallback_bytes) = serde_json::to_vec(&fallback) {
            fallback_bytes.push(b'\n');
            if connection.output.len().saturating_add(fallback_bytes.len()) <= MAX_RESPONSE_BYTES {
                connection.output.extend_from_slice(&fallback_bytes);
                return;
            }
        }
        connection.dead = true;
        return;
    }
    connection.output.extend_from_slice(&bytes);
}

/// Called from the existing 0.1 s timer. Requests are collected first so no
/// `with_state` borrow is held while an action re-enters the state.
pub(crate) fn poll() {
    let requests = with_state(|s| {
        s.control
            .as_mut()
            .map(ControlServer::poll)
            .unwrap_or_default()
    });
    for (connection_id, request) in requests {
        let response = handle_request(request);
        with_state(|s| {
            if let Some(control) = s.control.as_mut() {
                control.queue_response(connection_id, response);
            }
        });
    }
}

fn handle_request(request: ControlRequest) -> ControlResponse {
    match dispatch(&request.method, &request.params) {
        Ok(result) => ControlResponse::ok(result),
        Err(error) => ControlResponse::error(error),
    }
}

fn bad_parameter(message: impl Into<String>) -> ControlError {
    ControlError::new("bad_parameter", message)
}

fn required_string(params: &Value, key: &str) -> Result<String, ControlError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| bad_parameter(format!("Missing or invalid string parameter: {key}")))
}

fn optional_string(params: &Value, key: &str) -> Option<String> {
    params.get(key).and_then(Value::as_str).map(str::to_string)
}

fn optional_bool(params: &Value, key: &str) -> Option<bool> {
    params.get(key).and_then(Value::as_bool)
}

fn optional_usize(params: &Value, key: &str) -> Option<usize> {
    params
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn section_from_str(value: &str) -> Result<ClipboardSection, ControlError> {
    match value {
        "text" => Ok(ClipboardSection::Text),
        "images" => Ok(ClipboardSection::Images),
        "commands" => Ok(ClipboardSection::Commands),
        _ => Err(bad_parameter(
            "section must be one of text, images or commands",
        )),
    }
}

fn section_name(section: ClipboardSection) -> &'static str {
    match section {
        ClipboardSection::Text => "text",
        ClipboardSection::Images => "images",
        ClipboardSection::Commands => "commands",
    }
}

fn scope_from_str(value: &str) -> Result<ClearScope, ControlError> {
    match value {
        "all" | "everything" => Ok(ClearScope::Everything),
        "text" => Ok(ClearScope::Text),
        "images" => Ok(ClearScope::Images),
        "unpinned_text" => Ok(ClearScope::UnpinnedText),
        "unpinned_images" => Ok(ClearScope::UnpinnedImages),
        _ => Err(bad_parameter(
            "scope must be all, text, images, unpinned_text or unpinned_images",
        )),
    }
}

fn scope_name(scope: ClearScope) -> &'static str {
    match scope {
        ClearScope::Everything => "all",
        ClearScope::Text => "text",
        ClearScope::Images => "images",
        ClearScope::UnpinnedText => "unpinned_text",
        ClearScope::UnpinnedImages => "unpinned_images",
    }
}

fn kind_name(kind: ClipKind) -> &'static str {
    match kind {
        ClipKind::Text => "text",
        ClipKind::RichText => "rich_text",
        ClipKind::Html => "html",
        ClipKind::Image => "image",
        ClipKind::File => "file",
        ClipKind::Other => "other",
        ClipKind::Command => "command",
    }
}

fn clip_summary(clip: &Clip, include_text: bool) -> Value {
    json!({
        "id": clip.id,
        "kind": kind_name(clip.kind),
        "preview": clip.preview,
        "title": clip.title,
        "pinned": clip.pinned,
        "source_name": clip.source_name,
        "source_bundle": clip.source_bundle,
        "date_ms": clip.date_ms,
        "byte_len": clip.byte_len,
        "width": clip.width,
        "height": clip.height,
        "text": if include_text { clip.text.clone() } else { None }
    })
}

fn clip_full(clip: &Clip) -> Value {
    let mut value = clip_summary(clip, true);
    if let Some(object) = value.as_object_mut() {
        object.insert("data_type".to_string(), json!(clip.data_type));
        object.insert("blob".to_string(), json!(clip.blob));
        object.insert("hash".to_string(), json!(clip.hash));
        object.insert("plain_text".to_string(), json!(clip.plain_text()));
    }
    value
}

fn find_clip(id: &str) -> Result<Clip, ControlError> {
    with_state(|s| s.store.clips.iter().find(|clip| clip.id == id).cloned())
        .ok_or_else(|| ControlError::new("not_found", "No clipping has that id."))
}

fn counts_json() -> Value {
    with_state(|s| {
        json!({
            "text": s.store.count_scope(ClearScope::Text),
            "images": s.store.count_scope(ClearScope::Images),
            "commands": s.store.clips.iter().filter(|clip| clip.kind == ClipKind::Command).count(),
            "pinned": s.store.clips.iter().filter(|clip| clip.pinned && clip.kind != ClipKind::Command).count()
        })
    })
}

fn storage_json() -> Value {
    let history = with_state(|s| s.store.storage_usage());
    let cache = thumbnails::cache_usage_bytes();
    let total = history.history_total().saturating_add(cache);
    json!({
        "history_bytes": history.history_total(),
        "index_bytes": history.index_bytes,
        "blob_bytes": history.blob_bytes,
        "other_bytes": history.other_bytes,
        "cache_bytes": cache,
        "total_bytes": total
    })
}

fn settings_snapshot() -> Value {
    let settings = with_state(|s| s.settings.clone());
    json!({
        "max_clips": settings.max_clips,
        "record": settings.record,
        "confirm_delete": settings.confirm_delete,
        "paste_after_select": settings.paste_after_select,
        "start_at_login": settings.start_at_login,
        "hotkey": {
            "keycode": settings.hotkey_keycode,
            "modifiers": settings.hotkey_modifiers.bits(),
            "display": settings.hotkey_display,
            "enabled": settings.hotkey_enabled
        },
        "pinned_background": settings.pinned_background.to_u32(),
        "pinned_indicator": settings.pinned_indicator.to_u32(),
        "ignored_apps": settings.ignored_apps
    })
}

fn dispatch(method: &str, params: &Value) -> Result<Value, ControlError> {
    match method {
        "status" => status(),
        "list_clips" => list_clips(params),
        "get_clip" => get_clip(params),
        "get_clip_data" => get_clip_data(params),
        "copy_clip" => copy_clip(params),
        "pin_clip" => pin_clip(params),
        "delete_clip" => delete_clip(params),
        "clear_clips" => clear_clips(params),
        "set_title" => set_title(params),
        "edit_clip_text" => edit_clip_text(params),
        "get_settings" => Ok(settings_snapshot()),
        "update_settings" => update_settings(params),
        "add_ignored_app" => add_ignored_app(params),
        "remove_ignored_app" => remove_ignored_app(params),
        "clear_thumbnail_cache" => {
            clear_thumbnail_cache();
            Ok(json!({ "cleared": true }))
        }
        "refresh_commands" => {
            refresh_shell_history();
            Ok(json!({ "refreshed": true, "counts": counts_json() }))
        }
        "show_panel" => {
            show_panel();
            Ok(json!({ "shown": true }))
        }
        "hide_panel" => {
            hide_panel();
            Ok(json!({ "hidden": true }))
        }
        "set_section" => set_section_control(params),
        "set_search" => set_search_query(params),
        "select_row" => select_row(params),
        "open_preferences" => {
            open_preferences();
            Ok(json!({ "opened": true }))
        }
        "quick_look_clip" => quick_look_clip(params),
        "close_quick_look" => {
            close_quick_look();
            Ok(json!({ "closed": true }))
        }
        "export_images_pdf" => export_images_pdf(params),
        "quit_app" => quit_app(),
        _ => Err(ControlError::new(
            "unknown_method",
            "The requested control method is not supported.",
        )),
    }
}

fn status() -> Result<Value, ControlError> {
    let (version, pid) = (env!("CARGO_PKG_VERSION"), std::process::id());
    let (recording, section, history_dir, socket) = with_state(|s| {
        (
            s.settings.record,
            s.section,
            s.store.dir().display().to_string(),
            s.control
                .as_ref()
                .map(|control| control.path().display().to_string()),
        )
    });
    Ok(json!({
        "app": "AntiRustPaste",
        "version": version,
        "pid": pid,
        "recording": recording,
        "section": section_name(section),
        "counts": counts_json(),
        "storage": storage_json(),
        "history_dir": history_dir,
        "control_socket": socket
    }))
}

fn list_clips(params: &Value) -> Result<Value, ControlError> {
    let section = section_from_str(
        &optional_string(params, "section").unwrap_or_else(|| "text".to_string()),
    )?;
    let query = optional_string(params, "query").unwrap_or_default();
    let limit = optional_usize(params, "limit")
        .unwrap_or(100)
        .clamp(1, MAX_LIMIT);
    let offset = optional_usize(params, "offset").unwrap_or(0);
    let include_text = optional_bool(params, "include_text").unwrap_or(false);
    let indices = with_state(|s| core_filter_indices(&s.store.clips, section, &query));
    let total = indices.len();
    let clips = with_state(|s| {
        indices
            .iter()
            .skip(offset)
            .take(limit)
            .filter_map(|index| s.store.clips.get(*index))
            .map(|clip| clip_summary(clip, include_text))
            .collect::<Vec<_>>()
    });
    Ok(json!({
        "section": section_name(section),
        "query": query,
        "total": total,
        "offset": offset,
        "count": clips.len(),
        "clips": clips
    }))
}

fn get_clip(params: &Value) -> Result<Value, ControlError> {
    let id = required_string(params, "id")?;
    let clip = find_clip(&id)?;
    Ok(json!({ "clip": clip_full(&clip) }))
}

fn get_clip_data(params: &Value) -> Result<Value, ControlError> {
    let id = required_string(params, "id")?;
    let max_bytes = optional_usize(params, "max_bytes")
        .unwrap_or(262_144)
        .clamp(1, 8 * 1024 * 1024);
    let (clip, bytes) = with_state(|s| {
        let clip = s.store.clips.iter().find(|clip| clip.id == id)?.clone();
        let bytes = clip
            .blob
            .as_deref()
            .and_then(|blob| s.store.read_blob(blob));
        Some((clip, bytes))
    })
    .ok_or_else(|| ControlError::new("not_found", "No clipping has that id."))?;
    let Some(bytes) = bytes else {
        return Err(ControlError::new(
            "no_data",
            "This clipping has no stored binary data.",
        ));
    };
    if bytes.len() > max_bytes {
        return Err(ControlError::new(
            "too_large",
            format!(
                "The stored data is {} bytes; request a larger max_bytes value.",
                bytes.len()
            ),
        ));
    }
    Ok(json!({
        "id": clip.id,
        "kind": kind_name(clip.kind),
        "data_type": clip.data_type,
        "byte_len": bytes.len(),
        "encoding": "base64",
        "data": base64_encode(&bytes)
    }))
}

#[allow(deprecated)]
fn copy_clip(params: &Value) -> Result<Value, ControlError> {
    let id = required_string(params, "id")?;
    let plain = optional_bool(params, "plain").unwrap_or(false);
    let activate = optional_bool(params, "activate").unwrap_or(false);
    let paste = optional_bool(params, "paste").unwrap_or(false);
    let clip = find_clip(&id)?;
    if plain && clip.plain_text().is_none() {
        return Err(ControlError::new(
            "no_plain_text",
            "This clipping has no plain-text fallback.",
        ));
    }
    if !write_clip_to_pasteboard(&clip, plain) {
        return Err(ControlError::new(
            "write_failed",
            "The clipping could not be written to the clipboard.",
        ));
    }

    let should_activate = activate || paste;
    if should_activate {
        hide_panel();
        let target = resolve_target_app();
        if let Some(app) = target {
            app.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps);
        }
    }
    if paste {
        schedule_simulated_paste();
    }

    Ok(json!({
        "id": clip.id,
        "written": true,
        "activate": should_activate,
        "paste_scheduled": paste,
        "change_count": pasteboard::general().changeCount()
    }))
}

fn pin_clip(params: &Value) -> Result<Value, ControlError> {
    let id = required_string(params, "id")?;
    let pinned = with_state(|s| {
        s.store
            .clips
            .iter()
            .find(|clip| clip.id == id)
            .map(|clip| clip.pinned)
    })
    .ok_or_else(|| ControlError::new("not_found", "No clipping has that id."))?;
    let target = optional_bool(params, "pinned").unwrap_or(!pinned);
    let index = with_state(|s| s.store.clips.iter().position(|clip| clip.id == id))
        .ok_or_else(|| ControlError::new("not_found", "No clipping has that id."))?;
    let position = with_state(|s| {
        let position = s.store.set_pinned(index, target)?;
        s.store.save();
        Some(position)
    })
    .ok_or_else(|| ControlError::new("not_found", "No clipping has that id."))?;
    refresh_ui();
    Ok(json!({ "id": id, "pinned": target, "position": position }))
}

fn delete_clip(params: &Value) -> Result<Value, ControlError> {
    let id = required_string(params, "id")?;
    let removed = with_state(|s| {
        let index = s.store.clips.iter().position(|clip| clip.id == id)?;
        let removed = s.store.remove(index);
        if removed {
            s.store.save();
        }
        Some(removed)
    })
    .ok_or_else(|| ControlError::new("not_found", "No clipping has that id."))?;
    if !removed {
        return Err(ControlError::new(
            "delete_failed",
            "The clipping could not be deleted.",
        ));
    }
    refresh_ui();
    prune_thumbnails();
    Ok(json!({ "id": id, "deleted": true }))
}

fn clear_clips(params: &Value) -> Result<Value, ControlError> {
    let scope = scope_from_str(&required_string(params, "scope")?)?;
    let removed = perform_clear_scope(scope);
    Ok(json!({ "scope": scope_name(scope), "removed": removed }))
}

fn set_title(params: &Value) -> Result<Value, ControlError> {
    let id = required_string(params, "id")?;
    let title = required_string(params, "title")?;
    let current = with_state(|s| {
        let index = s.store.clips.iter().position(|clip| clip.id == id)?;
        let changed = s.store.set_title(index, &title);
        if changed {
            s.store.save();
        }
        s.store.clips.get(index).and_then(|clip| clip.title.clone())
    })
    .ok_or_else(|| ControlError::new("not_found", "No clipping has that id."))?;
    refresh_ui();
    Ok(json!({ "id": id, "title": current }))
}

fn edit_clip_text(params: &Value) -> Result<Value, ControlError> {
    let id = required_string(params, "id")?;
    let text = required_string(params, "text")?;
    let outcome = with_state(|s| {
        let index = s.store.clips.iter().position(|clip| clip.id == id)?;
        Some(s.store.set_text(index, &text))
    })
    .ok_or_else(|| ControlError::new("not_found", "No clipping has that id."))?;
    match outcome {
        Ok(changed) => {
            if changed {
                with_state(|s| s.store.save());
                refresh_ui();
            }
            Ok(json!({ "id": id, "changed": changed }))
        }
        Err(EditTextError::Unavailable) => Err(ControlError::new(
            "unavailable",
            "This clipping's text cannot be edited.",
        )),
        Err(EditTextError::Empty) => Err(ControlError::new(
            "empty",
            "The text cannot be empty or only whitespace.",
        )),
        Err(EditTextError::TooLarge) => Err(ControlError::new(
            "too_large",
            "The text is larger than the 1 MiB editing limit.",
        )),
        Err(EditTextError::Duplicate) => Err(ControlError::new(
            "duplicate",
            "Another clipping already has this exact text.",
        )),
    }
}

fn update_settings(params: &Value) -> Result<Value, ControlError> {
    if let Some(recording) = optional_bool(params, "record") {
        set_recording(recording);
    }
    if let Some(confirm) = optional_bool(params, "confirm_delete") {
        with_state(|s| {
            s.settings.confirm_delete = confirm;
            settings::save(&s.settings);
        });
    }
    if let Some(paste) = optional_bool(params, "paste_after_select") {
        with_state(|s| {
            s.settings.paste_after_select = paste;
            settings::save(&s.settings);
        });
    }
    if let Some(login) = optional_bool(params, "start_at_login") {
        apply_start_at_login(login).map_err(|error| {
            ControlError::new(
                "login_item",
                format!("Could not update login item: {error}"),
            )
        })?;
        with_state(|s| {
            s.settings.start_at_login = login;
            settings::save(&s.settings);
        });
    }
    if let Some(max_clips) = optional_usize(params, "max_clips") {
        let new_limit = max_clips.clamp(1, MAX_LIMIT);
        let old_limit = with_state(|s| s.settings.max_clips);
        if new_limit != old_limit {
            with_state(|s| {
                s.settings.max_clips = new_limit;
                s.store.trim_sections(new_limit);
                s.store.save();
                settings::save(&s.settings);
            });
            let prefs = with_state(|s| {
                s.prefs
                    .as_ref()
                    .map(|prefs| (prefs.max_field.clone(), prefs.max_label.clone()))
            });
            if let Some((field, label)) = prefs {
                field.setStringValue(&NSString::from_str(&new_limit.to_string()));
                label.setStringValue(&NSString::from_str(&format!(
                    "Maximum: {new_limit} per section"
                )));
            }
            refresh_ui();
            prune_thumbnails();
        }
    }
    Ok(settings_snapshot())
}

fn add_ignored_app(params: &Value) -> Result<Value, ControlError> {
    let name = required_string(params, "name")?;
    let bundle_id = required_string(params, "bundle_id")?;
    if bundle_id.is_empty() {
        return Err(bad_parameter("bundle_id cannot be empty"));
    }
    with_state(|s| {
        s.settings.add_ignored(IgnoredApp { name, bundle_id });
        settings::save(&s.settings);
    });
    resize_ignored_view();
    Ok(json!({ "ignored_apps": with_state(|s| s.settings.ignored_apps.clone()) }))
}

fn remove_ignored_app(params: &Value) -> Result<Value, ControlError> {
    let bundle_id = required_string(params, "bundle_id")?;
    let removed = with_state(|s| {
        let Some(index) = s
            .settings
            .ignored_apps
            .iter()
            .position(|app| app.bundle_id == bundle_id)
        else {
            return false;
        };
        s.settings.ignored_apps.remove(index);
        settings::save(&s.settings);
        true
    });
    if !removed {
        return Err(ControlError::new(
            "not_found",
            "No ignored application has that bundle id.",
        ));
    }
    resize_ignored_view();
    Ok(json!({ "ignored_apps": with_state(|s| s.settings.ignored_apps.clone()) }))
}

fn set_section_control(params: &Value) -> Result<Value, ControlError> {
    let section = section_from_str(&required_string(params, "section")?)?;
    set_section(section);
    Ok(json!({ "section": section_name(section) }))
}

fn set_search_query(params: &Value) -> Result<Value, ControlError> {
    let query = required_string(params, "query")?;
    let search = with_state(|s| s.search.clone());
    search.setStringValue(&NSString::from_str(&query));
    search_changed();
    Ok(json!({ "query": query }))
}

fn select_row(params: &Value) -> Result<Value, ControlError> {
    let index = optional_usize(params, "index")
        .ok_or_else(|| bad_parameter("Missing or invalid integer parameter: index"))?;
    let selected = with_state(|s| {
        let len = filtered_indices(s).len();
        s.selected = index.min(len.saturating_sub(1));
        s.selected
    });
    move_selection(0);
    Ok(json!({ "index": selected }))
}

fn quick_look_clip(params: &Value) -> Result<Value, ControlError> {
    let id = required_string(params, "id")?;
    toggle_preview_for_clip(&id);
    Ok(json!({ "id": id, "opened": true }))
}

fn export_images_pdf(params: &Value) -> Result<Value, ControlError> {
    let path = PathBuf::from(required_string(params, "path")?);
    if path.as_os_str().is_empty() {
        return Err(bad_parameter("path cannot be empty"));
    }
    let path = if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
    {
        path
    } else {
        path.with_extension("pdf")
    };
    let requested_ids: Option<Vec<String>> =
        params.get("ids").and_then(Value::as_array).map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        });

    let (images, missing) = with_state(|s| {
        let ids: Vec<String> = match &requested_ids {
            Some(ids) => ids.clone(),
            None => s
                .store
                .clips
                .iter()
                .filter(|clip| clip.kind == ClipKind::Image)
                .map(|clip| clip.id.clone())
                .collect(),
        };
        let mut images = Vec::new();
        let mut missing = false;
        for id in ids {
            let Some(clip) = s
                .store
                .clips
                .iter()
                .find(|clip| clip.id == id && clip.kind == ClipKind::Image)
            else {
                missing = true;
                continue;
            };
            let Some(bytes) = clip
                .blob
                .as_deref()
                .and_then(|blob| s.store.read_blob(blob))
            else {
                missing = true;
                continue;
            };
            images.push(PdfImage { bytes });
        }
        (images, missing)
    });
    if missing {
        return Err(ControlError::new(
            "missing_image",
            "One or more selected images could not be read.",
        ));
    }
    if images.is_empty() {
        return Err(ControlError::new(
            "no_images",
            "No image clippings were selected.",
        ));
    }

    let pages = images.len();
    let written = antirustpaste_pdf::write_pdf(&images)
        .map_err(|error| ControlError::new("pdf_failed", error.to_string()))
        .and_then(|bytes| {
            write_pdf_file(&path, &bytes).map_err(|error| ControlError::new("pdf_failed", error))
        })?;
    Ok(json!({
        "path": written.display().to_string(),
        "pages": pages
    }))
}

fn quit_app() -> Result<Value, ControlError> {
    let block = RcBlock::new(|_timer: NonNull<NSTimer>| {
        guard("control_quit", || {
            NSApplication::sharedApplication(main_thread()).terminate(None);
        });
    });
    unsafe {
        NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.25, false, &block);
    }
    Ok(json!({ "quitting": true }))
}
