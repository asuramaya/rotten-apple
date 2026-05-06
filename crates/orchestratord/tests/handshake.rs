//! End-to-end smoke test of the orchestratord scaffold:
//!   - bind a temp socket
//!   - speak the v0.1 hello handshake
//!   - exercise ping + an unknown method
//!   - shut the daemon down and clean up the socket file
//!
//! Uses `run_with_shutdown` so the test owns the shutdown flag and never
//! raises real signals into the cargo-test process.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use rotten_apple_orchestratord::{run_with_shutdown, DaemonConfig, ShutdownFlag};

fn temp_socket() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rotten-apple-test-{}-{}.sock",
        std::process::id(),
        // Nanosecond suffix so multiple tests in one process don't collide.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0),
    ))
}

fn wait_for_socket(p: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if p.exists() {
            return;
        }
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

#[test]
fn handshake_ping_and_unknown_method() {
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

    // ---- hello ----
    send_line(
        &mut writer,
        r#"{"jsonrpc":"2.0","method":"hello","params":{"protocol_version":"0.1"},"id":1}"#,
    );
    let hello_resp = read_line(&mut reader);
    assert_eq!(hello_resp["id"], 1);
    assert_eq!(hello_resp["result"]["protocol_version"], "0.1");
    assert_eq!(hello_resp["result"]["server"], "orchestratord");

    // ---- ping ----
    send_line(
        &mut writer,
        r#"{"jsonrpc":"2.0","method":"ping","params":{},"id":2}"#,
    );
    let ping_resp = read_line(&mut reader);
    assert_eq!(ping_resp["id"], 2);
    assert_eq!(ping_resp["result"]["pong"], true);

    // ---- unknown ----
    send_line(
        &mut writer,
        r#"{"jsonrpc":"2.0","method":"frobnicate","params":{},"id":3}"#,
    );
    let err_resp = read_line(&mut reader);
    assert_eq!(err_resp["id"], 3);
    assert_eq!(err_resp["error"]["code"], -32601);
    assert_eq!(err_resp["error"]["data"]["method"], "frobnicate");

    // Close client; ask daemon to shut down.
    drop(writer);
    drop(reader);

    shutdown.raise();

    let join_deadline = Instant::now() + Duration::from_secs(5);
    while !server.is_finished() && Instant::now() < join_deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(server.is_finished(), "server thread did not shut down");
    server.join().unwrap();

    assert!(!socket_path.exists(), "socket file should be removed on shutdown");
}

#[test]
fn host_info_and_domain_list_round_trip() {
    // Accepts either outcome:
    //   - real Xen dom0 → host.info reports backend="xen", domain.list
    //     returns an array.
    //   - CI without Xen → host.info reports backend="unavailable",
    //     domain.list returns BACKEND_UNAVAILABLE (-32001).
    // The same test binary must pass on both.
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
        r#"{"jsonrpc":"2.0","method":"hello","params":{"protocol_version":"0.1"},"id":1}"#,
    );
    let _ = read_line(&mut reader);

    // ---- host.info ----
    send_line(
        &mut writer,
        r#"{"jsonrpc":"2.0","method":"host.info","params":{},"id":2}"#,
    );
    let resp = read_line(&mut reader);
    assert_eq!(resp["id"], 2);
    let backend = resp["result"]["backend"].as_str().unwrap_or("");
    assert!(
        backend == "xen" || backend == "unavailable",
        "unexpected backend value: {backend:?}",
    );
    // libxl_version is a build-time string and always present.
    assert!(resp["result"]["libxl_version"].is_string());

    // ---- domain.list ----
    send_line(
        &mut writer,
        r#"{"jsonrpc":"2.0","method":"domain.list","params":{},"id":3}"#,
    );
    let list = read_line(&mut reader);
    assert_eq!(list["id"], 3);
    if let Some(arr) = list["result"]["domains"].as_array() {
        // Live Xen path. Each entry has the wire-stable shape.
        for d in arr {
            assert!(d["domid"].is_u64());
            assert!(d["name"].is_string());
        }
    } else {
        // CI path: backend-unavailable error on the libxl-touching method.
        assert_eq!(list["error"]["code"], -32001);
    }

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
fn handshake_protocol_mismatch_is_rejected() {
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
        r#"{"jsonrpc":"2.0","method":"hello","params":{"protocol_version":"9.9"},"id":1}"#,
    );
    let resp = read_line(&mut reader);
    assert_eq!(resp["error"]["code"], -32000);
    assert_eq!(resp["error"]["message"], "protocol version mismatch");

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
