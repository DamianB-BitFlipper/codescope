//! Plan tool parameters (schemars) and tool-call-arguments → [`VisualizationPlan`] parsing.
//!
//! Core plan types are deliberately serde-only (schemars lives here, per the architecture
//! decision log), so the tool parameters use a local [`PlanParams`] envelope whose `plan`
//! field carries the plan document; its JSON Schema is hand-maintained in
//! [`plan_value_schema`] as the **AI input dialect** of the research 05 §2 schema: it
//! intentionally narrows what core serde accepts (legacy list forms excluded, tighter
//! caps, required renderer-consumed fields) while parsing into the same core types, and the
//! dialect's compatibility with core serde output is tested rather than assumed. Semantic
//! checking is **not** serde's job — [`crate::validate`] is the boundary that decides what
//! renders.
//!
//! [`parse_plan`] is liberal in what it accepts at the transport layer (providers and
//! models differ): the arguments may be the bare plan object, a `{"plan": {...}}`
//! envelope, or an envelope whose `plan` is a double-encoded JSON string. Anything else is
//! [`AiError::MalformedPlan`].

use crate::error::AiError;
use crate::tools::{ToolDef, PLAN_TOOL_NAME};
use codescope_core::{
    VisualizationPlan, MAX_CODE_REF_LINES, MAX_FORMS_PER_PLAN, MAX_NODE_CODE_REFS, PLAN_VERSION,
};
use schemars::{json_schema, JsonSchema, Schema, SchemaGenerator};
use serde_json::Value;

/// AI-facing hard cap on nodes per form in the tool schema: the reviewer's first screen
/// defaults to 4 decisive nodes, with 5 as the exceptional ceiling when a code-owned
/// mechanism cannot be merged. The validator's MAX_FORM_NODES (12) stays as the
/// deterministic backstop, so a model that overshoots still validates after truncation.
pub const MAX_AI_FORM_NODES: usize = 5;

/// AI-facing hard cap on edges per form in the tool schema. A dense nonlinear flow
/// renders adjacency rows proportional to node+edge count, so without this a small
/// entityless relationship_flow could carry dozens of labeled writes edges and flood the
/// first screen. The validator keeps no edge backstop of its own; this is the boundary.
pub const MAX_AI_FORM_EDGES: usize = 8;

/// AI-facing hard cap on evidence entries in the tool schema: cite the 2-4 strongest
/// items. MAX_PLAN_EVIDENCE (6) remains the validator's deterministic backstop.
pub const MAX_AI_EVIDENCE: usize = 4;

/// Arguments of the `submit_visualization_plan` tool: a single `plan` document.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub struct PlanParams {
    /// The visualization plan document: one description, the smallest useful structural
    /// visual, and typed source evidence.
    #[schemars(schema_with = "plan_value_schema")]
    pub plan: Value,
}

/// Hand-maintained JSON Schema for the plan document, mirroring
/// [`codescope_core::VisualizationPlan`] serde output (kept in lock-step by the
/// round-trip tests below).
fn plan_value_schema(_gen: &mut SchemaGenerator) -> Schema {
    let range_schema = serde_json::json!({
        "type": "object",
        "description": "Zero-based line/col range, columns in UTF-8 code units.",
        "properties": {
            "start_line": {"type": "integer", "minimum": 0},
            "start_col": {"type": "integer", "minimum": 0},
            "end_line": {"type": "integer", "minimum": 0},
            "end_col": {"type": "integer", "minimum": 0}
        },
        "required": ["start_line", "start_col", "end_line", "end_col"],
        "additionalProperties": false
    });
    json_schema!({
        "type": "object",
        "description": "A reviewer-first explanation: one description, the single smallest useful structural visual, and typed source evidence. Every source fact MUST come from the current research brief or a tool result.",
        "properties": {
            "plan_version": {"type": "integer", "const": PLAN_VERSION},
            "epoch": {
                "type": "integer",
                "description": "Repo-state epoch counter echoed verbatim from the prompt. Plans carrying a different epoch are discarded as stale."
            },
            "intent": {
                "type": "string",
                "minLength": 8,
                "maxLength": 240,
                "description": "The single displayed description: one concise sentence explaining implemented behavior and purpose. Stop at the last behavior the supplied repository facts establish."
            },
            "forms": {
                "type": "array",
                "minItems": 1,
                "maxItems": 2,
                "items": {
                    "type": "object",
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": ["changed_symbol_tree", "call_tree", "type_impl_tree",
                                      "relationship_flow", "before_after", "sequence"],
                            "description": "before_after is exactly two flat states (nodes[0] = before, nodes[1] = after) with no children and at most one transition edge directed before -> after. Use it for localized literal, format-string, default-value, condition, or configuration changes that do not alter control flow or topology; use a tree or flow form for nested structure."
                        },
                        "nodes": {
                            "type": "array",
                            "maxItems": MAX_AI_FORM_NODES,
                            "description": "Each element is one complete node object with id, label, and detail - never a bare string, id, or field name.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "id": {"type": "string", "description": "Plan-local id, e.g. \"n1\"."},
                                    "entity": {
                                        "type": "object",
                                        "description": "Fact-store entity; must resolve exactly.",
                                        "properties": {
                                            "file": {"type": "string", "description": "Repo-relative path."},
                                            "symbol": {"type": "string", "description": "Fully-qualified symbol name; omit for file-level nodes."},
                                            "range": range_schema
                                        },
                                        "required": ["file"],
                                        "additionalProperties": false
                                    },
                                    "label": {
                                        "type": "string",
                                        "maxLength": 80,
                                        "description": "A real identifier or a short action/state, not a category or badge."
                                    },
                                    "detail": {
                                        "type": "string",
                                        "minLength": 3,
                                        "maxLength": 160,
                                        "description": "Collapsed box preview: one concrete reviewer fact, normally at most 8 words and 56 characters. Do not assert an external actor or outcome absent from supplied repository facts."
                                    },
                                    "expanded_detail": {
                                        "type": "string",
                                        "minLength": 8,
                                        "maxLength": 320,
                                        "description": "Optional complete, self-contained explanation shown inside this box when clicked. It may include the preview fact but must add useful context."
                                    },
                                    "code_refs": {
                                        "type": "array",
                                        "minItems": 1,
                                        "maxItems": MAX_NODE_CODE_REFS,
                                        "description": "Exact relevant diff lines highlighted when this box is hovered. Every node needs one or two complete range objects copied from an annotated git_diff_file result, never a bare string or line number.",
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "file": {"type": "string", "description": "Exact repo_path copied from git_diff_file; it must remain inside the current file/function or directory selection."},
                                                "hunk": {"type": "integer", "minimum": 0, "description": "Zero-based hunk_id copied from git_diff_file."},
                                                "side": {"type": "string", "enum": ["old", "new"], "description": "old for removed lines; new for added or post-change context lines."},
                                                "start_line": {"type": "integer", "minimum": 1, "description": "First one-based line copied from an old/new annotation, inclusive."},
                                                "end_line": {"type": "integer", "minimum": 1, "description": "Last one-based line on the same side and hunk, inclusive; at most 12 lines."}
                                            },
                                            "required": ["file", "hunk", "side", "start_line", "end_line"],
                                            "additionalProperties": false
                                        }
                                    },
                                    "children": {"type": "array", "items": {"type": "string"}, "description": "Child node ids (tree forms; depth at most 3)."},
                                    "hint": {
                                        "type": "object",
                                        "properties": {"highlight": {"type": "boolean"}, "collapsed": {"type": "boolean"}},
                                        "additionalProperties": false
                                    }
                                },
                                "required": ["id", "label", "detail", "code_refs"],
                                "additionalProperties": false
                            }
                        },
                        "edges": {
                            "type": "array",
                            "maxItems": MAX_AI_FORM_EDGES,
                            "description": "At most 8: keep only the decisive relationships.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "from": {"type": "string"},
                                    "to": {"type": "string"},
                                    "kind": {"type": "string", "enum": ["calls", "imports", "implements", "contains", "reads", "writes"]},
                                    "label": {
                                        "type": "string",
                                        "minLength": 2,
                                        "maxLength": 100,
                                        "description": "Required implemented trigger, condition, data movement, or effect shown on the arrow; do not repeat the edge kind or assert an external actor/outcome absent from supplied repository facts."
                                    }
                                },
                                "required": ["from", "to", "kind", "label"],
                                "additionalProperties": false
                            },
                            "description": "Edges select existing relationships; the validator rejects edges absent from the impact graph."
                        }
                    },
                    "required": ["kind", "nodes", "edges"],
                    "additionalProperties": false
                }
            },
            "evidence": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_AI_EVIDENCE,
                "description": "One to four typed source references supporting the visual; every distinct claim in intent/nodes/details/edges must be covered by at least one item. Hunks are zero-based here and displayed one-based by the UI.",
                "items": {
                    "type": "object",
                    "properties": {
                        "file": {"type": "string", "description": "Exact repo-relative path copied from the current brief or a tool result."},
                        "hunk": {"type": "integer", "minimum": 0, "description": "Zero-based hunk index copied from git_status_file or git_diff_file."},
                        "symbol": {"type": "string", "description": "Fully-qualified symbol copied from an exact current fact or tool result; omit when unavailable."},
                        "range": range_schema,
                        "reason": {
                            "type": "string",
                            "minLength": 3,
                            "maxLength": 160,
                            "description": "What these cited lines directly implement. Do not assert an external actor/outcome absent from supplied repository facts; a similarly named config field does not prove cross-system injection."
                        }
                    },
                    "required": ["file", "reason"],
                    "additionalProperties": false
                }
            }
        },
        "required": [
            "plan_version",
            "epoch",
            "intent",
            "forms",
            "evidence"
        ],
        "additionalProperties": false
    })
}

/// The `submit_visualization_plan` tool definition (schema generated from [`PlanParams`]).
#[must_use]
pub fn plan_tool() -> ToolDef {
    let schema = schemars::schema_for!(PlanParams);
    ToolDef {
        name: PLAN_TOOL_NAME,
        description: "Required final response. Always call this function instead of answering with text. Submit one complete visualization plan grounded in the current research brief and tool results.".into(),
        parameters: schema.to_value(),
    }
}

/// Parse `submit_visualization_plan` tool-call arguments into a typed plan.
///
/// Accepts the bare plan object, the `{"plan": {...}}` envelope, or an envelope whose
/// `plan` value is a JSON string (double-encoded). Rejects syntactically invalid JSON and
/// shape mismatches as [`AiError::MalformedPlan`], and a wrong `plan_version` as
/// [`AiError::PlanVersion`]. **Semantic** validity (entities, edges, epochs, caps) is
/// decided by [`crate::validate`], never here.
pub fn parse_plan(arguments: &str) -> Result<VisualizationPlan, AiError> {
    let value: Value = serde_json::from_str(arguments.trim())
        .map_err(|e| AiError::MalformedPlan(format!("arguments are not valid JSON: {e}")))?;

    // Unwrap the PlanParams envelope when present (a bare plan has plan_version itself).
    let value = match &value {
        Value::Object(obj) if obj.contains_key("plan") && !obj.contains_key("plan_version") => {
            match &obj["plan"] {
                Value::String(s) => serde_json::from_str::<Value>(s).map_err(|e| {
                    AiError::MalformedPlan(format!("envelope plan string is not valid JSON: {e}"))
                })?,
                other => other.clone(),
            }
        }
        _ => value,
    };

    let plan: VisualizationPlan = serde_json::from_value(value)
        .map_err(|e| AiError::MalformedPlan(format!("plan does not match schema: {e}")))?;

    if plan.plan_version != PLAN_VERSION {
        return Err(AiError::PlanVersion {
            got: plan.plan_version,
            expected: PLAN_VERSION,
        });
    }
    enforce_ai_input_contract(&plan)?;
    tracing::debug!(epoch = %plan.epoch, forms = plan.forms.len(), "plan parsed");
    Ok(plan)
}

/// Enforce the AI-facing input contract the tool schema advertises: model-facing caps
/// (forms, nodes per form, evidence) that are tighter than the validator's deterministic
/// backstops. Unlike validation (which
/// truncates), this is a parse-time rejection: the last lifecycle node may be the point
/// of the diagram, so an oversized plan must be **rewritten**, not silently truncated.
/// Errors are [`AiError::MalformedPlan`] so the service's bounded repair loop can ask for
/// a corrected submission with the observed/allowed counts.
fn enforce_ai_input_contract(plan: &VisualizationPlan) -> Result<(), AiError> {
    if plan.forms.is_empty() {
        return Err(AiError::MalformedPlan(
            "plan has no forms; submit exactly one structural form (two only for a distinct relationship)"
                .into(),
        ));
    }
    if plan.forms.len() > MAX_FORMS_PER_PLAN {
        let observed = plan.forms.len();
        return Err(AiError::MalformedPlan(format!(
            "plan has {observed} forms; the schema allows at most {MAX_FORMS_PER_PLAN} - keep one form, two only for a distinct relationship"
        )));
    }
    for (index, form) in plan.forms.iter().enumerate() {
        if form.nodes.len() > MAX_AI_FORM_NODES {
            let observed = form.nodes.len();
            return Err(AiError::MalformedPlan(format!(
                "form {index} ({:?}) has {observed} nodes; the schema allows at most {MAX_AI_FORM_NODES} - merge intermediate mechanics into decisive steps ending with the final lifecycle step",
                form.kind
            )));
        }
        if form.edges.len() > MAX_AI_FORM_EDGES {
            let observed = form.edges.len();
            return Err(AiError::MalformedPlan(format!(
                "form {index} ({:?}) has {observed} edges; the schema allows at most {MAX_AI_FORM_EDGES} - keep only the decisive relationships",
                form.kind
            )));
        }
        for node in &form.nodes {
            let count = node.code_refs.len();
            if count == 0 || count > MAX_NODE_CODE_REFS {
                return Err(AiError::MalformedPlan(format!(
                    "node {} in form {index} has {count} code_refs; every node requires 1-{MAX_NODE_CODE_REFS} exact ranges copied from an annotated git_diff_file result",
                    node.id
                )));
            }
            for code_ref in &node.code_refs {
                let line_count = code_ref
                    .end_line
                    .checked_sub(code_ref.start_line)
                    .and_then(|span| span.checked_add(1));
                if code_ref.start_line == 0
                    || line_count.is_none_or(|lines| lines > MAX_CODE_REF_LINES)
                {
                    return Err(AiError::MalformedPlan(format!(
                        "node {} has invalid code_ref {}#h{} {:?} {}..{}; use a nonempty inclusive one-based range of at most {MAX_CODE_REF_LINES} lines",
                        node.id,
                        code_ref.file,
                        code_ref.hunk,
                        code_ref.side,
                        code_ref.start_line,
                        code_ref.end_line
                    )));
                }
            }
            if node
                .expanded_detail
                .as_deref()
                .is_some_and(|detail| detail.trim().is_empty())
            {
                return Err(AiError::MalformedPlan(format!(
                    "node {} has a blank expanded_detail; omit it or add useful non-repeated context",
                    node.id
                )));
            }
        }
    }
    if plan.evidence.is_empty() {
        return Err(AiError::MalformedPlan(
            "plan has no evidence entries; a reviewer-first plan cites at least one exact file, symbol, range, or zero-based hunk supporting the visual"
                .into(),
        ));
    }
    if plan.evidence.len() > MAX_AI_EVIDENCE {
        let observed = plan.evidence.len();
        return Err(AiError::MalformedPlan(format!(
            "plan has {observed} evidence entries; the schema allows at most {MAX_AI_EVIDENCE} - keep the strongest items supporting the displayed description and visual"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codescope_core::{Epoch, FormKind, PlanNodeChange};

    fn sample_json(epoch: u64) -> Value {
        serde_json::json!({
            "plan_version": PLAN_VERSION,
            "epoch": epoch,
            "intent": "Show which entry point reaches SessionStore.load before a rename.",
            "forms": [{
                "kind": "call_tree",
                "nodes": [{
                    "id": "n1",
                    "entity": {
                        "file": "src/session/store.rs",
                        "symbol": "SessionStore.load",
                        "range": {"start_line": 121, "start_col": 4, "end_line": 140, "end_col": 5}
                    },
                    "label": "load",
                    "detail": "loads the requested session after a cache miss",
                    "expanded_detail": "Checks the cache before reading persistent session state.",
                    "code_refs": [{
                        "file": "src/session/store.rs",
                        "hunk": 0,
                        "side": "new",
                        "start_line": 122,
                        "end_line": 124
                    }],
                    "children": ["n2"]
                }, {
                    "id": "n2",
                    "entity": {"file": "src/main.rs", "symbol": "main"},
                    "label": "main",
                    "detail": "starts the request path that reaches load",
                    "code_refs": [{
                        "file": "src/main.rs",
                        "hunk": 0,
                        "side": "new",
                        "start_line": 10,
                        "end_line": 10
                    }]
                }],
                "edges": [{"from": "n2", "to": "n1", "kind": "calls", "label": "on boot"}]
            }],
            "evidence": [{
                "file": "src/session/store.rs",
                "symbol": "SessionStore.load",
                "range": {"start_line": 121, "start_col": 4, "end_line": 140, "end_col": 5},
                "reason": "defines the renamed entry point"
            }]
        })
    }

    #[test]
    fn parses_bare_plan() {
        let plan = parse_plan(&sample_json(7).to_string()).unwrap();
        assert_eq!(plan.epoch, Epoch(7));
        assert_eq!(plan.forms.len(), 1);
        assert_eq!(plan.forms[0].kind, FormKind::CallTree);
        assert_eq!(plan.forms[0].nodes[0].change, PlanNodeChange::Unchanged);
        assert_eq!(
            plan.forms[0].nodes[0].detail.as_deref(),
            Some("loads the requested session after a cache miss")
        );
        assert_eq!(plan.forms[0].edges[0].label.as_deref(), Some("on boot"));
    }

    #[test]
    fn parses_envelope_and_double_encoded() {
        let bare = sample_json(3);
        let envelope = serde_json::json!({"plan": bare});
        let plan = parse_plan(&envelope.to_string()).unwrap();
        assert_eq!(plan.epoch, Epoch(3));

        let double = serde_json::json!({"plan": bare.to_string()});
        let plan = parse_plan(&double.to_string()).unwrap();
        assert_eq!(plan.epoch, Epoch(3));
    }

    #[test]
    fn rejects_malformed_json() {
        let err = parse_plan(r#"{"plan_version":1,"epoch":"#).unwrap_err();
        assert!(matches!(err, AiError::MalformedPlan(_)), "{err}");
        let err = parse_plan("not json at all").unwrap_err();
        assert!(matches!(err, AiError::MalformedPlan(_)));
    }

    #[test]
    fn rejects_shape_mismatch() {
        // intent missing entirely.
        let err = parse_plan(r#"{"plan_version":1,"epoch":1,"forms":[]}"#).unwrap_err();
        assert!(matches!(err, AiError::MalformedPlan(_)));
        // unknown form kind.
        let mut bad = sample_json(1);
        bad["forms"][0]["kind"] = Value::String("mermaid_diagram".into());
        let err = parse_plan(&bad.to_string()).unwrap_err();
        assert!(matches!(err, AiError::MalformedPlan(_)));
    }

    #[test]
    fn rejects_wrong_plan_version() {
        let mut bad = sample_json(1);
        bad["plan_version"] = Value::from(99);
        let err = parse_plan(&bad.to_string()).unwrap_err();
        assert!(matches!(
            err,
            AiError::PlanVersion { got: 99, expected } if expected == PLAN_VERSION
        ));
    }

    #[test]
    fn rejects_removed_fields() {
        for field in ["focus", "title", "review_focus"] {
            let mut bad = sample_json(1);
            bad[field] = Value::String("obsolete".into());
            let error = parse_plan(&bad.to_string()).unwrap_err().to_string();
            assert!(error.contains("unknown field"), "{field}: {error}");
        }

        for field in ["title", "summary"] {
            let mut bad = sample_json(1);
            bad["forms"][0][field] = Value::String("obsolete".into());
            let error = parse_plan(&bad.to_string()).unwrap_err().to_string();
            assert!(error.contains("unknown field"), "form {field}: {error}");
        }
    }

    #[test]
    fn node_code_refs_are_required_bounded_and_non_reversed() {
        let mut missing = sample_json(1);
        missing["forms"][0]["nodes"][0]
            .as_object_mut()
            .unwrap()
            .remove("code_refs");
        let error = parse_plan(&missing.to_string()).unwrap_err().to_string();
        assert!(error.contains("0 code_refs"), "{error}");

        let mut reversed = sample_json(1);
        reversed["forms"][0]["nodes"][0]["code_refs"][0]["start_line"] = Value::from(130);
        reversed["forms"][0]["nodes"][0]["code_refs"][0]["end_line"] = Value::from(120);
        let error = parse_plan(&reversed.to_string()).unwrap_err().to_string();
        assert!(error.contains("invalid code_ref"), "{error}");
        assert!(error.contains("130..120"), "{error}");

        let mut too_wide = sample_json(1);
        too_wide["forms"][0]["nodes"][0]["code_refs"][0]["start_line"] = Value::from(100);
        too_wide["forms"][0]["nodes"][0]["code_refs"][0]["end_line"] =
            Value::from(100 + MAX_CODE_REF_LINES);
        let error = parse_plan(&too_wide.to_string()).unwrap_err().to_string();
        assert!(error.contains("at most 12 lines"), "{error}");

        let mut too_many = sample_json(1);
        let extra = too_many["forms"][0]["nodes"][0]["code_refs"][0].clone();
        too_many["forms"][0]["nodes"][0]["code_refs"]
            .as_array_mut()
            .unwrap()
            .extend([extra.clone(), extra]);
        let error = parse_plan(&too_many.to_string()).unwrap_err().to_string();
        assert!(error.contains("3 code_refs"), "{error}");
        assert!(error.contains("1-2"), "{error}");
    }

    /// The schema-advertised caps are enforced at parse time with observed/allowed counts,
    /// as repairable errors — never silent truncation (the final lifecycle node may be the
    /// point of the diagram).
    #[test]
    fn rejects_oversized_forms_nodes_and_evidence_with_counts() {
        // 7 nodes in one form (round-3 live failure: 7 despite maxItems 6, since lowered
        // to a five-node ceiling after round-3 attempt 2 also used 6 as a target).
        let mut seven = sample_json(1);
        for i in 2..7 {
            seven["forms"][0]["nodes"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({
                    "id": format!("n{i}"),
                    "entity": {"file": "src/main.rs", "symbol": format!("s{i}")},
                    "label": format!("s{i}"),
                    "detail": "adds an intermediate mechanics step",
                }));
        }
        let err = parse_plan(&seven.to_string()).unwrap_err();
        assert!(matches!(err, AiError::MalformedPlan(_)), "{err}");
        let msg = err.to_string();
        assert!(msg.contains("has 7 nodes"), "observed count: {msg}");
        assert!(msg.contains("at most 5"), "allowed count: {msg}");
        assert!(msg.contains("merge intermediate mechanics"), "{msg}");

        // Evidence cap: 5 entries exceeds MAX_AI_EVIDENCE (4).
        let mut ev = sample_json(1);
        for i in 0..4 {
            ev["evidence"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({
                    "file": "src/main.rs",
                    "reason": format!("extra evidence {i}"),
                }));
        }
        let err = parse_plan(&ev.to_string()).unwrap_err();
        assert!(err.to_string().contains("5 evidence"), "{}", err);

        // Forms cap: 3 forms exceeds MAX_FORMS_PER_PLAN (2).
        let mut forms = sample_json(1);
        let extra = forms["forms"][0].clone();
        for _ in 0..2 {
            forms["forms"].as_array_mut().unwrap().push(extra.clone());
        }
        let err = parse_plan(&forms.to_string()).unwrap_err();
        assert!(err.to_string().contains("3 forms"), "{}", err);
        assert!(err.to_string().contains("at most 2"), "{}", err);

        // Edges cap: 9 edges in one form exceeds MAX_AI_FORM_EDGES (8).
        let mut many_edges = sample_json(1);
        for i in 0..8 {
            many_edges["forms"][0]["edges"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({
                    "from": "n2",
                    "to": "n1",
                    "kind": "contains",
                    "label": format!("extra structural edge {i}"),
                }));
        }
        let err = parse_plan(&many_edges.to_string()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("has 9 edges"), "observed count: {msg}");
        assert!(msg.contains("at most 8"), "allowed count: {msg}");

        // Empty evidence: a reviewer-first plan needs at least one typed source.
        let mut no_evidence = sample_json(1);
        no_evidence["evidence"] = serde_json::json!([]);
        let err = parse_plan(&no_evidence.to_string()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no evidence entries"), "{msg}");
        assert!(msg.contains("at least one"), "{msg}");

        // Empty forms: matches the schema's minItems 1.
        let mut no_forms = sample_json(1);
        no_forms["forms"] = serde_json::json!([]);
        let err = parse_plan(&no_forms.to_string()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no forms"), "{msg}");
        assert!(msg.contains("exactly one structural form"), "{msg}");
    }

    #[test]
    fn plan_tool_schema_shape() {
        let tool = plan_tool();
        assert_eq!(tool.name, PLAN_TOOL_NAME);
        let params = &tool.parameters;
        assert_eq!(params["type"], "object");
        assert!(params["properties"]["plan"].is_object());
        assert!(params["required"]
            .as_array()
            .unwrap()
            .contains(&Value::String("plan".into())));
        // The AI-facing schema exposes only structural reviewer visuals.
        let kinds = params["properties"]["plan"]["properties"]["forms"]["items"]["properties"]
            ["kind"]["enum"]
            .as_array()
            .unwrap();
        assert_eq!(kinds.len(), 6);
        for k in [
            "changed_symbol_tree",
            "call_tree",
            "type_impl_tree",
            "relationship_flow",
            "before_after",
            "sequence",
        ] {
            assert!(kinds.contains(&Value::String(k.into())), "missing {k}");
        }
        assert_eq!(
            params["properties"]["plan"]["properties"]["plan_version"]["const"],
            Value::from(PLAN_VERSION)
        );
        let node_schema = &params["properties"]["plan"]["properties"]["forms"]["items"]
            ["properties"]["nodes"]["items"];
        assert!(node_schema["properties"].get("detail").is_some());
        assert!(node_schema["properties"].get("expanded_detail").is_some());
        let code_refs = &node_schema["properties"]["code_refs"];
        assert_eq!(code_refs["minItems"], Value::from(1));
        assert_eq!(code_refs["maxItems"], Value::from(MAX_NODE_CODE_REFS));
        assert_eq!(
            code_refs["items"]["properties"]["side"]["enum"],
            serde_json::json!(["old", "new"])
        );
        for field in ["file", "hunk", "side", "start_line", "end_line"] {
            assert!(code_refs["items"]["required"]
                .as_array()
                .unwrap()
                .contains(&Value::String(field.into())));
        }
        assert!(node_schema["properties"].get("severity").is_none());
        assert!(node_schema["properties"].get("change").is_none());
        assert!(node_schema["required"]
            .as_array()
            .unwrap()
            .contains(&Value::String("detail".into())));
        assert!(node_schema["required"]
            .as_array()
            .unwrap()
            .contains(&Value::String("code_refs".into())));
        // The nodes array names its element contract (a live GLM run submitted a bare
        // string where a node object was expected).
        let nodes_desc = &params["properties"]["plan"]["properties"]["forms"]["items"]
            ["properties"]["nodes"]["description"];
        assert!(
            nodes_desc
                .as_str()
                .is_some_and(|d| d.contains("never a bare string")),
            "nodes array description: {nodes_desc}"
        );
        // First-screen caps: the schema is tighter than the validator's backstops
        // (MAX_FORM_NODES 12 / MAX_PLAN_EVIDENCE 6), so the first render stays small.
        let form_item = &params["properties"]["plan"]["properties"]["forms"]["items"];
        assert_eq!(
            form_item["properties"]["nodes"]["maxItems"],
            Value::from(MAX_AI_FORM_NODES)
        );
        // Edge density cap and evidence floor: a dense flow cannot flood the first screen
        // and a reviewer-first plan always cites at least one source.
        assert_eq!(
            form_item["properties"]["edges"]["maxItems"],
            Value::from(MAX_AI_FORM_EDGES)
        );
        assert_eq!(MAX_AI_FORM_EDGES, 8);
        assert_eq!(
            params["properties"]["plan"]["properties"]["evidence"]["minItems"],
            Value::from(1)
        );
        assert_eq!(
            params["properties"]["plan"]["properties"]["forms"]["minItems"],
            Value::from(1)
        );
        let evidence_desc = params["properties"]["plan"]["properties"]["evidence"]["description"]
            .as_str()
            .unwrap();
        assert!(
            evidence_desc.contains("every distinct claim"),
            "{evidence_desc}"
        );
        // before_after's two-flat-states contract is advertised on the kind property.
        let kind_desc = form_item["properties"]["kind"]["description"]
            .as_str()
            .unwrap();
        assert!(kind_desc.contains("exactly two flat states"), "{kind_desc}");
        assert!(
            kind_desc.contains("at most one transition edge"),
            "{kind_desc}"
        );
        assert_eq!(
            params["properties"]["plan"]["properties"]["evidence"]["maxItems"],
            Value::from(MAX_AI_EVIDENCE)
        );
        assert_eq!(MAX_AI_FORM_NODES, 5);
        assert_eq!(MAX_AI_EVIDENCE, 4);
        let plan_schema = &params["properties"]["plan"];
        let plan_description = plan_schema["description"].as_str().unwrap();
        let plan_description = plan_description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            plan_description.contains("one description")
                && plan_description.contains("smallest useful structural visual"),
            "{plan_description}"
        );
        for required in ["intent", "forms", "evidence"] {
            assert!(plan_schema["required"]
                .as_array()
                .unwrap()
                .contains(&Value::String(required.into())));
        }
        for removed in ["focus", "title", "review_focus"] {
            assert!(plan_schema["properties"].get(removed).is_none());
        }
        let form_schema = &plan_schema["properties"]["forms"]["items"];
        for removed in ["title", "summary"] {
            assert!(form_schema["properties"].get(removed).is_none());
        }
    }

    #[test]
    fn core_serde_output_parses_through_the_ai_input_contract() {
        // A core-typed plan within the AI input contract round-trips through parse_plan,
        // proving the dialect the schema documents is the one core actually emits. The
        // schema is an AI input dialect that intentionally narrows core (no list forms,
        // tighter caps, required evidence) — compatibility-tested here, not an exact
        // schema identity.
        let mut plan = VisualizationPlan::new(Epoch(9));
        plan.intent = "The changed entry point stays reachable.".into();
        plan.forms.push(codescope_core::VizForm {
            kind: FormKind::CallTree,
            nodes: vec![
                codescope_core::PlanNode::new("n1", "l", PlanNodeChange::Added)
                    .with_detail("explains the changed entry point")
                    .with_code_ref(codescope_core::PlanCodeRef::new(
                        codescope_core::FileId::new("src/main.rs").unwrap(),
                        0,
                        codescope_core::DiffSide::New,
                        10,
                        12,
                    )),
            ],
            edges: vec![],
        });
        plan.evidence.push(codescope_core::PlanEvidence {
            file: codescope_core::FileId::new("src/main.rs").unwrap(),
            hunk: None,
            symbol: None,
            range: None,
            reason: "defines the entry point".into(),
        });
        let text = serde_json::to_string(&plan).unwrap();
        let back = parse_plan(&text).unwrap();
        assert_eq!(back, plan);
    }
}
