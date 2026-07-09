//! orchestratord — JSON-RPC daemon owning the libxl context.
//!
//! Architecture:
//!   - one local Unix socket plus an optional vsock listener; each accept
//!     loop spawns one thread per connection
//!   - each connection thread holds a clone of `ActorHandle`
//!   - one actor thread owns the `XenBackend` (libxl_ctx is `!Sync`)
//!   - everything serializes onto the actor's mpsc by construction
//!
//! No tokio. std threads + std::os::unix::net + std::sync::mpsc.

pub mod actor;
pub mod dispatch;
pub mod engine;
pub mod oneshot;
pub mod protocol;
pub mod transport;

use std::io::{BufReader, BufWriter};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::json;

use crate::actor::ActorHandle;
use crate::dispatch::dispatch as dispatch_method;
use crate::engine::EngineHandle;
use crate::protocol::{
    PARSE_ERROR, PROTOCOL_MISMATCH, PROTOCOL_VERSION, Response, SERVER_NAME,
    SERVER_VERSION,
};
use crate::transport::{
    bind_vsock_listener, read_request, write_response,
};

pub const DEFAULT_SOCKET_PATH: &str = "/run/rotten-apple.sock";

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
    pub vsock_port: Option<u32>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket_path: PathBuf::from(DEFAULT_SOCKET_PATH),
            // vsock is OFF by default. It binds VSOCK_CID_ANY, reachable
            // by EVERY guest, and dispatch() has no caller authorization
            // yet — so a default-on vsock is an unauthenticated guest→host
            // control plane (domain.kill/create/... from any VM). Opt in
            // explicitly with --vsock-port ONCE mesh-peer auth (fabric
            // Ed25519 signed requests, task #12) gates the privileged
            // methods. Until then the Unix socket (chmod 0660) is the
            // only surface. See project_orchestratord_auth_mesh_peer.
            vsock_port: None,
        }
    }
}

/// Termination signal — set by the signal handler, polled by the accept loop.
#[derive(Default)]
pub struct ShutdownFlag(AtomicBool);

impl ShutdownFlag {
    pub fn new() -> Arc<Self> { Arc::new(ShutdownFlag(AtomicBool::new(false))) }
    pub fn raise(&self) { self.0.store(true, Ordering::SeqCst); }
    pub fn is_raised(&self) -> bool { self.0.load(Ordering::SeqCst) }
}

/// Install SIGTERM / SIGINT handlers that flip the shutdown flag.
///
/// Same pattern as `rotten_apple_orchestrator::install_signal_handlers`:
/// only async-signal-safe operations (atomic store, Arc clone) inside
/// the handler. Install once before any concurrent reader exists.
pub fn install_signal_handlers(flag: Arc<ShutdownFlag>) {
    static mut FLAG: Option<Arc<ShutdownFlag>> = None;
    extern "C" fn handler(_sig: libc::c_int) {
        // SAFETY: install_signal_handlers is called once at startup before
        // any other thread reads FLAG; the AtomicBool inside is the only
        // mutated state from the handler.
        unsafe {
            #[allow(static_mut_refs)]
            if let Some(f) = &FLAG {
                f.raise();
            }
        }
    }
    // SAFETY: standard libc setup; SA_RESTART so blocking syscalls in
    // accept loops resume cleanly after handler return.
    unsafe {
        FLAG = Some(flag);
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT,  &sa, std::ptr::null_mut());
    }
}

/// Bind the socket, install signal handlers, run the accept loop until
/// shutdown. On clean shutdown the socket file is removed.
pub fn run(config: DaemonConfig) -> std::io::Result<()> {
    let shutdown = ShutdownFlag::new();
    install_signal_handlers(shutdown.clone());
    run_with_shutdown(config, shutdown)
}

/// Accept loop variant that takes an externally-owned shutdown flag.
/// Used by integration tests so they can shut the daemon down without
/// raising real signals into the test harness.
pub fn run_with_shutdown(
    config: DaemonConfig,
    shutdown: Arc<ShutdownFlag>,
) -> std::io::Result<()> {
    // Best-effort unlink of a stale socket. ENOENT is fine; anything else
    // we let bind() report a more useful error for.
    let _ = std::fs::remove_file(&config.socket_path);

    let listener = UnixListener::bind(&config.socket_path)?;
    // 0660: owner + group RW, world none. Group ACL is how cockpit/CLI
    // get access without running as root.
    let perms = std::fs::Permissions::from_mode(0o660);
    std::fs::set_permissions(&config.socket_path, perms)?;
    listener.set_nonblocking(true)?;
    let vsock_listener = match config.vsock_port {
        Some(port) => match bind_vsock_listener(port) {
            Ok(l) => {
                l.set_nonblocking(true)?;
                Some(l)
            }
            Err(e) => {
                eprintln!("[orchestratord] vsock disabled on port {port}: {e}");
                None
            }
        }
        None => None,
    };

    // The actor owns libxl. Spawned once for the daemon's lifetime;
    // cloned per-connection. If libxl_ctx_alloc fails the actor still
    // runs and answers BackendUnavailable per request.
    let actor = actor::spawn();

    // Engine ticks at 1 Hz against a clone of the actor handle. Start
    // it after the actor (engine calls into actor) and shut it down
    // before the actor (so in-flight engine calls don't see crashed).
    let engine = engine::start(actor.clone());

    let conn_id = AtomicU64::new(0);
    let mut workers: Vec<thread::JoinHandle<()>> = Vec::new();

    while !shutdown.is_raised() {
        let mut accepted = false;
        match listener.accept() {
            Ok((stream, _addr)) => {
                spawn_connection(
                    &mut workers,
                    &conn_id,
                    actor.clone(),
                    engine.clone(),
                    shutdown.clone(),
                    stream,
                )?;
                accepted = true;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => eprintln!("[orchestratord] unix accept error: {e}"),
        }
        if let Some(vsock) = vsock_listener.as_ref() {
            match vsock.accept() {
                Ok(stream) => {
                    spawn_connection(
                        &mut workers,
                        &conn_id,
                        actor.clone(),
                        engine.clone(),
                        shutdown.clone(),
                        stream,
                    )?;
                    accepted = true;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => eprintln!("[orchestratord] vsock accept error: {e}"),
            }
        }
        if !accepted {
            // Short sleep to avoid spinning between would-block accepts.
            // 100ms is a fine shutdown latency for a daemon.
            thread::sleep(Duration::from_millis(100));
        }
    }

    drop(listener);
    let _ = std::fs::remove_file(&config.socket_path);

    // Let in-flight connections finish on their own. We don't join with a
    // hard deadline yet — clients close on EOF when we drop the listener
    // (no, actually they don't; their stream stays alive). Best-effort:
    // give them a brief window, then return.
    for h in workers {
        let _ = h.join();
    }

    // Engine first (it calls into the actor) — then the actor (which
    // owns libxl). Order matters: shutting down the actor first would
    // make every in-flight engine tick observe ActorCrashed.
    engine.shutdown();
    actor.shutdown();

    Ok(())
}

fn spawn_connection(
    workers: &mut Vec<thread::JoinHandle<()>>,
    conn_id: &AtomicU64,
    actor: ActorHandle,
    engine: EngineHandle,
    shutdown: Arc<ShutdownFlag>,
    stream: UnixStream,
) -> std::io::Result<()> {
    let n = conn_id.fetch_add(1, Ordering::Relaxed);
    let name = format!("orchestratord-conn-{n}");
    let handle = thread::Builder::new()
        .name(name)
        .spawn(move || {
            if let Err(e) = handle_connection(stream, actor, engine, shutdown) {
                eprintln!("[orchestratord] connection error: {e}");
            }
        })?;
    workers.push(handle);
    workers.retain(|h| !h.is_finished());
    Ok(())
}

/// Per-connection handler. Reads the handshake, then loops on requests.
fn handle_connection(
    stream: UnixStream,
    actor: ActorHandle,
    engine: EngineHandle,
    shutdown: Arc<ShutdownFlag>,
) -> std::io::Result<()> {
    // Bound the handshake so a stuck client can't pin a worker forever.
    // Read timeout applies to the BufReader's underlying recv().
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;

    let read_half = stream.try_clone()?;
    let mut reader = BufReader::new(read_half);
    let mut writer = BufWriter::new(stream);

    // ---- Handshake ----------------------------------------------------
    let hello = match read_request(&mut reader)? {
        Some(Ok(r)) => r,
        Some(Err(e)) => {
            let resp = Response::err(
                PARSE_ERROR,
                format!("parse error: {e}"),
                None,
                serde_json::Value::Null,
            );
            let _ = write_response(&mut writer, &resp);
            return Ok(());
        }
        None => return Ok(()), // EOF before handshake
    };

    if hello.method != "hello" {
        let resp = Response::err(
            PROTOCOL_MISMATCH,
            "expected 'hello' as first method",
            None,
            hello.id.clone(),
        );
        let _ = write_response(&mut writer, &resp);
        return Ok(());
    }
    let client_proto = hello.params.get("protocol_version")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if client_proto != PROTOCOL_VERSION {
        let resp = Response::err(
            PROTOCOL_MISMATCH,
            "protocol version mismatch",
            Some(json!({
                "server_protocol_version": PROTOCOL_VERSION,
                "client_protocol_version": client_proto,
            })),
            hello.id.clone(),
        );
        let _ = write_response(&mut writer, &resp);
        return Ok(());
    }
    let resp = Response::ok(
        json!({
            "protocol_version": PROTOCOL_VERSION,
            "server":           SERVER_NAME,
            "server_version":   SERVER_VERSION,
        }),
        hello.id,
    );
    write_response(&mut writer, &resp)?;

    // Keep a short timeout so shutdown can break idle client sessions
    // promptly instead of hanging in join() until the peer disconnects.
    let _ = reader.get_ref().set_read_timeout(Some(Duration::from_secs(1)));

    // ---- Request loop -------------------------------------------------
    loop {
        if shutdown.is_raised() {
            return Ok(());
        }
        match read_request(&mut reader) {
            Ok(Some(Ok(req))) => {
                let resp = dispatch_method(&actor, &engine, &req);
                write_response(&mut writer, &resp)?;
            }
            Ok(Some(Err(e))) => {
                let resp = Response::err(
                    PARSE_ERROR,
                    format!("parse error: {e}"),
                    None,
                    serde_json::Value::Null,
                );
                write_response(&mut writer, &resp)?;
            }
            Err(e) if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) => continue,
            Err(e) => return Err(e),
            Ok(None) => return Ok(()), // clean EOF
        }
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{METHOD_NOT_FOUND, Request};

    #[test]
    fn protocol_version_is_pinned() {
        assert_eq!(PROTOCOL_VERSION, "0.1");
    }

    #[test]
    fn shutdown_flag_round_trip() {
        let f = ShutdownFlag::new();
        assert!(!f.is_raised());
        f.raise();
        assert!(f.is_raised());
    }

    #[test]
    fn default_socket_path_is_run() {
        let cfg = DaemonConfig::default();
        assert_eq!(cfg.socket_path, std::path::Path::new(DEFAULT_SOCKET_PATH));
    }

    #[test]
    fn vsock_is_off_by_default() {
        // Security: default-on vsock binds VSOCK_CID_ANY with no caller
        // auth = unauthenticated guest→host control plane. Must stay off
        // until mesh-peer auth (task #12) lands; opt in via --vsock-port.
        assert_eq!(DaemonConfig::default().vsock_port, None);
    }

    #[test]
    fn dispatch_ping_returns_pong() {
        let actor = actor::spawn();
        let engine = engine::start(actor.clone());
        let req: Request = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"ping","params":{},"id":1}"#
        ).unwrap();
        let resp = dispatch_method(&actor, &engine, &req);
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"]["pong"], true);
        engine.shutdown();
        actor.shutdown();
    }

    #[test]
    fn dispatch_unknown_returns_method_not_found() {
        let actor = actor::spawn();
        let engine = engine::start(actor.clone());
        let req: Request = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"frobnicate","id":2}"#
        ).unwrap();
        let resp = dispatch_method(&actor, &engine, &req);
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(v["error"]["data"]["method"], "frobnicate");
        engine.shutdown();
        actor.shutdown();
    }
}
