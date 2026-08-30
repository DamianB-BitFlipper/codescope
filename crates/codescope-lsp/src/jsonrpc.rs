//! JSON-RPC 2.0 message model for the LSP wire.
//!
//! The transport reads raw frame bodies ([`crate::framing`]) and classifies
//! them here into the three inbound shapes: a response to one of our requests,
//! a server notification, or a server→client request (which needs a reply).
//! Anything else is malformed and reported as such (logged + dropped by the
//! caller, never fatal).

use serde_json::Value;

/// Request ids we emit are integers; servers echo them back either as numbers
/// or (never seen in practice, but legal) strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RequestId {
    /// Numeric id (what we send).
    Number(i64),
    /// String id (tolerated on receive).
    String(String),
}

impl RequestId {
    fn from_value(v: &Value) -> Option<RequestId> {
        match v {
            Value::Number(n) => n.as_i64().map(RequestId::Number),
            Value::String(s) => Some(RequestId::String(s.clone())),
            _ => None,
        }
    }

    /// JSON form of the id (for echoing back in replies).
    #[must_use]
    pub fn to_value(&self) -> Value {
        match self {
            RequestId::Number(n) => Value::from(*n),
            RequestId::String(s) => Value::from(s.clone()),
        }
    }
}

/// JSON-RPC error object from a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseError {
    /// Error code (`-32601` = method not found, …).
    pub code: i64,
    /// Human-readable message.
    pub message: String,
}

/// One classified inbound message.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// A response to a request we sent.
    Response {
        /// Echoed request id.
        id: RequestId,
        /// `Ok(result)` or `Err(error object)`.
        result: Result<Value, ResponseError>,
    },
    /// A server notification (no id).
    Notification {
        /// Method name, e.g. `textDocument/publishDiagnostics`.
        method: String,
        /// `params` (or `Value::Null` when absent).
        params: Value,
    },
    /// A server→client request (has both `method` and `id`); must be answered.
    ServerRequest {
        /// Request id to echo in the reply.
        id: RequestId,
        /// Method name, e.g. `workspace/configuration`.
        method: String,
        /// `params` (or `Value::Null` when absent).
        params: Value,
    },
}

/// Classify one parsed JSON body. Returns `Err(reason)` for shapes that are
/// not valid JSON-RPC traffic.
pub fn classify(value: Value) -> Result<Incoming, String> {
    let Value::Object(mut obj) = value else {
        return Err("message body is not a JSON object".to_string());
    };

    let id = obj.get("id").and_then(RequestId::from_value);
    let method = obj.get("method").and_then(Value::as_str).map(String::from);

    match (id, method) {
        (Some(id), Some(method)) => {
            let params = obj.remove("params").unwrap_or(Value::Null);
            Ok(Incoming::ServerRequest { id, method, params })
        }
        (None, Some(method)) => {
            let params = obj.remove("params").unwrap_or(Value::Null);
            Ok(Incoming::Notification { method, params })
        }
        (Some(id), None) => {
            if let Some(err) = obj.get("error") {
                let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
                let message = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("<no message>")
                    .to_string();
                Ok(Incoming::Response {
                    id,
                    result: Err(ResponseError { code, message }),
                })
            } else if let Some(result) = obj.remove("result") {
                Ok(Incoming::Response {
                    id,
                    result: Ok(result),
                })
            } else {
                Err("response carries neither result nor error".to_string())
            }
        }
        (None, None) => Err("message has neither id nor method".to_string()),
    }
}

/// Serialize an outbound request.
#[must_use]
pub fn request(id: i64, method: &str, params: &Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

/// Serialize an outbound notification.
#[must_use]
pub fn notification(method: &str, params: &Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    })
}

/// Serialize a success reply to a server→client request.
#[must_use]
pub fn response_ok(id: &RequestId, result: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id.to_value(),
        "result": result,
    })
}

/// Serialize an error reply to a server→client request.
#[must_use]
pub fn response_err(id: &RequestId, code: i64, message: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id.to_value(),
        "error": { "code": code, "message": message },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_success_response() {
        let msg = classify(json!({"jsonrpc":"2.0","id":3,"result":{"ok":true}})).unwrap();
        assert_eq!(
            msg,
            Incoming::Response {
                id: RequestId::Number(3),
                result: Ok(json!({"ok":true})),
            }
        );
    }

    #[test]
    fn classifies_null_result_response() {
        // `"result": null` is a valid success (e.g. shutdown).
        let msg = classify(json!({"jsonrpc":"2.0","id":4,"result":null})).unwrap();
        assert_eq!(
            msg,
            Incoming::Response {
                id: RequestId::Number(4),
                result: Ok(Value::Null),
            }
        );
    }

    #[test]
    fn classifies_error_response() {
        let msg =
            classify(json!({"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"not found"}}))
                .unwrap();
        assert_eq!(
            msg,
            Incoming::Response {
                id: RequestId::Number(7),
                result: Err(ResponseError {
                    code: -32601,
                    message: "not found".to_string(),
                }),
            }
        );
    }

    #[test]
    fn classifies_notification_and_server_request() {
        let n = classify(json!({"jsonrpc":"2.0","method":"$/progress","params":{"a":1}})).unwrap();
        assert_eq!(
            n,
            Incoming::Notification {
                method: "$/progress".to_string(),
                params: json!({"a":1}),
            }
        );
        let r = classify(json!({"jsonrpc":"2.0","id":"s-1","method":"workspace/configuration"}))
            .unwrap();
        assert_eq!(
            r,
            Incoming::ServerRequest {
                id: RequestId::String("s-1".to_string()),
                method: "workspace/configuration".to_string(),
                params: Value::Null,
            }
        );
    }

    #[test]
    fn rejects_garbage_shapes() {
        assert!(classify(json!("hi")).is_err());
        assert!(classify(json!({"jsonrpc":"2.0"})).is_err());
        assert!(classify(json!({"jsonrpc":"2.0","id":1})).is_err());
    }

    #[test]
    fn builders_emit_expected_shapes() {
        assert_eq!(
            request(1, "initialize", &json!({"a":1})),
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"a":1}})
        );
        assert_eq!(
            notification("exit", &Value::Null),
            json!({"jsonrpc":"2.0","method":"exit","params":null})
        );
        let id = RequestId::Number(9);
        assert_eq!(
            response_ok(&id, json!([null])),
            json!({"jsonrpc":"2.0","id":9,"result":[null]})
        );
        assert_eq!(
            response_err(&id, -32601, "nope"),
            json!({"jsonrpc":"2.0","id":9,"error":{"code":-32601,"message":"nope"}})
        );
    }
}
