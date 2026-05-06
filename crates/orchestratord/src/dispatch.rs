//! JSON-RPC method registry.
//!
//! Each method translates JSON params → typed `ActorHandle` call →
//! JSON result. Errors from the actor become `ErrorObject`s with the
//! `-32001..-32007` codes from this crate's protocol module.
//!
//! Adding a method: extend the match in [`dispatch`] and the corresponding
//! `ActorRequest` variant + `ActorHandle` accessor.

use serde_json::{Value, json};

use rotten_apple_manifest::{PolicyMemory, Profile};

use crate::actor::{ActorError, ActorHandle};
use crate::engine::EngineHandle;
use crate::protocol::{
    BACKEND_INTERNAL, BACKEND_UNAVAILABLE, ErrorObject, GUEST_ALREADY_RUNNING,
    GUEST_NOT_FOUND, HARDWARE_UNAVAILABLE, INSUFFICIENT_RESOURCES, INVALID_REQUEST,
    METHOD_NOT_FOUND, PERMISSION_DENIED, Request, Response,
};

pub fn dispatch(actor: &ActorHandle, engine: &EngineHandle, req: &Request) -> Response {
    let id = req.id.clone();
    match req.method.as_str() {
        "ping" => Response::ok(json!({"pong": true}), id),

        "host.info" => match actor.host_info() {
            Ok(info) => Response::ok(serde_json::to_value(info).unwrap(), id),
            Err(e) => Response::Err {
                jsonrpc: "2.0".into(),
                error: ErrorObject::from_actor_error(&e),
                id,
            },
        },

        "host.resources" => match actor.host_resources() {
            Ok(r)  => Response::ok(serde_json::to_value(r).unwrap(), id),
            Err(e) => err_resp(&e, id),
        },

        "domain.list" => match actor.domain_list() {
            Ok(domains) => Response::ok(json!({ "domains": domains }), id),
            Err(e) => err_resp(&e, id),
        },

        "domain.get" => match parse_domid(&req.params) {
            Err(e) => invalid_resp(e, id),
            Ok(domid) => match actor.domain_get(domid) {
                Ok(info) => Response::ok(serde_json::to_value(info).unwrap(), id),
                Err(e) => err_resp(&e, id),
            },
        },

        "domain.start" => match parse_domid(&req.params) {
            Err(e) => invalid_resp(e, id),
            Ok(domid) => match actor.domain_start(domid) {
                Ok(()) => Response::ok(json!({"ok": true}), id),
                Err(e) => err_resp(&e, id),
            },
        },

        "domain.shutdown" => match parse_domid(&req.params) {
            Err(e) => invalid_resp(e, id),
            Ok(domid) => {
                let force = req.params.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
                match actor.domain_shutdown(domid, force) {
                    Ok(()) => Response::ok(json!({"ok": true}), id),
                    Err(e) => err_resp(&e, id),
                }
            }
        },

        // domain.kill is the explicit force=true path. Distinct method
        // name keeps caller intent visible at the wire ("kill" reads
        // better than "shutdown with force=true").
        "domain.kill" => match parse_domid(&req.params) {
            Err(e) => invalid_resp(e, id),
            Ok(domid) => match actor.domain_shutdown(domid, true) {
                Ok(()) => Response::ok(json!({"ok": true}), id),
                Err(e) => err_resp(&e, id),
            },
        },

        "domain.balloon" => match parse_domid(&req.params) {
            Err(e) => invalid_resp(e, id),
            Ok(domid) => match req.params.get("target_kb").and_then(|v| v.as_u64()) {
                None => invalid_resp("missing or invalid target_kb (u64)".into(), id),
                Some(target_kb) => match actor.domain_balloon(domid, target_kb) {
                    Ok(()) => Response::ok(json!({"ok": true, "target_kb": target_kb}), id),
                    Err(e) => err_resp(&e, id),
                },
            },
        },

        // ---- domain.create ------------------------------------------------
        // Accepts either { manifest_path } pointing at a TOML file the
        // daemon will read, or { profile_inline } carrying the TOML body
        // verbatim. Inline form keeps callers off the daemon's filesystem
        // when they're a remote MCP client.
        "domain.create" => match load_profile(&req.params) {
            Err(e) => invalid_resp(e, id),
            Ok(profile) => match actor.domain_create(profile) {
                Ok(domid) => Response::ok(json!({"domid": domid}), id),
                Err(e) => err_resp(&e, id),
            },
        },

        // ---- engine.* -----------------------------------------------------
        "engine.status" => {
            let s = engine.status();
            Response::ok(serde_json::to_value(&s).unwrap(), id)
        }

        "engine.set_policy" => match parse_set_policy(&req.params) {
            Err(e) => invalid_resp(e, id),
            Ok((domid, policy)) => {
                engine.set_policy(domid, policy);
                Response::ok(json!({"ok": true}), id)
            }
        },

        // ---- events.tail --------------------------------------------------
        "events.tail" => match parse_tail(&req.params) {
            Err(e) => invalid_resp(e, id),
            Ok((since, limit)) => {
                let rx = engine.events();
                let (cursor, mut events) = rx.drain_since(since);
                if events.len() > limit {
                    events.truncate(limit);
                }
                Response::ok(
                    json!({"cursor": cursor, "events": events}),
                    id,
                )
            }
        },

        other => Response::err(
            METHOD_NOT_FOUND,
            "Method not found",
            Some(json!({"method": other})),
            id,
        ),
    }
}

/// Pull a u32 `domid` out of the params object. Accepts integer JSON only;
/// strings are rejected so a typo'd "0" (string) fails fast with -32600
/// rather than silently parsing.
fn parse_domid(params: &Value) -> Result<u32, String> {
    let v = params.get("domid").ok_or_else(|| "missing 'domid' parameter".to_string())?;
    let n = v.as_u64().ok_or_else(|| "'domid' must be a non-negative integer".to_string())?;
    u32::try_from(n).map_err(|_| "'domid' out of u32 range".to_string())
}

fn invalid_resp(msg: String, id: Value) -> Response {
    Response::err(INVALID_REQUEST, msg, None, id)
}

/// Load a `Profile` from `domain.create` params. Accepts:
///   - `{ "manifest_path": "/path/to.toml" }` — daemon reads the file.
///   - `{ "profile_inline": "[profile] ..." }` — TOML body inline.
fn load_profile(params: &Value) -> Result<Profile, String> {
    if let Some(p) = params.get("manifest_path").and_then(|v| v.as_str()) {
        return Profile::load(p)
            .map_err(|e| format!("failed to load manifest {p:?}: {e}"));
    }
    if let Some(s) = params.get("profile_inline").and_then(|v| v.as_str()) {
        return Profile::from_str(s)
            .map_err(|e| format!("failed to parse inline profile: {e}"));
    }
    Err("missing 'manifest_path' or 'profile_inline'".into())
}

/// Parse `{ domid, policy: { min_mb?, max_mb?, target_headroom_pct?, cooldown_s? } }`.
fn parse_set_policy(params: &Value) -> Result<(u32, PolicyMemory), String> {
    let domid = parse_domid(params)?;
    let policy_v = params.get("policy")
        .ok_or_else(|| "missing 'policy' parameter".to_string())?;
    let policy: PolicyMemory = serde_json::from_value(policy_v.clone())
        .map_err(|e| format!("invalid 'policy': {e}"))?;
    Ok((domid, policy))
}

/// Parse `{ since: u64, limit?: u64 }` for events.tail.
fn parse_tail(params: &Value) -> Result<(u64, usize), String> {
    let since = params.get("since")
        .ok_or_else(|| "missing 'since' parameter".to_string())?
        .as_u64()
        .ok_or_else(|| "'since' must be a non-negative integer".to_string())?;
    let limit = match params.get("limit") {
        None => 256,
        Some(v) => {
            let n = v.as_u64()
                .ok_or_else(|| "'limit' must be a non-negative integer".to_string())?;
            // Clamp wildly large limits to the ring capacity. usize on
            // 32-bit hosts can't represent every u64; saturate.
            usize::try_from(n).unwrap_or(usize::MAX)
        }
    };
    Ok((since, limit))
}

fn err_resp(e: &ActorError, id: Value) -> Response {
    Response::Err {
        jsonrpc: "2.0".into(),
        error: ErrorObject::from_actor_error(e),
        id,
    }
}

impl ErrorObject {
    /// Build a JSON-RPC error from an `ActorError`. The code/message are
    /// the daemon-stable contract; `data` carries the libxl detail string
    /// for operator debugging.
    pub fn from_actor_error(e: &ActorError) -> Self {
        let (code, message, detail) = match e {
            ActorError::BackendUnavailable(d)    => (BACKEND_UNAVAILABLE,    "backend unavailable",    Some(d.clone())),
            ActorError::BackendInternal(d)       => (BACKEND_INTERNAL,       "backend internal error", Some(d.clone())),
            ActorError::PermissionDenied(d)      => (PERMISSION_DENIED,      "permission denied",      Some(d.clone())),
            ActorError::GuestNotFound(d)         => (GUEST_NOT_FOUND,        "domain not found",       Some(d.clone())),
            ActorError::GuestAlreadyRunning(d)   => (GUEST_ALREADY_RUNNING,  "domain already running", Some(d.clone())),
            ActorError::InsufficientResources(d) => (INSUFFICIENT_RESOURCES, "insufficient resources", Some(d.clone())),
            ActorError::HardwareUnavailable(d)   => (HARDWARE_UNAVAILABLE,   "hardware unavailable",   Some(d.clone())),
            ActorError::ActorCrashed             => (BACKEND_INTERNAL,       "actor thread crashed",   None),
        };
        ErrorObject {
            code,
            message: message.into(),
            data: detail.map(|d| json!({"detail": d})),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{actor, engine};

    fn req(method: &str, params: Value, id: i64) -> Request {
        Request {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params,
            id: json!(id),
        }
    }

    fn spawn_pair() -> (crate::actor::ActorHandle, crate::engine::EngineHandle) {
        let a = actor::spawn();
        let e = engine::start(a.clone());
        (a, e)
    }

    fn teardown(a: crate::actor::ActorHandle, e: crate::engine::EngineHandle) {
        e.shutdown();
        a.shutdown();
    }

    #[test]
    fn unknown_method_returns_method_not_found() {
        let (a, e) = spawn_pair();
        let r = dispatch(&a, &e, &req("frobnicate", json!({}), 1));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["error"]["code"], METHOD_NOT_FOUND);
        teardown(a, e);
    }

    #[test]
    fn host_info_returns_unavailable_backend_in_test_env() {
        let (a, e) = spawn_pair();
        let r = dispatch(&a, &e, &req("host.info", json!({}), 1));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["result"]["backend"], "unavailable");
        assert_eq!(v["result"]["running_under_xen"], false);
        teardown(a, e);
    }

    #[test]
    fn domain_list_returns_backend_unavailable_in_test_env() {
        let (a, e) = spawn_pair();
        let r = dispatch(&a, &e, &req("domain.list", json!({}), 1));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["error"]["code"], BACKEND_UNAVAILABLE);
        teardown(a, e);
    }

    #[test]
    fn domain_get_missing_param_is_invalid_request() {
        let (a, e) = spawn_pair();
        let r = dispatch(&a, &e, &req("domain.get", json!({}), 1));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["error"]["code"], INVALID_REQUEST);
        teardown(a, e);
    }

    #[test]
    fn domain_balloon_requires_target_kb() {
        let (a, e) = spawn_pair();
        let r = dispatch(&a, &e, &req("domain.balloon", json!({"domid": 1}), 1));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["error"]["code"], INVALID_REQUEST);
        teardown(a, e);
    }

    #[test]
    fn domain_kill_routes_through_force_shutdown() {
        // In CI without Xen this returns BACKEND_UNAVAILABLE rather than
        // succeeding, but the test still exercises the dispatch path:
        // missing domid → INVALID_REQUEST; well-formed → backend call.
        let (a, e) = spawn_pair();
        let bad = dispatch(&a, &e, &req("domain.kill", json!({}), 1));
        let v = serde_json::to_value(&bad).unwrap();
        assert_eq!(v["error"]["code"], INVALID_REQUEST);

        let good = dispatch(&a, &e, &req("domain.kill", json!({"domid": 0}), 2));
        let v = serde_json::to_value(&good).unwrap();
        // Either backend_unavailable (CI) or some real backend reply —
        // we only check the request did not bounce as method-not-found.
        assert!(v["error"].is_object() || v["result"].is_object());
        assert_ne!(v["error"]["code"], METHOD_NOT_FOUND);
        teardown(a, e);
    }

    #[test]
    fn engine_status_returns_running_with_empty_domains() {
        let (a, e) = spawn_pair();
        let r = dispatch(&a, &e, &req("engine.status", json!({}), 1));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["result"]["running"], true);
        assert!(v["result"]["controlled_domains"].is_array());
        teardown(a, e);
    }

    #[test]
    fn engine_set_policy_round_trips_through_status() {
        let (a, e) = spawn_pair();
        let r = dispatch(&a, &e, &req(
            "engine.set_policy",
            json!({"domid": 42, "policy": {"min_mb": 256, "max_mb": 4096}}),
            1,
        ));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["result"]["ok"], true);

        let r = dispatch(&a, &e, &req("engine.status", json!({}), 2));
        let v = serde_json::to_value(&r).unwrap();
        let domains = v["result"]["controlled_domains"].as_array().unwrap();
        assert!(domains.iter().any(|d| d.as_u64() == Some(42)));
        teardown(a, e);
    }

    #[test]
    fn engine_set_policy_rejects_missing_domid() {
        let (a, e) = spawn_pair();
        let r = dispatch(&a, &e, &req(
            "engine.set_policy",
            json!({"policy": {"min_mb": 256}}),
            1,
        ));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["error"]["code"], INVALID_REQUEST);
        teardown(a, e);
    }

    #[test]
    fn engine_set_policy_rejects_missing_policy() {
        let (a, e) = spawn_pair();
        let r = dispatch(&a, &e, &req(
            "engine.set_policy",
            json!({"domid": 1}),
            1,
        ));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["error"]["code"], INVALID_REQUEST);
        teardown(a, e);
    }

    #[test]
    fn events_tail_returns_cursor_and_array() {
        let (a, e) = spawn_pair();
        let r = dispatch(&a, &e, &req("events.tail", json!({"since": 0}), 1));
        let v = serde_json::to_value(&r).unwrap();
        assert!(v["result"]["cursor"].is_u64());
        assert!(v["result"]["events"].is_array());
        teardown(a, e);
    }

    #[test]
    fn events_tail_requires_since() {
        let (a, e) = spawn_pair();
        let r = dispatch(&a, &e, &req("events.tail", json!({}), 1));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["error"]["code"], INVALID_REQUEST);
        teardown(a, e);
    }

    #[test]
    fn domain_create_without_path_or_inline_is_invalid() {
        let (a, e) = spawn_pair();
        let r = dispatch(&a, &e, &req("domain.create", json!({}), 1));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["error"]["code"], INVALID_REQUEST);
        teardown(a, e);
    }

    #[test]
    fn domain_create_with_missing_manifest_path_is_invalid() {
        let (a, e) = spawn_pair();
        let r = dispatch(&a, &e, &req(
            "domain.create",
            json!({"manifest_path": "/nonexistent/path/to/manifest.toml"}),
            1,
        ));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["error"]["code"], INVALID_REQUEST);
        teardown(a, e);
    }
}
