//! End-to-end tests that drive the MCP server via the public `serve()`
//! entry point against a recording fake upstream.
//!
//! Spinning up the real orchestratord here would require Xen and a
//! writable socket path; CI runs without either. The fake upstream
//! exercises the same code paths as the real one (the only difference
//! is the UpstreamClient impl), so this is the higher-leverage test.

use std::collections::VecDeque;

use serde_json::{json, Value};

use rotten_apple_mcp_server::{
    serve,
    upstream::{UpstreamClient, UpstreamError},
};

struct FakeUpstream {
    calls:   Vec<(String, Value)>,
    replies: VecDeque<Result<Value, UpstreamError>>,
}

impl FakeUpstream {
    fn new() -> Self { Self { calls: Vec::new(), replies: VecDeque::new() } }
    fn push_ok(&mut self, v: Value) { self.replies.push_back(Ok(v)); }
}

impl UpstreamClient for FakeUpstream {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, UpstreamError> {
        self.calls.push((method.to_string(), params));
        self.replies.pop_front().unwrap_or_else(|| {
            Err(UpstreamError::Rpc {
                code:    -32603,
                message: "no canned reply".into(),
                data:    None,
            })
        })
    }
}

fn run_session(input: &str, up: &mut FakeUpstream) -> Vec<Value> {
    let mut reader = std::io::Cursor::new(input.as_bytes());
    let mut out: Vec<u8> = Vec::new();
    serve(&mut reader, &mut out, up).expect("serve");
    let s = String::from_utf8(out).expect("utf8");
    s.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("json"))
        .collect()
}

#[test]
fn full_initialize_list_call_session() {
    // Mirror the conversation Claude Code drives at startup, then a
    // single tools/call for host_info.
    let input = concat!(
        r#"{"jsonrpc":"2.0","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"claude-code","version":"x"}},"id":1}"#, "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, "\n",
        r#"{"jsonrpc":"2.0","method":"tools/list","id":2}"#, "\n",
        r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"host_info","arguments":{}},"id":3}"#, "\n",
    );

    let mut up = FakeUpstream::new();
    up.push_ok(json!({ "xen_version": "4.17", "free_kb": 1024 }));

    let responses = run_session(input, &mut up);

    // initialize, tools/list, tools/call — the notification produces nothing.
    assert_eq!(responses.len(), 3);

    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[0]["result"]["protocolVersion"], "2024-11-05");

    assert_eq!(responses[1]["id"], 2);
    let tools = responses[1]["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"host_info"));
    assert!(names.contains(&"domain_balloon"));

    assert_eq!(responses[2]["id"], 3);
    assert_eq!(responses[2]["result"]["isError"], false);
    assert_eq!(up.calls.len(), 1);
    assert_eq!(up.calls[0].0, "host.info");
}

#[test]
fn tools_call_translates_per_tool_arguments() {
    let input = concat!(
        r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"domain_get","arguments":{"domid":4}},"id":1}"#, "\n",
        r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"domain_shutdown","arguments":{"domid":4,"force":true}},"id":2}"#, "\n",
        r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"domain_balloon","arguments":{"domid":4,"target_kb":262144}},"id":3}"#, "\n",
    );

    let mut up = FakeUpstream::new();
    up.push_ok(json!({ "domid": 4 }));
    up.push_ok(json!({ "ok": true }));
    up.push_ok(json!({ "ok": true }));

    let _ = run_session(input, &mut up);

    assert_eq!(up.calls.len(), 3);
    assert_eq!(up.calls[0].0, "domain.get");
    assert_eq!(up.calls[0].1["domid"], 4);
    assert_eq!(up.calls[1].0, "domain.shutdown");
    assert_eq!(up.calls[1].1["force"], true);
    assert_eq!(up.calls[2].0, "domain.balloon");
    assert_eq!(up.calls[2].1["target_kb"], 262144);
}

#[test]
fn tools_call_invalid_params_short_circuits_upstream() {
    let input = concat!(
        r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"domain_get","arguments":{}},"id":1}"#, "\n",
    );
    let mut up = FakeUpstream::new();
    let responses = run_session(input, &mut up);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["error"]["code"], -32602);
    assert_eq!(up.calls.len(), 0);
}
