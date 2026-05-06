//! End-to-end smoke tests for the engine.* and events.tail RPC methods,
//! plus domain.create error paths. Test env has no Xen so every actual
//! libxl call still bounces as BackendUnavailable — these tests focus on
//! the dispatch + engine wiring being correct without depending on libxl.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use rotten_apple_orchestratord::{DaemonConfig, ShutdownFlag, run_with_shutdown};

use std::sync::atomic::{AtomicU64, Ordering};

static SOCK_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_socket() -> PathBuf {
    let n = SOCK_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "rotten-apple-engine-test-{}-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0),
        n,
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

#[test]
fn engine_status_returns_defaults() {
    let (server, shutdown, mut writer, mut reader, socket_path) = connect_and_hello();
    send_line(
        &mut writer,
        r#"{"jsonrpc":"2.0","method":"engine.status","params":{},"id":1}"#,
    );
    let resp = read_line(&mut reader);
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"]["running"], true);
    assert!(resp["result"]["controlled_domains"].is_array());
    assert_eq!(resp["result"]["controlled_domains"].as_array().unwrap().len(), 0);
    teardown(server, shutdown, writer, reader, socket_path);
}

#[test]
fn engine_set_policy_round_trips_via_status() {
    let (server, shutdown, mut writer, mut reader, socket_path) = connect_and_hello();
    send_line(
        &mut writer,
        r#"{"jsonrpc":"2.0","method":"engine.set_policy","params":{"domid":7,"policy":{"min_mb":256,"max_mb":4096,"cooldown_s":30}},"id":1}"#,
    );
    let resp = read_line(&mut reader);
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"]["ok"], true);

    send_line(
        &mut writer,
        r#"{"jsonrpc":"2.0","method":"engine.status","params":{},"id":2}"#,
    );
    let resp = read_line(&mut reader);
    let domains = resp["result"]["controlled_domains"].as_array().unwrap();
    assert!(domains.iter().any(|d| d.as_u64() == Some(7)));
    teardown(server, shutdown, writer, reader, socket_path);
}

#[test]
fn events_tail_returns_cursor_and_events_array() {
    let (server, shutdown, mut writer, mut reader, socket_path) = connect_and_hello();
    send_line(
        &mut writer,
        r#"{"jsonrpc":"2.0","method":"events.tail","params":{"since":0},"id":1}"#,
    );
    let resp = read_line(&mut reader);
    assert_eq!(resp["id"], 1);
    assert!(resp["result"]["cursor"].is_u64());
    assert!(resp["result"]["events"].is_array());
    teardown(server, shutdown, writer, reader, socket_path);
}

#[test]
fn engine_set_policy_bad_params_returns_invalid_request() {
    let (server, shutdown, mut writer, mut reader, socket_path) = connect_and_hello();
    send_line(
        &mut writer,
        r#"{"jsonrpc":"2.0","method":"engine.set_policy","params":{"domid":1},"id":1}"#,
    );
    let resp = read_line(&mut reader);
    assert_eq!(resp["error"]["code"], -32600);
    teardown(server, shutdown, writer, reader, socket_path);
}

#[test]
fn events_tail_missing_since_returns_invalid_request() {
    let (server, shutdown, mut writer, mut reader, socket_path) = connect_and_hello();
    send_line(
        &mut writer,
        r#"{"jsonrpc":"2.0","method":"events.tail","params":{},"id":1}"#,
    );
    let resp = read_line(&mut reader);
    assert_eq!(resp["error"]["code"], -32600);
    teardown(server, shutdown, writer, reader, socket_path);
}

#[test]
fn domain_create_missing_params_returns_invalid_request() {
    let (server, shutdown, mut writer, mut reader, socket_path) = connect_and_hello();
    send_line(
        &mut writer,
        r#"{"jsonrpc":"2.0","method":"domain.create","params":{},"id":1}"#,
    );
    let resp = read_line(&mut reader);
    assert_eq!(resp["error"]["code"], -32600);
    teardown(server, shutdown, writer, reader, socket_path);
}

#[test]
fn domain_create_missing_manifest_path_returns_invalid_request() {
    let (server, shutdown, mut writer, mut reader, socket_path) = connect_and_hello();
    send_line(
        &mut writer,
        r#"{"jsonrpc":"2.0","method":"domain.create","params":{"manifest_path":"/nonexistent/manifest.toml"},"id":1}"#,
    );
    let resp = read_line(&mut reader);
    // File missing on disk → invalid request from the loader.
    assert_eq!(resp["error"]["code"], -32600);
    teardown(server, shutdown, writer, reader, socket_path);
}
