//! Impact graph: changed symbols and their 1-hop relationships (research 03, arch.).
//!
//! Nodes are fact-store entities ([`EntityRef`]); edges are typed by [`RelationKind`].
//! The graph is built by `codescope-analysis` from [`Evidence`]-wrapped relationship
//! queries and consumed by the TUI (render) and `codescope-ai` (edge-existence validation:
//! the AI may select edges, never assert new ones — research 05 §3).
//!
//! Node ids are producer-assigned strings, unique within the graph (recommended:
//! `file:fq_name`); they are *not* plan-local ids.

use crate::mapping::ChangeKind;
use crate::relation::{DiagnosticSeverity, RelationKind};
use crate::semantic::EntityRef;
use std::collections::HashSet;

/// One node in the impact graph.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ImpactNode {
    /// Graph-unique id (producer-assigned, e.g. `file:fq_name`).
    pub id: String,
    /// The fact-store entity this node represents (file-level when `symbol` is `None`).
    pub entity: EntityRef,
    /// How the entity changed; `None` for unchanged context (e.g. an unaffected caller).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<ChangeKind>,
    /// Worst diagnostic severity attached to this entity, for badge rendering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic_severity: Option<DiagnosticSeverity>,
}

/// A directed, typed edge between two impact nodes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ImpactEdge {
    /// Source node id.
    pub from: String,
    /// Target node id.
    pub to: String,
    /// Relationship from source to target.
    pub kind: RelationKind,
}

/// Whether a neighboring edge leaves or enters the queried node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeDirection {
    /// The queried node is the edge source.
    Outgoing,
    /// The queried node is the edge target.
    Incoming,
}

/// One neighbor of a node returned by [`ImpactGraph::neighbors`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Neighbor<'a> {
    /// The neighbor's node id.
    pub id: &'a str,
    /// The edge's relation kind.
    pub kind: RelationKind,
    /// Whether the edge goes out of or into the queried node.
    pub direction: EdgeDirection,
}

/// Counts returned by [`ImpactGraph::dedupe`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DedupeReport {
    /// Nodes removed because their id appeared earlier.
    pub nodes_removed: usize,
    /// Edges removed because an equal `(from, to, kind)` appeared earlier.
    pub edges_removed: usize,
}

/// A small directed graph of changed symbols and their relationships.
///
/// Preserves insertion order; use [`ImpactGraph::dedupe`] to collapse duplicate ids/edges
/// (stable: keeps the first occurrence) and [`ImpactGraph::prune_dangling_edges`] to drop
/// edges whose endpoints are missing.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ImpactGraph {
    /// All nodes (unique by `id` after dedupe).
    #[serde(default)]
    pub nodes: Vec<ImpactNode>,
    /// All edges (unique by `(from, to, kind)` after dedupe).
    #[serde(default)]
    pub edges: Vec<ImpactEdge>,
}

impl ImpactGraph {
    /// An empty graph.
    #[must_use]
    pub fn new() -> Self {
        ImpactGraph::default()
    }

    /// Append a node (may duplicate; call [`ImpactGraph::dedupe`] to collapse).
    pub fn add_node(&mut self, node: ImpactNode) {
        self.nodes.push(node);
    }

    /// Append an edge (may duplicate; call [`ImpactGraph::dedupe`] to collapse).
    pub fn add_edge(&mut self, edge: ImpactEdge) {
        self.edges.push(edge);
    }

    /// Number of nodes.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of edges.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// `true` when the graph has no nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Look up a node by id (first occurrence if duplicated).
    #[must_use]
    pub fn node(&self, id: &str) -> Option<&ImpactNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// `true` when an edge `(from, to, kind)` exists.
    #[must_use]
    pub fn contains_edge(&self, from: &str, to: &str, kind: RelationKind) -> bool {
        self.edges
            .iter()
            .any(|e| e.from == from && e.to == to && e.kind == kind)
    }

    /// Edges leaving `id`.
    pub fn edges_from<'a>(&'a self, id: &'a str) -> impl Iterator<Item = &'a ImpactEdge> + 'a {
        self.edges.iter().filter(move |e| e.from == id)
    }

    /// Edges entering `id`.
    pub fn edges_to<'a>(&'a self, id: &'a str) -> impl Iterator<Item = &'a ImpactEdge> + 'a {
        self.edges.iter().filter(move |e| e.to == id)
    }

    /// All neighbors of `id` in both directions (a self-loop yields two entries).
    #[must_use]
    pub fn neighbors(&self, id: &str) -> Vec<Neighbor<'_>> {
        let mut out = Vec::new();
        for e in &self.edges {
            if e.from == id {
                out.push(Neighbor {
                    id: e.to.as_str(),
                    kind: e.kind,
                    direction: EdgeDirection::Outgoing,
                });
            }
            if e.to == id {
                out.push(Neighbor {
                    id: e.from.as_str(),
                    kind: e.kind,
                    direction: EdgeDirection::Incoming,
                });
            }
        }
        out
    }

    /// Nodes that changed (have a [`ChangeKind`]).
    pub fn changed_nodes(&self) -> impl Iterator<Item = &ImpactNode> {
        self.nodes.iter().filter(|n| n.change.is_some())
    }

    /// Nodes carrying a diagnostic badge.
    pub fn nodes_with_diagnostics(&self) -> impl Iterator<Item = &ImpactNode> {
        self.nodes
            .iter()
            .filter(|n| n.diagnostic_severity.is_some())
    }

    /// Remove duplicate nodes (same id, keeps first) and duplicate edges
    /// (same `(from, to, kind)`, keeps first). Stable: preserves first-occurrence order.
    pub fn dedupe(&mut self) -> DedupeReport {
        let nodes_before = self.nodes.len();
        let mut seen_nodes: HashSet<String> = HashSet::new();
        self.nodes.retain(|n| seen_nodes.insert(n.id.clone()));
        let edges_before = self.edges.len();
        let mut seen_edges: HashSet<(String, String, RelationKind)> = HashSet::new();
        self.edges
            .retain(|e| seen_edges.insert((e.from.clone(), e.to.clone(), e.kind)));
        DedupeReport {
            nodes_removed: nodes_before - self.nodes.len(),
            edges_removed: edges_before - self.edges.len(),
        }
    }

    /// Drop edges whose endpoints are not present in `nodes`; returns how many were dropped.
    pub fn prune_dangling_edges(&mut self) -> usize {
        let ids: HashSet<&str> = self.nodes.iter().map(|n| n.id.as_str()).collect();
        let before = self.edges.len();
        self.edges
            .retain(|e| ids.contains(e.from.as_str()) && ids.contains(e.to.as_str()));
        before - self.edges.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::FileId;

    fn node(id: &str, change: Option<ChangeKind>) -> ImpactNode {
        ImpactNode {
            id: id.to_string(),
            entity: EntityRef::for_file(FileId::new(format!("{id}.go")).unwrap()),
            change,
            diagnostic_severity: None,
        }
    }

    fn edge(from: &str, to: &str, kind: RelationKind) -> ImpactEdge {
        ImpactEdge {
            from: from.to_string(),
            to: to.to_string(),
            kind,
        }
    }

    fn sample_graph() -> ImpactGraph {
        let mut g = ImpactGraph::new();
        g.add_node(node("a", Some(ChangeKind::Modified)));
        g.add_node(node("b", None));
        g.add_node(node("c", None));
        g.add_edge(edge("b", "a", RelationKind::Calls));
        g.add_edge(edge("c", "a", RelationKind::Calls));
        g.add_edge(edge("a", "c", RelationKind::References));
        g
    }

    #[test]
    fn neighbors_cover_both_directions() {
        let g = sample_graph();
        let ns = g.neighbors("a");
        assert_eq!(ns.len(), 3);
        let b = ns.iter().find(|n| n.id == "b").unwrap();
        assert_eq!(b.kind, RelationKind::Calls);
        assert_eq!(b.direction, EdgeDirection::Incoming);
        let c_ref = ns
            .iter()
            .find(|n| n.id == "c" && n.kind == RelationKind::References)
            .unwrap();
        assert_eq!(c_ref.direction, EdgeDirection::Outgoing);
    }

    #[test]
    fn edges_from_and_to() {
        let g = sample_graph();
        assert_eq!(g.edges_from("a").count(), 1);
        assert_eq!(g.edges_to("a").count(), 2);
        assert!(g.contains_edge("b", "a", RelationKind::Calls));
        assert!(!g.contains_edge("a", "b", RelationKind::Calls));
        assert_eq!(g.node("b").unwrap().entity.file.to_string(), "b.go");
        assert!(g.node("zzz").is_none());
    }

    #[test]
    fn dedupe_keeps_first_and_reports_counts() {
        let mut g = sample_graph();
        g.add_node(node("a", Some(ChangeKind::Added))); // duplicate id
        g.add_edge(edge("b", "a", RelationKind::Calls)); // duplicate edge
        let report = g.dedupe();
        assert_eq!(
            report,
            DedupeReport {
                nodes_removed: 1,
                edges_removed: 1
            }
        );
        assert_eq!(g.node_count(), 3);
        assert_eq!(g.edge_count(), 3);
        // First occurrence wins.
        assert_eq!(g.node("a").unwrap().change, Some(ChangeKind::Modified));
    }

    #[test]
    fn prune_dangling_edges() {
        let mut g = sample_graph();
        g.add_edge(edge("a", "ghost", RelationKind::Calls));
        assert_eq!(g.prune_dangling_edges(), 1);
        assert_eq!(g.edge_count(), 3);
        assert_eq!(g.prune_dangling_edges(), 0);
    }

    #[test]
    fn changed_and_diagnostic_filters() {
        let mut g = sample_graph();
        assert_eq!(g.changed_nodes().count(), 1);
        assert_eq!(g.nodes_with_diagnostics().count(), 0);
        g.add_node(ImpactNode {
            id: "d".to_string(),
            entity: EntityRef::for_file(FileId::new("d.go").unwrap()),
            change: None,
            diagnostic_severity: Some(DiagnosticSeverity::Warning),
        });
        assert_eq!(g.nodes_with_diagnostics().count(), 1);
        assert!(!g.is_empty());
    }
}
