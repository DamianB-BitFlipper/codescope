//! AI-assisted visualization: plan schema, validation report, AI status (research 05).
//!
//! The AI only *chooses and parameterizes* views; codescope owns facts, validation, and
//! rendering. Agents may build plans incrementally through [`DiagramDraft`](crate::DiagramDraft)
//! commands, but every finished projection passes the deterministic validation boundary (epoch
//! gate, entity resolution, semantic-edge evidence or Sequence transition adjacency, and hunks by
//! reference) before publication.
//!
//! Field names and enum values serialize exactly as in the research 05 §2 schema
//! (`snake_case` kinds, flat [`LineRange`](crate::LineRange) entities).

use crate::epoch::Epoch;
use crate::relation::DiagnosticSeverity;
use crate::semantic::EntityRef;

/// Current [`VisualizationPlan::plan_version`]. Bump on any schema change.
pub const PLAN_VERSION: u32 = 6;

/// Hard cap on nodes per form (Show Me rule S4; enforced at validation).
pub const MAX_FORM_NODES: usize = 12;

/// Hard cap on tree depth within a form (Show Me rule S4).
pub const MAX_FORM_DEPTH: usize = 3;

/// Maximum forms per plan ("one form per plan, two max").
pub const MAX_FORMS_PER_PLAN: usize = 2;

/// Maximum source references attached to a plan.
pub const MAX_PLAN_EVIDENCE: usize = 6;

/// Maximum exact diff ranges attached to one visual node.
pub const MAX_NODE_CODE_REFS: usize = 2;

/// Maximum inclusive source lines covered by one node code range.
pub const MAX_CODE_REF_LINES: u32 = 12;

/// A visualization plan: the AI's answer to one focus question, as renderable forms.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisualizationPlan {
    /// Schema version; must equal [`PLAN_VERSION`].
    pub plan_version: u32,
    /// Repo-state epoch echoed from the prompt; the validator gates on it.
    pub epoch: Epoch,
    /// One concise sentence explaining the change's intent or resulting behavior.
    pub intent: String,
    /// One or two forms ([`MAX_FORMS_PER_PLAN`]).
    #[serde(default)]
    pub forms: Vec<VizForm>,
    /// Typed source references supporting the visual, never the visual itself.
    #[serde(default)]
    pub evidence: Vec<PlanEvidence>,
}

impl VisualizationPlan {
    /// A new empty plan at the current schema version.
    #[must_use]
    pub fn new(epoch: Epoch) -> Self {
        VisualizationPlan {
            plan_version: PLAN_VERSION,
            epoch,
            intent: String::new(),
            forms: Vec::new(),
            evidence: Vec::new(),
        }
    }

    /// `true` when the plan carries no forms.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.forms.is_empty()
    }

    /// Number of forms in the plan.
    #[must_use]
    pub fn len(&self) -> usize {
        self.forms.len()
    }
}

/// One validated source reference explaining where a visual claim comes from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlanEvidence {
    /// Repo-relative source path.
    pub file: crate::FileId,
    /// Zero-based diff hunk index. The UI presents this as one-based.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hunk: Option<u32>,
    /// Fully-qualified symbol name, when the evidence is symbol-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// Exact source range, when supplied by the fact digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<crate::LineRange>,
    /// Why this source supports the description or visual.
    pub reason: String,
}

/// Visualization forms understood by core. Plan schema v6 accepts only the six structural
/// forms; `ImpactSummary` and `FocusedDiff` remain deserializable for stored/internal data
/// but are rejected at the AI validation boundary.
///
/// Serializes as `snake_case` (`"call_tree"`, `changed_symbol_tree`, …), matching the plan
/// JSON schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormKind {
    /// Diff-shaped symbol tree of the changed files.
    ChangedSymbolTree,
    /// Callers/callees tree around a focus symbol.
    CallTree,
    /// Type ↔ implementation tree (interfaces, implementers).
    TypeImplTree,
    /// Relationship flow graph (calls/imports/implements/contains edges).
    RelationshipFlow,
    /// Grouped counts + entry points (≤8 bullets).
    ImpactSummary,
    /// Subset of real hunks, ordered, with a one-line rationale each. Hunks are referenced
    /// by [`HunkId`](crate::HunkId) and re-read from git — never written by the AI.
    FocusedDiff,
    /// Two structural trees/diffs side by side (base vs worktree).
    BeforeAfter,
    /// Time-ordered interaction; nodes are participants.
    Sequence,
}

impl FormKind {
    /// Tree forms drop invalid nodes and re-parent children at validation; a bad root or
    /// >20% invalid nodes rejects the form (research 05 §3).
    #[must_use]
    pub fn is_tree_form(self) -> bool {
        matches!(
            self,
            FormKind::ChangedSymbolTree
                | FormKind::CallTree
                | FormKind::TypeImplTree
                | FormKind::BeforeAfter
        )
    }

    /// Flow/sequence forms: an invalid endpoint breaks ordering semantics → reject the form.
    #[must_use]
    pub fn is_flow_form(self) -> bool {
        matches!(self, FormKind::RelationshipFlow | FormKind::Sequence)
    }
}

/// One renderable form inside a [`VisualizationPlan`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VizForm {
    /// Which form to render.
    pub kind: FormKind,
    /// Plan nodes, keyed by [`PlanNode::id`] (≤ [`MAX_FORM_NODES`]).
    #[serde(default)]
    pub nodes: Vec<PlanNode>,
    /// Plan edges (flow/sequence/relationship forms).
    #[serde(default)]
    pub edges: Vec<PlanEdge>,
}

impl VizForm {
    /// Look up a node by plan-local id.
    #[must_use]
    pub fn node(&self, id: &str) -> Option<&PlanNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Number of nodes in the form.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
}

/// Which side of a unified diff an exact code range refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffSide {
    /// The pre-change (`-`) side. Use this for removed lines.
    Old,
    /// The post-change (`+`) side. Use this for added lines and post-change context.
    New,
}

/// An exact, hoverable range of lines in one diff hunk.
///
/// Line numbers are git-native (one-based) and both endpoints are inclusive. The hunk id is
/// zero-based, matching [`PlanEvidence::hunk`]. Validation confirms every line exists on the
/// selected side of that hunk before the range can drive UI highlighting.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PlanCodeRef {
    /// Repo-relative changed file.
    pub file: crate::FileId,
    /// Zero-based hunk index in diff order.
    pub hunk: u32,
    /// Old or new side of the diff.
    pub side: DiffSide,
    /// First one-based source line, inclusive.
    pub start_line: u32,
    /// Last one-based source line, inclusive.
    pub end_line: u32,
}

impl PlanCodeRef {
    /// Build an exact inclusive line range on one side of a hunk.
    #[must_use]
    pub fn new(
        file: crate::FileId,
        hunk: u32,
        side: DiffSide,
        start_line: u32,
        end_line: u32,
    ) -> Self {
        Self {
            file,
            hunk,
            side,
            start_line,
            end_line,
        }
    }
}

/// One node in a form. Ids are plan-local strings (`"n1"`, …) referenced by
/// [`PlanEdge`]s and [`PlanNode::children`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlanNode {
    /// Plan-local id.
    pub id: String,
    /// The fact-store entity this node represents; must resolve to exactly one entry
    /// (unresolvable = hallucination). `None` only for purely presentational nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<EntityRef>,
    /// Short display label (the TUI may re-derive it from the entity).
    pub label: String,
    /// Concise, always-visible explanation of this node's role or effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Optional deeper explanation shown on explicit expansion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expanded_detail: Option<String>,
    /// Exact diff ranges highlighted while this visual node is hovered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code_refs: Vec<PlanCodeRef>,
    /// Change badge. The default `unchanged` badge is omitted from serialized output.
    #[serde(default, skip_serializing_if = "PlanNodeChange::is_unchanged")]
    pub change: PlanNodeChange,
    /// Optional diagnostic badge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<DiagnosticSeverity>,
    /// Child node ids for tree forms.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<String>,
    /// Render hints only; never semantic. Default hints are omitted from serialized
    /// output.
    #[serde(default, skip_serializing_if = "NodeHint::is_default")]
    pub hint: NodeHint,
}

impl PlanNode {
    /// A node with no entity, severity, children, or hints.
    #[must_use]
    pub fn new(id: impl Into<String>, label: impl Into<String>, change: PlanNodeChange) -> Self {
        PlanNode {
            id: id.into(),
            entity: None,
            label: label.into(),
            detail: None,
            expanded_detail: None,
            code_refs: Vec::new(),
            change,
            severity: None,
            children: Vec::new(),
            hint: NodeHint::default(),
        }
    }

    /// Attach a fact-store entity.
    #[must_use]
    pub fn with_entity(mut self, entity: EntityRef) -> Self {
        self.entity = Some(entity);
        self
    }

    /// Attach a concise reviewer-facing explanation.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Attach optional detail shown when the rendered node box expands in place.
    #[must_use]
    pub fn with_expanded_detail(mut self, detail: impl Into<String>) -> Self {
        self.expanded_detail = Some(detail.into());
        self
    }

    /// Attach one exact hover-highlight range.
    #[must_use]
    pub fn with_code_ref(mut self, code_ref: PlanCodeRef) -> Self {
        self.code_refs.push(code_ref);
        self
    }
}

/// Change badge on a [`PlanNode`] (research 05 §2: `added|modified|removed|unchanged|diagnostic`).
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum PlanNodeChange {
    /// Added by the change-set.
    Added,
    /// Modified by the change-set.
    Modified,
    /// Removed by the change-set.
    Removed,
    /// Present for context, unchanged.
    #[default]
    Unchanged,
    /// Flagged because a diagnostic touches it.
    Diagnostic,
}

impl PlanNodeChange {
    /// `true` for the default [`PlanNodeChange::Unchanged`] badge; serialized plans omit
    /// the `change` field when this holds.
    #[must_use]
    pub fn is_unchanged(&self) -> bool {
        matches!(self, PlanNodeChange::Unchanged)
    }
}

/// Render hints for a [`PlanNode`] — presentation only, never semantic.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct NodeHint {
    /// Visually highlight the node.
    #[serde(default)]
    pub highlight: bool,
    /// Render the node collapsed.
    #[serde(default)]
    pub collapsed: bool,
}

impl NodeHint {
    /// `true` when no hints are set; serialized plans omit the `hint` field when this
    /// holds.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Kind of edge the AI may draw between plan nodes (research 05 §2).
///
/// Edges asserting `calls`/`implements`/`imports` must exist in the impact graph —
/// the AI selects edges, it never asserts new ones. [`PlanEdgeKind::FlowsTo`] is a
/// renderer-native chronological/control-flow transition, not a graph fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanEdgeKind {
    /// Renderer-native chronological/control-flow transition, not an impact-graph fact.
    FlowsTo,
    /// Call relationship.
    Calls,
    /// Import dependency.
    Imports,
    /// Interface/trait implementation.
    Implements,
    /// Containment (file ⊃ symbol, type ⊃ member).
    Contains,
    /// Data read.
    Reads,
    /// Data write.
    Writes,
}

/// A directed edge between plan nodes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlanEdge {
    /// Source plan-local node id.
    pub from: String,
    /// Target plan-local node id.
    pub to: String,
    /// Edge kind.
    pub kind: PlanEdgeKind,
    /// Optional edge label (e.g. "on cache miss").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Verdict of the deterministic plan-validation boundary (research 05 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationVerdict {
    /// Everything resolved; render as-is.
    Valid,
    /// Some nodes/edges/bullets were dropped; render the remainder.
    ValidWithDrops,
    /// The plan's epoch no longer matches repository state; do not publish it as current and
    /// request fresh generation. Prior validated structure may remain only as an untrusted seed.
    Stale,
    /// The plan (or every remaining form) is unusable; do not publish it.
    Rejected,
}

/// One item dropped during validation, for the plan-validation debug pane.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DroppedItem {
    /// What was dropped (e.g. `node n3 in form 0`, `form 1`, `bullet 2`).
    pub subject: String,
    /// Why it was dropped (e.g. "entity does not resolve", "edge not in impact graph").
    pub reason: String,
}

/// Result of validating a [`VisualizationPlan`] against the fact store.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ValidationReport {
    /// Overall verdict.
    pub verdict: ValidationVerdict,
    /// Everything that was dropped, with reasons.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<DroppedItem>,
    /// Free-form notes (re-parented children, re-resolved entities, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl ValidationReport {
    /// A clean validation.
    #[must_use]
    pub fn valid() -> Self {
        ValidationReport {
            verdict: ValidationVerdict::Valid,
            dropped: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Validation succeeded but dropped items.
    #[must_use]
    pub fn with_drops(dropped: Vec<DroppedItem>) -> Self {
        ValidationReport {
            verdict: ValidationVerdict::ValidWithDrops,
            dropped,
            notes: Vec::new(),
        }
    }

    /// The plan is stale (epoch mismatch).
    #[must_use]
    pub fn stale() -> Self {
        ValidationReport {
            verdict: ValidationVerdict::Stale,
            dropped: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// The plan is unusable; `reason` is recorded as a note.
    #[must_use]
    pub fn rejected(reason: impl Into<String>) -> Self {
        ValidationReport {
            verdict: ValidationVerdict::Rejected,
            dropped: Vec::new(),
            notes: vec![reason.into()],
        }
    }

    /// `true` when the plan may be rendered ([`ValidationVerdict::Valid`] or
    /// [`ValidationVerdict::ValidWithDrops`]).
    #[must_use]
    pub fn is_renderable(&self) -> bool {
        matches!(
            self.verdict,
            ValidationVerdict::Valid | ValidationVerdict::ValidWithDrops
        )
    }
}

/// Lifecycle of the AI subsystem, for the status bar.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiStatus {
    /// No plan requested or in flight.
    Idle,
    /// The selected file's asynchronous symbol inventory is not ready yet.
    WaitingForSymbols {
        /// Repo-state epoch whose symbols are being analyzed.
        epoch: Epoch,
    },
    /// The current selection is waiting for its navigation debounce to settle.
    Debouncing {
        /// Repo-state epoch the eventual request will explain.
        epoch: Epoch,
    },
    /// A plan request is in flight (started at `since_epoch`).
    Loading {
        /// Epoch the in-flight request was started against.
        since_epoch: Epoch,
    },
    /// A validated plan is available for `epoch`.
    Ready {
        /// Epoch the rendered plan was validated against.
        epoch: Epoch,
    },
    /// Repository state changed and generated output is being refreshed. Stale generated plans
    /// are not rendered as current; prior validated structure may be retained only as an untrusted seed.
    Stale {
        /// Current repository epoch awaiting a newly validated plan.
        epoch: Epoch,
    },
    /// The last generated request failed; deterministic impact remains available separately.
    Failed {
        /// Human-readable failure reason (never contains secrets).
        reason: String,
    },
}

impl AiStatus {
    /// `true` only when a current-epoch validated generated plan can be displayed.
    #[must_use]
    pub fn has_plan(&self) -> bool {
        matches!(self, AiStatus::Ready { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::FileId;
    use crate::position::LineRange;

    #[test]
    fn form_kind_classification() {
        assert!(FormKind::CallTree.is_tree_form());
        assert!(FormKind::ChangedSymbolTree.is_tree_form());
        assert!(FormKind::TypeImplTree.is_tree_form());
        assert!(FormKind::BeforeAfter.is_tree_form());
        assert!(!FormKind::Sequence.is_tree_form());
        assert!(FormKind::RelationshipFlow.is_flow_form());
        assert!(FormKind::Sequence.is_flow_form());
        assert!(!FormKind::ImpactSummary.is_flow_form());
        assert!(!FormKind::FocusedDiff.is_tree_form());
    }

    #[test]
    fn form_kind_serde_matches_research_schema() {
        let cases = [
            (FormKind::ChangedSymbolTree, "changed_symbol_tree"),
            (FormKind::CallTree, "call_tree"),
            (FormKind::TypeImplTree, "type_impl_tree"),
            (FormKind::RelationshipFlow, "relationship_flow"),
            (FormKind::ImpactSummary, "impact_summary"),
            (FormKind::FocusedDiff, "focused_diff"),
            (FormKind::BeforeAfter, "before_after"),
            (FormKind::Sequence, "sequence"),
        ];
        assert_eq!(cases.len(), 8);
        for (kind, json) in cases {
            assert_eq!(serde_json::to_value(kind).unwrap(), serde_json::json!(json));
            assert_eq!(
                serde_json::from_value::<FormKind>(serde_json::json!(json)).unwrap(),
                kind
            );
        }
    }

    #[test]
    fn plan_node_builder() {
        let file = FileId::new("src/session/store.rs").unwrap();
        let n = PlanNode::new("n1", "load", PlanNodeChange::Modified)
            .with_entity(EntityRef::for_symbol(
                file.clone(),
                "session::store::SessionStore::load",
                Some(LineRange::new(121, 4, 140, 5)),
            ))
            .with_detail("loads a session")
            .with_expanded_detail("Falls through to storage after a cache miss.")
            .with_code_ref(PlanCodeRef::new(file, 0, DiffSide::New, 122, 124));
        assert!(n.entity.is_some());
        assert_eq!(n.children.len(), 0);
        assert_eq!(n.code_refs[0].start_line, 122);
        assert!(n.expanded_detail.is_some());
        assert!(!n.hint.highlight);
    }

    #[test]
    fn validation_report_states() {
        assert!(ValidationReport::valid().is_renderable());
        let drops = ValidationReport::with_drops(vec![DroppedItem {
            subject: "node n3".into(),
            reason: "entity does not resolve".into(),
        }]);
        assert!(drops.is_renderable());
        assert_eq!(drops.dropped.len(), 1);
        assert!(!ValidationReport::stale().is_renderable());
        let rej = ValidationReport::rejected("root entity invalid");
        assert!(!rej.is_renderable());
        assert_eq!(rej.notes, ["root entity invalid"]);
    }

    #[test]
    fn ai_status_plan_availability() {
        assert!(!AiStatus::Idle.has_plan());
        assert!(
            !AiStatus::Loading {
                since_epoch: Epoch(1)
            }
            .has_plan()
        );
        assert!(AiStatus::Ready { epoch: Epoch(1) }.has_plan());
        assert!(!AiStatus::Stale { epoch: Epoch(1) }.has_plan());
        assert!(
            !AiStatus::Failed {
                reason: "boom".into()
            }
            .has_plan()
        );
    }

    #[test]
    fn viz_form_helpers() {
        let form = VizForm {
            kind: FormKind::CallTree,
            nodes: vec![PlanNode::new("n1", "load", PlanNodeChange::Modified)],
            edges: vec![],
        };
        assert_eq!(form.node_count(), 1);
        assert!(form.node("n1").is_some());
        assert!(form.node("n9").is_none());
    }

    /// Default node fields (`change: "unchanged"`, default `hint`) are omitted from
    /// serialized plans; deserialization restores them, so the round-trip is exact.
    #[test]
    fn default_fields_are_omitted_from_serialized_forms() {
        let form = VizForm {
            kind: FormKind::ChangedSymbolTree,
            nodes: vec![PlanNode::new("n1", "load", PlanNodeChange::Unchanged)],
            edges: Vec::new(),
        };

        let json = serde_json::to_value(&form).expect("serialize");
        let node = &json["nodes"][0];
        assert!(node.get("change").is_none(), "unchanged badge omitted");
        assert!(node.get("hint").is_none(), "default hint omitted");
        assert_eq!(node["label"], "load", "required fields stay");

        let back: VizForm = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, form, "defaults restore exactly");
        assert_eq!(back.nodes[0].change, PlanNodeChange::Unchanged);
        assert_eq!(back.nodes[0].hint, NodeHint::default());
    }

    /// Non-`unchanged` badges and nondefault hints keep serializing.
    #[test]
    fn nondefault_fields_are_preserved_in_serialized_forms() {
        let form = VizForm {
            kind: FormKind::ChangedSymbolTree,
            nodes: vec![PlanNode {
                hint: NodeHint {
                    highlight: true,
                    collapsed: true,
                },
                ..PlanNode::new("n1", "load", PlanNodeChange::Modified)
            }],
            edges: Vec::new(),
        };

        let json = serde_json::to_value(&form).expect("serialize");
        let node = &json["nodes"][0];
        assert_eq!(node["change"], "modified");
        assert_eq!(
            node["hint"],
            serde_json::json!({"highlight": true, "collapsed": true})
        );

        let back: VizForm = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, form, "preserved fields round-trip exactly");
    }
}
