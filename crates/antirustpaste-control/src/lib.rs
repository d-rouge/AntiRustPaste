//! Local loopback HTTP control protocol shared by the app and its MCP bridge.
//!
//! The app listens on an ephemeral `127.0.0.1` port and publishes a small
//! endpoint file (port, token, pid) inside its data directory with mode `0600`.
//! The MCP bridge reads that file and sends one authenticated `POST /control`
//! request per tool call. No network interface other than loopback is used.

use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Overrides the endpoint-file path. Must be absolute.
pub const CONTROL_FILE_ENV: &str = "ANTIRUSTPASTE_CONTROL_FILE";
/// Same history-directory override used by the app and smoke tests.
pub const DATA_DIR_ENV: &str = "ANTIRUSTPASTE_DATA_DIR";
pub const CONTROL_FILE_NAME: &str = "control.json";
pub const ENDPOINT_VERSION: u32 = 1;

/// A request body larger than this is refused before it is parsed.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
/// A response body larger than this is refused.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// Bound on the HTTP header block.
pub const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_ENDPOINT_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ControlRequest {
    pub method: String,
    #[serde(default = "null_value")]
    pub params: Value,
}

impl ControlRequest {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            method: method.into(),
            params,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ControlResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ControlError>,
}

impl ControlResponse {
    pub fn ok(result: Value) -> Self {
        Self {
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(error: ControlError) -> Self {
        Self {
            ok: false,
            result: None,
            error: Some(error),
        }
    }

    pub fn into_result(self) -> Result<Value, ControlError> {
        if self.ok {
            Ok(self.result.unwrap_or(Value::Null))
        } else {
            Err(self.error.unwrap_or_else(|| {
                ControlError::new("internal", "The app returned an incomplete response.")
            }))
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ControlError {
    pub code: String,
    pub message: String,
}

impl ControlError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ControlError {}

/// What the app publishes for the bridge.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ControlEndpoint {
    pub version: u32,
    pub port: u16,
    pub token: String,
    pub pid: u32,
}

impl ControlEndpoint {
    pub fn new(port: u16, token: String) -> Self {
        Self {
            version: ENDPOINT_VERSION,
            port,
            token,
            pid: std::process::id(),
        }
    }

    pub fn is_valid(&self) -> bool {
        self.version == ENDPOINT_VERSION
            && self.port != 0
            && !self.token.is_empty()
            && self.token.len() <= 128
            && self
                .token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    }
}

fn null_value() -> Value {
    Value::Null
}

/// The private endpoint-file path.
///
/// `ANTIRUSTPASTE_CONTROL_FILE` wins when it is absolute. Otherwise the file
/// lives next to the history files, so an isolated `ANTIRUSTPASTE_DATA_DIR` also
/// isolates the control channel. Relative paths are refused.
pub fn endpoint_path() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os(CONTROL_FILE_ENV)
        && !value.is_empty()
    {
        let path = PathBuf::from(value);
        return path.is_absolute().then_some(path);
    }

    let dir = if let Some(value) = std::env::var_os(DATA_DIR_ENV)
        && !value.is_empty()
    {
        PathBuf::from(value)
    } else {
        dirs::data_dir()?.join("AntiRustPaste")
    };
    let path = dir.join(CONTROL_FILE_NAME);
    path.is_absolute().then_some(path)
}

/// A fresh bearer token. Two UUIDv4 values give 256 bits of randomness from the
/// OS without adding another random-number dependency.
pub fn new_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// Write the endpoint atomically with private permissions where the OS supports
/// them. The MCP bridge reads this file to discover the port and token.
pub fn write_endpoint(path: &Path, endpoint: &ControlEndpoint) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "The control endpoint path has no parent directory.",
        ));
    };
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let bytes = serde_json::to_vec(endpoint)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    }
    replace_file(&temporary, path)?;
    Ok(())
}

fn replace_file(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    match fs::rename(temporary, destination) {
        Ok(()) => Ok(()),
        Err(error) => {
            // Windows cannot rename over an existing file. The app removes a
            // stale endpoint before starting, but keep this path portable.
            #[cfg(windows)]
            {
                if destination.exists() {
                    fs::remove_file(destination)?;
                    return fs::rename(temporary, destination);
                }
            }
            let _ = fs::remove_file(temporary);
            Err(error)
        }
    }
}

/// Read and validate the endpoint published by the running app.
pub fn read_endpoint(path: &Path) -> std::io::Result<ControlEndpoint> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "The AntiRustPaste control endpoint is not a regular file.",
        ));
    }
    if metadata.len() > MAX_ENDPOINT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "The AntiRustPaste control endpoint file is too large.",
        ));
    }
    let raw = fs::read(path)?;
    let endpoint: ControlEndpoint = serde_json::from_slice(&raw)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if !endpoint.is_valid() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "The AntiRustPaste control endpoint is not valid.",
        ));
    }
    Ok(endpoint)
}

/// Send one control request to the running app over loopback HTTP.
pub fn call(request: &ControlRequest) -> std::io::Result<ControlResponse> {
    let path = endpoint_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "The AntiRustPaste control endpoint path is not available.",
        )
    })?;
    let endpoint = read_endpoint(&path)?;
    let body = serde_json::to_vec(request)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let wire = encode_http_request(&endpoint, &body)?;

    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, endpoint.port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let _ = stream.set_nodelay(true);
    stream.write_all(&wire)?;
    stream.flush()?;

    let mut raw = Vec::new();
    let limit = MAX_RESPONSE_BYTES.saturating_add(MAX_HEADER_BYTES);
    let mut limited = stream.take(limit as u64 + 1);
    limited.read_to_end(&mut raw)?;
    if raw.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "The AntiRustPaste response is too large.",
        ));
    }
    let (status, body) = parse_http_response(&raw, MAX_RESPONSE_BYTES)?;
    if status != 200 {
        let detail = String::from_utf8_lossy(&body);
        let detail = detail.chars().take(200).collect::<String>();
        return Err(std::io::Error::other(format!(
            "AntiRustPaste control returned HTTP {status}: {detail}"
        )));
    }
    serde_json::from_slice(&body)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Encode the authenticated request used by the bridge.
pub fn encode_http_request(endpoint: &ControlEndpoint, body: &[u8]) -> std::io::Result<Vec<u8>> {
    if body.len() > MAX_REQUEST_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "The control request is too large.",
        ));
    }
    let head = format!(
        "POST /control HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        endpoint.port,
        endpoint.token,
        body.len()
    );
    let mut wire = head.into_bytes();
    wire.extend_from_slice(body);
    Ok(wire)
}

/// Encode a complete HTTP response with a JSON body.
pub fn encode_http_response(status: u16, reason: &str, body: &[u8]) -> Vec<u8> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut wire = head.into_bytes();
    wire.extend_from_slice(body);
    wire
}

#[derive(Debug)]
pub struct HttpRequestView<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub host: Option<&'a str>,
    pub authorization: Option<&'a str>,
    pub body: &'a [u8],
}

/// Parse one bounded HTTP/1.x request. An incomplete header or body returns
/// `UnexpectedEof`; malformed input returns `InvalidData`; an oversized header
/// or body returns `InvalidInput`.
pub fn parse_http_request(raw: &[u8], max_body: usize) -> std::io::Result<HttpRequestView<'_>> {
    let Some(header_end) = find_header_end(raw) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "The HTTP headers are incomplete.",
        ));
    };
    if header_end > MAX_HEADER_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "The HTTP headers are too large.",
        ));
    }
    let headers = std::str::from_utf8(&raw[..header_end]).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "The HTTP headers are not valid UTF-8.",
        )
    })?;
    let mut lines = headers.split("\r\n");
    let request_line = lines.next().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Missing HTTP request line.",
        )
    })?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "Missing HTTP method.")
    })?;
    let path = parts.next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "Missing HTTP path.")
    })?;
    let version = parts.next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "Missing HTTP version.")
    })?;
    if parts.next().is_some() || (version != "HTTP/1.1" && version != "HTTP/1.0") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Unsupported HTTP request line.",
        ));
    }

    let mut host = None;
    let mut authorization = None;
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Malformed HTTP header.",
            ));
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("host") {
            set_unique(&mut host, value)?;
        } else if name.eq_ignore_ascii_case("authorization") {
            set_unique(&mut authorization, value)?;
        } else if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Duplicate Content-Length header.",
                ));
            }
            content_length = Some(value.parse::<usize>().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Invalid Content-Length header.",
                )
            })?);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Transfer-Encoding is not supported.",
            ));
        }
    }

    let length = content_length.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Missing Content-Length header.",
        )
    })?;
    if length > max_body {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "The HTTP body is too large.",
        ));
    }
    let body_start = header_end.saturating_add(4);
    let body_end = body_start.saturating_add(length);
    if raw.len() < body_end {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "The HTTP body is incomplete.",
        ));
    }
    if raw.len() > body_end {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Unexpected data after the HTTP body.",
        ));
    }
    Ok(HttpRequestView {
        method,
        path,
        host,
        authorization,
        body: &raw[body_start..body_end],
    })
}

fn set_unique<'a>(slot: &mut Option<&'a str>, value: &'a str) -> std::io::Result<()> {
    if slot.is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Duplicate HTTP header.",
        ));
    }
    *slot = Some(value);
    Ok(())
}

fn find_header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_http_response(raw: &[u8], max_body: usize) -> std::io::Result<(u16, Vec<u8>)> {
    let Some(header_end) = find_header_end(raw) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "The HTTP response has no header terminator.",
        ));
    };
    if header_end > MAX_HEADER_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "The HTTP response headers are too large.",
        ));
    }
    let headers = std::str::from_utf8(&raw[..header_end]).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "The HTTP response headers are not valid UTF-8.",
        )
    })?;
    let mut lines = headers.split("\r\n");
    let status_line = lines.next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "Missing HTTP status line.")
    })?;
    let mut parts = status_line.split_whitespace();
    let version = parts.next().unwrap_or_default();
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Unsupported HTTP response version.",
        ));
    }
    let status = parts
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid HTTP status code.")
        })?;

    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") && content_length.is_none() {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    let length = content_length.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "The HTTP response has no Content-Length.",
        )
    })?;
    if length > max_body {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "The AntiRustPaste response is too large.",
        ));
    }
    let body_start = header_end.saturating_add(4);
    let body_end = body_start.saturating_add(length);
    if raw.len() < body_end {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "The HTTP response body is incomplete.",
        ));
    }
    Ok((status, raw[body_start..body_end].to_vec()))
}

/// Standard base64 with padding. Used only by the optional binary-data tool so
/// the workspace does not need another dependency.
pub fn base64_encode(data: &[u8]) -> String {
    fn digit(value: u8) -> char {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        TABLE.get(value as usize).copied().unwrap_or(b'=') as char
    }

    let mut out = String::with_capacity(data.len().div_ceil(3).saturating_mul(4));
    for chunk in data.chunks(3) {
        let Some(&first) = chunk.first() else {
            continue;
        };
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        out.push(digit(first >> 2));
        out.push(digit(((first & 0x03) << 4) | (second >> 4)));
        if chunk.len() > 1 {
            out.push(digit(((second & 0x0f) << 2) | (third >> 6)));
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(digit(third & 0x3f));
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn responses_round_trip_without_empty_fields() {
        let ok = ControlResponse::ok(serde_json::json!({"value": 1}));
        let text = serde_json::to_string(&ok).expect("serialize");
        assert!(!text.contains("error"));
        let decoded: ControlResponse = serde_json::from_str(&text).expect("deserialize");
        assert!(decoded.ok);
        assert_eq!(
            decoded.into_result().expect("ok"),
            serde_json::json!({"value": 1})
        );

        let error = ControlResponse::error(ControlError::new("missing", "No clip"));
        let text = serde_json::to_string(&error).expect("serialize");
        assert!(!text.contains("result"));
        let decoded: ControlResponse = serde_json::from_str(&text).expect("deserialize");
        assert!(!decoded.ok);
    }

    #[test]
    fn endpoint_tokens_are_private_and_valid() {
        let token = new_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert!(ControlEndpoint::new(12345, token).is_valid());
        assert!(!ControlEndpoint::new(0, "a".repeat(64)).is_valid());
        assert!(!ControlEndpoint::new(1, "bad token".to_string()).is_valid());
    }

    #[test]
    fn http_request_round_trips_through_the_parser() {
        let endpoint = ControlEndpoint::new(43210, new_token());
        let body = br#"{"method":"status","params":{}}"#;
        let wire = encode_http_request(&endpoint, body).expect("encode");
        let request = parse_http_request(&wire, MAX_REQUEST_BYTES).expect("parse");
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/control");
        assert_eq!(request.host, Some("127.0.0.1:43210"));
        assert_eq!(
            request.authorization,
            Some(format!("Bearer {}", endpoint.token).as_str())
        );
        assert_eq!(request.body, body);
    }

    #[test]
    fn http_response_parser_checks_length_and_status() {
        let body = b"{\"ok\":true}";
        let wire = encode_http_response(200, "OK", body);
        let (status, parsed) = parse_http_response(&wire, MAX_RESPONSE_BYTES).expect("parse");
        assert_eq!(status, 200);
        assert_eq!(parsed, body);
    }

    #[test]
    fn incomplete_http_input_is_reported_as_eof() {
        let error =
            parse_http_request(b"POST /control HTTP/1.1\r\n", 1024).expect_err("incomplete");
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }
}
