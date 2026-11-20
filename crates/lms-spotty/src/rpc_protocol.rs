//! Newline-delimited JSON-RPC 2.0 framing, bearer token validation, and atomic
//! `daemon.endpoint` descriptor persistence.

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process;

use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use spotty_types::{Result, SpottyError};

/// Package version advertised by `system.ping` and `--check`.
pub const SPOTTY_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default endpoint filename written under `<cache_dir>/spotty/`.
pub const DEFAULT_ENDPOINT_FILENAME: &str = "daemon.endpoint";

/// JSON-RPC 2.0 parse error code.
pub const ERR_PARSE: i64 = -32700;

/// JSON-RPC 2.0 invalid request error code.
pub const ERR_INVALID_REQUEST: i64 = -32600;

/// JSON-RPC 2.0 method not found error code.
pub const ERR_METHOD_NOT_FOUND: i64 = -32601;

/// JSON-RPC 2.0 invalid params error code.
pub const ERR_INVALID_PARAMS: i64 = -32602;

/// JSON-RPC 2.0 internal error code.
pub const ERR_INTERNAL: i64 = -32603;

/// Custom JSON-RPC 2.0 unauthorized bearer token error code.
pub const ERR_UNAUTHORIZED: i64 = -32001;

/// Endpoint discovery descriptor written atomically to `<endpoint_file>`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonEndpointInfo {
    /// Bound socket address (for example `127.0.0.1:41234`).
    pub addr: String,
    /// Hex-encoded bearer token required on every JSON-RPC request.
    pub token: String,
    /// Process identifier of the running daemon.
    pub pid: u32,
    /// Daemon version string (`4.62.3`).
    pub version: String,
}

impl DaemonEndpointInfo {
    /// Creates a new [`DaemonEndpointInfo`] descriptor.
    #[must_use]
    pub fn new(
        addr: impl Into<String>,
        token: impl Into<String>,
        pid: u32,
        version: impl Into<String>,
    ) -> Self {
        Self {
            addr: addr.into(),
            token: token.into(),
            pid,
            version: version.into(),
        }
    }
}

/// Newline-delimited JSON-RPC 2.0 request envelope with bearer token.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// Protocol version string (must equal `"2.0"`).
    pub jsonrpc: String,
    /// Request correlation identifier.
    #[serde(default)]
    pub id: Value,
    /// Shared bearer secret matching [`DaemonEndpointInfo::token`].
    #[serde(default)]
    pub token: String,
    /// Target RPC method name (for example `"system.ping"`).
    pub method: String,
    /// Optional structured parameters object or array.
    #[serde(default)]
    pub params: Value,
}

impl JsonRpcRequest {
    /// Creates a new JSON-RPC 2.0 request envelope.
    #[must_use]
    pub fn new(
        id: Value,
        token: impl Into<String>,
        method: impl Into<String>,
        params: Value,
    ) -> Self {
        Self {
            jsonrpc: String::from("2.0"),
            id,
            token: token.into(),
            method: method.into(),
            params,
        }
    }

    /// Returns `true` when `jsonrpc` equals `"2.0"` and `method` is non-empty.
    #[must_use]
    pub fn is_valid_version(&self) -> bool {
        self.jsonrpc == "2.0" && !self.method.trim().is_empty()
    }

    /// Validates the request bearer token against `expected_token` in constant
    /// time.
    #[must_use]
    pub fn is_authorized(&self, expected_token: &str) -> bool {
        !expected_token.is_empty()
            && constant_time_eq(self.token.as_bytes(), expected_token.as_bytes())
    }
}

/// JSON-RPC 2.0 error object.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Numeric JSON-RPC error code.
    pub code: i64,
    /// Human-readable error description.
    pub message: String,
    /// Optional structured error context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    /// Creates a new [`JsonRpcError`].
    #[must_use]
    pub fn new(code: i64, message: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            code,
            message: message.into(),
            data,
        }
    }
}

/// Newline-delimited JSON-RPC 2.0 response envelope.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// Protocol version string (`"2.0"`).
    pub jsonrpc: String,
    /// Request correlation identifier echoed back to the caller.
    pub id: Value,
    /// Result payload on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error object on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    /// Constructs a successful JSON-RPC 2.0 response.
    #[must_use]
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: String::from("2.0"),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Constructs a JSON-RPC 2.0 error response.
    #[must_use]
    pub fn error(id: Value, code: i64, message: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            jsonrpc: String::from("2.0"),
            id,
            result: None,
            error: Some(JsonRpcError::new(code, message, data)),
        }
    }
}

/// Compares two byte slices without early return on content mismatch.
#[must_use]
pub fn constant_time_eq(lhs: &[u8], rhs: &[u8]) -> bool {
    let max_len = lhs.len().max(rhs.len());
    let mut diff = usize::from(lhs.len() != rhs.len());
    for idx in 0..max_len {
        let a = lhs.get(idx).copied().unwrap_or(0);
        let b = rhs.get(idx).copied().unwrap_or(0);
        diff |= usize::from(a ^ b);
    }
    diff == 0
}

/// Generates a 256-bit cryptographically random lowercase hex bearer token.
#[must_use]
pub fn generate_bearer_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Returns the default `<cache_dir>/spotty/daemon.endpoint` path.
#[must_use]
pub fn default_endpoint_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("spotty").join(DEFAULT_ENDPOINT_FILENAME)
}

/// Atomically writes `info` as JSON to `path` via a sibling temporary file and
/// rename.
///
/// # Errors
///
/// Returns [`SpottyError::Io`] if parent directory creation, temporary file
/// writing, flushing, or atomic rename fails, or [`SpottyError::Serialization`]
/// if JSON encoding fails.
pub fn write_endpoint_file(path: &Path, info: &DaemonEndpointInfo) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        SpottyError::Io(format!(
            "endpoint file path has no parent directory: {}",
            path.display()
        ))
    })?;
    fs::create_dir_all(parent)?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(DEFAULT_ENDPOINT_FILENAME);
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}", process::id()));

    let encoded = serde_json::to_vec_pretty(info)?;
    {
        let mut file = fs::File::create(&tmp_path)?;
        file.write_all(&encoded)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }

    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(SpottyError::Io(format!(
            "failed to rename {} -> {}: {err}",
            tmp_path.display(),
            path.display()
        )));
    }
    Ok(())
}

/// Reads and deserializes a [`DaemonEndpointInfo`] descriptor from `path`.
///
/// # Errors
///
/// Returns [`SpottyError::Io`] or [`SpottyError::Serialization`] if reading or
/// parsing the endpoint JSON fails.
pub fn read_endpoint_file(path: &Path) -> Result<DaemonEndpointInfo> {
    let raw = fs::read_to_string(path)?;
    let info: DaemonEndpointInfo = serde_json::from_str(&raw)?;
    Ok(info)
}

/// Removes the endpoint descriptor file if it exists.
///
/// # Errors
///
/// Returns [`SpottyError::Io`] if deletion fails for a reason other than
/// `NotFound`.
pub fn remove_endpoint_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(SpottyError::Io(err.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn constant_time_eq_handles_equal_and_mismatched_inputs() {
        assert!(constant_time_eq(b"secret-token", b"secret-token"));
        assert!(!constant_time_eq(b"secret-token", b"secret-tokeN"));
        assert!(!constant_time_eq(b"short", b"longer-value"));
    }

    #[test]
    fn atomic_endpoint_roundtrip_and_removal() {
        let temp = TempDir::new().expect("tempdir must succeed");
        let endpoint_path = default_endpoint_path(temp.path());
        let info = DaemonEndpointInfo::new(
            "127.0.0.1:41234",
            generate_bearer_token(),
            1234,
            SPOTTY_VERSION,
        );

        write_endpoint_file(&endpoint_path, &info).expect("endpoint write must succeed");
        let loaded = read_endpoint_file(&endpoint_path).expect("endpoint read must succeed");
        assert_eq!(loaded, info);
        assert_eq!(loaded.token.len(), 64);

        remove_endpoint_file(&endpoint_path).expect("endpoint remove must succeed");
        assert!(!endpoint_path.exists());
    }
}
