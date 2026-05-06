//! JSON-RPC 2.0 wire types for orchestratord.
//!
//! Scaffold only: the dispatcher in `lib.rs` answers `hello` and `ping`
//! and returns Method-Not-Found for everything else. Real method routing
//! lands when the libxl actor lands.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: &str = "0.1";
pub const SERVER_NAME:      &str = "orchestratord";
pub const SERVER_VERSION:   &str = "0.0.2";

// JSON-RPC 2.0 reserved range + our protocol-mismatch / backend errors in
// the -32000..-32099 implementation-defined band.
pub const PARSE_ERROR:        i32 = -32700;
pub const INVALID_REQUEST:    i32 = -32600;
pub const METHOD_NOT_FOUND:   i32 = -32601;
pub const PROTOCOL_MISMATCH:  i32 = -32000;
// Backend / actor errors. Stable wire contract — clients switch on these.
// `data` carries `{ detail: "..." }` with the libxl diagnostic when known.
pub const BACKEND_UNAVAILABLE:    i32 = -32001;
pub const BACKEND_INTERNAL:       i32 = -32002;
pub const PERMISSION_DENIED:      i32 = -32003;
pub const GUEST_NOT_FOUND:        i32 = -32004;
pub const GUEST_ALREADY_RUNNING:  i32 = -32005;
pub const INSUFFICIENT_RESOURCES: i32 = -32006;
pub const HARDWARE_UNAVAILABLE:   i32 = -32007;

#[derive(Debug, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub method:  String,
    #[serde(default)]
    pub params:  serde_json::Value,
    pub id:      serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Response {
    Ok  { jsonrpc: String, result: serde_json::Value, id: serde_json::Value },
    Err { jsonrpc: String, error:  ErrorObject,       id: serde_json::Value },
}

#[derive(Debug, Serialize)]
pub struct ErrorObject {
    pub code:    i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data:    Option<serde_json::Value>,
}

impl Response {
    pub fn ok(result: serde_json::Value, id: serde_json::Value) -> Self {
        Response::Ok { jsonrpc: "2.0".into(), result, id }
    }

    pub fn err(
        code: i32,
        message: impl Into<String>,
        data: Option<serde_json::Value>,
        id: serde_json::Value,
    ) -> Self {
        Response::Err {
            jsonrpc: "2.0".into(),
            error: ErrorObject { code, message: message.into(), data },
            id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_round_trip() {
        let raw = r#"{"jsonrpc":"2.0","method":"ping","params":{},"id":7}"#;
        let req: Request = serde_json::from_str(raw).unwrap();
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.method, "ping");
        assert_eq!(req.id, json!(7));
    }

    #[test]
    fn request_default_params() {
        // params omitted is allowed because of #[serde(default)]
        let raw = r#"{"jsonrpc":"2.0","method":"ping","id":1}"#;
        let req: Request = serde_json::from_str(raw).unwrap();
        assert!(req.params.is_null());
    }

    #[test]
    fn response_ok_serializes() {
        let r = Response::ok(json!({"pong": true}), json!(1));
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["result"]["pong"], true);
        assert_eq!(v["id"], 1);
        assert!(v.get("error").is_none());
    }

    #[test]
    fn response_err_serializes_and_omits_data() {
        let r = Response::err(METHOD_NOT_FOUND, "Method not found", None, json!(2));
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(v["error"]["message"], "Method not found");
        assert!(v["error"].get("data").is_none());
        assert_eq!(v["id"], 2);
    }

    #[test]
    fn response_err_with_data_includes_it() {
        let r = Response::err(
            METHOD_NOT_FOUND,
            "Method not found",
            Some(json!({"method": "frobnicate"})),
            json!(3),
        );
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error"]["data"]["method"], "frobnicate");
    }
}
