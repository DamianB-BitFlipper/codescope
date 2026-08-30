//! 1-hop impact graph over the language service (research 03 / architecture).
//!
//! [`build_impact_graph`] lazily fetches, per changed symbol: callers
//! (`callHierarchy/incomingCalls`), callees (`outgoingCalls`), and — for interfaces —
//! implementers (`textDocument/implementation`, falling back to `typeHierarchy/subtypes`).
//! Every query is gated on the resolved [`FeatureSet`](codescope_core::FeatureSet):
//! unsupported relations are skipped silently and recorded in the graph-level notes.
//!
//! The result is wrapped in [`Evidence`]: completeness is the worst across all underlying
//! queries (a timeout or truncated answer degrades the whole graph to `Partial`, with a
//! note saying why), matching the honesty-layer rule that codescope never claims a
//! complete project graph.

use std::collections::BTreeSet;

use codescope_core::{
    ChangeKind, Completeness, Diagnostic, EntityRef, Evidence, Feature, ImpactEdge, ImpactGraph,
    ImpactNode, Position, RelationKind, Revision, SymbolKind, SymbolRef,
};
use codescope_lsp::SemanticError;

use crate::changes::ChangedSymbolInfo;
use crate::source::SemanticSource;

/// Cap on neighbors kept per relationship query; overflow degrades the graph to
/// [`Completeness::Partial`] with a note.
pub const MAX_NEIGHBORS_PER_QUERY: usize = 50;

/// Build the 1-hop impact graph around `changed` symbols using `svc`.
///
/// Deleted symbols (base revision) get a node but no live queries — the language server
/// only knows the worktree. Duplicate nodes/edges from symmetric queries (A calls B where
/// both changed) are collapsed via [`ImpactGraph::dedupe`]; changed nodes are inserted
/// first, so dedupe keeps their change annotation.
pub async fn build_impact_graph<S: SemanticSource>(
    changed: &[ChangedSymbolInfo],
    svc: &S,
) -> Evidence<ImpactGraph> {
    let mut graph = ImpactGraph::new();
    let mut acc = EvidenceAccumulator::default();

    // Changed nodes first (dedupe keeps first occurrence → annotations survive).
    for info in changed {
        graph.add_node(ImpactNode {
            id: node_id_for(info),
            entity: EntityRef::for_symbol(info.file.clone(), info.name.clone(), Some(info.range)),
            change: Some(info.record.change_kind),
            diagnostic_severity: None,
        });
    }

    let features = svc.features();
    let mut skipped_deleted = 0usize;
    for info in changed {
        if info.revision == Revision::Base || info.record.change_kind == ChangeKind::Deleted {
            skipped_deleted += 1;
            continue;
        }
        let id = node_id_for(info);
        let pos = info.selection.start();

        // Callers.
        if features.is_supported(Feature::CallHierarchyIncoming) {
            if let Some(callers) = acc.run(
                "incoming calls",
                &id,
                svc.incoming_calls(&info.file, pos).await,
            ) {
                for peer in callers {
                    let peer_id = ref_id(&peer);
                    add_neighbor(&mut graph, &peer, &peer_id);
                    graph.add_edge(ImpactEdge {
                        from: peer_id,
                        to: id.clone(),
                        kind: RelationKind::Calls,
                    });
                }
            }
        } else {
            acc.skip(Feature::CallHierarchyIncoming);
        }

        // Callees.
        if features.is_supported(Feature::CallHierarchyOutgoing) {
            if let Some(callees) = acc.run(
                "outgoing calls",
                &id,
                svc.outgoing_calls(&info.file, pos).await,
            ) {
                for peer in callees {
                    let peer_id = ref_id(&peer);
                    add_neighbor(&mut graph, &peer, &peer_id);
                    graph.add_edge(ImpactEdge {
                        from: id.clone(),
                        to: peer_id,
                        kind: RelationKind::Calls,
                    });
                }
            }
        } else {
            acc.skip(Feature::CallHierarchyOutgoing);
        }

        // Implementers of changed interfaces.
        if info.kind == SymbolKind::Interface {
            if let Some(impls) = query_implementers(svc, &mut acc, info, &id, pos).await {
                for peer in impls {
                    let peer_id = ref_id(&peer);
                    add_neighbor(&mut graph, &peer, &peer_id);
                    graph.add_edge(ImpactEdge {
                        from: peer_id,
                        to: id.clone(),
                        kind: RelationKind::Implements,
                    });
                }
            }
        }
    }

    if skipped_deleted > 0 {
        acc.notes.insert(format!(
            "no live queries for {skipped_deleted} deleted symbol(s) (base revision)"
        ));
    }

    let report = graph.dedupe();
    let completeness = acc.completeness();
    tracing::debug!(
        nodes = graph.node_count(),
        edges = graph.edge_count(),
        nodes_deduped = report.nodes_removed,
        edges_deduped = report.edges_removed,
        ?completeness,
        "built impact graph"
    );
    Evidence {
        value: graph,
        completeness,
        notes: acc.notes.into_iter().collect(),
    }
}

/// Implementers of an interface: `textDocument/implementation` when supported, else
/// `typeHierarchy/subtypes` (for a Go interface, subtypes are its implementers).
async fn query_implementers<S: SemanticSource>(
    svc: &S,
    acc: &mut EvidenceAccumulator,
    info: &ChangedSymbolInfo,
    id: &str,
    pos: Position,
) -> Option<Vec<SymbolRef>> {
    let features = svc.features();
    if features.is_supported(Feature::Implementation) {
        return acc.run(
            "implementations",
            id,
            svc.implementations(&info.file, pos).await,
        );
    }
    if features.is_supported(Feature::TypeHierarchySub) {
        acc.notes
            .insert("implementation unsupported; used type-hierarchy subtypes".to_string());
        return acc.run(
            "type subtypes",
            id,
            svc.type_subtypes(&info.file, pos).await,
        );
    }
    acc.skip(Feature::Implementation);
    None
}

/// Annotate graph nodes with the worst diagnostic severity touching their entity.
///
/// Nodes with a range use line intersection; range-less nodes (1-hop neighbors) match any
/// diagnostic in their file. Severity ordering follows the enum (`Error` is worst).
pub fn annotate_diagnostics(graph: &mut ImpactGraph, diagnostics: &[Diagnostic]) {
    for node in &mut graph.nodes {
        let worst = diagnostics
            .iter()
            .filter(|d| {
                d.file == node.entity.file
                    && node
                        .entity
                        .range
                        .is_none_or(|r| r.intersects_lines(&d.range))
            })
            .map(|d| d.severity)
            .min();
        node.diagnostic_severity = worst;
    }
}

/// Graph node id for a changed symbol: `file:qualified_name` (impact-graph convention).
#[must_use]
pub fn node_id_for(info: &ChangedSymbolInfo) -> String {
    format!("{}:{}", info.file, info.name)
}

fn ref_id(peer: &SymbolRef) -> String {
    format!("{}:{}", peer.file, peer.name)
}

fn add_neighbor(graph: &mut ImpactGraph, peer: &SymbolRef, peer_id: &str) {
    graph.add_node(ImpactNode {
        id: peer_id.to_string(),
        entity: EntityRef::for_symbol(peer.file.clone(), peer.name.clone(), None),
        change: None,
        diagnostic_severity: None,
    });
}

/// Folds per-query [`Evidence`] metadata (and errors) into graph-level honesty data.
///
/// Final completeness: `Partial` when any query was definitely partial (or failed),
/// else `Unknown` when any query's completeness was indeterminate, else `Complete`.
#[derive(Default)]
struct EvidenceAccumulator {
    has_partial: bool,
    has_unknown: bool,
    notes: BTreeSet<String>,
}

impl EvidenceAccumulator {
    fn completeness(&self) -> Completeness {
        if self.has_partial {
            Completeness::Partial
        } else if self.has_unknown {
            Completeness::Unknown
        } else {
            Completeness::Complete
        }
    }

    /// Unwrap one query result: folds completeness/notes, converts errors into a
    /// `Partial` downgrade plus note (the graph is best-effort), truncates oversized
    /// answers.
    fn run<T>(
        &mut self,
        what: &str,
        node: &str,
        result: Result<Evidence<Vec<T>>, SemanticError>,
    ) -> Option<Vec<T>> {
        match result {
            Ok(ev) => {
                self.merge_completeness(ev.completeness);
                for note in ev.notes {
                    self.notes.insert(format!("{what} for {node}: {note}"));
                }
                let mut value = ev.value;
                if value.len() > MAX_NEIGHBORS_PER_QUERY {
                    self.merge_completeness(Completeness::Partial);
                    self.notes.insert(format!(
                        "{what} for {node}: kept {MAX_NEIGHBORS_PER_QUERY} of {} results",
                        value.len()
                    ));
                    value.truncate(MAX_NEIGHBORS_PER_QUERY);
                }
                Some(value)
            }
            Err(SemanticError::Unsupported(feature)) => {
                // Feature gating should have prevented the call; the server lied.
                self.notes
                    .insert(format!("{what} for {node}: server rejected {feature:?}"));
                None
            }
            Err(err) => {
                self.merge_completeness(Completeness::Partial);
                self.notes
                    .insert(format!("{what} for {node} failed: {err}"));
                tracing::warn!(query = what, node, error = %err, "impact-graph query failed");
                None
            }
        }
    }

    fn skip(&mut self, feature: Feature) {
        self.notes
            .insert(format!("skipped {feature:?} (unsupported by server)"));
    }

    fn merge_completeness(&mut self, other: Completeness) {
        match other {
            Completeness::Complete => {}
            Completeness::Partial => self.has_partial = true,
            Completeness::Unknown => self.has_unknown = true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{Reply, ScriptedSource};
    use codescope_core::{
        Availability, ChangedSymbol, DiagnosticSeverity, FeatureSet, FileId, LineRange,
        MappingConfidence, SymbolId,
    };

    fn info(file: &str, name: &str, kind: SymbolKind, line: u32) -> ChangedSymbolInfo {
        ChangedSymbolInfo {
            file: FileId::new(file).unwrap(),
            name: name.to_string(),
            kind,
            detail: None,
            range: LineRange::new(line, 0, line + 10, 1),
            selection: LineRange::new(line, 5, line, 10),
            revision: Revision::Worktree,
            record: ChangedSymbol::new(
                SymbolId::new("0"),
                ChangeKind::Modified,
                vec![],
                MappingConfidence::Exact,
            ),
            signature_touch: false,
        }
    }

    fn sref(file: &str, name: &str) -> SymbolRef {
        SymbolRef {
            file: FileId::new(file).unwrap(),
            name: name.to_string(),
            kind: SymbolKind::Function,
        }
    }

    fn call_features() -> FeatureSet {
        let mut f = FeatureSet::new();
        f.set(Feature::CallHierarchyIncoming, Availability::Supported);
        f.set(Feature::CallHierarchyOutgoing, Availability::Supported);
        f.set(Feature::Implementation, Availability::Supported);
        f
    }

    fn key(info: &ChangedSymbolInfo) -> (FileId, Position) {
        (info.file.clone(), info.selection.start())
    }

    #[tokio::test]
    async fn dedupes_symmetric_edges_and_keeps_change_annotations() {
        // main calls Hello; both changed → incoming(Hello)=main, outgoing(main)=Hello
        // produce the same edge twice and re-introduce existing nodes.
        let main = info("cmd/main.go", "main", SymbolKind::Function, 3);
        let hello = info("pkg/greet.go", "Hello", SymbolKind::Function, 10);
        let mut svc = ScriptedSource {
            features: call_features(),
            ..ScriptedSource::default()
        };
        svc.outgoing.insert(
            key(&main),
            Reply::Ok(Evidence::complete(vec![sref("pkg/greet.go", "Hello")])),
        );
        svc.incoming.insert(
            key(&hello),
            Reply::Ok(Evidence::complete(vec![sref("cmd/main.go", "main")])),
        );

        let ev = build_impact_graph(&[main, hello], &svc).await;
        assert_eq!(ev.completeness, Completeness::Complete);
        let g = &ev.value;
        assert_eq!(
            g.node_count(),
            2,
            "neighbor nodes dedupe into changed nodes"
        );
        assert_eq!(g.edge_count(), 1, "symmetric queries dedupe into one edge");
        assert!(g.contains_edge(
            "cmd/main.go:main",
            "pkg/greet.go:Hello",
            RelationKind::Calls
        ));
        // First-inserted (changed) nodes keep their annotations.
        assert_eq!(
            g.node("cmd/main.go:main").unwrap().change,
            Some(ChangeKind::Modified)
        );
        assert_eq!(
            g.node("pkg/greet.go:Hello").unwrap().change,
            Some(ChangeKind::Modified)
        );
    }

    #[tokio::test]
    async fn unsupported_features_are_skipped_silently_with_notes() {
        let mut features = FeatureSet::new();
        features.set(Feature::CallHierarchyIncoming, Availability::Supported);
        features.set(Feature::CallHierarchyOutgoing, Availability::Unsupported);
        let main = info("cmd/main.go", "main", SymbolKind::Function, 3);
        let mut svc = ScriptedSource {
            features,
            ..ScriptedSource::default()
        };
        svc.incoming.insert(
            key(&main),
            Reply::Ok(Evidence::complete(vec![sref("pkg/api.go", "Serve")])),
        );

        let ev = build_impact_graph(&[main], &svc).await;
        assert_eq!(ev.completeness, Completeness::Complete);
        assert_eq!(svc.calls_of("outgoing_calls"), 0, "gated before the wire");
        assert_eq!(svc.calls_of("implementations"), 0, "not an interface");
        assert!(ev.notes.iter().any(|n| n.contains("CallHierarchyOutgoing")));
        assert!(ev.value.contains_edge(
            "pkg/api.go:Serve",
            "cmd/main.go:main",
            RelationKind::Calls
        ));
    }

    #[tokio::test]
    async fn partial_evidence_and_query_errors_degrade_completeness() {
        let main = info("cmd/main.go", "main", SymbolKind::Function, 3);
        let mut svc = ScriptedSource {
            features: call_features(),
            ..ScriptedSource::default()
        };
        svc.incoming.insert(
            key(&main),
            Reply::Ok(Evidence::partial(
                vec![sref("pkg/api.go", "Serve")],
                vec!["truncated by server".to_string()],
            )),
        );
        svc.outgoing.insert(key(&main), Reply::Timeout);

        let ev = build_impact_graph(&[main], &svc).await;
        assert_eq!(ev.completeness, Completeness::Partial);
        assert!(ev.notes.iter().any(|n| n.contains("truncated by server")));
        assert!(ev.notes.iter().any(|n| n.contains("timed out")));
        // The successful query still contributed its edge.
        assert!(ev.value.contains_edge(
            "pkg/api.go:Serve",
            "cmd/main.go:main",
            RelationKind::Calls
        ));
    }

    #[tokio::test]
    async fn interfaces_get_implementers_via_implementation_feature() {
        let iface = info("pkg/store.go", "Repository", SymbolKind::Interface, 5);
        let mut svc = ScriptedSource {
            features: call_features(),
            ..ScriptedSource::default()
        };
        svc.impls.insert(
            key(&iface),
            Reply::Ok(Evidence::complete(vec![sref(
                "pkg/postgres.go",
                "PostgresRepo",
            )])),
        );

        let ev = build_impact_graph(&[iface], &svc).await;
        assert_eq!(svc.calls_of("implementations"), 1);
        assert_eq!(svc.calls_of("type_subtypes"), 0);
        assert!(ev.value.contains_edge(
            "pkg/postgres.go:PostgresRepo",
            "pkg/store.go:Repository",
            RelationKind::Implements
        ));
    }

    #[tokio::test]
    async fn implementation_falls_back_to_type_subtypes() {
        let mut features = FeatureSet::new();
        features.set(Feature::Implementation, Availability::Unsupported);
        features.set(Feature::TypeHierarchySub, Availability::Supported);
        let iface = info("pkg/store.go", "Repository", SymbolKind::Interface, 5);
        let mut svc = ScriptedSource {
            features,
            ..ScriptedSource::default()
        };
        svc.subtypes.insert(
            key(&iface),
            Reply::Ok(Evidence::complete(vec![sref(
                "pkg/memory.go",
                "MemoryRepo",
            )])),
        );

        let ev = build_impact_graph(&[iface], &svc).await;
        assert_eq!(svc.calls_of("implementations"), 0);
        assert_eq!(svc.calls_of("type_subtypes"), 1);
        assert!(ev.notes.iter().any(|n| n.contains("subtypes")));
        assert!(ev.value.contains_edge(
            "pkg/memory.go:MemoryRepo",
            "pkg/store.go:Repository",
            RelationKind::Implements
        ));
    }

    #[tokio::test]
    async fn oversized_answers_are_capped_with_partial_note() {
        let main = info("cmd/main.go", "main", SymbolKind::Function, 3);
        let callers: Vec<SymbolRef> = (0..60)
            .map(|i| sref("pkg/x.go", &format!("f{i}")))
            .collect();
        let mut svc = ScriptedSource {
            features: call_features(),
            ..ScriptedSource::default()
        };
        svc.incoming
            .insert(key(&main), Reply::Ok(Evidence::complete(callers)));

        let ev = build_impact_graph(&[main], &svc).await;
        assert_eq!(ev.completeness, Completeness::Partial);
        assert!(ev.notes.iter().any(|n| n.contains("kept 50 of 60")));
        // 1 changed node + 50 capped neighbors.
        assert_eq!(ev.value.node_count(), 51);
        assert_eq!(ev.value.edge_count(), 50);
    }

    #[tokio::test]
    async fn deleted_symbols_get_nodes_but_no_queries() {
        let mut deleted = info("pkg/old.go", "Legacy", SymbolKind::Function, 4);
        deleted.revision = Revision::Base;
        deleted.record.change_kind = ChangeKind::Deleted;
        let svc = ScriptedSource {
            features: call_features(),
            ..ScriptedSource::default()
        };

        let ev = build_impact_graph(&[deleted], &svc).await;
        assert_eq!(svc.calls_of("incoming_calls"), 0);
        assert_eq!(svc.calls_of("outgoing_calls"), 0);
        assert_eq!(ev.value.node_count(), 1);
        assert_eq!(
            ev.value.node("pkg/old.go:Legacy").unwrap().change,
            Some(ChangeKind::Deleted)
        );
        assert!(ev.notes.iter().any(|n| n.contains("deleted symbol")));
        assert_eq!(ev.completeness, Completeness::Complete);
    }

    #[tokio::test]
    async fn diagnostics_annotate_worst_severity() {
        let main = info("cmd/main.go", "main", SymbolKind::Function, 3);
        let mut svc = ScriptedSource {
            features: call_features(),
            ..ScriptedSource::default()
        };
        svc.incoming.insert(
            key(&main),
            Reply::Ok(Evidence::complete(vec![sref("pkg/api.go", "Serve")])),
        );
        let mut ev = build_impact_graph(&[main], &svc).await;

        let diags = vec![
            codescope_core::Diagnostic {
                file: FileId::new("cmd/main.go").unwrap(),
                range: LineRange::new(4, 0, 4, 5),
                severity: DiagnosticSeverity::Warning,
                code: None,
                message: "w".into(),
                source: None,
            },
            codescope_core::Diagnostic {
                file: FileId::new("cmd/main.go").unwrap(),
                range: LineRange::new(5, 0, 5, 5),
                severity: DiagnosticSeverity::Error,
                code: None,
                message: "e".into(),
                source: None,
            },
            // Outside main's range → does not affect the ranged node.
            codescope_core::Diagnostic {
                file: FileId::new("cmd/main.go").unwrap(),
                range: LineRange::new(90, 0, 90, 5),
                severity: DiagnosticSeverity::Error,
                code: None,
                message: "far".into(),
                source: None,
            },
            // Neighbor (no range) matches any diagnostic in its file.
            codescope_core::Diagnostic {
                file: FileId::new("pkg/api.go").unwrap(),
                range: LineRange::new(1, 0, 1, 2),
                severity: DiagnosticSeverity::Hint,
                code: None,
                message: "h".into(),
                source: None,
            },
        ];
        annotate_diagnostics(&mut ev.value, &diags);
        assert_eq!(
            ev.value
                .node("cmd/main.go:main")
                .unwrap()
                .diagnostic_severity,
            Some(DiagnosticSeverity::Error)
        );
        assert_eq!(
            ev.value
                .node("pkg/api.go:Serve")
                .unwrap()
                .diagnostic_severity,
            Some(DiagnosticSeverity::Hint)
        );
    }
}
