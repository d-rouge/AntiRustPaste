//! Local JSON-line control protocol shared by the app and its MCP bridge.
//!
//! The app listens on a per-user Unix socket inside its data directory. A tool
//! call is one [`ControlRequest`] line and one [`ControlResponse`] line. The
//! protocol is deliberately small: the MCP server owns the MCP schemas and the
//! app owns the actions.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Overrides the socket path. Must be absolute.
pub const CONTROL_SOCKET_ENV: &str = "ANTIRUSTPASTE_CONTROL_SOCKET";
/// Same history-directory override used by the app and smoke tests.
pub const DATA_DIR_ENV: &str = "ANTIRUSTPASTE_DATA_DIR";
pub const SOCKET_NAME: &str = "control.sock";

/// A request larger than this is refused before it is parsed.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
/// A response larger than this closes the connection instead of growing memory.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

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

fn null_value() -> Value {
    Value::Null
}

/// The private control socket path.
///
/// `ANTIRUSTPASTE_CONTROL_SOCKET` wins when it is absolute. Otherwise the socket
/// lives next to the history files, so an isolated `ANTIRUSTPASTE_DATA_DIR`
/// also isolates the control channel. Relative paths are refused.
pub fn socket_path() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os(CONTROL_SOCKET_ENV)
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
    let path = dir.join(SOCKET_NAME);
    path.is_absolute().then_some(path)
}

/// Send one request to the running app and wait for its response.
pub fn call(request: &ControlRequest) -> std::io::Result<ControlResponse> {
    let path = socket_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "The AntiRustPaste control socket path is not available.",
        )
    })?;
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;

    let mut encoded = serde_json::to_vec(request)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if encoded.len() + 1 > MAX_REQUEST_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "The control request is too large.",
        ));
    }
    encoded.push(b'\n');
    stream.write_all(&encoded)?;
    stream.flush()?;

    let reader = BufReader::new(stream);
    let mut line = String::new();
    let mut limited = reader.take((MAX_RESPONSE_BYTES + 1) as u64);
    let read = limited.read_line(&mut line)?;
    if read == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "AntiRustPaste closed the control connection without a response.",
        ));
    }
    if line.len() > MAX_RESPONSE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "The AntiRustPaste response is too large.",
        ));
    }
    serde_json::from_str(&line)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
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

/// Read the rest of a stream without allowing an unbounded allocation.
pub fn read_bounded<R: Read>(reader: R, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut limited = reader.take(limit as u64 + 1);
    limited.read_to_end(&mut data)?;
    if data.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "The control payload is too large.",
        ));
    }
    Ok(data)
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
}
