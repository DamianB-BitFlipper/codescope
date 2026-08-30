//! Integration-style tests: impact-graph construction and query through the public API,
//! shaped the way `codescope-analysis`/`codescope-ai` will use it.

use codescope_core::*;

fn sym_node(id: &str, file: &str, fq: &str, change: Option<ChangeKind>) -> ImpactNode {
    ImpactNode {
        id: id.to_string(),
        entity: EntityRef::for_symbol(
            FileId::new(file).unwrap(),
            fq,
            Some(LineRange::new(1, 0, 10, 1)),
        ),
        change,
        diagnostic_severity: None,
    }
}

fn graph_with_duplicates() -> ImpactGraph {
    let mut g = ImpactGraph::new();
    // Changed symbol plus its callers (research 05 digest tier 4).
    g.add_node(sym_node(
        "a.go:Load",
        "a.go",
        "pkg.Load",
        Some(ChangeKind::Modified),
    ));
    g.add_node(sym_node("b.go:Main", "b.go", "pkg.Main", None));
    g.add_node(sym_node("c.go:Handler", "c.go", "pkg.Handler", None));
    g.add_edge(ImpactEdge {
        from: "b.go:Main".into(),
        to: "a.go:Load".into(),
        kind: RelationKind::Calls,
    });
    g.add_edge(ImpactEdge {
        from: "c.go:Handler".into(),
        to: "a.go:Load".into(),
        kind: RelationKind::Calls,
    });
    g.add_edge(ImpactEdge {
        from: "a.go:Load".into(),
        to: "c.go:Handler".into(),
        kind: RelationKind::References,
    });
    // Duplicates as they arise from merging Evidence-wrapped query results.
    g.add_node(sym_node(
        "a.go:Load",
        "a.go",
        "pkg.Load",
        Some(ChangeKind::Modified),
    ));
    g.add_edge(ImpactEdge {
        from: "b.go:Main".into(),
        to: "a.go:Load".into(),
        kind: RelationKind::Calls,
    });
    g
}

#[test]
fn build_query_dedupe_pipeline() {
    let mut g = graph_with_duplicates();
    assert_eq!(g.node_count(), 4);
    assert_eq!(g.edge_count(), 4);

    let report = g.dedupe();
    assert_eq!(report.nodes_removed, 1);
    assert_eq!(report.edges_removed, 1);
    assert_eq!(g.node_count(), 3);
    assert_eq!(g.edge_count(), 3);

    // The changed symbol is the hub: two incoming `calls`, one outgoing `references`.
    let ns = g.neighbors("a.go:Load");
    assert_eq!(ns.len(), 3);
    let incoming_calls = ns
        .iter()
        .filter(|n| n.direction == EdgeDirection::Incoming && n.kind == RelationKind::Calls)
        .count();
    assert_eq!(incoming_calls, 2);

    // Edge-existence validation (research 05 §3): AI may select, never assert.
    assert!(g.contains_edge("b.go:Main", "a.go:Load", RelationKind::Calls));
    assert!(!g.contains_edge("a.go:Load", "b.go:Main", RelationKind::Calls));
    assert!(!g.contains_edge("b.go:Main", "a.go:Load", RelationKind::Implements));

    // TUI focus: exactly one changed node.
    let changed: Vec<&ImpactNode> = g.changed_nodes().collect();
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].entity.symbol.as_deref(), Some("pkg.Load"));
}

#[test]
fn prune_then_dedupe_is_idempotent() {
    let mut g = graph_with_duplicates();
    g.add_edge(ImpactEdge {
        from: "a.go:Load".into(),
        to: "ghost".into(),
        kind: RelationKind::Calls,
    });
    assert_eq!(g.prune_dangling_edges(), 1);
    assert_eq!(g.prune_dangling_edges(), 0);
    let first = g.dedupe();
    let second = g.dedupe();
    assert!(first.nodes_removed > 0 || first.edges_removed > 0);
    assert_eq!(
        second,
        DedupeReport {
            nodes_removed: 0,
            edges_removed: 0
        }
    );
}

#[test]
fn evidence_wraps_relationship_queries() {
    // research 01: every relationship query returns Evidence; the UI/AI must see honesty.
    let callers: Evidence<Vec<Location>> = Evidence::partial(
        vec![Location {
            file: FileId::new("b.go").unwrap(),
            range: LineRange::new(30, 2, 30, 6),
        }],
        vec!["vendor/ not indexed".to_string()],
    );
    assert!(!callers.is_complete());
    assert_eq!(callers.value.len(), 1);
    let names = callers.map(|locs| locs.iter().map(|l| l.file.to_string()).collect::<Vec<_>>());
    assert_eq!(names.value, ["b.go"]);
    assert_eq!(names.completeness, Completeness::Partial);
    assert_eq!(names.notes, ["vendor/ not indexed"]);
}

#[test]
fn feature_gating_precedes_queries() {
    // research 01: never send requests the server didn't advertise.
    let mut caps = FeatureSet::new();
    caps.set(Feature::DocumentSymbols, Availability::Supported);
    caps.set(Feature::CallHierarchyIncoming, Availability::Supported);
    caps.set(Feature::TypeHierarchySuper, Availability::Unsupported); // rust-analyzer has none

    assert!(caps.is_supported(Feature::DocumentSymbols));
    assert!(!caps.is_supported(Feature::TypeHierarchySuper));
    // Absent entries are Unknown, and Unknown is not Supported.
    assert_eq!(caps.get(Feature::Hover), Availability::Unknown);
    assert!(!caps.is_supported(Feature::Hover));

    let supported: Vec<Feature> = caps.supported().collect();
    assert_eq!(supported.len(), 2);
}
