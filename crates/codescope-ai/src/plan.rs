//! Renderer-plan parsing and the AI-facing structural limits.
//!
//! Incremental diagram commands are projected to [`VisualizationPlan`] and passed through
//! this parser before deterministic fact validation. The old one-shot plan-submission tool
//! is intentionally absent.

use crate::error::AiError;
use codescope_core::{
    VisualizationPlan, MAX_CODE_REF_LINES, MAX_FORMS_PER_PLAN, MAX_NODE_CODE_REFS, PLAN_VERSION,
};
#[cfg(test)]
use serde_json::Value;

/// AI-facing hard cap on nodes per form.
pub const MAX_AI_FORM_NODES: usize = 5;

/// AI-facing hard cap on edges per form.
pub const MAX_AI_FORM_EDGES: usize = 8;

/// AI-facing hard cap on evidence entries.
pub const MAX_AI_EVIDENCE: usize = 4;

/// Parse the renderer plan projected from an incremental diagram draft.
///
/// Shape mismatches become [`AiError::MalformedPlan`], and a wrong `plan_version` becomes
/// [`AiError::PlanVersion`]. Semantic validity is decided by [`crate::validate`].
pub fn parse_plan(arguments: &str) -> Result<VisualizationPlan, AiError> {
    let plan: VisualizationPlan = serde_json::from_str(arguments.trim())
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

/// Enforce the AI-facing input contract: model-facing caps
/// (forms, nodes per form, evidence) that are tighter than the validator's deterministic
/// backstops. Unlike validation (which
/// truncates), this is a parse-time rejection: the last lifecycle node may be the point
/// of the diagram, so an oversized plan must be **rewritten**, not silently truncated.
/// Errors are [`AiError::MalformedPlan`] so the service's bounded repair loop can ask for
/// a corrected draft with the observed/allowed counts.
fn enforce_ai_input_contract(plan: &VisualizationPlan) -> Result<(), AiError> {
    if plan.forms.is_empty() {
        return Err(AiError::MalformedPlan(
            "plan has no forms; create exactly one structural form (two only for a distinct relationship)"
                .into(),
        ));
    }
    if plan.forms.len() > MAX_FORMS_PER_PLAN {
        let observed = plan.forms.len();
        return Err(AiError::MalformedPlan(format!(
            "plan has {observed} forms; the renderer contract allows at most {MAX_FORMS_PER_PLAN} - keep one form, two only for a distinct relationship"
        )));
    }
    for (index, form) in plan.forms.iter().enumerate() {
        if form.nodes.len() > MAX_AI_FORM_NODES {
            let observed = form.nodes.len();
            return Err(AiError::MalformedPlan(format!(
                "form {index} ({:?}) has {observed} nodes; the renderer contract allows at most {MAX_AI_FORM_NODES} - merge intermediate mechanics into decisive steps ending with the final lifecycle step",
                form.kind
            )));
        }
        if form.edges.len() > MAX_AI_FORM_EDGES {
            let observed = form.edges.len();
            return Err(AiError::MalformedPlan(format!(
                "form {index} ({:?}) has {observed} edges; the renderer contract allows at most {MAX_AI_FORM_EDGES} - keep only the decisive relationships",
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
            "plan has {observed} evidence entries; the renderer contract allows at most {MAX_AI_EVIDENCE} - keep the strongest items supporting the displayed description and visual"
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
    fn core_serde_output_parses_through_the_renderer_contract() {
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
