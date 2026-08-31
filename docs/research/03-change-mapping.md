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

## Mapping algorithm (per changed file) — changed-run mapping (review 20)

Input: hierarchical symbol tree(s) and hunks whose `DiffLine` bodies carry exact
`Add`/`Del`/`Context` coordinates (`old_ln`/`new_ln`). Output: one `HunkMapping` **per
changed run** (a hunk can yield several, or none).

The envelope bug this replaces: mapping the whole `-U3` hunk header range
(`new_start..new_start+new_len`) counted unchanged CONTEXT lines as evidence, so any symbol
that merely *brushed* a hunk (a doc comment, the tail of a neighbouring function) was
reported as changed. The fix maps only the changed lines.

```
for hunk in hunks:
    # 1. extract runs: maximal same-kind consecutive-coordinate Add/Del runs.
    #    Context only separates runs; it is NEVER evidence.
    runs = extract(hunk.lines)   # Add runs carry new_ln, Del runs carry old_ln
    for run in runs:
        # 2. map each run against the tree for its own side
        if run.side == New:  map run.range against WORKTREE tree
        if run.side == Old:
            if base tree:  map run.range against BASE tree (confidence DeletedHunkBaseMapped)
            else:          nearest surviving symbol around the run's own new-side anchor
        # 3. deepest semantic frontier: a line in a nested field maps to the field, never
        #    to its parent unless the parent's own declaration/body changed
        # 4. fold base-side targets onto surviving worktree symbols by (name, kind)
        # 5. a run in a gap attaches approximately to the nearest symbol (doc comment);
        #    a run with no credible symbol stays Unmapped (imports, far changes)
```

Key properties:
- Context lines never select a symbol and never set `signature_touch`.
- Two disjoint edits in one Git hunk produce two independent mappings.
- A replacement produces an old-side run (base tree) AND a new-side run (worktree tree).
- `signature_touch` is per-target: it is set when the run's changed lines intersect the
  target's *selection* range, not when context crosses it.
- A hunk body with no Add/Del lines emits no mapping record.

Edge cases (unchanged semantics, now driven by runs not envelopes):
- Whole symbol added → the Add run covers it → Exact.
- Whole file added → every top-level symbol is Exact-added (ChangeKind::Added).
- Whole file deleted → mapped via the base tree only, ChangeKind::Deleted.
- Hunk spanning multiple symbols (one run genuinely crosses them) → HunkSpansSymbols.
- Renamed file → path from git rename detection; symbols matched by name/kind across trees.

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
