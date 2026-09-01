//! Read-only tool surface the AI may call while composing a plan (research 05 §4).
//!
//! Nine tool **definitions** (name / description / JSON-Schema parameters) plus the
//! [`ToolExecutor`] boundary the binary implements against the fact store. Every tool is
//! read-only, repo-root-sandboxed, and result-capped by its implementation; tool results
//! embed the exact `entity` JSON the model must echo back into plan nodes so everything it
//! cites is resolvable by the validator.
//!
//! The per-plan budget is [`MAX_TOOL_CALLS`]; [`AiService`](crate::AiService) enforces it.

use futures::future::BoxFuture;
use serde_json::{json, Value};

/// Hard budget of read-only tool calls per plan (research 05 §4: total ≤ 8 calls).
pub const MAX_TOOL_CALLS: u32 = 8;

/// Name of the required plan-submission tool (research 05 §5).
pub const PLAN_TOOL_NAME: &str = "submit_visualization_plan";

/// One tool definition in OpenAI tool-calling format.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ToolDef {
    /// Tool (function) name.
    pub name: &'static str,
    /// Model-facing description.
    pub description: String,
    /// JSON Schema of the arguments object.
    pub parameters: Value,
}

impl ToolDef {
    /// Render as an OpenAI `tools[]` entry: `{"type":"function","function":{...}}`.
    #[must_use]
    pub fn to_openai(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }
}

/// The nine read-only tool definitions (research 05 §4 table).
#[must_use]
pub fn read_only_tools() -> Vec<ToolDef> {
    let file_prop = json!({
        "type": "string",
        "description": "Repo-relative file path exactly as given in the digest or a previous tool result."
    });
    let symbol_prop = json!({
        "type": "string",
        "description": "Fully-qualified symbol name exactly as given in the digest or a previous tool result."
    });
    vec![
        ToolDef {
            name: "get_file_outline",
            description: "List the symbols of one file: name, kind, range, container (capped at 200). Results include the exact `entity` JSON to echo back in plan nodes.".into(),
            parameters: json!({
                "type": "object",
                "properties": {"file": file_prop},
                "required": ["file"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "get_symbol",
            description: "Get one symbol's signature, doc comment (first 20 lines), range and kind.".into(),
            parameters: json!({
                "type": "object",
                "properties": {"file": file_prop, "symbol": symbol_prop},
                "required": ["file", "symbol"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "get_hunk",
            description: "Read one diff hunk verbatim (capped at 200 lines), addressed by file and zero-based hunk index from the digest. Cite the same file and hunk index in plan evidence.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": file_prop,
                    "hunk_index": {"type": "integer", "minimum": 0,
                                    "description": "Zero-based index into the file's hunks, diff order."}
                },
                "required": ["file", "hunk_index"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "get_references",
            description: "Find reference sites of a symbol: file, range, one preview line each.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "symbol": symbol_prop,
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 20}
                },
                "required": ["symbol"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "get_callers",
            description: "Call hierarchy upward: who calls this symbol (tree of fully-qualified names).".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "symbol": symbol_prop,
                    "depth": {"type": "integer", "minimum": 1, "maximum": 2, "default": 1}
                },
                "required": ["symbol"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "get_callees",
            description: "Call hierarchy downward: what this symbol calls (tree of fully-qualified names).".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "symbol": symbol_prop,
                    "depth": {"type": "integer", "minimum": 1, "maximum": 2, "default": 1}
                },
                "required": ["symbol"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "get_implementations",
            description: "Implementations of an interface (or the interfaces a type implements), with file and range.".into(),
            parameters: json!({
                "type": "object",
                "properties": {"symbol": symbol_prop},
                "required": ["symbol"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "search_symbols",
            description: "Fuzzy workspace symbol search (capped at 20 matches).".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Fuzzy symbol name query."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 20, "default": 10}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "get_diagnostics",
            description: "Current compiler/linter diagnostics, optionally filtered to one file (capped at 50).".into(),
            parameters: json!({
                "type": "object",
                "properties": {"file": file_prop},
                "required": [],
                "additionalProperties": false
            }),
        },
    ]
}

/// `true` when `name` is one of the read-only tools (not the plan-submission tool).
#[must_use]
pub fn is_read_only_tool(name: &str) -> bool {
    read_only_tools().iter().any(|t| t.name == name)
}

/// A tool execution failure, reported back to the model as an error tool result (it never
/// aborts the plan request on its own).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ToolExecError(pub String);

impl ToolExecError {
    /// Convenience constructor.
    #[must_use]
    pub fn new(msg: impl Into<String>) -> Self {
        ToolExecError(msg.into())
    }
}

/// Execution boundary for the read-only tools, implemented by the binary against the fact
/// store (git + LSP caches). Implementations **must** be sandboxed:
///
/// - resolve paths repo-relative only; reject absolute paths and `..` escapes;
/// - serve results exclusively from the fact store / read-only git plumbing — never
///   mutate the repository or execute anything;
/// - cap result sizes per the research 05 §4 table;
/// - include the ready-to-echo `entity` JSON in results wherever entities appear.
///
/// `Ok` carries the tool result as a string (JSON recommended) that is sent back to the
/// model verbatim (after repo-root redaction by the service); `Err` is converted into an
/// error tool result so the model can recover — it does not abort the plan request.
///
/// The trait returns a [`BoxFuture`] (instead of `async fn`) so it stays dyn-compatible
/// for `&dyn ToolExecutor` wiring.
pub trait ToolExecutor: Send + Sync {
    /// Read-only tools this executor can actually serve.
    ///
    /// The service advertises only this set to the model. Implementations that use the
    /// complete production surface may keep the default; executors without a fact-store
    /// backend must return an empty list so automatic tool choice cannot create a futile
    /// request/failed-tool/request loop.
    fn available_tools(&self) -> Vec<ToolDef> {
        read_only_tools()
    }

    /// Execute one read-only tool call. `arguments` is the parsed JSON arguments object.
    fn execute<'a>(
        &'a self,
        name: &'a str,
        arguments: &'a Value,
    ) -> BoxFuture<'a, Result<String, ToolExecError>>;
}

/// A [`ToolExecutor`] that fails every call; useful when no fact store is wired (the
/// model is told tools are unavailable and should plan from the digest alone).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoToolExecutor;

impl ToolExecutor for NoToolExecutor {
    fn available_tools(&self) -> Vec<ToolDef> {
        Vec::new()
    }

    fn execute<'a>(
        &'a self,
        name: &'a str,
        _arguments: &'a Value,
    ) -> BoxFuture<'a, Result<String, ToolExecError>> {
        Box::pin(async move {
            Err(ToolExecError::new(format!(
                "tool {name} unavailable: no fact store wired; compose the plan from the digest"
            )))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nine_read_only_tools_with_expected_names() {
        let tools = read_only_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            [
                "get_file_outline",
                "get_symbol",
                "get_hunk",
                "get_references",
                "get_callers",
                "get_callees",
                "get_implementations",
                "search_symbols",
                "get_diagnostics",
            ]
        );
    }

    #[test]
    fn every_definition_is_an_object_schema() {
        for tool in read_only_tools() {
            assert_eq!(
                tool.parameters["type"], "object",
                "{} parameters must be an object schema",
                tool.name
            );
            assert!(
                tool.parameters["properties"].is_object(),
                "{} must declare properties",
                tool.name
            );
            assert!(
                tool.parameters["required"].is_array(),
                "{} must declare required",
                tool.name
            );
            assert!(!tool.description.is_empty());
        }
    }

    #[test]
    fn openai_projection_shape() {
        let t = &read_only_tools()[0];
        let v = t.to_openai();
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "get_file_outline");
        assert!(v["function"]["parameters"].is_object());
    }

    #[test]
    fn read_only_membership() {
        assert!(is_read_only_tool("get_hunk"));
        assert!(!is_read_only_tool(PLAN_TOOL_NAME));
        assert!(!is_read_only_tool("rm_rf"));
    }

    #[test]
    fn limits_match_research_table() {
        let tools = read_only_tools();
        let by_name = |n: &str| tools.iter().find(|t| t.name == n).unwrap();
        assert_eq!(
            by_name("get_references").parameters["properties"]["limit"]["maximum"],
            50
        );
        assert_eq!(
            by_name("search_symbols").parameters["properties"]["limit"]["maximum"],
            20
        );
        assert_eq!(
            by_name("get_callers").parameters["properties"]["depth"]["maximum"],
            2
        );
        assert_eq!(
            by_name("get_callees").parameters["properties"]["depth"]["maximum"],
            2
        );
        assert_eq!(MAX_TOOL_CALLS, 8);
    }

    #[tokio::test]
    async fn no_tool_executor_reports_unavailable() {
        let exec = NoToolExecutor;
        assert!(exec.available_tools().is_empty());
        let err = exec
            .execute("get_symbol", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.0.contains("unavailable"));
    }
}
