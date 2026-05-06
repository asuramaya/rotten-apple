//! Integration test: spin up `orchestratord::run_with_shutdown` on a
//! temp socket, connect a `DaemonClient`, run the handshake, and call a
//! few non-libxl-touching methods. Validates the cockpit's client side
//! end-to-end without needing a real Xen host (the daemon's libxl
//! actor handles "no Xen" gracefully).
//!
//! Mirrors the pattern in `crates/orchestratord/tests/handshake.rs`,
//! reused so we don't drift on shutdown handling.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use rotten_apple_cockpit::daemon_client::{DaemonClient, DaemonError};
use rotten_apple_orchestratord::{DaemonConfig, ShutdownFlag, run_with_shutdown};

fn temp_socket() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rotten-apple-cockpit-test-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0),
    ))
}

fn wait_for_socket(p: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if p.exists() { return }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("socket {} never appeared", p.display());
}

#[test]
fn handshake_against_real_orchestratord_succeeds() {
    let socket_path = temp_socket();
    let cfg = DaemonConfig {
        socket_path: socket_path.clone(),
        vsock_port: None,
    };
    let shutdown = ShutdownFlag::new();

    let server_shutdown = shutdown.clone();
    let server = thread::spawn(move || {
        run_with_shutdown(cfg, server_shutdown).unwrap();
    });

    wait_for_socket(&socket_path);

    let mut client = DaemonClient::connect(&socket_path)
        .expect("connect to socket");
    client.handshake().expect("handshake");

    // ping is libxl-free, so it succeeds in CI even without Xen.
    let pong = client.call("ping", json!({})).expect("ping");
    assert_eq!(pong["pong"], true);

    // host.info is also libxl-free in this test env (the actor returns
    // backend="unavailable" when XenBackend::new fails). Confirms the
    // larger response shape parses cleanly.
    let info = client.call("host.info", json!({})).expect("host.info");
    let backend = info["backend"].as_str().unwrap_or("");
    assert!(
        backend == "xen" || backend == "unavailable",
        "unexpected backend: {backend:?}",
    );

    drop(client);
    shutdown.raise();

    let join_deadline = Instant::now() + Duration::from_secs(5);
    while !server.is_finished() && Instant::now() < join_deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(server.is_finished(), "daemon never shut down");
    server.join().unwrap();
    assert!(!socket_path.exists());
}

#[test]
fn handshake_against_unbound_path_returns_io_error() {
    let path = std::env::temp_dir()
        .join("rotten-apple-cockpit-no-such-socket.sock");
    let _ = std::fs::remove_file(&path);
    let err = match DaemonClient::connect(&path) {
        Err(e) => e,
        Ok(_) => panic!("connect to nonexistent socket unexpectedly succeeded"),
    };
    // Either NotFound (path missing) or ConnectionRefused (path exists
    // but not bound). Both are valid first-run signals to fall through
    // to direct-libxl mode.
    assert!(matches!(
        err.kind(),
        std::io::ErrorKind::NotFound
        | std::io::ErrorKind::ConnectionRefused,
    ));
}

#[test]
fn unknown_method_surfaces_as_rpc_error() {
    let socket_path = temp_socket();
    let cfg = DaemonConfig {
        socket_path: socket_path.clone(),
        vsock_port: None,
    };
    let shutdown = ShutdownFlag::new();

    let server_shutdown = shutdown.clone();
    let server = thread::spawn(move || {
        run_with_shutdown(cfg, server_shutdown).unwrap();
    });

    wait_for_socket(&socket_path);

    let mut client = DaemonClient::connect(&socket_path).unwrap();
    client.handshake().unwrap();

    let err = client.call("frobnicate", json!({})).unwrap_err();
    match err {
        DaemonError::Rpc { code, .. } => assert_eq!(code, -32601),
        DaemonError::Io(e) => panic!("unexpected io: {e}"),
    }

    drop(client);
    shutdown.raise();
    let join_deadline = Instant::now() + Duration::from_secs(5);
    while !server.is_finished() && Instant::now() < join_deadline {
        thread::sleep(Duration::from_millis(50));
    }
    server.join().unwrap();
}
