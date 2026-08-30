//! Integration of the pure analysis pipeline: hunks → mappings → changed symbols →
//! digest, over a hand-built two-file change-set (no git, no LSP).

use camino::Utf8PathBuf;
use codescope_analysis::digest::{change_digest, DIGEST_DEFAULT_TOKEN_BUDGET};
use codescope_analysis::{changed_symbols_detailed, map_changes_with_base};
use codescope_core::{
    ApproxReason, BaseInfo, BaseSource, ChangeKind, ChangeScope, ChangeSet, DiffLine, EntityRef,
    Evidence, FileChange, FileId, FileStatus, HeadState, Hunk, ImpactEdge, ImpactGraph,
    ImpactNode, LineRange, MappingConfidence, Oid, RelationKind, RepoContext, Revision, SymbolId,
    SymbolKind, SymbolNode, SymbolTree,
};

fn node(id: &str, name: &str, kind: SymbolKind, start: u32, end: u32) -> SymbolNode {
    SymbolNode {
        id: SymbolId::new(id),
        name: name.to_string(),
        detail: Some(format!("func {name}()")),
        kind,
        range: LineRange::new(start, 0, end, 1),
        selection: LineRange::new(start, 5, start, 5 + name.len() as u32),
        children: Vec::new(),
    }
}

/// greet.go worktree: Greeter interface 4-8, SpanishGreeter 12-20, (SpanishGreeter).Hello 24-32.
fn greet_worktree() -> SymbolTree {
    SymbolTree::new(
        FileId::new("pkg/greet.go").unwrap(),
        Revision::Worktree,
        vec![
            node("0", "Greeter", SymbolKind::Interface, 4, 8),
            node("1", "SpanishGreeter", SymbolKind::Struct, 12, 20),
            node("2", "(SpanishGreeter).Hello", SymbolKind::Method, 24, 32),
        ],
    )
}

/// greet.go base: Greeter interface 4-8, LegacyGreeter 12-22.
fn greet_base() -> SymbolTree {
    SymbolTree::new(
        FileId::new("pkg/greet.go").unwrap(),
        Revision::Base,
        vec![
            node("0", "Greeter", SymbolKind::Interface, 4, 8),
            node("1", "LegacyGreeter", SymbolKind::Struct, 12, 22),
        ],
    )
}

/// main.go worktree: main 3-15.
fn main_worktree() -> SymbolTree {
    SymbolTree::new(
        FileId::new("cmd/main.go").unwrap(),
        Revision::Worktree,
        vec![node("0", "main", SymbolKind::Function, 3, 15)],
    )
}

fn changeset() -> ChangeSet {
    ChangeSet::new(
        ChangeScope::Branch,
        vec![
            FileChange {
                path: Utf8PathBuf::from("cmd/main.go"),
                old_path: None,
                status: FileStatus::Modified,
                hunks: vec![Hunk {
                    old_start: 8,
                    old_len: 1,
                    new_start: 8,
                    new_len: 2,
                    section: Some("func main()".to_string()),
                    lines: vec![
                        DiffLine::del(8, "greet.Legacy()"),
                        DiffLine::add(8, "g := greet.SpanishGreeter{}"),
                        DiffLine::add(9, "g.Hello()"),
                    ],
                }],
                binary: false,
            },
            FileChange {
                path: Utf8PathBuf::from("pkg/greet.go"),
                old_path: None,
                status: FileStatus::Modified,
                hunks: vec![
                    // Replaces LegacyGreeter with SpanishGreeter + method (new-side 13-33).
                    Hunk {
                        old_start: 13,
                        old_len: 11,
                        new_start: 13,
                        new_len: 21,
                        section: None,
                        lines: vec![DiffLine::add(13, "type SpanishGreeter struct{}")],
                    },
                ],
                binary: false,
            },
        ],
    )
}

#[test]
fn pure_pipeline_produces_digest() {
    let cs = changeset();

    // Per-file mapping + aggregation.
    let main_changed = changed_symbols_detailed(Some(&main_worktree()), None, &cs.files[0]);
    assert_eq!(main_changed.len(), 1);
    assert_eq!(main_changed[0].name, "main");
    assert_eq!(main_changed[0].record.change_kind, ChangeKind::Modified);
    assert_eq!(main_changed[0].record.confidence, MappingConfidence::Exact);

    let greet_changed =
        changed_symbols_detailed(Some(&greet_worktree()), Some(&greet_base()), &cs.files[1]);
    let names: Vec<&str> = greet_changed.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"SpanishGreeter"));
    assert!(names.contains(&"(SpanishGreeter).Hello"));
    assert!(names.contains(&"LegacyGreeter"));
    let legacy = greet_changed.iter().find(|c| c.name == "LegacyGreeter").unwrap();
    assert_eq!(legacy.record.change_kind, ChangeKind::Deleted);
    assert_eq!(legacy.revision, Revision::Base);

    // Hand-built 1-hop graph (the LS-driven builder is exercised elsewhere).
    let mut graph = ImpactGraph::new();
    for info in main_changed.iter().chain(&greet_changed) {
        graph.add_node(ImpactNode {
            id: format!("{}:{}", info.file, info.name),
            entity: EntityRef::for_symbol(info.file.clone(), info.name.clone(), Some(info.range)),
            change: Some(info.record.change_kind),
            diagnostic_severity: None,
        });
    }
    graph.add_edge(ImpactEdge {
        from: "cmd/main.go:main".to_string(),
        to: "pkg/greet.go:(SpanishGreeter).Hello".to_string(),
        kind: RelationKind::Calls,
    });
    graph.add_edge(ImpactEdge {
        from: "pkg/greet.go:SpanishGreeter".to_string(),
        to: "pkg/greet.go:Greeter".to_string(),
        kind: RelationKind::Implements,
    });
    graph.dedupe();

    let repo_ctx = RepoContext {
        toplevel: Utf8PathBuf::from("/repo"),
        head: HeadState::Branch("feature/spanish".to_string()),
        upstream: None,
        base: Some(BaseInfo {
            source: BaseSource::OriginHead,
            ref_name: "origin/main".to_string(),
            merge_base: Oid::new("deadbeef"),
        }),
    };

    let all_changed: Vec<_> = main_changed.into_iter().chain(greet_changed).collect();
    let mut digest = change_digest(
        &all_changed,
        &cs,
        &Evidence::complete(graph),
        &[],
        &repo_ctx,
    );
    digest.truncate_to_budget(DIGEST_DEFAULT_TOKEN_BUDGET);

    let text = digest.render();
    assert!(text.contains("cmd/main.go:main"));
    assert!(text.contains("pkg/greet.go:SpanishGreeter"));
    assert!(text.contains("-Implements->"));
    assert!(text.contains("head=feature/spanish"));
    assert_eq!(digest.repo.dirs, vec![("cmd".to_string(), 1), ("pkg".to_string(), 1)]);
    assert!(digest.estimated_tokens() <= DIGEST_DEFAULT_TOKEN_BUDGET);
}

#[test]
fn deletion_mapping_survives_missing_base() {
    // The same greet.go change without a base tree: deletion degrades, worktree symbols map.
    let wt = greet_worktree();
    let hunks = vec![Hunk {
        old_start: 13,
        old_len: 11,
        new_start: 12,
        new_len: 0,
        section: None,
        lines: vec![],
    }];
    let maps = map_changes_with_base(&wt, None, &hunks);
    assert_eq!(
        maps[0].confidence,
        MappingConfidence::Approximate(ApproxReason::DocCommentOrGap)
    );
}
