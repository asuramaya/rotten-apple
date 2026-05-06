//! Client side of the orchestratord JSON-RPC transport.
//!
//! Same line-delimited framing as `crates/orchestratord/src/transport.rs`.
//! One in-flight request at a time; the MCP server is single-threaded so
//! there's no need for request multiplexing.

use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{json, Value};

use rotten_apple_orchestratord::{
    DEFAULT_SOCKET_PATH,
    protocol::PROTOCOL_VERSION as UPSTREAM_PROTOCOL_VERSION,
    transport::{DEFAULT_VSOCK_PORT, VSOCK_HOST_CID, connect_vsock},
};

/// Trait so tests can swap in a recording fake without spinning up a daemon.
pub trait UpstreamClient {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, UpstreamError>;
}

#[derive(Debug)]
pub enum UpstreamError {
    Io(io::Error),
    Rpc { code: i32, message: String, data: Option<Value> },
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamError::Io(e) => write!(f, "io: {e}"),
            UpstreamError::Rpc { code, message, .. } => {
                write!(f, "orchestratord error {code}: {message}")
            }
        }
    }
}

impl From<io::Error> for UpstreamError {
    fn from(e: io::Error) -> Self { UpstreamError::Io(e) }
}

pub struct Upstream {
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
    next_id: AtomicU64,
}

impl Upstream {
    pub fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        Self::from_stream(stream)
    }

    /// Connect to the host-side daemon over vsock. This is the
    /// cross-domain path for lifted guests talking back to dom0.
    pub fn connect_vsock_host(port: u32) -> io::Result<Self> {
        let stream = connect_vsock(VSOCK_HOST_CID, port)?;
        Self::from_stream(stream)
    }

    /// Prefer the configured Unix socket, then fall back to the default
    /// host-vsock transport. This keeps MCP aligned with cockpit so all
    /// surfaces discover the daemon the same way.
    pub fn connect_default(path: &Path) -> io::Result<Self> {
        match Self::connect(path) {
            Ok(c) => Ok(c),
            Err(unix_err) => {
                Self::connect_vsock_host(DEFAULT_VSOCK_PORT).map_err(|vsock_err| {
                    let socket_label = if path == Path::new(DEFAULT_SOCKET_PATH) {
                        DEFAULT_SOCKET_PATH.to_string()
                    } else {
                        path.display().to_string()
                    };
                    io::Error::new(
                        vsock_err.kind(),
                        format!(
                            "unix {}: {}; vsock host:{}: {}",
                            socket_label,
                            unix_err,
                            DEFAULT_VSOCK_PORT,
                            vsock_err,
                        ),
                    )
                })
            }
        }
    }

    fn from_stream(stream: UnixStream) -> io::Result<Self> {
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let read_half = stream.try_clone()?;
        Ok(Upstream {
            reader: BufReader::new(read_half),
            writer: BufWriter::new(stream),
            next_id: AtomicU64::new(1),
        })
    }

    /// Send `hello` and validate the protocol version match. Returns Err on
    /// transport faults or any non-OK reply from the daemon.
    pub fn handshake(&mut self) -> Result<(), UpstreamError> {
        let result = self.call(
            "hello",
            json!({ "protocol_version": UPSTREAM_PROTOCOL_VERSION }),
        )?;
        let server_proto = result.get("protocol_version")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if server_proto != UPSTREAM_PROTOCOL_VERSION {
            return Err(UpstreamError::Rpc {
                code: -32000,
                message: format!(
                    "upstream protocol mismatch: client {UPSTREAM_PROTOCOL_VERSION}, server {server_proto}"
                ),
                data: Some(result),
            });
        }
        let _ = self.reader.get_ref().set_read_timeout(None);
        Ok(())
    }

    fn next_id(&self) -> u64 { self.next_id.fetch_add(1, Ordering::Relaxed) }
}

impl UpstreamClient for Upstream {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, UpstreamError> {
        let id = self.next_id();
        let req = json!({
            "jsonrpc": "2.0",
            "method":  method,
            "params":  params,
            "id":      id,
        });
        let mut buf = serde_json::to_vec(&req)
            .map_err(|e| UpstreamError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
        buf.push(b'\n');
        self.writer.write_all(&buf)?;
        self.writer.flush()?;

        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Err(UpstreamError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "orchestratord closed the connection",
            )));
        }
        let resp: Value = serde_json::from_str(line.trim())
            .map_err(|e| UpstreamError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
        if let Some(err) = resp.get("error") {
            let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or(-32603) as i32;
            let message = err.get("message").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let data = err.get("data").cloned();
            return Err(UpstreamError::Rpc { code, message, data });
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }
}
