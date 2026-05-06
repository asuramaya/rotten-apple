//! MCP JSON-RPC 2.0 wire types.
//!
//! Subset sufficient for Claude Code: `initialize`, `notifications/initialized`,
//! `tools/list`, `tools/call`. Params and results stay as `serde_json::Value`
//! so we don't strongly type every method variant.

use serde::{Deserialize, Serialize};

pub const JSONRPC_VERSION: &str = "2.0";
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
pub const SERVER_NAME:    &str = "rotten-apple";
pub const SERVER_VERSION: &str = "0.0.2";

// JSON-RPC 2.0 reserved error codes.
pub const PARSE_ERROR:      i32 = -32700;
pub const INVALID_REQUEST:  i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS:   i32 = -32602;
pub const INTERNAL_ERROR:   i32 = -32603;

#[derive(Debug, Deserialize)]
pub struct McpRequest {
    pub jsonrpc: String,
    pub method:  String,
    #[serde(default)]
    pub params:  serde_json::Value,
    /// Notifications omit `id`; requests carry it.
    #[serde(default)]
    pub id:      Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum McpResponse {
    Ok  { jsonrpc: String, result: serde_json::Value, id: serde_json::Value },
    Err { jsonrpc: String, error:  McpError,          id: serde_json::Value },
}

#[derive(Debug, Serialize)]
pub struct McpError {
    pub code:    i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data:    Option<serde_json::Value>,
}

impl McpResponse {
    pub fn ok(result: serde_json::Value, id: serde_json::Value) -> Self {
        McpResponse::Ok { jsonrpc: JSONRPC_VERSION.into(), result, id }
    }

    pub fn err(
        code: i32,
        message: impl Into<String>,
        data: Option<serde_json::Value>,
        id: serde_json::Value,
    ) -> Self {
        McpResponse::Err {
            jsonrpc: JSONRPC_VERSION.into(),
            error: McpError { code, message: message.into(), data },
            id,
        }
    }
}

/// MCP tool descriptor returned by `tools/list`.
#[derive(Debug, Serialize)]
pub struct Tool {
    pub name:        &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_round_trip() {
        let raw = r#"{"jsonrpc":"2.0","method":"tools/list","params":{},"id":3}"#;
        let req: McpRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.method, "tools/list");
        assert_eq!(req.id, Some(json!(3)));
    }

    #[test]
    fn notification_has_no_id() {
        let raw = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let req: McpRequest = serde_json::from_str(raw).unwrap();
        assert!(req.id.is_none());
    }

    #[test]
    fn response_ok_serializes() {
        let r = McpResponse::ok(json!({"ok": true}), json!(1));
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["result"]["ok"], true);
        assert_eq!(v["id"], 1);
        assert!(v.get("error").is_none());
    }

    #[test]
    fn response_err_omits_data_when_absent() {
        let r = McpResponse::err(METHOD_NOT_FOUND, "Method not found", None, json!(2));
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["error"]["code"], METHOD_NOT_FOUND);
        assert!(v["error"].get("data").is_none());
    }
}
