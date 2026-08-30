# 03 — Semantic change mapping (diff hunks → symbols)

Recovered from sub-agent `research-mapping` verified experiments (agent stalled before writing;
findings reconstructed by lead engineer from its session log; all gopls/git claims below were
verified live against gopls 0.21.0 / git 2.50.1 in /tmp/codescope_probe).

## Verified facts (gopls 0.21, git 2.50)

1. With `hierarchicalDocumentSymbolSupport: true`, gopls returns hierarchical `DocumentSymbol[]`:
   `range` = full symbol extent (body incl. braces), `selectionRange` = identifier only.
2. Nesting: struct fields appear as `children` (kind Field=8). Go methods are NOT children of
   their type: `(Greeter).Hello` is top-level kind Method=6. Symbol names for methods include
   receiver: `(Greeter).Hello`.
3. Doc comments are EXCLUDED from symbol ranges (range starts at the declaration line).
   A change inside a doc comment is a "gap" change → approximate mapping.
4. Imports produce NO symbols. Import changes are file-level (unmapped at symbol granularity).
5. Flat fallback: without hierarchical support, gopls returns `SymbolInformation[]` and DROPS
   struct fields (no children, containerName unreliable). => Always advertise hierarchical
   support; flat mode is a degraded, top-level-only fallback.
6. gopls honors didOpen overlay text that differs from disk => base-revision content
   (`git show <base>:<path>`) can be opened as an in-memory overlay to get base symbols.
   (Caveat: file must exist in module context; opening overlays for deleted files is unreliable —
   treat deleted-file symbol mapping as approximate.)
7. Pure deletion hunk header: `@@ -15,5 +14,0 @@` — new side count=0, start = line after which
   deletion occurred. Deleted code can only be mapped against the BASE symbol tree, or to the
   nearest surviving symbol (approximate).
8. Free fallback: git hunk headers carry function context text (`@@ ... @@ func (g Greeter)
   Hello() {`) via the built-in Go userdiff driver. Crude (nearest preceding match, not true
   containment) but zero-cost and language-neutral-ish via .gitattributes userdiff.

## Mapping algorithm (per changed file)

Input: symbol tree (hierarchical, sorted by range), list of hunks with old_range/new_range.
Output: for each hunk, a Mapping { symbol_id | None, confidence }.

```
for hunk in hunks:
    target_range = hunk.new_range (if count>0) else insertion point
    if hunk is pure deletion:
        if base symbol tree available: map old_range against base tree → confidence=DeletedIn(symbol)
        else: nearest symbol above/below insertion point → Approximate
        continue
    containing = smallest symbol whose range fully contains target_range  # walk tree depth-first
    if containing:
        if symbol.selectionRange intersects target: Exact(signature-ish change)
        else: Exact(body change)
    else:
        # gap: between top-level symbols, doc comment, import block
        below = nearest symbol starting after target end (same file)
        above = nearest symbol ending before target start
        if above/below within N=3 lines: Approximate(that symbol)  # likely doc comment/signature edit
        else: Unmapped(file-level)
```

Edge cases:
- Hunk spanning multiple symbols → attach to the smallest common ancestor (often file-level);
  record all intersected symbols.
- Whole symbol added → Exact on that symbol (it is contained in a new-side-only hunk).
- Whole file added → every top-level symbol is Exact-added (enum ChangeKind::Added on symbol).
- Whole file deleted → map via base tree only, ChangeKind::Deleted.
- Renamed file → path mapping from git rename detection; symbols matched by name/kind across
  old/new trees when possible.

## Confidence model

```rust
pub enum MappingConfidence { Exact, Approximate(ApproxReason), Unmapped }
pub enum ApproxReason { DocCommentOrGap, DeletedHunkBaseMapped, HunkSpansSymbols, FlatSymbolFallback }
```
UI: show `~` marker for approximate, `?` for unmapped/file-level. AI digest: include confidence;
plans referencing approximate symbols must carry `uncertainty` notes (validation boundary keeps this).

## Before/after (diff-of-symbols) — prototype scope

Feasible via overlays (verified fact 6), but scope-limited for the prototype:
- DO: per changed file, extract base symbol tree via overlay, diff name+kind sets →
  added/removed/renamed-symbol lists for the "How has the shape changed" view.
- DON'T: full cross-file structural diff, type-hierarchy snapshots of base revision (later).

## LSP-only vs tree-sitter

Prototype: LSP document symbols only (single source of truth, matches relationships).
Tree-sitter later as (a) fallback when no LS supports a language, (b) cheap local re-mapping
during rapid edits before LS catches up. Design the mapping layer against an internal
`SymbolTree` type so tree-sitter can become a second producer without touching consumers.

## Rust data types (recommendation)

```rust
pub struct SymbolNode { pub id: SymbolId, pub name: String, pub detail: Option<String>,
    pub kind: SymbolKind, pub range: Range, pub selection: Range, pub children: Vec<SymbolNode> }
pub struct SymbolTree { pub file: PathBuf, pub revision: Revision, pub roots: Vec<SymbolNode> }
pub enum Revision { Base, Worktree, Staged }
pub struct HunkMapping { pub hunk: HunkId, pub targets: Vec<SymbolId>,
    pub confidence: MappingConfidence }
pub struct ChangedSymbol { pub symbol: SymbolId, pub kind: ChangeKind, /* Added|Modified|Deleted */
    pub hunks: Vec<HunkId>, pub confidence: MappingConfidence }
```

## Recommended decisions

1. Always advertise `hierarchicalDocumentSymbolSupport`; flat mode = degraded top-level-only.
2. Map against worktree symbols for new-side hunks; base symbols (overlay) for pure deletions.
3. Three-level confidence (Exact/Approximate/Unmapped) carried end-to-end to UI + AI digest.
4. Use git hunk-header function context only as a last-resort label hint, never as a fact.
5. Keep mapper pure: `fn map(tree: &SymbolTree, hunks: &[Hunk]) -> Vec<HunkMapping>` — unit-testable
   without LSP or git.
6. Doc comments/imports gaps → approximate mapping to nearest symbol within 3 lines, else file-level.
