//! MCP stdio bridge for the running AntiRustPaste app.
//!
//! MCP clients launch this binary, which forwards tool calls over the app's
//! private loopback HTTP control endpoint. The bridge never reads history files
//! directly: the app remains the single writer and the UI stays in sync.

use std::io::{self, BufRead, Write};

use antirustpaste_control::{ControlRequest, call};
use serde_json::{Value, json};

const SERVER_NAME: &str = "antirustpaste";
const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug)]
struct McpError {
    code: i64,
    message: String,
}

impl McpError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn main() {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                eprintln!("antirustpaste-mcp: could not read stdin: {error}");
                break;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(trimmed) {
            Ok(message) => message,
            Err(_) => {
                let _ = write_message(
                    &mut writer,
                    json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": { "code": -32700, "message": "Parse error" }
                    }),
                );
                continue;
            }
        };

        // Notifications have no id and never get a response.
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let envelope = match handle_request(method, params) {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": error.code, "message": error.message }
            }),
        };
        if write_message(&mut writer, envelope).is_err() {
            break;
        }
    }
}

fn write_message(writer: &mut impl Write, message: Value) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, &message)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn handle_request(method: &str, params: Value) -> Result<Value, McpError> {
    match method {
        "initialize" => {
            let version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROTOCOL_VERSION);
            Ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": SERVER_NAME,
                    "version": env!("CARGO_PKG_VERSION")
                },
                "instructions": "Controls the running AntiRustPaste app through its private loopback HTTP control endpoint. History and clipboard contents are private; return only what the user needs."
            }))
        }
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => call_tool(&params),
        "ping" => Ok(json!({})),
        _ => Err(McpError::new(-32601, format!("Method not found: {method}"))),
    }
}

fn call_tool(params: &Value) -> Result<Value, McpError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| McpError::new(-32602, "Missing tool name"))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));

    let response = match call(&ControlRequest::new(name, arguments)) {
        Ok(response) => response.into_result(),
        Err(error) => {
            return Ok(tool_text(
                format!("AntiRustPaste is not reachable: {error}"),
                true,
            ));
        }
    };
    match response {
        Ok(result) => Ok(tool_text(
            serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string()),
            false,
        )),
        Err(error) => Ok(tool_text(error.message, true)),
    }
}

fn tool_text(text: String, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error
    })
}

fn tools() -> Value {
    json!([
        {
            "name": "status",
            "description": "Return app version, recording state, active tab, section counts and disk usage.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "list_clips",
            "description": "List history rows in one tab, newest first and pinned rows first. Returns metadata and previews, not full text unless include_text is true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "section": { "type": "string", "enum": ["text", "images", "commands"], "default": "text" },
                    "query": { "type": "string", "description": "Case-insensitive search" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 9999, "default": 100 },
                    "offset": { "type": "integer", "minimum": 0, "default": 0 },
                    "include_text": { "type": "boolean", "default": false }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "get_clip",
            "description": "Return one clipping by id, including its full stored text and metadata.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false
            }
        },
        {
            "name": "get_clip_data",
            "description": "Return a clipping's stored binary blob as base64, within the requested byte limit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "max_bytes": { "type": "integer", "minimum": 1, "maximum": 8388608, "default": 262144 }
                },
                "required": ["id"],
                "additionalProperties": false
            }
        },
        {
            "name": "copy_clip",
            "description": "Write a clipping to the clipboard. Optionally activate the previous app and schedule a synthetic paste.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "plain": { "type": "boolean", "default": false, "description": "Write only the plain-text fallback of a rich clip" },
                    "activate": { "type": "boolean", "default": false, "description": "Activate the app that was frontmost when AntiRustPaste opened" },
                    "paste": { "type": "boolean", "default": false, "description": "Synthesize Command-V after copying (requires Accessibility)" }
                },
                "required": ["id"],
                "additionalProperties": false
            }
        },
        {
            "name": "pin_clip",
            "description": "Pin or unpin a clipping. Omit pinned to toggle the current state.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "pinned": { "type": "boolean" }
                },
                "required": ["id"],
                "additionalProperties": false
            }
        },
        {
            "name": "delete_clip",
            "description": "Delete one clipping and its stored blob.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false
            }
        },
        {
            "name": "clear_clips",
            "description": "Delete clippings by scope. Terminal commands are never removed by scope clears.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scope": {
                        "type": "string",
                        "enum": ["all", "text", "images", "unpinned_text", "unpinned_images"]
                    }
                },
                "required": ["scope"],
                "additionalProperties": false
            }
        },
        {
            "name": "set_title",
            "description": "Set or clear a clipping's optional searchable title. An empty title clears it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "title": { "type": "string" }
                },
                "required": ["id", "title"],
                "additionalProperties": false
            }
        },
        {
            "name": "edit_clip_text",
            "description": "Replace the stored text of a text or command clipping. Refuses empty, oversized and duplicate text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "text": { "type": "string" }
                },
                "required": ["id", "text"],
                "additionalProperties": false
            }
        },
        {
            "name": "get_settings",
            "description": "Return all preferences, including the global shortcut and ignored apps.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "update_settings",
            "description": "Update one or more preferences. Omitted fields stay unchanged.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "record": { "type": "boolean" },
                    "confirm_delete": { "type": "boolean" },
                    "paste_after_select": { "type": "boolean" },
                    "start_at_login": { "type": "boolean" },
                    "max_text": { "type": "integer", "minimum": 1, "maximum": 9999 },
                    "max_images": { "type": "integer", "minimum": 1, "maximum": 9999 },
                    "max_commands": { "type": "integer", "minimum": 1, "maximum": 9999 },
                    "max_clips": { "type": "integer", "minimum": 1, "maximum": 9999 }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "add_ignored_app",
            "description": "Ignore future clipboard changes from an application.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "bundle_id": { "type": "string" }
                },
                "required": ["name", "bundle_id"],
                "additionalProperties": false
            }
        },
        {
            "name": "remove_ignored_app",
            "description": "Remove an application from the ignored list.",
            "inputSchema": {
                "type": "object",
                "properties": { "bundle_id": { "type": "string" } },
                "required": ["bundle_id"],
                "additionalProperties": false
            }
        },
        {
            "name": "clear_thumbnail_cache",
            "description": "Delete cached thumbnails and Quick Look preview files. History is kept.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "refresh_commands",
            "description": "Re-read the zsh and bash history files.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "show_panel",
            "description": "Open the clipboard history popup.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "hide_panel",
            "description": "Close the clipboard history popup.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "set_section",
            "description": "Switch the active popup tab.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "section": { "type": "string", "enum": ["text", "images", "commands"] }
                },
                "required": ["section"],
                "additionalProperties": false
            }
        },
        {
            "name": "set_search",
            "description": "Replace the popup search query.",
            "inputSchema": {
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"],
                "additionalProperties": false
            }
        },
        {
            "name": "select_row",
            "description": "Select a visible row by zero-based index in the active filtered tab.",
            "inputSchema": {
                "type": "object",
                "properties": { "index": { "type": "integer", "minimum": 0 } },
                "required": ["index"],
                "additionalProperties": false
            }
        },
        {
            "name": "open_preferences",
            "description": "Open the Preferences window.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "quick_look_clip",
            "description": "Open Quick Look for one clipping.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false
            }
        },
        {
            "name": "close_quick_look",
            "description": "Close the Quick Look panel and clean up its preview file.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "export_images_pdf",
            "description": "Write selected image clips, or every image, to one A4 page each in a PDF file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Destination PDF path" },
                    "ids": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        },
        {
            "name": "quit_app",
            "description": "Quit AntiRustPaste after the response is sent.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_are_unique_and_have_schemas() {
        let tools = tools();
        let list = tools.as_array().expect("tool array");
        assert!(!list.is_empty());
        let mut names = std::collections::HashSet::new();
        for tool in list {
            let name = tool.get("name").and_then(Value::as_str).expect("name");
            assert!(names.insert(name.to_string()), "duplicate tool {name}");
            assert!(tool.get("description").is_some(), "missing description");
            assert!(tool.get("inputSchema").is_some(), "missing schema");
        }
    }

    #[test]
    fn initialize_echoes_the_client_protocol() {
        let result = handle_request("initialize", json!({ "protocolVersion": "2025-06-18" }))
            .expect("initialize");
        assert_eq!(
            result.get("protocolVersion").and_then(Value::as_str),
            Some("2025-06-18")
        );
        assert!(result.get("capabilities").is_some());
    }
}
