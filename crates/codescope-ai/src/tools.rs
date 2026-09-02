//! Research and incremental diagram tools the AI may call while composing a plan.
//!
//! Tool **definitions** (name / description / JSON-Schema parameters) plus the
//! [`ToolExecutor`] boundary the binary implements against the fact store. The semantic
//! Fact tools expose language-server facts; research tools provide a deliberately small,
//! bash-like view of the selected diff. Those tools are read-only, repo-root-sandboxed, and
//! result-capped. Diagram tools mutate only a bounded in-memory [`codescope_core::DiagramDraft`].
//!
//! The per-plan budget is [`MAX_TOOL_CALLS`]; [`AiService`](crate::AiService) enforces it.

use futures::future::BoxFuture;
use serde_json::{json, Value};

/// Hard budget of research and diagram-edit tool calls per plan. Incremental construction
/// needs room to inspect, create, revise, and validate without turning one missed detail
/// into a terminal failure.
pub const MAX_TOOL_CALLS: u32 = 48;

/// Mutate the in-progress renderer-native diagram with one [`codescope_core::DiagramCommand`].
pub const DIAGRAM_EDIT_TOOL_NAME: &str = "edit_visualization";
/// Read the complete in-progress diagram draft.
pub const DIAGRAM_INSPECT_TOOL_NAME: &str = "inspect_visualization";

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

/// Bash-like research tools used by the interactive diff summarizer.
///
/// Directory paths are relative to the executor's virtual working directory. File tools also
/// accept exact or uniquely identifiable repo-relative paths. Implementations decide the exact
/// selection boundary and return canonical repo-relative paths in their results.
#[must_use]
pub fn research_tools() -> Vec<ToolDef> {
    let path_prop = json!({
        "type": "string",
        "description": "Path relative to the virtual current working directory. Absolute paths and parent traversal are forbidden."
    });
    let file_path_prop = json!({
        "type": "string",
        "description": "Changed-file path. Accepts a virtual-cwd-relative path, an exact repo_path returned by a tool, or an unambiguous repo-path suffix. Absolute paths and parent traversal are forbidden."
    });
    vec![
        ToolDef {
            name: "list_directory",
            description: "List changed files and child directories at a path inside the current selection (like a bounded `ls`). Use `.` for the virtual cwd.".into(),
            parameters: json!({
                "type": "object",
                "properties": {"path": path_prop},
                "required": [],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "read_file",
            description: "Read a numbered section of a changed file from the current worktree (like `sed -n`). Returns at most 200 lines and may be unavailable for deleted files.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": file_path_prop,
                    "start_line": {"type": "integer", "minimum": 1, "default": 1,
                                   "description": "One-based first line."},
                    "end_line": {"type": "integer", "minimum": 1,
                                 "description": "One-based inclusive final line; capped to 200 returned lines."}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "search_changed_files",
            description: "Literal text search across readable changed files under a path (like a bounded `rg`). Returns at most 50 numbered matches.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "minLength": 1,
                              "description": "Literal, case-sensitive search text."},
                    "path": path_prop,
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 20}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "git_status_file",
            description: "Show the captured Git status for one changed file: comparison scope, status, rename source, binary flag, line counts, and every hunk header.".into(),
            parameters: json!({
                "type": "object",
                "properties": {"path": file_path_prop},
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "git_diff_file",
            description: "Read the captured unified diff for one changed file. Omit hunk_index for a bounded overview or supply a zero-based hunk index for exact annotated lines used by plan code_refs.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": file_path_prop,
                    "hunk_index": {"type": "integer", "minimum": 0,
                                   "description": "Optional zero-based hunk index from git_status_file."}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
    ]
}

/// Incremental diagram tools. `edit_visualization` accepts the same tagged command JSON as
/// the live `codescope agent diagram apply` controller endpoint.
#[must_use]
pub fn diagram_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: DIAGRAM_EDIT_TOOL_NAME,
            description: "Apply one atomic renderer-native diagram command. Build the answer incrementally: set intent, create a form, create/update/delete boxes and edges, then add exact evidence. The `op` and remaining arguments are exactly the same JSON accepted by `codescope agent diagram apply`. Use inspect_visualization whenever you need the current ids/state.".into(),
            parameters: json!({
                "type": "object",
                "description": "One DiagramCommand. Required fields depend on op. Nodes are complete PlanNode objects; edges are complete PlanEdge objects. Update patches contain only replacement fields plus clear_* booleans.",
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["reset", "set_intent", "create_form", "delete_form",
                                 "create_node", "update_node", "delete_node", "create_edge",
                                 "update_edge", "delete_edge", "add_evidence", "delete_evidence"],
                        "description": "Atomic edit operation."
                    },
                    "intent": {"type": "string", "description": "set_intent: the one concrete sentence displayed above the diagram."},
                    "form_id": {"type": "string", "description": "Stable editor id such as main; not displayed."},
                    "kind": {"type": "string", "enum": ["changed_symbol_tree", "call_tree", "type_impl_tree", "relationship_flow", "before_after", "sequence"]},
                    "node_id": {"type": "string", "description": "Existing plan-local node id for update/delete."},
                    "node": {
                        "type": "object",
                        "description": "create_node: complete box. Required: id, label, detail, code_refs. Optional: entity, expanded_detail, change, severity, children, hint. code_refs contain file, zero-based hunk, old|new side, and one-based start_line/end_line.",
                        "properties": {
                            "id": {"type": "string"},
                            "entity": {"type": "object"},
                            "label": {"type": "string"},
                            "detail": {"type": "string"},
                            "expanded_detail": {"type": "string"},
                            "code_refs": {"type": "array", "minItems": 1, "maxItems": 2, "items": {"type": "object"}},
                            "change": {"type": "string", "enum": ["added", "modified", "removed", "unchanged", "diagnostic"]},
                            "severity": {"type": "string"},
                            "children": {"type": "array", "items": {"type": "string"}},
                            "hint": {"type": "object"}
                        },
                        "required": ["id", "label", "detail", "code_refs"],
                        "additionalProperties": false
                    },
                    "patch": {"type": "object", "description": "update_node or update_edge replacement fields. Node patches support label, detail, expanded_detail, entity, code_refs, change, severity, children, hint and clear_detail/clear_expanded_detail/clear_entity/clear_severity. Edge patches support from, to, kind, label, clear_label."},
                    "edge": {
                        "type": "object",
                        "description": "create_edge: directed relationship with from, to, kind, and a specific label.",
                        "properties": {
                            "from": {"type": "string"},
                            "to": {"type": "string"},
                            "kind": {"type": "string", "enum": ["calls", "imports", "implements", "contains", "reads", "writes"]},
                            "label": {"type": "string"}
                        },
                        "required": ["from", "to", "kind", "label"],
                        "additionalProperties": false
                    },
                    "from": {"type": "string", "description": "Existing edge source for update/delete."},
                    "to": {"type": "string", "description": "Existing edge target for update/delete."},
                    "evidence": {"type": "object", "description": "add_evidence: exact citation with file, optional zero-based hunk/symbol/range, and reason."},
                    "index": {"type": "integer", "minimum": 0, "description": "delete_evidence: current zero-based index."}
                },
                "required": ["op"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: DIAGRAM_INSPECT_TOOL_NAME,
            description: "Return the entire current diagram draft, including stable form ids, boxes, relationships, intent, and evidence. Use before targeted edits when ids or current text are uncertain.".into(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        },
    ]
}

/// `true` when `name` is one of the read-only research tools.
#[must_use]
pub fn is_read_only_tool(name: &str) -> bool {
    read_only_tools()
        .into_iter()
        .chain(research_tools())
        .any(|tool| tool.name == name)
}

/// `true` when `name` is part of the shared incremental diagram API.
#[must_use]
pub fn is_diagram_tool(name: &str) -> bool {
    matches!(name, DIAGRAM_EDIT_TOOL_NAME | DIAGRAM_INSPECT_TOOL_NAME)
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

    /// Whether this executor represents a research workflow that must inspect at least one
    /// fact before a diagram can be completed. This is `false` for minimal fact stores and tests;
    /// the scoped diff executor enables it so the small initial brief cannot be guessed from.
    fn requires_research(&self) -> bool {
        false
    }

    /// Execute one read-only tool call. `arguments` is the parsed JSON arguments object.
    fn execute<'a>(
        &'a self,
        name: &'a str,
        arguments: &'a Value,
    ) -> BoxFuture<'a, Result<String, ToolExecError>>;
}

/// A [`ToolExecutor`] that fails every research call; the service still supplies its own
/// incremental diagram editor, so the model can build from the brief alone.
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
                "tool {name} unavailable: no fact store wired; compose the diagram from the brief"
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
    fn five_scoped_research_tools_with_expected_names() {
        let names: Vec<&str> = research_tools().iter().map(|tool| tool.name).collect();
        assert_eq!(
            names,
            [
                "list_directory",
                "read_file",
                "search_changed_files",
                "git_status_file",
                "git_diff_file",
            ]
        );
    }

    #[test]
    fn every_definition_is_an_object_schema() {
        for tool in read_only_tools()
            .into_iter()
            .chain(research_tools())
            .chain(diagram_tools())
        {
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
        assert!(is_read_only_tool("read_file"));
        assert!(is_read_only_tool("git_status_file"));
        assert!(!is_read_only_tool("rm_rf"));
        assert!(is_diagram_tool(DIAGRAM_EDIT_TOOL_NAME));
        assert!(is_diagram_tool(DIAGRAM_INSPECT_TOOL_NAME));
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
        assert_eq!(MAX_TOOL_CALLS, 48);
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
