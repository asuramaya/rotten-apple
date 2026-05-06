//! rotten-apple MCP stdio server.
//!
//! Speaks Anthropic's Model Context Protocol JSON-RPC on stdin/stdout
//! and translates `tools/call` requests into JSON-RPC calls against
//! orchestratord. Transport policy matches cockpit: try the local Unix
//! socket first, then fall back to host-vsock for cross-domain use.
//! The tool surface is a 1:1 mirror of the orchestratord method surface.
//!
//! Framing: one JSON object per line, `\n`-terminated. Same shape as
//! orchestratord uses upstream — trivial to drive from any MCP client.

pub mod protocol;
pub mod upstream;

use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use rotten_apple_images::{Catalog, DEFAULT_CATALOG_PATH};
use rotten_apple_instances::{
    DEFAULT_MANIFESTS_DIR, DEFAULT_MEMORY_MB, DEFAULT_VCPUS, NewInstanceParams,
    create_instance,
};
use serde_json::{json, Value};

use crate::protocol::{
    McpRequest, McpResponse, Tool,
    INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND, PARSE_ERROR,
    MCP_PROTOCOL_VERSION, SERVER_NAME, SERVER_VERSION,
};
use crate::upstream::{Upstream, UpstreamClient, UpstreamError};

pub const DEFAULT_SOCKET_PATH: &str = "/run/rotten-apple.sock";

#[derive(Debug, Clone)]
pub struct McpConfig {
    pub socket_path: PathBuf,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self { socket_path: PathBuf::from(DEFAULT_SOCKET_PATH) }
    }
}

/// Connect to orchestratord, perform the upstream handshake, then drive
/// the MCP request loop on stdin/stdout until EOF.
///
/// On upstream-connect or handshake failure we emit a synthetic
/// `initialize` failure response on stdout (so the spawning client sees
/// a structured error instead of an empty pipe) and return the io error.
pub fn run(config: McpConfig) -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());

    let mut upstream = match Upstream::connect_default(&config.socket_path) {
        Ok(u) => u,
        Err(e) => {
            emit_init_failure(&mut writer, &format!(
                "cannot connect to orchestratord via {} or host-vsock: {e}",
                config.socket_path.display()
            ))?;
            return Err(e);
        }
    };
    if let Err(e) = upstream.handshake() {
        emit_init_failure(&mut writer, &format!("orchestratord handshake failed: {e}"))?;
        return Err(io::Error::other(format!("handshake: {e}")));
    }

    serve(&mut reader, &mut writer, &mut upstream)
}

/// Synthetic error reply for the bootstrap path: the MCP client hasn't
/// sent `initialize` yet, but writing nothing leaves it hanging. Emit a
/// JSON-RPC error with id null so the client surfaces something useful.
fn emit_init_failure<W: Write>(writer: &mut W, message: &str) -> io::Result<()> {
    let resp = McpResponse::err(
        protocol::INTERNAL_ERROR,
        message,
        None,
        Value::Null,
    );
    write_response(writer, &resp)
}

/// MCP request loop. Returns on stdin EOF.
pub fn serve<R, W, U>(reader: &mut R, writer: &mut W, upstream: &mut U) -> io::Result<()>
where
    R: BufRead,
    W: Write,
    U: UpstreamClient,
{
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 { return Ok(()); }
        if line.trim().is_empty() { continue; }

        let req: McpRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = McpResponse::err(
                    PARSE_ERROR,
                    format!("parse error: {e}"),
                    None,
                    Value::Null,
                );
                write_response(writer, &resp)?;
                continue;
            }
        };

        // Notifications carry no `id` and get no response.
        let is_notification = req.id.is_none();
        let id = req.id.clone().unwrap_or(Value::Null);

        if req.jsonrpc != "2.0" {
            if !is_notification {
                let resp = McpResponse::err(
                    INVALID_REQUEST,
                    "jsonrpc must be \"2.0\"",
                    None,
                    id,
                );
                write_response(writer, &resp)?;
            }
            continue;
        }

        let resp = handle(&req, upstream);
        if is_notification {
            // Notifications get nothing on the wire even on error.
            continue;
        }
        let resp = resp.unwrap_or_else(|| {
            // Should never happen — handle() only returns None for
            // notifications, and we already filtered those.
            McpResponse::err(protocol::INTERNAL_ERROR, "no response produced", None, id.clone())
        });
        write_response(writer, &resp)?;
    }
}

/// Dispatch one MCP request. Returns `None` for notifications (no reply).
fn handle<U: UpstreamClient>(req: &McpRequest, upstream: &mut U) -> Option<McpResponse> {
    let id = req.id.clone().unwrap_or(Value::Null);
    match req.method.as_str() {
        "initialize" => Some(McpResponse::ok(
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities":    { "tools": {} },
                "serverInfo":      { "name": SERVER_NAME, "version": SERVER_VERSION },
            }),
            id,
        )),
        "notifications/initialized" => None,
        "tools/list" => Some(McpResponse::ok(
            json!({ "tools": tool_list() }),
            id,
        )),
        "tools/call" => Some(handle_tools_call(&req.params, id, upstream)),
        // Notifications we don't implement: silently drop.
        m if m.starts_with("notifications/") => None,
        other => Some(McpResponse::err(
            METHOD_NOT_FOUND,
            format!("method not found: {other}"),
            None,
            id,
        )),
    }
}

fn handle_tools_call<U: UpstreamClient>(
    params: &Value,
    id: Value,
    upstream: &mut U,
) -> McpResponse {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return McpResponse::err(
            INVALID_PARAMS, "missing tool name", None, id,
        ),
    };
    let args = params.get("arguments").cloned().unwrap_or(Value::Object(Default::default()));

    if name == "instance_create" {
        return handle_instance_create(&args, id, upstream);
    }

    let translated = match translate_tool(name, &args) {
        Ok(t) => t,
        Err(msg) => return McpResponse::err(INVALID_PARAMS, msg, None, id),
    };

    match upstream.call(translated.method, translated.params) {
        Ok(result) => McpResponse::ok(tool_result_text(&result, false), id),
        Err(UpstreamError::Rpc { code, message, data }) => {
            let mut detail = format!("orchestratord error {code}: {message}");
            if let Some(d) = &data {
                detail.push_str(&format!("\ndata: {d}"));
            }
            McpResponse::ok(
                json!({
                    "content":  [{ "type": "text", "text": detail }],
                    "isError":  true,
                }),
                id,
            )
        }
        Err(UpstreamError::Io(e)) => McpResponse::ok(
            json!({
                "content":  [{ "type": "text", "text": format!("transport error: {e}") }],
                "isError":  true,
            }),
            id,
        ),
    }
}

/// Wrap a JSON value into the MCP `content` block shape.
fn tool_result_text(result: &Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(result)
        .unwrap_or_else(|_| result.to_string());
    json!({
        "content":  [{ "type": "text", "text": text }],
        "isError":  is_error,
    })
}

fn tool_error_text(message: impl Into<String>) -> Value {
    json!({
        "content": [{ "type": "text", "text": message.into() }],
        "isError": true,
    })
}

fn require_string_arg(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("missing or non-string '{key}'"))
}

fn optional_u64_arg(args: &Value, key: &str, default: u64) -> Result<u64, String> {
    match args.get(key) {
        None => Ok(default),
        Some(v) => v.as_u64()
            .ok_or_else(|| format!("missing or non-integer '{key}'")),
    }
}

fn optional_u32_arg(args: &Value, key: &str, default: u32) -> Result<u32, String> {
    let n = optional_u64_arg(args, key, u64::from(default))?;
    u32::try_from(n).map_err(|_| format!("'{key}' out of u32 range"))
}

fn optional_bool_arg(args: &Value, key: &str, default: bool) -> Result<bool, String> {
    match args.get(key) {
        None => Ok(default),
        Some(v) => v.as_bool()
            .ok_or_else(|| format!("missing or non-boolean '{key}'")),
    }
}

fn ensure_base_image_present(name: &str) -> Result<(), String> {
    let catalog_path = Path::new(DEFAULT_CATALOG_PATH);
    let mut cat = Catalog::load_or_empty(catalog_path);
    if cat.find(name).is_some() {
        return Ok(());
    }
    let dest = catalog_path.parent().unwrap_or_else(||
        Path::new("/var/lib/rotten-apple/images"));
    let entry = rotten_apple_images::pull(name, dest, false)
        .map_err(|e| format!("pull {name}: {e}"))?;
    cat.upsert(entry);
    cat.save(catalog_path)
        .map_err(|e| format!("save image catalog {}: {e}", catalog_path.display()))
}

fn handle_instance_create<U: UpstreamClient>(
    args: &Value,
    id: Value,
    upstream: &mut U,
) -> McpResponse {
    let instance_id = match require_string_arg(args, "id") {
        Ok(v) => v,
        Err(msg) => return McpResponse::err(INVALID_PARAMS, msg, None, id),
    };
    let base_image = match require_string_arg(args, "base_image") {
        Ok(v) => v,
        Err(msg) => return McpResponse::err(INVALID_PARAMS, msg, None, id),
    };
    let memory_mb = match optional_u64_arg(args, "memory_mb", DEFAULT_MEMORY_MB) {
        Ok(v) => v,
        Err(msg) => return McpResponse::err(INVALID_PARAMS, msg, None, id),
    };
    let vcpus = match optional_u32_arg(args, "vcpus", DEFAULT_VCPUS) {
        Ok(v) => v,
        Err(msg) => return McpResponse::err(INVALID_PARAMS, msg, None, id),
    };
    let ephemeral = match optional_bool_arg(args, "ephemeral", false) {
        Ok(v) => v,
        Err(msg) => return McpResponse::err(INVALID_PARAMS, msg, None, id),
    };

    let params = NewInstanceParams {
        id: instance_id.clone(),
        base_image: base_image.clone(),
        memory_mb,
        vcpus,
        ephemeral,
    };

    let entry = match create_instance(params, false) {
        Ok(entry) => entry,
        Err(rotten_apple_instances::InstanceError::BaseImageNotFound(_)) => {
            if let Err(e) = ensure_base_image_present(&base_image) {
                return McpResponse::ok(tool_error_text(format!(
                    "instance create {instance_id}: auto-pull {base_image}: {e}"
                )), id);
            }
            match create_instance(NewInstanceParams {
                id: instance_id.clone(),
                base_image: base_image.clone(),
                memory_mb,
                vcpus,
                ephemeral,
            }, false) {
                Ok(entry) => entry,
                Err(e) => return McpResponse::ok(tool_error_text(format!(
                    "instance create {instance_id}: {e}"
                )), id),
            }
        }
        Err(e) => return McpResponse::ok(tool_error_text(format!(
            "instance create {instance_id}: {e}"
        )), id),
    };

    let manifest_path = Path::new(DEFAULT_MANIFESTS_DIR)
        .join(format!("{}.toml", entry.id));
    match upstream.call("domain.create", json!({
        "manifest_path": manifest_path.to_string_lossy(),
    })) {
        Ok(result) => McpResponse::ok(tool_result_text(&json!({
            "instance": entry,
            "domain": result,
        }), false), id),
        Err(UpstreamError::Rpc { code, message, data }) => {
            let mut detail = format!("orchestratord error {code}: {message}");
            if let Some(d) = &data {
                detail.push_str(&format!("\ndata: {d}"));
            }
            McpResponse::ok(tool_error_text(detail), id)
        }
        Err(UpstreamError::Io(e)) => McpResponse::ok(
            tool_error_text(format!("transport error: {e}")),
            id,
        ),
    }
}

// ---------------------------------------------------------------------------
// Tool surface

struct Translated {
    method: &'static str,
    params: Value,
}

/// Translate an MCP `tools/call` invocation into an orchestratord
/// `(method, params)` pair. Returns a human-readable error on bad params.
fn translate_tool(name: &str, args: &Value) -> Result<Translated, String> {
    fn require_domid(args: &Value) -> Result<i64, String> {
        args.get("domid")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| "missing or non-integer 'domid'".to_string())
    }

    match name {
        "host_info" => Ok(Translated {
            method: "host.info",
            params: json!({}),
        }),
        "domain_list" => Ok(Translated {
            method: "domain.list",
            params: json!({}),
        }),
        "domain_get" => {
            let domid = require_domid(args)?;
            Ok(Translated { method: "domain.get", params: json!({ "domid": domid }) })
        }
        "domain_start" => {
            let domid = require_domid(args)?;
            Ok(Translated { method: "domain.start", params: json!({ "domid": domid }) })
        }
        "domain_shutdown" => {
            let domid = require_domid(args)?;
            let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
            Ok(Translated {
                method: "domain.shutdown",
                params: json!({ "domid": domid, "force": force }),
            })
        }
        "domain_kill" => {
            let domid = require_domid(args)?;
            Ok(Translated { method: "domain.kill", params: json!({ "domid": domid }) })
        }
        "domain_balloon" => {
            let domid = require_domid(args)?;
            let target_kb = args.get("target_kb")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| "missing or non-integer 'target_kb'".to_string())?;
            Ok(Translated {
                method: "domain.balloon",
                params: json!({ "domid": domid, "target_kb": target_kb }),
            })
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

fn domid_schema() -> Value {
    json!({
        "type":        "integer",
        "minimum":     0,
        "description": "Xen domain id (domid). 0 is dom0.",
    })
}

/// All tools exposed to MCP clients. Order is stable so `tools/list`
/// output is deterministic.
pub fn tool_list() -> Vec<Tool> {
    vec![
        Tool {
            name: "instance_create",
            description: "Create a fresh instance from a base image, auto-pulling the base if needed, then ask the daemon to create the domain from the generated manifest. Use this to spawn new guests from one MCP surface.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Stable rotten-apple instance id. Also becomes the overlay and manifest name.",
                    },
                    "base_image": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Base image shorthand such as ubuntu:24.04, ubuntu:22.04, or debian:12.",
                    },
                    "memory_mb": {
                        "type": "integer",
                        "minimum": 1,
                        "default": DEFAULT_MEMORY_MB,
                        "description": "Guest memory target in MiB.",
                    },
                    "vcpus": {
                        "type": "integer",
                        "minimum": 1,
                        "default": DEFAULT_VCPUS,
                        "description": "Number of virtual CPUs for the new guest.",
                    },
                    "ephemeral": {
                        "type": "boolean",
                        "default": false,
                        "description": "Whether the instance should be marked ephemeral in the registry.",
                    }
                },
                "required": ["id", "base_image"],
                "additionalProperties": false,
            }),
        },
        Tool {
            name: "host_info",
            description: "Return host hypervisor facts: Xen version, dom0 memory, CPU count, free memory. Use first when investigating a host before issuing other calls.",
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        Tool {
            name: "domain_list",
            description: "List all domains known to the hypervisor with id, name, state, and resource usage. Cheap; use to discover domids before per-domain calls.",
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        Tool {
            name: "domain_get",
            description: "Fetch detailed state for a single domain by domid: name, run state, vcpus, memory, uuid. Use after domain_list to drill into a specific guest.",
            input_schema: json!({
                "type": "object",
                "properties": { "domid": domid_schema() },
                "required": ["domid"],
                "additionalProperties": false,
            }),
        },
        Tool {
            name: "domain_start",
            description: "Resume or unpause a stopped domain. The domain must already be defined; this does not create new guests.",
            input_schema: json!({
                "type": "object",
                "properties": { "domid": domid_schema() },
                "required": ["domid"],
                "additionalProperties": false,
            }),
        },
        Tool {
            name: "domain_shutdown",
            description: "Request graceful guest shutdown via the ACPI/PV channel. Set force=true to skip the cooperative path and proceed straight to a hard stop. Prefer this over domain_kill for normal operation.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "domid": domid_schema(),
                    "force": {
                        "type": "boolean",
                        "default": false,
                        "description": "If true, bypass guest cooperation and stop unconditionally.",
                    },
                },
                "required": ["domid"],
                "additionalProperties": false,
            }),
        },
        Tool {
            name: "domain_kill",
            description: "Hard-kill a domain immediately, equivalent to pulling its power. The guest is not given a chance to flush; use only when shutdown has hung.",
            input_schema: json!({
                "type": "object",
                "properties": { "domid": domid_schema() },
                "required": ["domid"],
                "additionalProperties": false,
            }),
        },
        Tool {
            name: "domain_balloon",
            description: "Set a guest's memory target in kilobytes. Use to grow or shrink a domain's RAM allocation; the guest's balloon driver enforces the change.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "domid":     domid_schema(),
                    "target_kb": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "New memory target for the domain, in kilobytes.",
                    },
                },
                "required": ["domid", "target_kb"],
                "additionalProperties": false,
            }),
        },
    ]
}

fn write_response<W: Write>(writer: &mut W, resp: &McpResponse) -> io::Result<()> {
    let mut buf = serde_json::to_vec(resp)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    buf.push(b'\n');
    writer.write_all(&buf)?;
    writer.flush()
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Recording fake upstream for translation tests.
    struct FakeUpstream {
        calls:   Vec<(String, Value)>,
        replies: VecDeque<Result<Value, UpstreamError>>,
    }

    impl FakeUpstream {
        fn new() -> Self {
            Self { calls: Vec::new(), replies: VecDeque::new() }
        }
        fn push_ok(&mut self, v: Value) { self.replies.push_back(Ok(v)); }
        fn push_rpc_err(&mut self, code: i32, msg: &str) {
            self.replies.push_back(Err(UpstreamError::Rpc {
                code, message: msg.into(), data: None,
            }));
        }
    }

    impl UpstreamClient for FakeUpstream {
        fn call(&mut self, method: &str, params: Value) -> Result<Value, UpstreamError> {
            self.calls.push((method.to_string(), params));
            self.replies.pop_front().unwrap_or_else(|| {
                Err(UpstreamError::Rpc {
                    code: -32603,
                    message: "no canned reply".into(),
                    data: None,
                })
            })
        }
    }

    fn req(method: &str, params: Value, id: i64) -> McpRequest {
        McpRequest {
            jsonrpc: "2.0".into(),
            method:  method.into(),
            params,
            id:      Some(json!(id)),
        }
    }

    #[test]
    fn tools_list_contains_all_tools() {
        let resp = handle(&req("tools/list", json!({}), 1), &mut FakeUpstream::new()).unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        let names: Vec<&str> = v["result"]["tools"]
            .as_array().unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec![
            "instance_create",
            "host_info",
            "domain_list",
            "domain_get",
            "domain_start",
            "domain_shutdown",
            "domain_kill",
            "domain_balloon",
        ]);
    }

    #[test]
    fn initialize_returns_protocol_version() {
        let resp = handle(&req("initialize", json!({}), 1), &mut FakeUpstream::new()).unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(v["result"]["serverInfo"]["name"], SERVER_NAME);
        assert!(v["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn notifications_initialized_yields_no_response() {
        let r = McpRequest {
            jsonrpc: "2.0".into(),
            method:  "notifications/initialized".into(),
            params:  json!({}),
            id:      None,
        };
        assert!(handle(&r, &mut FakeUpstream::new()).is_none());
    }

    #[test]
    fn unknown_method_returns_method_not_found() {
        let resp = handle(&req("frobnicate", json!({}), 9), &mut FakeUpstream::new()).unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn tools_call_translates_host_info() {
        let mut up = FakeUpstream::new();
        up.push_ok(json!({ "xen_version": "4.17" }));
        let resp = handle(
            &req("tools/call", json!({ "name": "host_info", "arguments": {} }), 1),
            &mut up,
        ).unwrap();
        assert_eq!(up.calls.len(), 1);
        assert_eq!(up.calls[0].0, "host.info");
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"]["isError"], false);
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("xen_version"));
    }

    #[test]
    fn tools_call_translates_domain_balloon() {
        let mut up = FakeUpstream::new();
        up.push_ok(json!({ "ok": true }));
        let resp = handle(
            &req("tools/call", json!({
                "name": "domain_balloon",
                "arguments": { "domid": 5, "target_kb": 524288 },
            }), 2),
            &mut up,
        ).unwrap();
        assert_eq!(up.calls.len(), 1);
        assert_eq!(up.calls[0].0, "domain.balloon");
        assert_eq!(up.calls[0].1["domid"], 5);
        assert_eq!(up.calls[0].1["target_kb"], 524288);
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"]["isError"], false);
    }

    #[test]
    fn tools_call_default_force_false() {
        let mut up = FakeUpstream::new();
        up.push_ok(json!({}));
        handle(
            &req("tools/call", json!({
                "name": "domain_shutdown",
                "arguments": { "domid": 3 },
            }), 1),
            &mut up,
        );
        assert_eq!(up.calls[0].1["force"], false);
    }

    #[test]
    fn tools_call_missing_domid_is_invalid_params() {
        let mut up = FakeUpstream::new();
        let resp = handle(
            &req("tools/call", json!({
                "name": "domain_get",
                "arguments": {},
            }), 1),
            &mut up,
        ).unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["error"]["code"], INVALID_PARAMS);
        assert_eq!(up.calls.len(), 0);
    }

    #[test]
    fn tools_call_unknown_tool_is_invalid_params() {
        let mut up = FakeUpstream::new();
        let resp = handle(
            &req("tools/call", json!({
                "name": "nonsense",
                "arguments": {},
            }), 1),
            &mut up,
        ).unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn tools_call_backend_error_returns_iserror_true() {
        let mut up = FakeUpstream::new();
        up.push_rpc_err(-32601, "Method not found");
        let resp = handle(
            &req("tools/call", json!({
                "name": "host_info",
                "arguments": {},
            }), 1),
            &mut up,
        ).unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        // Backend errors are reported as result.isError = true, not as
        // an MCP-level error object.
        assert!(v.get("error").is_none() || v["error"].is_null());
        assert_eq!(v["result"]["isError"], true);
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Method not found"));
    }

    #[test]
    fn serve_drives_request_loop_until_eof() {
        // Two requests on stdin, then EOF. Verify two responses on stdout.
        let input = concat!(
            r#"{"jsonrpc":"2.0","method":"initialize","params":{},"id":1}"#, "\n",
            r#"{"jsonrpc":"2.0","method":"tools/list","params":{},"id":2}"#, "\n",
        );
        let mut reader = std::io::Cursor::new(input.as_bytes());
        let mut out: Vec<u8> = Vec::new();
        let mut up = FakeUpstream::new();
        serve(&mut reader, &mut out, &mut up).unwrap();
        let s = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 2);
        let r1: Value = serde_json::from_str(lines[0]).unwrap();
        let r2: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(r1["id"], 1);
        assert_eq!(r1["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(r2["id"], 2);
        assert!(r2["result"]["tools"].is_array());
    }

    #[test]
    fn serve_skips_response_for_notification() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, "\n",
            r#"{"jsonrpc":"2.0","method":"tools/list","id":7}"#, "\n",
        );
        let mut reader = std::io::Cursor::new(input.as_bytes());
        let mut out: Vec<u8> = Vec::new();
        let mut up = FakeUpstream::new();
        serve(&mut reader, &mut out, &mut up).unwrap();
        let s = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 1);
        let r: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(r["id"], 7);
    }

    #[test]
    fn serve_emits_parse_error_on_garbage_line() {
        let input = "not json\n";
        let mut reader = std::io::Cursor::new(input.as_bytes());
        let mut out: Vec<u8> = Vec::new();
        let mut up = FakeUpstream::new();
        serve(&mut reader, &mut out, &mut up).unwrap();
        let s = String::from_utf8(out).unwrap();
        let line = s.lines().next().unwrap();
        let v: Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["error"]["code"], PARSE_ERROR);
    }
}
