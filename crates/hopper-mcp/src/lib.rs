//! hopper's MCP server: the engine API for agents, as about thirteen typed tools over stdio.
//!
//! Transport: JSON-RPC 2.0, one message per line (MCP's stdio transport). The server answers
//! `initialize` (negotiating `2025-06-18` or `2025-11-25`: a supported client version is echoed,
//! anything else gets the newest), `ping`, `tools/list` and `tools/call`; notifications get no
//! answer.
//!
//! Every tool result is JSON (in `structuredContent`, and the same JSON as text for clients
//! without structured output). Time is always explicit: every result names the `moment` it
//! describes as a position URL (`hopper://<recording>/<frame_time>.<tid>.<ticks>.<sub>`), and
//! every tool that reads state takes an optional `moment` (default: the cursor). Queries never
//! move the cursor; only `seek`, `step` and `run_until` do. `origin` hops carry their ownership
//! layer (move, clone, borrow, ... with a confidence grade) beside the machine layer.
//!
//! Engine errors and bad arguments become tool results with `isError: true`, so a model can
//! read and correct them; only unknown tools and malformed requests are JSON-RPC errors.

mod tools;

use std::io::{self, BufRead, Write};
use std::sync::Arc;

use hopper_core::api::Debugger;
use serde_json::{Value, json};

pub use tools::tool_definitions;

/// The protocol versions this server speaks, newest last.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 2] = ["2025-06-18", "2025-11-25"];

const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Serve MCP over newline-delimited JSON-RPC until `reader` reaches end of input.
pub fn serve(
    debugger: Arc<dyn Debugger>,
    reader: impl BufRead,
    mut writer: impl Write,
) -> io::Result<()> {
    let server = Server { debugger };
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = server.handle_line(&line) {
            serde_json::to_writer(&mut writer, &reply)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
    }
    Ok(())
}

struct Server {
    debugger: Arc<dyn Debugger>,
}

fn error_reply(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

impl Server {
    /// Answer one line; `None` for notifications and client responses.
    fn handle_line(&self, line: &str) -> Option<Value> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(error_reply(
                    Value::Null,
                    PARSE_ERROR,
                    format!("parse error: {e}"),
                ));
            }
        };
        let Some(obj) = msg.as_object() else {
            return Some(error_reply(
                Value::Null,
                INVALID_REQUEST,
                "expected one JSON-RPC object",
            ));
        };
        let id = obj.get("id").cloned();
        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            // A response to a request we never send, or garbage: answer garbage that has an id.
            return match (id, obj.contains_key("result") || obj.contains_key("error")) {
                (Some(id), false) => Some(error_reply(id, INVALID_REQUEST, "missing method")),
                _ => None,
            };
        };
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            // Notifications (`notifications/initialized`, `notifications/cancelled`, ...).
            return None;
        };
        let result = match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tool_definitions()})),
            "tools/call" => self.call(&params),
            other => Err((METHOD_NOT_FOUND, format!("method not found: {other}"))),
        };
        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => error_reply(id, code, message),
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        let asked = params.get("protocolVersion").and_then(Value::as_str);
        let version = match asked {
            Some(v) if SUPPORTED_PROTOCOL_VERSIONS.contains(&v) => v,
            _ => LATEST_PROTOCOL_VERSION,
        };
        json!({
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {
                "name": "hopper",
                "title": "hopper time-travel debugger",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": "hopper debugs one rr recording. Every point in time is a position \
                URL (hopper://...). Tools that read state take an optional `moment` (default: \
                the cursor) and never move the cursor; seek, step and run_until move it. Use \
                `origin` to ask where a value came from: its hops are labelled with Rust \
                ownership events (move, clone, borrow) as well as machine copies.",
        })
    }

    fn call(&self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, "tools/call needs a tool name".to_string()))?;
        let args = match params.get("arguments") {
            None | Some(Value::Null) => serde_json::Map::new(),
            Some(Value::Object(m)) => m.clone(),
            Some(_) => {
                return Ok(tool_error("arguments must be an object".into()));
            }
        };
        let Some(outcome) = tools::call(self.debugger.as_ref(), name, &args) else {
            return Err((INVALID_PARAMS, format!("unknown tool: {name}")));
        };
        Ok(match outcome {
            Ok(value) => json!({
                "content": [{"type": "text", "text": value.to_string()}],
                "structuredContent": value,
                "isError": false,
            }),
            Err(e) => tool_error(e.to_string()),
        })
    }
}

fn tool_error(message: String) -> Value {
    json!({"content": [{"type": "text", "text": message}], "isError": true})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        Server {
            debugger: Arc::new(hopper_fake::FakeDebugger::new()),
        }
    }

    #[test]
    fn negotiates_protocol_versions() {
        let s = server();
        for v in SUPPORTED_PROTOCOL_VERSIONS {
            let line = json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                "params":{"protocolVersion": v, "capabilities":{}, "clientInfo":{"name":"t","version":"0"}}});
            let r = s.handle_line(&line.to_string()).unwrap();
            assert_eq!(r["result"]["protocolVersion"], v);
        }
        let line = json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2024-11-05"}});
        let r = s.handle_line(&line.to_string()).unwrap();
        assert_eq!(r["result"]["protocolVersion"], LATEST_PROTOCOL_VERSION);
        assert!(r["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn protocol_errors() {
        let s = server();
        assert_eq!(
            s.handle_line("{nope").unwrap()["error"]["code"],
            PARSE_ERROR
        );
        assert_eq!(
            s.handle_line("[1]").unwrap()["error"]["code"],
            INVALID_REQUEST
        );
        let r = s
            .handle_line(r#"{"jsonrpc":"2.0","id":3,"method":"bogus"}"#)
            .unwrap();
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
        assert!(
            s.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .is_none()
        );
        assert!(
            s.handle_line(r#"{"jsonrpc":"2.0","id":9,"result":{}}"#)
                .is_none()
        );
        let r = s
            .handle_line(
                r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope"}}"#,
            )
            .unwrap();
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        let r = s
            .handle_line(r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#)
            .unwrap();
        assert_eq!(r["id"], "p");
        assert_eq!(r["result"], json!({}));
    }
}
