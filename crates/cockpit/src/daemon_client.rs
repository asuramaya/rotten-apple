//! Client side of the orchestratord JSON-RPC transport.
//!
//! Mirrors `crates/mcp-server/src/upstream.rs` framing: one
//! line-delimited JSON object per request, one per response. The cockpit
//! holds at most one connection from its single worker thread, so no
//! request multiplexing is needed.
//!
//! On any io / framing fault, the worker is expected to fall back to
//! direct libxl mode rather than blocking the UI; that policy lives at
//! the `pick_worker_strategy` site, not here.

use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use rotten_apple_orchestratord::{
    DEFAULT_SOCKET_PATH,
    protocol::PROTOCOL_VERSION,
    transport::{DEFAULT_VSOCK_PORT, VSOCK_HOST_CID, connect_vsock},
};

/// Errors a daemon call can produce. Transport faults vs. RPC error
/// objects are kept separate so callers can decide whether to give up
/// on the connection (`Io`) or just surface the error to the user (`Rpc`).
#[derive(Debug)]
pub enum DaemonError {
    Io(io::Error),
    Rpc { code: i32, message: String },
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonError::Io(e) => write!(f, "io: {e}"),
            DaemonError::Rpc { code, message } => {
                write!(f, "daemon error {code}: {message}")
            }
        }
    }
}

impl From<io::Error> for DaemonError {
    fn from(e: io::Error) -> Self { DaemonError::Io(e) }
}

/// A live connection to orchestratord.
///
/// Single-threaded by construction: `call` mutably borrows `self`, so
/// nobody can interleave a second request while the first is in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonTransport {
    Unix,
    Vsock,
}

pub struct DaemonClient {
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
    next_id: u64,
    transport: DaemonTransport,
}

impl DaemonClient {
    /// Open a Unix socket connection. Does NOT perform the handshake —
    /// call `handshake()` next. Splitting these lets the caller bound
    /// the handshake with a read timeout and treat protocol-mismatch as
    /// distinct from "socket file is missing".
    pub fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        Self::from_stream(stream, DaemonTransport::Unix)
    }

    /// Connect to the host-side daemon over vsock. This is the
    /// cross-domain path for a lifted guest talking to dom0.
    pub fn connect_vsock_host(port: u32) -> io::Result<Self> {
        let stream = connect_vsock(VSOCK_HOST_CID, port)?;
        Self::from_stream(stream, DaemonTransport::Vsock)
    }

    /// Prefer the local Unix socket; fall back to the default host-vsock
    /// transport. This lets the same cockpit binary run on dom0 and in a
    /// lifted guest without a flag day.
    pub fn connect_default() -> io::Result<Self> {
        match Self::connect(Path::new(DEFAULT_SOCKET_PATH)) {
            Ok(c) => Ok(c),
            Err(unix_err) => {
                Self::connect_vsock_host(DEFAULT_VSOCK_PORT).map_err(|vsock_err| {
                    io::Error::new(
                        vsock_err.kind(),
                        format!(
                            "unix {}: {}; vsock host:{}: {}",
                            DEFAULT_SOCKET_PATH,
                            unix_err,
                            DEFAULT_VSOCK_PORT,
                            vsock_err,
                        ),
                    )
                })
            }
        }
    }

    fn from_stream(stream: UnixStream, transport: DaemonTransport) -> io::Result<Self> {
        // 5s is plenty for a local handshake; if the daemon isn't
        // responsive in that window we'd rather fall back to direct mode.
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let read_half = stream.try_clone()?;
        Ok(DaemonClient {
            reader: BufReader::new(read_half),
            writer: BufWriter::new(stream),
            next_id: 1,
            transport,
        })
    }

    pub fn transport(&self) -> DaemonTransport { self.transport }

    /// Send `hello`, validate the protocol version. Drops the read
    /// timeout on success so subsequent calls don't trip over the
    /// background poll cadence.
    pub fn handshake(&mut self) -> Result<(), DaemonError> {
        let result = self.call(
            "hello",
            json!({ "protocol_version": PROTOCOL_VERSION }),
        )?;
        let server_proto = result.get("protocol_version")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if server_proto != PROTOCOL_VERSION {
            return Err(DaemonError::Rpc {
                code: -32000,
                message: format!(
                    "protocol mismatch: client {PROTOCOL_VERSION}, \
                     server {server_proto}"),
            });
        }
        // Long-lived connection from here on; no read timeout.
        let _ = self.reader.get_ref().set_read_timeout(None);
        Ok(())
    }

    /// One request / one response. Numbered ids strictly grow so
    /// debugging tails can correlate without ambiguity.
    pub fn call(&mut self, method: &str, params: Value)
        -> Result<Value, DaemonError>
    {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({
            "jsonrpc": "2.0",
            "method":  method,
            "params":  params,
            "id":      id,
        });
        let mut buf = serde_json::to_vec(&req).map_err(|e|
            DaemonError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
        buf.push(b'\n');
        self.writer.write_all(&buf)?;
        self.writer.flush()?;

        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Err(DaemonError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "orchestratord closed the connection",
            )));
        }
        let resp: Value = serde_json::from_str(line.trim()).map_err(|e|
            DaemonError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
        if let Some(err) = resp.get("error") {
            let code = err.get("code")
                .and_then(|v| v.as_i64())
                .unwrap_or(-32603) as i32;
            let message = err.get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            return Err(DaemonError::Rpc { code, message });
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_error_display_io_branch() {
        let e = DaemonError::Io(io::Error::other("boom"));
        let s = format!("{e}");
        assert!(s.contains("io"));
        assert!(s.contains("boom"));
    }

    #[test]
    fn daemon_error_display_rpc_branch() {
        let e = DaemonError::Rpc { code: -32601, message: "Method not found".into() };
        let s = format!("{e}");
        assert!(s.contains("-32601"));
        assert!(s.contains("Method not found"));
    }

    #[test]
    fn connect_to_missing_socket_is_io_error() {
        let path = std::env::temp_dir()
            .join("rotten-apple-cockpit-noexist-socket-xyz");
        let _ = std::fs::remove_file(&path);
        let err = match DaemonClient::connect(&path) {
            Err(e) => e,
            Ok(_) => panic!("connect to nonexistent path unexpectedly succeeded"),
        };
        // Exact kind varies by environment/sandbox; the important thing
        // is that a missing path does not somehow succeed and produces a
        // concrete transport error.
        assert!(!err.to_string().is_empty());
    }
}
