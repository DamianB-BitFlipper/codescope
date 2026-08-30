//! Serde round-trips and exact-JSON spot checks against the research-05 plan schema.

use codescope_core::*;

fn roundtrip<T>(value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug + PartialEq,
{
    let json = serde_json::to_string_pretty(value).expect("serialize");
    let back: T = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(&back, value, "round-trip mismatch; json was:\n{json}");
}

fn sample_file_change() -> FileChange {
    FileChange {
        path: Utf8PathBuf::from("pkg/store.go"),
        old_path: Some(Utf8PathBuf::from("pkg/session_store.go")),
        status: FileStatus::Renamed { score: 87 },
        hunks: vec![Hunk {
            old_start: 15,
            old_len: 5,
            new_start: 14,
            new_len: 7,
            section: Some("func (s *Store) load() {".to_string()),
            lines: vec![
                DiffLine::context(15, 14, "\tmu.Lock()"),
                DiffLine::del(16, "\treturn s.v"),
                DiffLine::add(15, "\tdefer mu.Unlock()"),
                DiffLine::add(16, "\treturn s.loadSlow()"),
            ],
        }],
        binary: false,
    }
}

#[test]
fn git_domain_roundtrips() {
    roundtrip(&Oid::new("b3f1c9a2"));
    roundtrip(&HeadState::Branch("feature/x".to_string()));
    roundtrip(&HeadState::Detached(Oid::new("abc")));
    roundtrip(&HeadState::Unborn);
    roundtrip(&Upstream {
        name: "origin/main".to_string(),
        ahead: 2,
        behind: 1,
    });
    roundtrip(&BaseInfo {
        source: BaseSource::ForkPoint,
        ref_name: "origin/main".to_string(),
        merge_base: Oid::new("deadbeef"),
    });
    roundtrip(&RepoContext {
        toplevel: Utf8PathBuf::from("/repo"),
        head: HeadState::Branch("main".to_string()),
        upstream: Some(Upstream {
            name: "origin/main".to_string(),
            ahead: 0,
            behind: 0,
        }),
        base: None,
    });
    for scope in [
        ChangeScope::Branch,
        ChangeScope::Staged,
        ChangeScope::Unstaged,
    ] {
        roundtrip(&scope);
    }
    for status in [
        FileStatus::Added,
        FileStatus::Modified,
        FileStatus::Deleted,
        FileStatus::Renamed { score: 96 },
        FileStatus::Copied { score: 100 },
        FileStatus::TypeChanged,
        FileStatus::Unmerged,
        FileStatus::Untracked,
        FileStatus::Gitlink,
    ] {
        roundtrip(&status);
    }
    let change_set = ChangeSet::new(ChangeScope::Unstaged, vec![sample_file_change()]);
    roundtrip(&change_set);
    roundtrip(&HunkId {
        file: Utf8PathBuf::from("a.go"),
        index: 3,
    });
}

#[test]
fn file_status_serde_shape() {
    assert_eq!(
        serde_json::to_value(FileStatus::Modified).unwrap(),
        serde_json::json!("modified")
    );
    assert_eq!(
        serde_json::to_value(FileStatus::Renamed { score: 96 }).unwrap(),
        serde_json::json!({"renamed": {"score": 96}})
    );
}

#[test]
fn semantic_domain_roundtrips() {
    let tree = SymbolTree::new(
        FileId::new("main.go").unwrap(),
        Revision::Worktree,
        vec![SymbolNode {
            id: SymbolId::new("0"),
            name: "Greeter".to_string(),
            detail: Some("struct".to_string()),
            kind: SymbolKind::Struct,
            range: LineRange::new(12, 0, 30, 1),
            selection: LineRange::new(12, 5, 12, 12),
            children: vec![SymbolNode {
                id: SymbolId::new("0/0"),
                name: "Name".to_string(),
                detail: None,
                kind: SymbolKind::Field,
                range: LineRange::new(13, 1, 13, 15),
                selection: LineRange::new(13, 1, 13, 5),
                children: vec![],
            }],
        }],
    );
    roundtrip(&tree);
    roundtrip(&SymbolRef {
        file: FileId::new("main.go").unwrap(),
        name: "(Greeter).Hello".to_string(),
        kind: SymbolKind::Method,
    });
    roundtrip(&Location {
        file: FileId::new("main.go").unwrap(),
        range: LineRange::new(1, 2, 3, 4),
    });
    roundtrip(&EntityRef::for_file(FileId::new("main.go").unwrap()));
    roundtrip(&EntityRef::for_symbol(
        FileId::new("main.go").unwrap(),
        "main.main",
        Some(LineRange::new(1, 0, 10, 1)),
    ));
    for rev in [Revision::Base, Revision::Staged, Revision::Worktree] {
        roundtrip(&rev);
    }
}

#[test]
fn relationship_domain_roundtrips() {
    for kind in [
        RelationKind::Calls,
        RelationKind::CalledBy,
        RelationKind::Implements,
        RelationKind::ImplementedBy,
        RelationKind::References,
        RelationKind::Contains,
        RelationKind::SubtypeOf,
        RelationKind::SupertypeOf,
    ] {
        roundtrip(&kind);
    }
    for c in [
        Completeness::Complete,
        Completeness::Partial,
        Completeness::Unknown,
    ] {
        roundtrip(&c);
    }
    let ev = Evidence::partial(
        vec![Location {
            file: FileId::new("a.go").unwrap(),
            range: LineRange::new(1, 0, 1, 10),
        }],
        vec!["timed out".to_string()],
    );
    roundtrip(&ev);

    let mut fs = FeatureSet::new();
    fs.set(Feature::DocumentSymbols, Availability::Supported);
    fs.set(Feature::TypeHierarchySuper, Availability::Unsupported);
    fs.set(Feature::Hover, Availability::Unknown);
    roundtrip(&fs);

    roundtrip(&Diagnostic {
        file: FileId::new("a.go").unwrap(),
        range: LineRange::new(2, 1, 2, 8),
        severity: DiagnosticSeverity::Warning,
        code: Some("SA1006".to_string()),
        message: "unused value".to_string(),
        source: Some("staticcheck".to_string()),
    });
}

#[test]
fn feature_set_serializes_as_string_keyed_map() {
    let mut fs = FeatureSet::new();
    fs.set(Feature::CallHierarchyIncoming, Availability::Supported);
    let v = serde_json::to_value(&fs).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"map": {"call_hierarchy_incoming": "supported"}}),
        "FeatureSet must serialize as a snake_case string-keyed map"
    );
    let back: FeatureSet = serde_json::from_value(v).unwrap();
    assert_eq!(back, fs);
}

#[test]
fn mapping_domain_roundtrips() {
    for conf in [
        MappingConfidence::Exact,
        MappingConfidence::Approximate(ApproxReason::DocCommentOrGap),
        MappingConfidence::Approximate(ApproxReason::DeletedHunkBaseMapped),
        MappingConfidence::Approximate(ApproxReason::HunkSpansSymbols),
        MappingConfidence::Approximate(ApproxReason::FlatSymbolFallback),
        MappingConfidence::Unmapped,
    ] {
        roundtrip(&conf);
    }
    for kind in [ChangeKind::Added, ChangeKind::Modified, ChangeKind::Deleted] {
        roundtrip(&kind);
    }
    roundtrip(&HunkMapping {
        hunk: HunkId {
            file: Utf8PathBuf::from("a.go"),
            index: 1,
        },
        targets: vec![SymbolId::new("0/1")],
        confidence: MappingConfidence::Approximate(ApproxReason::HunkSpansSymbols),
    });
    roundtrip(&ChangedSymbol::new(
        SymbolId::new("2"),
        ChangeKind::Deleted,
        vec![HunkId {
            file: Utf8PathBuf::from("a.go"),
            index: 0,
        }],
        MappingConfidence::Approximate(ApproxReason::DeletedHunkBaseMapped),
    ));
}

#[test]
fn impact_graph_roundtrip() {
    let mut g = ImpactGraph::new();
    g.add_node(ImpactNode {
        id: "pkg/a.go:pkg.Load".to_string(),
        entity: EntityRef::for_symbol(
            FileId::new("pkg/a.go").unwrap(),
            "pkg.Load",
            Some(LineRange::new(10, 0, 40, 1)),
        ),
        change: Some(ChangeKind::Modified),
        diagnostic_severity: Some(DiagnosticSeverity::Error),
    });
    g.add_node(ImpactNode {
        id: "pkg/b.go:pkg.Main".to_string(),
        entity: EntityRef::for_symbol(FileId::new("pkg/b.go").unwrap(), "pkg.Main", None),
        change: None,
        diagnostic_severity: None,
    });
    g.add_edge(ImpactEdge {
        from: "pkg/b.go:pkg.Main".to_string(),
        to: "pkg/a.go:pkg.Load".to_string(),
        kind: RelationKind::Calls,
    });
    roundtrip(&g);
}

fn sample_plan() -> VisualizationPlan {
    let mut plan = VisualizationPlan::new(Epoch(7), "What breaks if I rename SessionStore.load?");
    plan.forms.push(VizForm {
        kind: FormKind::CallTree,
        title: "Callers of load".to_string(),
        summary: "load has 3 callers; 2 are in changed files.".to_string(),
        nodes: vec![
            PlanNode {
                id: "n1".to_string(),
                entity: Some(EntityRef::for_symbol(
                    FileId::new("src/session/store.rs").unwrap(),
                    "session::store::SessionStore::load",
                    Some(LineRange::new(121, 4, 140, 5)),
                )),
                label: "load".to_string(),
                change: PlanNodeChange::Modified,
                severity: Some(DiagnosticSeverity::Error),
                children: vec!["n2".to_string()],
                hint: NodeHint {
                    highlight: true,
                    collapsed: false,
                },
            },
            PlanNode::new("n2", "handle", PlanNodeChange::Unchanged),
        ],
        edges: vec![PlanEdge {
            from: "n2".to_string(),
            to: "n1".to_string(),
            kind: PlanEdgeKind::Calls,
            label: Some("on cache miss".to_string()),
        }],
    });
    plan
}

#[test]
fn viz_domain_roundtrips() {
    for kind in [
        FormKind::ChangedSymbolTree,
        FormKind::CallTree,
        FormKind::TypeImplTree,
        FormKind::RelationshipFlow,
        FormKind::ImpactSummary,
        FormKind::FocusedDiff,
        FormKind::BeforeAfter,
        FormKind::Sequence,
    ] {
        roundtrip(&kind);
    }
    for change in [
        PlanNodeChange::Added,
        PlanNodeChange::Modified,
        PlanNodeChange::Removed,
        PlanNodeChange::Unchanged,
        PlanNodeChange::Diagnostic,
    ] {
        roundtrip(&change);
    }
    for kind in [
        PlanEdgeKind::Calls,
        PlanEdgeKind::Imports,
        PlanEdgeKind::Implements,
        PlanEdgeKind::Contains,
        PlanEdgeKind::Reads,
        PlanEdgeKind::Writes,
    ] {
        roundtrip(&kind);
    }
    roundtrip(&sample_plan());
    roundtrip(&ValidationReport::with_drops(vec![DroppedItem {
        subject: "node n3 in form 0".to_string(),
        reason: "entity does not resolve".to_string(),
    }]));
    for verdict in [
        ValidationVerdict::Valid,
        ValidationVerdict::ValidWithDrops,
        ValidationVerdict::Stale,
        ValidationVerdict::Rejected,
    ] {
        roundtrip(&verdict);
    }
    for status in [
        AiStatus::Disabled,
        AiStatus::Idle,
        AiStatus::Loading {
            since_epoch: Epoch(3),
        },
        AiStatus::Ready { epoch: Epoch(3) },
        AiStatus::Stale { epoch: Epoch(2) },
        AiStatus::Failed {
            reason: "timeout".to_string(),
        },
    ] {
        roundtrip(&status);
    }
    for status in [
        LsStatus::Starting,
        LsStatus::Indexing,
        LsStatus::Ready,
        LsStatus::Degraded,
        LsStatus::Failed,
    ] {
        roundtrip(&status);
    }
}

/// The exact JSON example from research 05 §2 must deserialize into our plan types.
#[test]
fn research_05_schema_example_deserializes() {
    let json = serde_json::json!({
        "plan_version": 1,
        "epoch": 42,
        "focus": "What breaks if I rename SessionStore.load?",
        "forms": [{
            "kind": "call_tree",
            "title": "Callers of load",
            "summary": "load has 3 callers; 2 are in changed files.",
            "nodes": [{
                "id": "n1",
                "entity": {
                    "file": "src/session/store.rs",
                    "symbol": "session::store::SessionStore::load",
                    "range": {"start_line": 121, "start_col": 4, "end_line": 140, "end_col": 5}
                },
                "label": "load",
                "change": "modified",
                "severity": "error",
                "children": ["n2"],
                "hint": {"highlight": true, "collapsed": false}
            }],
            "edges": [{
                "from": "n1", "to": "n2", "kind": "calls", "label": "on cache miss"
            }]
        }]
    });
    let plan: VisualizationPlan = serde_json::from_value(json).expect("research-05 example parses");
    assert_eq!(plan.plan_version, PLAN_VERSION);
    assert_eq!(plan.epoch, Epoch(42));
    assert_eq!(plan.forms.len(), 1);
    let node = &plan.forms[0].nodes[0];
    let entity = node.entity.as_ref().expect("entity present");
    assert_eq!(entity.file.to_string(), "src/session/store.rs");
    assert_eq!(
        entity.symbol.as_deref(),
        Some("session::store::SessionStore::load")
    );
    assert_eq!(entity.range, Some(LineRange::new(121, 4, 140, 5)));
    assert_eq!(node.change, PlanNodeChange::Modified);
    assert_eq!(node.severity, Some(DiagnosticSeverity::Error));
    assert!(node.hint.highlight);
    assert_eq!(plan.forms[0].edges[0].kind, PlanEdgeKind::Calls);
}

/// AI output often omits optional fields entirely; defaults must fill them in.
#[test]
fn minimal_plan_json_uses_defaults() {
    let json = serde_json::json!({
        "plan_version": 1,
        "epoch": 0,
        "focus": "shape of the change",
        "forms": [{
            "kind": "impact_summary",
            "title": "Impact",
            "nodes": [{"id": "n1", "label": "3 symbols changed", "change": "unchanged"}]
        }]
    });
    let plan: VisualizationPlan = serde_json::from_value(json).expect("minimal plan parses");
    let node = &plan.forms[0].nodes[0];
    assert!(node.entity.is_none());
    assert!(node.severity.is_none());
    assert!(node.children.is_empty());
    assert_eq!(
        node.hint,
        NodeHint {
            highlight: false,
            collapsed: false
        }
    );
    assert!(plan.forms[0].edges.is_empty());
    assert_eq!(plan.forms[0].summary, "");
}
