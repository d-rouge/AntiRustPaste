# MCP control

AntiRustPaste can be driven through the Model Context Protocol (MCP). The app
stays the only writer of its history: an MCP client launches the
`antirustpaste-mcp` bridge, and the bridge forwards tool calls over a private
loopback HTTP endpoint to the running app. The menu-bar UI, clipboard polling,
settings and MCP calls all run on the same main thread, so they never race.

MCP control is currently implemented in the macOS shell only. The Linux
GTK4/X11 shell in the workspace does not bind the control endpoint yet.

## Enable and connect

The app publishes its control endpoint when it starts. It lives next to the
history:

```
~/Library/Application Support/AntiRustPaste/control.json
```

The file is mode `0600` and contains the loopback port, a random bearer token
and the app pid. The app binds only to `127.0.0.1` on an ephemeral port; it does
not listen on a LAN interface. `ANTIRUSTPASTE_DATA_DIR` moves both the history
and the endpoint file; `ANTIRUSTPASTE_CONTROL_FILE` overrides the endpoint file
path (it must be absolute). The app and the MCP bridge must see the same value
when either override is used. The bridge reads the port and token from this file
automatically; do not paste the token into a prompt or log.

The MCP bridge is shipped inside the app bundle:

```
/Applications/AntiRustPaste.app/Contents/MacOS/antirustpaste-mcp
```

For a development build, use `target/release/antirustpaste-mcp`. Example MCP
client configuration:

```json
{
  "mcpServers": {
    "antirustpaste": {
      "command": "/Applications/AntiRustPaste.app/Contents/MacOS/antirustpaste-mcp"
    }
  }
}
```

The app must already be running. If it is not, tool calls return an `isError`
result explaining that AntiRustPaste is not reachable.

## Tools

History:

| Tool | Purpose |
| --- | --- |
| `status` | Version, recording state, active tab, counts and disk usage |
| `list_clips` | List text, image or command rows with search, limit and offset |
| `get_clip` | Full stored text and metadata for one clipping |
| `get_clip_data` | Stored image/RTF/HTML bytes as base64, capped by `max_bytes` |

Actions:

| Tool | Purpose |
| --- | --- |
| `copy_clip` | Write a clipping to the clipboard; optional activate and ⌘V |
| `pin_clip` | Pin, unpin or toggle a clipping |
| `delete_clip` | Delete one clipping and its blob |
| `clear_clips` | Clear all/text/images/unpinned text/unpinned images |
| `set_title` | Set or clear the searchable title |
| `edit_clip_text` | Replace the text of a text or command clip |
| `refresh_commands` | Re-read zsh and bash history |
| `clear_thumbnail_cache` | Delete thumbnails and Quick Look previews |
| `export_images_pdf` | Write selected images, or all images, to a PDF |

Settings and UI:

| Tool | Purpose |
| --- | --- |
| `get_settings` | Preferences, shortcut and ignored apps |
| `update_settings` | Recording, confirmations, auto-paste, login item and the three history limits |
| `add_ignored_app` / `remove_ignored_app` | Manage the ignored-app list |
| `show_panel` / `hide_panel` | Show or hide the history popup |
| `set_section` / `set_search` / `select_row` | Drive the popup list |
| `open_preferences` | Open the Preferences window |
| `quick_look_clip` / `close_quick_look` | Control the Quick Look panel |
| `quit_app` | Quit after the response is sent |

Terminal commands are never removed by `clear_clips`; call `delete_clip` with a
command id when that is explicitly wanted. The global shortcut and the update
check remain UI actions because they need key recording and a release dialog.
`update_settings` accepts independent `max_text`, `max_images` and
`max_commands` values (1–9999); the legacy `max_clips` still sets all three.

## Safety and limits

- The endpoint carries clipboard contents and settings. Treat every MCP client
  connected to it as trusted with the same data, exactly like a local process
  reading the history directory.
- The listener is loopback-only, requires `Authorization: Bearer`, checks the
  `Host` header against the bound port, sends no CORS headers and closes the
  connection after one `POST /control`. Keep `control.json` private.
- Requests are capped at 1 MiB and responses at 16 MiB. `get_clip_data` is
  capped at 8 MiB; larger blobs can be exported through `export_images_pdf`
  instead of copied into a tool result. Use an absolute destination path for
  `export_images_pdf`.
- The app never logs clipboard text, titles, tool parameters or the token.
- A normal quit removes the endpoint file. A stale regular file is replaced at
  the next start; any other file at that path disables the bridge rather than
  being replaced. The obsolete 0.4.0 `control.sock`, when present and actually a
  Unix socket, is removed at startup.
