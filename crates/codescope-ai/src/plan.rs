//! Plan tool parameters (schemars) and tool-call-arguments → [`VisualizationPlan`] parsing.
//!
//! Core plan types are deliberately serde-only (schemars lives here, per the architecture
//! decision log), so the tool parameters use a local [`PlanParams`] envelope whose `plan`
//! field carries the plan document; its JSON Schema is hand-maintained in
//! [`plan_value_schema`] to mirror the research 05 §2 schema exactly. Semantic checking is
//! **not** serde's job — [`crate::validate`] is the boundary that decides what renders.
//!
//! [`parse_plan`] is liberal in what it accepts at the transport layer (providers and
//! models differ): the arguments may be the bare plan object, a `{"plan": {...}}`
//! envelope, or an envelope whose `plan` is a double-encoded JSON string. Anything else is
//! [`AiError::MalformedPlan`].

use crate::error::AiError;
use crate::tools::{ToolDef, PLAN_TOOL_NAME};
use codescope_core::{VisualizationPlan, PLAN_VERSION};
use schemars::{json_schema, JsonSchema, Schema, SchemaGenerator};
use serde_json::Value;

/// Arguments of the `submit_visualization_plan` tool: a single `plan` document.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub struct PlanParams {
    /// The visualization plan document (research 05 §2 schema).
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
        "description": "A visualization plan: one focus question answered by at most two forms. Every entity MUST be echoed verbatim from the digest or a tool result; the validator drops or rejects anything that does not resolve.",
        "properties": {
            "plan_version": {"type": "integer", "const": PLAN_VERSION},
            "epoch": {
                "type": "integer",
                "description": "Repo-state epoch counter echoed verbatim from the prompt. Plans carrying a different epoch are discarded as stale."
            },
            "focus": {
                "type": "string",
                "description": "The single question this plan answers (one sentence)."
            },
            "forms": {
                "type": "array",
                "maxItems": 2,
                "items": {
                    "type": "object",
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": ["changed_symbol_tree", "call_tree", "type_impl_tree",
                                      "relationship_flow", "impact_summary", "focused_diff",
                                      "before_after", "sequence"]
                        },
                        "title": {"type": "string"},
                        "summary": {
                            "type": "string",
                            "description": "Prose summary, at most 3 lines."
                        },
                        "nodes": {
                            "type": "array",
                            "maxItems": 12,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "id": {"type": "string", "description": "Plan-local id, e.g. \"n1\"."},
                                    "entity": {
                                        "type": "object",
                                        "description": "Fact-store entity; must resolve. For focused_diff hunks use symbol \"hunk:<index>\".",
                                        "properties": {
                                            "file": {"type": "string", "description": "Repo-relative path."},
                                            "symbol": {"type": "string", "description": "Fully-qualified symbol name; omit for file-level nodes."},
                                            "range": range_schema
                                        },
                                        "required": ["file"],
                                        "additionalProperties": false
                                    },
                                    "label": {"type": "string"},
                                    "change": {"type": "string", "enum": ["added", "modified", "removed", "unchanged", "diagnostic"]},
                                    "severity": {"type": "string", "enum": ["error", "warning", "information", "hint"]},
                                    "children": {"type": "array", "items": {"type": "string"}, "description": "Child node ids (tree forms; depth at most 3)."},
                                    "hint": {
                                        "type": "object",
                                        "properties": {"highlight": {"type": "boolean"}, "collapsed": {"type": "boolean"}},
                                        "additionalProperties": false
                                    }
                                },
                                "required": ["id", "label", "change"],
                                "additionalProperties": false
                            }
                        },
                        "edges": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "from": {"type": "string"},
                                    "to": {"type": "string"},
                                    "kind": {"type": "string", "enum": ["calls", "imports", "implements", "contains", "reads", "writes"]},
                                    "label": {"type": "string"}
                                },
                                "required": ["from", "to", "kind"],
                                "additionalProperties": false
                            },
                            "description": "Edges select existing relationships; the validator rejects edges absent from the impact graph."
                        }
                    },
                    "required": ["kind", "title"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["plan_version", "epoch", "focus", "forms"],
        "additionalProperties": false
    })
}

/// The `submit_visualization_plan` tool definition (schema generated from [`PlanParams`]).
#[must_use]
pub fn plan_tool() -> ToolDef {
    let schema = schemars::schema_for!(PlanParams);
    ToolDef {
        name: PLAN_TOOL_NAME,
        description: "Submit the final visualization plan. Required exactly once per request, after at most 8 read-only tool calls. All entities must be echoed from the digest or tool results.".into(),
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
    tracing::debug!(epoch = %plan.epoch, forms = plan.forms.len(), "plan parsed");
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codescope_core::{Epoch, FormKind, PlanNodeChange};

    fn sample_json(epoch: u64) -> Value {
        serde_json::json!({
            "plan_version": PLAN_VERSION,
            "epoch": epoch,
            "focus": "What breaks if I rename SessionStore.load?",
            "forms": [{
                "kind": "call_tree",
                "title": "Callers of load",
                "summary": "load has 3 callers.",
                "nodes": [{
                    "id": "n1",
                    "entity": {
                        "file": "src/session/store.rs",
                        "symbol": "SessionStore.load",
                        "range": {"start_line": 121, "start_col": 4, "end_line": 140, "end_col": 5}
                    },
                    "label": "load",
                    "change": "modified",
                    "children": ["n2"]
                }, {
                    "id": "n2",
                    "entity": {"file": "src/main.rs", "symbol": "main"},
                    "label": "main",
                    "change": "unchanged"
                }],
                "edges": [{"from": "n2", "to": "n1", "kind": "calls", "label": "on boot"}]
            }]
        })
    }

    #[test]
    fn parses_bare_plan() {
        let plan = parse_plan(&sample_json(7).to_string()).unwrap();
        assert_eq!(plan.epoch, Epoch(7));
        assert_eq!(plan.forms.len(), 1);
        assert_eq!(plan.forms[0].kind, FormKind::CallTree);
        assert_eq!(plan.forms[0].nodes[0].change, PlanNodeChange::Modified);
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
        // focus missing entirely.
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
        // The embedded plan schema names all 8 form kinds.
        let kinds = params["properties"]["plan"]["properties"]["forms"]["items"]["properties"]
            ["kind"]["enum"]
            .as_array()
            .unwrap();
        assert_eq!(kinds.len(), 8);
        for k in [
            "changed_symbol_tree",
            "call_tree",
            "type_impl_tree",
            "relationship_flow",
            "impact_summary",
            "focused_diff",
            "before_after",
            "sequence",
        ] {
            assert!(kinds.contains(&Value::String(k.into())), "missing {k}");
        }
        assert_eq!(
            params["properties"]["plan"]["properties"]["plan_version"]["const"],
            Value::from(PLAN_VERSION)
        );
    }

    #[test]
    fn schema_matches_core_serde_output() {
        // A core-typed plan round-trips through parse_plan, proving the serde dialect the
        // schema documents is the one core actually emits.
        let mut plan = VisualizationPlan::new(Epoch(9), "focus?");
        plan.forms.push(codescope_core::VizForm {
            kind: FormKind::ImpactSummary,
            title: "t".into(),
            summary: "s".into(),
            nodes: vec![codescope_core::PlanNode::new(
                "n1",
                "l",
                PlanNodeChange::Added,
            )],
            edges: vec![],
        });
        let text = serde_json::to_string(&plan).unwrap();
        let back = parse_plan(&text).unwrap();
        assert_eq!(back, plan);
    }
}
