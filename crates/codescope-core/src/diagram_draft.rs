//! Incremental, renderer-native diagram editing shared by AI tools and live controllers.
//!
//! A draft owns stable form ids while it is being edited. Converting it to a
//! [`VisualizationPlan`](crate::VisualizationPlan) strips those editor-only ids; the normal
//! fact validator remains the authority on whether the result may be rendered or published.

use crate::{
    DiagnosticSeverity, EntityRef, Epoch, FormKind, MAX_FORM_NODES, MAX_FORMS_PER_PLAN,
    MAX_NODE_CODE_REFS, MAX_PLAN_EVIDENCE, NodeHint, PLAN_VERSION, PlanCodeRef, PlanEdge,
    PlanEdgeKind, PlanEvidence, PlanNode, PlanNodeChange, VisualizationPlan, VizForm,
};

/// Defensive draft cap. AI-facing validation may impose a smaller presentation limit.
pub const MAX_DRAFT_EDGES: usize = 24;

/// Editable visualization state. `epoch` and `plan_version` are server-owned.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagramDraft {
    /// Current visualization schema version.
    pub plan_version: u32,
    /// Repository epoch this draft describes.
    pub epoch: Epoch,
    /// Reviewer-facing description displayed above the visual.
    pub intent: String,
    /// Editable forms with stable controller ids.
    pub forms: Vec<DiagramDraftForm>,
    /// Source evidence supporting the whole visual.
    pub evidence: Vec<PlanEvidence>,
}

impl DiagramDraft {
    /// Create an empty draft for a repository epoch.
    #[must_use]
    pub fn new(epoch: Epoch) -> Self {
        Self {
            plan_version: PLAN_VERSION,
            epoch,
            intent: String::new(),
            forms: Vec::new(),
            evidence: Vec::new(),
        }
    }

    /// Turn a previously validated plan into an editable draft.
    #[must_use]
    pub fn from_plan(plan: &VisualizationPlan) -> Self {
        Self {
            plan_version: PLAN_VERSION,
            epoch: plan.epoch,
            intent: plan.intent.clone(),
            forms: plan
                .forms
                .iter()
                .enumerate()
                .map(|(index, form)| DiagramDraftForm {
                    id: format!("form-{}", index + 1),
                    kind: form.kind,
                    nodes: form.nodes.clone(),
                    edges: form.edges.clone(),
                })
                .collect(),
            evidence: plan.evidence.clone(),
        }
    }

    /// Current plan-shaped projection. It is not trusted until the caller validates it.
    #[must_use]
    pub fn plan(&self) -> VisualizationPlan {
        VisualizationPlan {
            plan_version: PLAN_VERSION,
            epoch: self.epoch,
            intent: self.intent.clone(),
            forms: self
                .forms
                .iter()
                .map(|form| VizForm {
                    kind: form.kind,
                    nodes: form.nodes.clone(),
                    edges: form.edges.clone(),
                })
                .collect(),
            evidence: self.evidence.clone(),
        }
    }

    /// Apply one shared editor command atomically.
    pub fn apply(&mut self, command: &DiagramCommand) -> Result<String, DiagramEditError> {
        let mut next = self.clone();
        let summary = next.apply_inner(command)?;
        *self = next;
        Ok(summary)
    }

    fn apply_inner(&mut self, command: &DiagramCommand) -> Result<String, DiagramEditError> {
        match command {
            DiagramCommand::SetIntent { intent } => {
                ensure_text("intent", intent, 1_000)?;
                self.intent = intent.trim().to_string();
                Ok("updated the diagram intent".into())
            }
            DiagramCommand::CreateForm { form_id, kind } => {
                ensure_id("form_id", form_id)?;
                ensure_form_kind(*kind)?;
                if self.forms.len() >= MAX_FORMS_PER_PLAN {
                    return Err(DiagramEditError::Invalid(format!(
                        "a draft supports at most {MAX_FORMS_PER_PLAN} forms"
                    )));
                }
                if self.forms.iter().any(|form| form.id == *form_id) {
                    return Err(DiagramEditError::Invalid(format!(
                        "form {form_id:?} already exists"
                    )));
                }
                self.forms.push(DiagramDraftForm {
                    id: form_id.clone(),
                    kind: *kind,
                    nodes: Vec::new(),
                    edges: Vec::new(),
                });
                Ok(format!("created form {form_id}"))
            }
            DiagramCommand::DeleteForm { form_id } => {
                let before = self.forms.len();
                self.forms.retain(|form| form.id != *form_id);
                ensure_removed(before, self.forms.len(), "form", form_id)?;
                Ok(format!("deleted form {form_id}"))
            }
            DiagramCommand::CreateNode { form_id, node } => {
                ensure_node(node)?;
                let form = self.form_mut(form_id)?;
                if form.nodes.len() >= MAX_FORM_NODES {
                    return Err(DiagramEditError::Invalid(format!(
                        "form {form_id:?} supports at most {MAX_FORM_NODES} nodes"
                    )));
                }
                if form.nodes.iter().any(|existing| existing.id == node.id) {
                    return Err(DiagramEditError::Invalid(format!(
                        "node {:?} already exists in form {form_id:?}",
                        node.id
                    )));
                }
                form.nodes.push(node.clone());
                Ok(format!("created node {} in {form_id}", node.id))
            }
            DiagramCommand::UpdateNode {
                form_id,
                node_id,
                patch,
            } => {
                let node = self
                    .form_mut(form_id)?
                    .nodes
                    .iter_mut()
                    .find(|node| node.id == *node_id)
                    .ok_or_else(|| missing("node", node_id))?;
                patch.apply(node)?;
                ensure_node(node)?;
                Ok(format!("updated node {node_id} in {form_id}"))
            }
            DiagramCommand::DeleteNode { form_id, node_id } => {
                let form = self.form_mut(form_id)?;
                let before = form.nodes.len();
                form.nodes.retain(|node| node.id != *node_id);
                ensure_removed(before, form.nodes.len(), "node", node_id)?;
                form.edges
                    .retain(|edge| edge.from != *node_id && edge.to != *node_id);
                for node in &mut form.nodes {
                    node.children.retain(|child| child != node_id);
                }
                Ok(format!("deleted node {node_id} and its relationships"))
            }
            DiagramCommand::CreateEdge { form_id, edge } => {
                ensure_id("edge.from", &edge.from)?;
                ensure_id("edge.to", &edge.to)?;
                let form = self.form_mut(form_id)?;
                ensure_flows_to_form(form_id, form.kind, edge.kind)?;
                if form.edges.len() >= MAX_DRAFT_EDGES {
                    return Err(DiagramEditError::Invalid(format!(
                        "form {form_id:?} supports at most {MAX_DRAFT_EDGES} edges"
                    )));
                }
                if form
                    .edges
                    .iter()
                    .any(|existing| existing.from == edge.from && existing.to == edge.to)
                {
                    return Err(DiagramEditError::Invalid(format!(
                        "edge {} -> {} already exists in form {form_id:?}",
                        edge.from, edge.to
                    )));
                }
                form.edges.push(edge.clone());
                Ok(format!("created edge {} -> {}", edge.from, edge.to))
            }
            DiagramCommand::UpdateEdge {
                form_id,
                from,
                to,
                patch,
            } => {
                let form = self.form_mut(form_id)?;
                let form_kind = form.kind;
                let edge = form
                    .edges
                    .iter_mut()
                    .find(|edge| edge.from == *from && edge.to == *to)
                    .ok_or_else(|| missing("edge", &format!("{from} -> {to}")))?;
                patch.apply(edge)?;
                ensure_id("edge.from", &edge.from)?;
                ensure_id("edge.to", &edge.to)?;
                ensure_flows_to_form(form_id, form_kind, edge.kind)?;
                Ok(format!("updated edge {from} -> {to}"))
            }
            DiagramCommand::DeleteEdge { form_id, from, to } => {
                let form = self.form_mut(form_id)?;
                let before = form.edges.len();
                form.edges
                    .retain(|edge| edge.from != *from || edge.to != *to);
                ensure_removed(before, form.edges.len(), "edge", &format!("{from} -> {to}"))?;
                Ok(format!("deleted edge {from} -> {to}"))
            }
            DiagramCommand::AddEvidence { evidence } => {
                if self.evidence.len() >= MAX_PLAN_EVIDENCE {
                    return Err(DiagramEditError::Invalid(format!(
                        "a draft supports at most {MAX_PLAN_EVIDENCE} evidence items"
                    )));
                }
                ensure_text("evidence.reason", &evidence.reason, 2_000)?;
                self.evidence.push(evidence.clone());
                Ok(format!("added evidence item {}", self.evidence.len() - 1))
            }
            DiagramCommand::DeleteEvidence { index } => {
                if *index >= self.evidence.len() {
                    return Err(DiagramEditError::Invalid(format!(
                        "evidence index {index} does not exist"
                    )));
                }
                self.evidence.remove(*index);
                Ok(format!("deleted evidence item {index}"))
            }
            DiagramCommand::Finish => Ok("requested draft validation and publication".into()),
        }
    }

    fn form_mut(&mut self, form_id: &str) -> Result<&mut DiagramDraftForm, DiagramEditError> {
        self.forms
            .iter_mut()
            .find(|form| form.id == form_id)
            .ok_or_else(|| missing("form", form_id))
    }
}

/// One editable form. `id` exists only for mutation routing.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagramDraftForm {
    /// Stable id used by edit commands.
    pub id: String,
    /// Renderer-native visual grammar.
    pub kind: FormKind,
    /// Boxes in display/document order.
    pub nodes: Vec<PlanNode>,
    /// Directed relationships between node ids.
    pub edges: Vec<PlanEdge>,
}

/// A single atomic mutation supported by both model tools and the live controller API.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum DiagramCommand {
    /// Set the sentence displayed above the diagram.
    SetIntent {
        /// Concrete reviewer-facing sentence.
        intent: String,
    },
    /// Add an empty renderer-native form.
    CreateForm {
        /// Stable editor id, such as `main`.
        form_id: String,
        /// Visual grammar used to render the form.
        kind: FormKind,
    },
    /// Remove a form and all of its boxes/relationships.
    DeleteForm {
        /// Stable form id.
        form_id: String,
    },
    /// Add one complete box to a form.
    CreateNode {
        /// Stable form id.
        form_id: String,
        /// Complete renderer-native node.
        node: PlanNode,
    },
    /// Patch the content or structure of one box.
    UpdateNode {
        /// Stable form id.
        form_id: String,
        /// Plan-local node id.
        node_id: String,
        /// Fields to replace or clear.
        patch: DiagramNodePatch,
    },
    /// Remove one box, its edges, and child references.
    DeleteNode {
        /// Stable form id.
        form_id: String,
        /// Plan-local node id.
        node_id: String,
    },
    /// Add a directed relationship between boxes.
    CreateEdge {
        /// Stable form id.
        form_id: String,
        /// Complete edge.
        edge: PlanEdge,
    },
    /// Patch a relationship selected by its current endpoints.
    UpdateEdge {
        /// Stable form id.
        form_id: String,
        /// Current source node id.
        from: String,
        /// Current target node id.
        to: String,
        /// Fields to replace or clear.
        patch: DiagramEdgePatch,
    },
    /// Remove a relationship by endpoints.
    DeleteEdge {
        /// Stable form id.
        form_id: String,
        /// Source node id.
        from: String,
        /// Target node id.
        to: String,
    },
    /// Append one source citation.
    AddEvidence {
        /// Complete evidence item.
        evidence: PlanEvidence,
    },
    /// Remove one source citation by its current zero-based index.
    DeleteEvidence {
        /// Current zero-based evidence index.
        index: usize,
    },
    /// Ask the validator to publish the current draft.
    Finish,
}

/// Optional replacements for a box. `clear_*` flags distinguish deletion from omission.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiagramNodePatch {
    /// Replacement label.
    pub label: Option<String>,
    /// Replacement collapsed detail.
    pub detail: Option<String>,
    /// Remove collapsed detail.
    pub clear_detail: bool,
    /// Replacement expanded detail.
    pub expanded_detail: Option<String>,
    /// Remove expanded detail.
    pub clear_expanded_detail: bool,
    /// Replacement fact-store entity.
    pub entity: Option<EntityRef>,
    /// Remove the entity and make the box presentational.
    pub clear_entity: bool,
    /// Complete replacement code-reference list.
    pub code_refs: Option<Vec<PlanCodeRef>>,
    /// Replacement change badge.
    pub change: Option<PlanNodeChange>,
    /// Replacement diagnostic severity.
    pub severity: Option<DiagnosticSeverity>,
    /// Remove diagnostic severity.
    pub clear_severity: bool,
    /// Complete replacement child-id list.
    pub children: Option<Vec<String>>,
    /// Replacement render hints.
    pub hint: Option<NodeHint>,
}

impl DiagramNodePatch {
    fn apply(&self, node: &mut PlanNode) -> Result<(), DiagramEditError> {
        if let Some(label) = &self.label {
            node.label = label.clone();
        }
        if self.clear_detail {
            node.detail = None;
        } else if let Some(detail) = &self.detail {
            node.detail = Some(detail.clone());
        }
        if self.clear_expanded_detail {
            node.expanded_detail = None;
        } else if let Some(detail) = &self.expanded_detail {
            node.expanded_detail = Some(detail.clone());
        }
        if self.clear_entity {
            node.entity = None;
        } else if let Some(entity) = &self.entity {
            node.entity = Some(entity.clone());
        }
        if let Some(code_refs) = &self.code_refs {
            node.code_refs = code_refs.clone();
        }
        if let Some(change) = self.change {
            node.change = change;
        }
        if self.clear_severity {
            node.severity = None;
        } else if let Some(severity) = self.severity {
            node.severity = Some(severity);
        }
        if let Some(children) = &self.children {
            for child in children {
                ensure_id("child", child)?;
            }
            node.children = children.clone();
        }
        if let Some(hint) = self.hint {
            node.hint = hint;
        }
        Ok(())
    }
}

/// Optional replacements for a relationship.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiagramEdgePatch {
    /// Replacement source node id.
    pub from: Option<String>,
    /// Replacement target node id.
    pub to: Option<String>,
    /// Replacement typed relationship.
    pub kind: Option<PlanEdgeKind>,
    /// Replacement displayed relationship text.
    pub label: Option<String>,
    /// Remove displayed relationship text.
    pub clear_label: bool,
}

impl DiagramEdgePatch {
    fn apply(&self, edge: &mut PlanEdge) -> Result<(), DiagramEditError> {
        if let Some(from) = &self.from {
            edge.from = from.clone();
        }
        if let Some(to) = &self.to {
            edge.to = to.clone();
        }
        if let Some(kind) = self.kind {
            edge.kind = kind;
        }
        if self.clear_label {
            edge.label = None;
        } else if let Some(label) = &self.label {
            edge.label = Some(label.clone());
        }
        Ok(())
    }
}

/// A rejected draft mutation. The original draft remains unchanged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiagramEditError {
    /// The command would produce malformed or unbounded editor state.
    #[error("invalid diagram edit: {0}")]
    Invalid(String),
}

fn ensure_form_kind(kind: FormKind) -> Result<(), DiagramEditError> {
    if matches!(kind, FormKind::ImpactSummary | FormKind::FocusedDiff) {
        return Err(DiagramEditError::Invalid(format!(
            "form kind {kind:?} is legacy and cannot be created"
        )));
    }
    Ok(())
}

/// `flows_to` is renderer-native sequence grammar, never a relationship-flow graph fact.
fn ensure_flows_to_form(
    form_id: &str,
    form_kind: FormKind,
    edge_kind: PlanEdgeKind,
) -> Result<(), DiagramEditError> {
    if edge_kind == PlanEdgeKind::FlowsTo && form_kind != FormKind::Sequence {
        return Err(DiagramEditError::Invalid(format!(
            "edge kind flows_to is only valid in sequence form {form_id:?}"
        )));
    }
    Ok(())
}

fn ensure_node(node: &PlanNode) -> Result<(), DiagramEditError> {
    ensure_id("node.id", &node.id)?;
    ensure_text("node.label", &node.label, 512)?;
    if let Some(detail) = &node.detail {
        ensure_text("node.detail", detail, 2_000)?;
    }
    if let Some(detail) = &node.expanded_detail {
        ensure_text("node.expanded_detail", detail, 4_000)?;
    }
    if node.code_refs.len() > MAX_NODE_CODE_REFS {
        return Err(DiagramEditError::Invalid(format!(
            "node {:?} supports at most {MAX_NODE_CODE_REFS} code references",
            node.id
        )));
    }
    Ok(())
}

fn ensure_id(field: &str, id: &str) -> Result<(), DiagramEditError> {
    ensure_text(field, id, 128)
}

fn ensure_text(field: &str, text: &str, max_chars: usize) -> Result<(), DiagramEditError> {
    if text.trim().is_empty() {
        return Err(DiagramEditError::Invalid(format!(
            "{field} must not be empty"
        )));
    }
    if text.chars().count() > max_chars {
        return Err(DiagramEditError::Invalid(format!(
            "{field} exceeds {max_chars} characters"
        )));
    }
    Ok(())
}

fn ensure_removed(
    before: usize,
    after: usize,
    kind: &str,
    id: &str,
) -> Result<(), DiagramEditError> {
    if before == after {
        Err(missing(kind, id))
    } else {
        Ok(())
    }
}

fn missing(kind: &str, id: &str) -> DiagramEditError {
    DiagramEditError::Invalid(format!("{kind} {id:?} does not exist"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, label: &str) -> PlanNode {
        PlanNode::new(id, label, PlanNodeChange::Modified)
    }

    #[test]
    fn commands_build_update_and_delete_a_plan() {
        let mut draft = DiagramDraft::new(Epoch(7));
        draft
            .apply(&DiagramCommand::SetIntent {
                intent: "Requests now pass through the bounded queue.".into(),
            })
            .unwrap();
        draft
            .apply(&DiagramCommand::CreateForm {
                form_id: "main".into(),
                kind: FormKind::RelationshipFlow,
            })
            .unwrap();
        for (id, label) in [("request", "Request"), ("queue", "Queue")] {
            draft
                .apply(&DiagramCommand::CreateNode {
                    form_id: "main".into(),
                    node: node(id, label),
                })
                .unwrap();
        }
        draft
            .apply(&DiagramCommand::CreateEdge {
                form_id: "main".into(),
                edge: PlanEdge {
                    from: "request".into(),
                    to: "queue".into(),
                    kind: PlanEdgeKind::Writes,
                    label: Some("enqueues".into()),
                },
            })
            .unwrap();
        draft
            .apply(&DiagramCommand::UpdateNode {
                form_id: "main".into(),
                node_id: "queue".into(),
                patch: DiagramNodePatch {
                    label: Some("Active queue".into()),
                    detail: Some("Keeps up to sixteen requests".into()),
                    ..DiagramNodePatch::default()
                },
            })
            .unwrap();
        assert_eq!(draft.plan().forms[0].nodes[1].label, "Active queue");
        draft
            .apply(&DiagramCommand::DeleteNode {
                form_id: "main".into(),
                node_id: "request".into(),
            })
            .unwrap();
        assert!(draft.plan().forms[0].edges.is_empty());
    }

    #[test]
    fn flows_to_is_sequence_only_and_rejected_edits_are_atomic() {
        let mut draft = DiagramDraft::new(Epoch(1));
        for (form_id, kind) in [
            ("flow", FormKind::RelationshipFlow),
            ("sequence", FormKind::Sequence),
        ] {
            draft
                .apply(&DiagramCommand::CreateForm {
                    form_id: form_id.into(),
                    kind,
                })
                .unwrap();
        }

        let transition = PlanEdge {
            from: "first".into(),
            to: "second".into(),
            kind: PlanEdgeKind::FlowsTo,
            label: None,
        };
        let before = draft.clone();
        let error = draft
            .apply(&DiagramCommand::CreateEdge {
                form_id: "flow".into(),
                edge: transition.clone(),
            })
            .unwrap_err();
        assert!(error.to_string().contains("flows_to"));
        assert!(error.to_string().contains("sequence"));
        assert_eq!(draft, before, "failed create must not mutate the draft");

        draft
            .apply(&DiagramCommand::CreateEdge {
                form_id: "sequence".into(),
                edge: transition,
            })
            .unwrap();
        // Fact-aware validation decides whether a semantic sequence edge is supported; the
        // draft keeps the edit grammar renderer-neutral and accepts it provisionally.
        draft
            .apply(&DiagramCommand::CreateEdge {
                form_id: "sequence".into(),
                edge: PlanEdge {
                    from: "second".into(),
                    to: "third".into(),
                    kind: PlanEdgeKind::Calls,
                    label: None,
                },
            })
            .unwrap();

        draft
            .apply(&DiagramCommand::CreateEdge {
                form_id: "flow".into(),
                edge: PlanEdge {
                    from: "first".into(),
                    to: "second".into(),
                    kind: PlanEdgeKind::Calls,
                    label: None,
                },
            })
            .unwrap();
        let before = draft.clone();
        let error = draft
            .apply(&DiagramCommand::UpdateEdge {
                form_id: "flow".into(),
                from: "first".into(),
                to: "second".into(),
                patch: DiagramEdgePatch {
                    kind: Some(PlanEdgeKind::FlowsTo),
                    ..DiagramEdgePatch::default()
                },
            })
            .unwrap_err();
        assert!(error.to_string().contains("flows_to"));
        assert_eq!(draft, before, "failed update must not mutate the draft");
    }

    #[test]
    fn failed_edit_is_atomic() {
        let mut draft = DiagramDraft::new(Epoch(1));
        let before = draft.clone();
        let error = draft
            .apply(&DiagramCommand::DeleteForm {
                form_id: "missing".into(),
            })
            .unwrap_err();
        assert!(error.to_string().contains("does not exist"));
        assert_eq!(draft, before);
    }
}
