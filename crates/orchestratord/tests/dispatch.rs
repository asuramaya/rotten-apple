//! Integration tests for the JSON-RPC method registry.
//!
//! Test env has no Xen, so libxl-touching methods all return -32001
//! BackendUnavailable. The point of these tests is to exercise the
//! end-to-end accept loop + actor + dispatch wiring without depending
//! on libxl actually working — and to prove the daemon doesn't hang
//! or panic when libxl is absent.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use rotten_apple_orchestratord::{DaemonConfig, ShutdownFlag, run_with_shutdown};

// ---- helpers, copied terse from handshake.rs ----------------------------

fn temp_socket() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rotten-apple-test-{}-{}.sock",
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
        if p.exists() { return; }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("socket {} never appeared", p.display());
}

fn send_line(stream: &mut UnixStream, s: &str) {
    stream.write_all(s.as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
}

fn read_line(reader: &mut BufReader<UnixStream>) -> serde_json::Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

/// Spawn the daemon, do the hello handshake, return (server-thread,
/// shutdown-flag, writer, reader, socket-path) so the test body can
/// drive requests and tear down cleanly.
fn connect_and_hello() -> (
    thread::JoinHandle<()>,
    std::sync::Arc<ShutdownFlag>,
    UnixStream,
    BufReader<UnixStream>,
    PathBuf,
) {
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

    let stream = UnixStream::connect(&socket_path).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);

    send_line(
        &mut writer,
        r#"{"jsonrpc":"2.0","method":"hello","params":{"protocol_version":"0.1"},"id":0}"#,
    );
    let _ = read_line(&mut reader);

    (server, shutdown, writer, reader, socket_path)
}

fn teardown(
    server: thread::JoinHandle<()>,
    shutdown: std::sync::Arc<ShutdownFlag>,
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    socket_path: PathBuf,
) {
    drop(writer);
    drop(reader);
    shutdown.raise();
    let join_deadline = Instant::now() + Duration::from_secs(5);
    while !server.is_finished() && Instant::now() < join_deadline {
        thread::sleep(Duration::from_millis(50));
    }
    server.join().unwrap();
    assert!(!socket_path.exists());
}

// ---- tests --------------------------------------------------------------

#[test]
fn domain_list_returns_backend_unavailable_in_non_dom0() {
    // -32001 BackendUnavailable, NOT a hang and NOT a panic.
    let (server, shutdown, mut writer, mut reader, socket_path) = connect_and_hello();
    send_line(
        &mut writer,
        r#"{"jsonrpc":"2.0","method":"domain.list","params":{},"id":1}"#,
    );
    let resp = read_line(&mut reader);
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["error"]["code"], -32001);
    teardown(server, shutdown, writer, reader, socket_path);
}

#[test]
fn domain_get_unknown_domid_returns_error() {
    let (server, shutdown, mut writer, mut reader, socket_path) = connect_and_hello();
    send_line(
        &mut writer,
        r#"{"jsonrpc":"2.0","method":"domain.get","params":{"domid":999},"id":2}"#,
    );
    let resp = read_line(&mut reader);
    assert_eq!(resp["id"], 2);
    // In a Xen env this would be -32004 GuestNotFound; in the test env
    // we never get past BackendUnavailable. Either is an acceptable error
    // — the daemon must NOT return a result.
    let code = resp["error"]["code"].as_i64().expect("error code present");
    assert!(code == -32001 || code == -32004,
            "expected -32001 or -32004, got {code}");
    teardown(server, shutdown, writer, reader, socket_path);
}

#[test]
fn unknown_method_still_returns_method_not_found() {
    // Unrelated to the actor — pure dispatch concern. Make sure the
    // backend wiring didn't accidentally swallow this case.
    let (server, shutdown, mut writer, mut reader, socket_path) = connect_and_hello();
    send_line(
        &mut writer,
        r#"{"jsonrpc":"2.0","method":"nope","params":{},"id":3}"#,
    );
    let resp = read_line(&mut reader);
    assert_eq!(resp["id"], 3);
    assert_eq!(resp["error"]["code"], -32601);
    teardown(server, shutdown, writer, reader, socket_path);
}

#[test]
fn multiple_sequential_requests_round_trip() {
    // Five pings in a row on the same connection. Failure mode this
    // catches: writer not flushing per-frame, reader getting confused
    // about boundaries, actor channel deadlock.
    let (server, shutdown, mut writer, mut reader, socket_path) = connect_and_hello();
    for i in 1..=5 {
        let line = format!(
            r#"{{"jsonrpc":"2.0","method":"ping","params":{{}},"id":{i}}}"#
        );
        send_line(&mut writer, &line);
        let resp = read_line(&mut reader);
        assert_eq!(resp["id"], i);
        assert_eq!(resp["result"]["pong"], true);
    }
    teardown(server, shutdown, writer, reader, socket_path);
}
