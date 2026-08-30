//! Integration-style tests: building symbol trees from LSP `DocumentSymbol`s and using the
//! tree helpers the way `codescope-analysis` will (smallest-container mapping, gap fallback).

use codescope_core::*;
use lsp_types::{
    DocumentSymbol, Position as LspPosition, Range as LspRange, SymbolKind as LspKind,
};

fn lsp_range(sl: u32, sc: u32, el: u32, ec: u32) -> LspRange {
    LspRange::new(LspPosition::new(sl, sc), LspPosition::new(el, ec))
}

#[allow(deprecated)] // DocumentSymbol carries a deprecated field we must fill.
fn doc_symbol(
    name: &str,
    kind: LspKind,
    range: LspRange,
    selection: LspRange,
    children: Vec<DocumentSymbol>,
) -> DocumentSymbol {
    DocumentSymbol {
        name: name.to_string(),
        detail: None,
        kind,
        tags: None,
        deprecated: None,
        range,
        selection_range: selection,
        children: if children.is_empty() {
            None
        } else {
            Some(children)
        },
    }
}

/// Mirrors the gopls shape from research 03: struct fields are children; methods are
/// top-level with receiver-qualified names; doc comments are excluded from ranges.
fn gopls_like_symbols() -> Vec<DocumentSymbol> {
    vec![
        // package-level func main at lines 20..30 (doc comment on line 19 excluded)
        doc_symbol(
            "main",
            LspKind::FUNCTION,
            lsp_range(20, 0, 30, 1),
            lsp_range(20, 5, 20, 9),
            vec![],
        ),
        // type Greeter struct { Name string } at lines 10..14, field Name at 12
        doc_symbol(
            "Greeter",
            LspKind::STRUCT,
            lsp_range(10, 0, 14, 1),
            lsp_range(10, 5, 10, 12),
            vec![doc_symbol(
                "Name",
                LspKind::FIELD,
                lsp_range(12, 1, 12, 12),
                lsp_range(12, 1, 12, 5),
                vec![],
            )],
        ),
        // func (Greeter) Hello() at lines 16..18
        doc_symbol(
            "(Greeter).Hello",
            LspKind::METHOD,
            lsp_range(16, 0, 18, 1),
            lsp_range(16, 16, 16, 21),
            vec![],
        ),
    ]
}

#[test]
fn from_document_symbols_builds_sorted_hierarchical_tree() {
    let file = FileId::new("main.go").unwrap();
    let tree =
        SymbolTree::from_document_symbols(file.clone(), Revision::Worktree, gopls_like_symbols());
    assert_eq!(tree.file, file);
    assert_eq!(tree.revision, Revision::Worktree);

    // Roots sorted by range: Greeter (10), Hello (16), main (20).
    let root_names: Vec<&str> = tree.roots.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(root_names, ["Greeter", "(Greeter).Hello", "main"]);

    // Field is nested under the struct; method is NOT nested under its type.
    let greeter = &tree.roots[0];
    assert_eq!(greeter.kind, SymbolKind::Struct);
    assert_eq!(greeter.children.len(), 1);
    assert_eq!(greeter.children[0].kind, SymbolKind::Field);
    assert_eq!(tree.roots[1].kind, SymbolKind::Method);

    // Ids are unique hierarchical paths.
    let ids: Vec<&str> = tree.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(ids, ["0", "0/0", "1", "2"]);
}

#[test]
fn hunk_to_symbol_mapping_primitives() {
    // The pure core-side half of the research-03 algorithm: given zero-based line spans for
    // a hunk, find the smallest containing symbol, else the nearest neighbors.
    let file = FileId::new("main.go").unwrap();
    let tree = SymbolTree::from_document_symbols(file, Revision::Worktree, gopls_like_symbols());

    // Body change inside Hello (lines 16..18) -> Exact on the method.
    let hunk = LineRange::from_line_span(17, 17);
    let hit = tree.find_smallest_containing(&hunk).unwrap();
    assert_eq!(hit.name, "(Greeter).Hello");

    // Change inside the struct field -> smallest container is the field, not the struct.
    let hunk = LineRange::from_line_span(12, 12);
    let hit = tree.find_smallest_containing(&hunk).unwrap();
    assert_eq!(hit.name, "Name");

    // Doc comment above main (line 19) -> gap: no container, nearest neighbors apply.
    let hunk = LineRange::from_line_span(19, 19);
    assert!(tree.find_smallest_containing(&hunk).is_none());
    assert_eq!(tree.nearest_above(19).unwrap().name, "(Greeter).Hello");
    assert_eq!(tree.nearest_below(19).unwrap().name, "main");

    // Import block (lines 0..9, before all symbols) -> Unmapped at symbol granularity.
    let hunk = LineRange::from_line_span(2, 5);
    assert!(tree.find_smallest_containing(&hunk).is_none());
    assert!(tree.nearest_above(2).is_none());
    assert_eq!(tree.nearest_below(5).unwrap().name, "Greeter");
}

#[test]
fn pure_deletion_maps_against_base_revision_tree() {
    // Base tree still contains a symbol the worktree deleted.
    let base = SymbolTree::from_document_symbols(
        FileId::new("main.go").unwrap(),
        Revision::Base,
        gopls_like_symbols(),
    );
    let worktree = SymbolTree::new(
        FileId::new("main.go").unwrap(),
        Revision::Worktree,
        base.roots
            .iter()
            .filter(|n| n.name != "(Greeter).Hello")
            .cloned()
            .collect(),
    );

    // Hunk `@@ -17,3 +16,0 @@`: deleted Hello's body (1-based old lines 17..19),
    // i.e. zero-based old-side span 16..18 — exactly Hello's base-tree extent.
    let old_span = LineRange::from_line_span(16, 18);
    assert!(worktree.find_smallest_containing(&old_span).is_none());
    let hit = base.find_smallest_containing(&old_span).unwrap();
    assert_eq!(hit.name, "(Greeter).Hello");
    assert_eq!(base.revision, Revision::Base);
}

#[test]
fn line_range_line_helpers_ignore_columns() {
    let sym = LineRange::new(10, 5, 20, 1);
    let whole_lines = LineRange::from_line_span(10, 20);
    assert!(sym.contains_lines(&whole_lines));
    assert!(!sym.contains_range(&whole_lines)); // col 0 precedes col 5
    assert_eq!(sym.line_span(), (10, 20));
    assert_eq!(sym.len_lines(), 10);
    assert!(!sym.is_single_line());
    assert!(LineRange::point(Position::new(3, 7)).is_single_line());
}
