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
/// Inspect language-server facts rooted at the current changed-file selection.
pub const LSP_INSPECT_TOOL_NAME: &str = "inspect_language_server";

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

/// Language-neutral semantic research available from the active language-server adapter.
///
/// One operation-discriminated tool keeps the provider prompt compact while allowing new
/// adapters and semantic query kinds to be added without multiplying top-level tools.
#[must_use]
pub fn semantic_tools() -> Vec<ToolDef> {
    vec![ToolDef {
        name: LSP_INSPECT_TOOL_NAME,
        description: "Explore bounded, read-only language-server facts for the current selection. Use capabilities to discover query names supported by the active adapter. Inspection can be anchored by the current Codescope selection, an exact symbol copied from a symbols result, or an explicit source position. Standard queries include symbols, references, callers, callees, implementations, supertypes, subtypes, diagnostics, hover, and semantic_tokens; future adapters may advertise more without changing this tool. Results identify worktree revision, epoch, completeness, truncation, and unavailable/unsupported states. Paths follow the same virtual-cwd rules as the Git tools."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 64,
                    "description": "Capability-discoverable semantic fact family. Start with capabilities rather than assuming a fixed query set."
                },
                "path": {
                    "type": "string",
                    "description": "Optional selected changed-file path, relative to the virtual cwd or copied from repo_path. Required for symbols and when there is no current file/symbol selection."
                },
                "symbol": {
                    "type": "string",
                    "description": "Optional exact symbol name copied from a symbols result."
                },
                "line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional one-based source line for position-oriented inspection."
                },
                "column": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Optional zero-based UTF-8 byte column; defaults to 0 when line is supplied."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50,
                    "default": 20,
                    "description": "Maximum returned facts; byte limits may truncate earlier."
                }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
    }]
}

/// Incremental diagram tools. `edit_visualization` accepts the same tagged command JSON as
/// the live `codescope agent diagram apply` controller endpoint.
#[must_use]
pub fn diagram_tools() -> Vec<ToolDef> {
    let form_id = || {
        json!({
            "type": "string",
            "minLength": 1,
            "maxLength": 128,
            "description": "Existing stable form id, normally `main`. Create it first with create_form."
        })
    };
    let node_id = || {
        json!({
            "type": "string",
            "minLength": 1,
            "maxLength": 128,
            "description": "Plan-local box id such as `n1`."
        })
    };
    let edge_endpoint = || {
        json!({
            "type": "string",
            "minLength": 1,
            "maxLength": 128,
            "description": "Existing node id in the specified form."
        })
    };

    let variants = vec![
        diagram_command_variant(
            "reset",
            "Clear the draft while preserving the server-owned epoch.",
            Vec::new(),
            &[],
            json!({"op": "reset"}),
        ),
        diagram_command_variant(
            "set_intent",
            "Set the single reviewer-facing sentence displayed above the diagram.",
            vec![(
                "intent",
                json!({
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 1000,
                    "description": "One concrete sentence describing the behavior shown by the diagram."
                }),
            )],
            &["intent"],
            json!({
                "op": "set_intent",
                "intent": "Start the API service and route requests through its initialized dependencies."
            }),
        ),
        diagram_command_variant(
            "create_form",
            "Create an empty renderer-native form before adding its nodes.",
            vec![
                (
                    "form_id",
                    json!({
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 128,
                        "description": "New stable form id, normally `main`."
                    }),
                ),
                ("kind", diagram_form_kind_schema()),
            ],
            &["form_id", "kind"],
            json!({"op": "create_form", "form_id": "main", "kind": "sequence"}),
        ),
        diagram_command_variant(
            "delete_form",
            "Delete one existing form and all of its nodes and edges.",
            vec![("form_id", form_id())],
            &["form_id"],
            json!({"op": "delete_form", "form_id": "main"}),
        ),
        diagram_command_variant(
            "create_node",
            "Create one complete box in an existing form. `change` is a string enum; `hint.highlight` and `hint.collapsed` are booleans.",
            vec![("form_id", form_id()), ("node", diagram_node_schema())],
            &["form_id", "node"],
            json!({
                "op": "create_node",
                "form_id": "main",
                "node": {
                    "id": "n1",
                    "label": "Start API service",
                    "detail": "Constructs and starts the HTTP server",
                    "code_refs": [{
                        "file": "cmd/api/main.go",
                        "hunk": 0,
                        "side": "new",
                        "start_line": 18,
                        "end_line": 22
                    }],
                    "change": "added",
                    "hint": {"highlight": true, "collapsed": false}
                }
            }),
        ),
        diagram_command_variant(
            "update_node",
            "Replace or explicitly clear selected fields on an existing box.",
            vec![
                ("form_id", form_id()),
                ("node_id", node_id()),
                ("patch", diagram_node_patch_schema()),
            ],
            &["form_id", "node_id", "patch"],
            json!({
                "op": "update_node",
                "form_id": "main",
                "node_id": "n1",
                "patch": {"detail": "Starts the configured HTTP listener", "clear_expanded_detail": true}
            }),
        ),
        diagram_command_variant(
            "delete_node",
            "Delete one existing box, its edges, and child references.",
            vec![("form_id", form_id()), ("node_id", node_id())],
            &["form_id", "node_id"],
            json!({"op": "delete_node", "form_id": "main", "node_id": "n1"}),
        ),
        diagram_command_variant(
            "create_edge",
            "Create one labeled, directed relationship between existing boxes.",
            vec![("form_id", form_id()), ("edge", diagram_edge_schema())],
            &["form_id", "edge"],
            json!({
                "op": "create_edge",
                "form_id": "main",
                "edge": {"from": "n1", "to": "n2", "kind": "calls", "label": "starts listener"}
            }),
        ),
        diagram_command_variant(
            "update_edge",
            "Patch an existing relationship selected by its current source and target ids.",
            vec![
                ("form_id", form_id()),
                ("from", edge_endpoint()),
                ("to", edge_endpoint()),
                ("patch", diagram_edge_patch_schema()),
            ],
            &["form_id", "from", "to", "patch"],
            json!({
                "op": "update_edge",
                "form_id": "main",
                "from": "n1",
                "to": "n2",
                "patch": {"label": "passes initialized service"}
            }),
        ),
        diagram_command_variant(
            "delete_edge",
            "Delete an existing relationship selected by its current source and target ids.",
            vec![
                ("form_id", form_id()),
                ("from", edge_endpoint()),
                ("to", edge_endpoint()),
            ],
            &["form_id", "from", "to"],
            json!({"op": "delete_edge", "form_id": "main", "from": "n1", "to": "n2"}),
        ),
        diagram_command_variant(
            "add_evidence",
            "Append one exact source citation supporting claims in the diagram.",
            vec![("evidence", diagram_evidence_schema())],
            &["evidence"],
            json!({
                "op": "add_evidence",
                "evidence": {
                    "file": "cmd/api/main.go",
                    "hunk": 0,
                    "reason": "The added lines construct and start the API server."
                }
            }),
        ),
        diagram_command_variant(
            "delete_evidence",
            "Delete one evidence item by the current zero-based index returned by inspect_visualization.",
            vec![(
                "index",
                json!({
                    "type": "integer",
                    "minimum": 0,
                    "description": "Current zero-based evidence index."
                }),
            )],
            &["index"],
            json!({"op": "delete_evidence", "index": 0}),
        ),
    ];

    vec![
        ToolDef {
            name: DIAGRAM_EDIT_TOOL_NAME,
            description: "Apply exactly one atomic diagram command. Choose the operation-specific schema branch matching `op`; every branch lists its complete required fields, nested types, and a valid example. Build in this order: set_intent, create_form, create_node, create_edge, add_evidence. For create_node, pass `form_id` beside `node`; use `change: \"added\"|...` and only booleans inside `hint`. The arguments are the same JSON accepted by `codescope agent diagram apply`. Use inspect_visualization when ids or current state are uncertain.".into(),
            parameters: json!({
                "type": "object",
                "description": "A discriminated union of DiagramCommand objects. Exactly one `oneOf` branch must match the selected `op`.",
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["reset", "set_intent", "create_form", "delete_form",
                                 "create_node", "update_node", "delete_node", "create_edge",
                                 "update_edge", "delete_edge", "add_evidence", "delete_evidence"],
                        "description": "Atomic edit operation."
                    }
                },
                "required": ["op"],
                "oneOf": variants
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

fn diagram_command_variant(
    op: &str,
    description: &str,
    fields: Vec<(&str, Value)>,
    required_fields: &[&str],
    example: Value,
) -> Value {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "op".to_string(),
        json!({
            "const": op,
            "description": format!("Selects the `{op}` command.")
        }),
    );
    for (name, schema) in fields {
        properties.insert(name.to_string(), schema);
    }
    let mut required = vec!["op"];
    required.extend_from_slice(required_fields);
    json!({
        "title": op,
        "description": description,
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
        "examples": [example]
    })
}

fn diagram_form_kind_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["changed_symbol_tree", "call_tree", "type_impl_tree", "relationship_flow", "before_after", "sequence"],
        "description": "Renderer-owned layout grammar. Use sequence for chronological steps, relationship_flow for topology, a tree kind for hierarchy, and before_after only for two literal states."
    })
}

fn diagram_line_range_schema() -> Value {
    json!({
        "type": "object",
        "description": "Exact zero-based UTF-8 source range copied from a semantic tool result.",
        "properties": {
            "start_line": {"type": "integer", "minimum": 0},
            "start_col": {"type": "integer", "minimum": 0},
            "end_line": {"type": "integer", "minimum": 0},
            "end_col": {"type": "integer", "minimum": 0}
        },
        "required": ["start_line", "start_col", "end_line", "end_col"],
        "additionalProperties": false
    })
}

fn diagram_entity_schema() -> Value {
    json!({
        "type": "object",
        "description": "Optional exact fact-store identity. Omit this entire object for a presentational node; never invent symbol or range values.",
        "properties": {
            "file": {"type": "string", "minLength": 1, "description": "Exact repo-relative file returned by a research tool."},
            "symbol": {"type": "string", "minLength": 1, "description": "Exact symbol name returned by a semantic tool."},
            "range": diagram_line_range_schema()
        },
        "required": ["file"],
        "additionalProperties": false
    })
}

fn diagram_code_ref_schema() -> Value {
    json!({
        "type": "object",
        "description": "Exact changed-line citation copied from git_diff_file. Hunk is zero-based; source lines are one-based and inclusive.",
        "properties": {
            "file": {"type": "string", "minLength": 1, "description": "Exact repo_path from git_diff_file."},
            "hunk": {"type": "integer", "minimum": 0, "description": "Zero-based hunk index."},
            "side": {"type": "string", "enum": ["old", "new"], "description": "Use old for removed lines and new for added/post-change lines."},
            "start_line": {"type": "integer", "minimum": 1, "description": "First one-based source line, inclusive."},
            "end_line": {"type": "integer", "minimum": 1, "description": "Last one-based source line, inclusive."}
        },
        "required": ["file", "hunk", "side", "start_line", "end_line"],
        "additionalProperties": false
    })
}

fn diagram_hint_schema() -> Value {
    json!({
        "type": "object",
        "description": "Presentation flags only. Both values are booleans; change badges such as `added` or `removed` belong in node.change, never here.",
        "properties": {
            "highlight": {"type": "boolean", "description": "Visually emphasize this box."},
            "collapsed": {"type": "boolean", "description": "Initially render this box collapsed."}
        },
        "additionalProperties": false
    })
}

fn diagram_node_schema() -> Value {
    json!({
        "type": "object",
        "description": "One complete renderer box. All four required fields must be present. `change` is a string; values inside `hint` are booleans.",
        "properties": {
            "id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "New plan-local id such as n1."},
            "entity": diagram_entity_schema(),
            "label": {"type": "string", "minLength": 1, "maxLength": 512, "description": "Short identifier or action displayed as the box title."},
            "detail": {"type": "string", "minLength": 1, "maxLength": 2000, "description": "Required concrete reviewer-facing preview; keep it to at most 8 words and 56 characters."},
            "expanded_detail": {"type": "string", "minLength": 1, "maxLength": 4000, "description": "Optional self-contained deeper explanation shown in the box inspector."},
            "code_refs": {"type": "array", "minItems": 1, "maxItems": 2, "items": diagram_code_ref_schema(), "description": "One or two exact changed-line references."},
            "change": {"type": "string", "enum": ["added", "modified", "removed", "unchanged", "diagnostic"], "description": "Optional change badge string."},
            "severity": {"type": "string", "enum": ["error", "warning", "information", "hint"], "description": "Optional diagnostic severity badge."},
            "children": {"type": "array", "maxItems": 12, "items": {"type": "string", "minLength": 1}, "description": "Child node ids for tree forms only."},
            "hint": diagram_hint_schema()
        },
        "required": ["id", "label", "detail", "code_refs"],
        "additionalProperties": false
    })
}

fn diagram_node_patch_schema() -> Value {
    json!({
        "type": "object",
        "minProperties": 1,
        "description": "Node-only patch. Supply replacement values or a clear_* boolean; omitted fields stay unchanged.",
        "properties": {
            "label": {"type": "string", "minLength": 1, "maxLength": 512},
            "detail": {"type": "string", "minLength": 1, "maxLength": 2000},
            "clear_detail": {"type": "boolean", "description": "Set true to remove detail; do not pass a string."},
            "expanded_detail": {"type": "string", "minLength": 1, "maxLength": 4000},
            "clear_expanded_detail": {"type": "boolean", "description": "Set true to remove expanded_detail; do not pass a string."},
            "entity": diagram_entity_schema(),
            "clear_entity": {"type": "boolean", "description": "Set true to remove entity; do not pass a string."},
            "code_refs": {"type": "array", "minItems": 1, "maxItems": 2, "items": diagram_code_ref_schema()},
            "change": {"type": "string", "enum": ["added", "modified", "removed", "unchanged", "diagnostic"]},
            "severity": {"type": "string", "enum": ["error", "warning", "information", "hint"]},
            "clear_severity": {"type": "boolean", "description": "Set true to remove severity; do not pass a string."},
            "children": {"type": "array", "maxItems": 12, "items": {"type": "string", "minLength": 1}},
            "hint": diagram_hint_schema()
        },
        "additionalProperties": false
    })
}

fn diagram_edge_kind_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["calls", "imports", "implements", "contains", "reads", "writes"],
        "description": "Typed directed relationship. Semantic kinds must be supported by research evidence."
    })
}

fn diagram_edge_schema() -> Value {
    json!({
        "type": "object",
        "description": "Complete directed relationship between two existing node ids.",
        "properties": {
            "from": {"type": "string", "minLength": 1, "maxLength": 128},
            "to": {"type": "string", "minLength": 1, "maxLength": 128},
            "kind": diagram_edge_kind_schema(),
            "label": {"type": "string", "minLength": 1, "maxLength": 1000, "description": "Specific trigger, data, condition, or effect displayed on the arrow."}
        },
        "required": ["from", "to", "kind", "label"],
        "additionalProperties": false
    })
}

fn diagram_edge_patch_schema() -> Value {
    json!({
        "type": "object",
        "minProperties": 1,
        "description": "Edge-only patch. The outer from/to identify the current edge; these optional fields replace its values.",
        "properties": {
            "from": {"type": "string", "minLength": 1, "maxLength": 128},
            "to": {"type": "string", "minLength": 1, "maxLength": 128},
            "kind": diagram_edge_kind_schema(),
            "label": {"type": "string", "minLength": 1, "maxLength": 1000},
            "clear_label": {"type": "boolean", "description": "Set true to remove the label; do not pass a string."}
        },
        "additionalProperties": false
    })
}

fn diagram_evidence_schema() -> Value {
    json!({
        "type": "object",
        "description": "One exact citation. File and reason are always required; only copy optional hunk, symbol, and range from research results.",
        "properties": {
            "file": {"type": "string", "minLength": 1, "description": "Exact repo-relative file from a research result."},
            "hunk": {"type": "integer", "minimum": 0, "description": "Optional zero-based diff hunk index."},
            "symbol": {"type": "string", "minLength": 1, "description": "Optional exact semantic symbol name."},
            "range": diagram_line_range_schema(),
            "reason": {"type": "string", "minLength": 1, "maxLength": 2000, "description": "Concrete statement of what the cited source directly establishes."}
        },
        "required": ["file", "reason"],
        "additionalProperties": false
    })
}

/// Return the canonical example embedded in the strict schema branch for `op`.
/// Parse failures use this to give the model a concrete recovery shape without maintaining
/// a second, potentially divergent set of examples.
pub(crate) fn diagram_command_example(op: Option<&str>) -> Value {
    let edit = diagram_tools()
        .into_iter()
        .next()
        .expect("edit tool exists");
    let variants = edit.parameters["oneOf"]
        .as_array()
        .expect("edit tool has command variants");
    variants
        .iter()
        .find(|variant| {
            op.is_some_and(|op| variant["properties"]["op"]["const"].as_str() == Some(op))
        })
        .or_else(|| {
            variants.iter().find(|variant| {
                variant["properties"]["op"]["const"].as_str() == Some("create_form")
            })
        })
        .and_then(|variant| variant["examples"].as_array())
        .and_then(|examples| examples.first())
        .cloned()
        .unwrap_or_else(|| json!({"op": "create_form", "form_id": "main", "kind": "sequence"}))
}

/// `true` when `name` is one of the read-only research tools.
#[must_use]
pub fn is_read_only_tool(name: &str) -> bool {
    read_only_tools()
        .into_iter()
        .chain(research_tools())
        .chain(semantic_tools())
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
    fn one_open_semantic_inspection_tool_is_capability_discoverable() {
        let tools = semantic_tools();
        assert_eq!(tools.len(), 1);
        let tool = &tools[0];
        assert_eq!(tool.name, LSP_INSPECT_TOOL_NAME);
        assert!(tool.parameters["properties"]["query"].get("enum").is_none());
        assert_eq!(tool.parameters["properties"]["line"]["minimum"], 1);
        assert_eq!(tool.parameters["properties"]["column"]["minimum"], 0);
        assert!(tool
            .description
            .contains("future adapters may advertise more"));
    }

    #[test]
    fn every_definition_is_an_object_schema() {
        for tool in read_only_tools()
            .into_iter()
            .chain(research_tools())
            .chain(semantic_tools())
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
    fn diagram_editor_schema_is_a_strict_documented_union() {
        let edit = diagram_tools()
            .into_iter()
            .find(|tool| tool.name == DIAGRAM_EDIT_TOOL_NAME)
            .unwrap();
        let wire_bytes = serde_json::to_string(&edit.to_openai()).unwrap().len();
        assert!(
            wire_bytes < 25_000,
            "strict editor schema regressed to {wire_bytes} bytes"
        );
        let variants = edit.parameters["oneOf"].as_array().unwrap();
        assert_eq!(variants.len(), 12);
        for variant in variants {
            assert_eq!(variant["type"], "object");
            assert_eq!(variant["additionalProperties"], false);
            assert!(variant["description"]
                .as_str()
                .is_some_and(|v| !v.is_empty()));
            assert_eq!(variant["examples"].as_array().unwrap().len(), 1);
            let example = variant["examples"][0].clone();
            serde_json::from_value::<codescope_core::DiagramCommand>(example)
                .expect("every documented example must deserialize through the shared API");
        }

        let by_op = |op: &str| {
            variants
                .iter()
                .find(|variant| variant["properties"]["op"]["const"] == op)
                .unwrap()
        };
        let create_node = by_op("create_node");
        assert!(create_node["required"]
            .as_array()
            .unwrap()
            .contains(&json!("form_id")));
        assert!(create_node["required"]
            .as_array()
            .unwrap()
            .contains(&json!("node")));
        let node = &create_node["properties"]["node"];
        assert_eq!(
            node["properties"]["hint"]["properties"]["highlight"]["type"],
            "boolean"
        );
        assert_eq!(
            node["properties"]["hint"]["properties"]["collapsed"]["type"],
            "boolean"
        );
        assert_eq!(
            node["properties"]["code_refs"]["items"]["required"],
            json!(["file", "hunk", "side", "start_line", "end_line"])
        );

        let update_node = by_op("update_node");
        let patch = &update_node["properties"]["patch"];
        assert_eq!(patch["properties"]["clear_entity"]["type"], "boolean");
        assert!(patch["properties"].get("clear_label").is_none());
        let update_edge = by_op("update_edge");
        assert_eq!(
            update_edge["properties"]["patch"]["properties"]["clear_label"]["type"],
            "boolean"
        );
        assert!(update_edge["properties"]["patch"]["properties"]
            .get("clear_entity")
            .is_none());
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
        assert!(is_read_only_tool(LSP_INSPECT_TOOL_NAME));
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
