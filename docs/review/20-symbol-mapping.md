# Review 20 — changed-line symbol mapping

Reviewed `HEAD` `7900133` statically. I also inspected the reported
`attachment.go` diff read-only to confirm the shape of the failure. I did not run Cargo
tests. This review changes no Rust source; this file is the only artifact it adds.

## Executive finding

The root cause is confirmed. The mapper treats the complete `-U3` hunk header range as
change evidence. That range includes unchanged context. The parsed `DiffLine` records
already contain the evidence that the mapper needs, but `map_one` does not read them.

The fix should be evidence-preserving, not a display filter:

1. Extract contiguous `Add` and `Del` runs from `Hunk::lines`.
2. Map each `Add` run, using `new_ln`, against the worktree tree.
3. Map each `Del` run, using `old_ln`, against the base tree.
4. Aggregate the run mappings by symbol. Fold an old-side target onto a surviving
   worktree symbol with the same `(qualified name, kind)`.
5. Keep the recursive base/worktree symbol-set sweep for true additions and deletions.
6. Never use `Context` lines to select a symbol or set `signature_touch`.

A normal Git hunk can contain both old-side and new-side evidence, and can contain several
context-separated edit islands. `Hunk::is_pure_deletion()` is therefore not a side selector.
It only describes the rare header whose complete new-side span has length zero.

The TUI and non-interactive backend already meet at this mapper and aggregation boundary.
Once `changed_symbols_detailed` is corrected, the Changed Files count, its `~` markers, the
impact graph's changed-node set, and AI digest tier 1 inherit the corrected symbol set. No
UI-side suppression should be added.

## Confirmed root cause

### The bad evidence path

- `crates/codescope-git/src/repo.rs:22-29` asks Git for `-U3`, so a hunk header normally
  includes three unchanged lines on each edge.
- `crates/codescope-git/src/diff.rs:175-211` parses every body line and assigns exact old/new
  coordinates.
- `crates/codescope-core/src/git.rs:273-295` stores the header envelope and `lines`.
  `DiffLineKind::{Add,Del,Context}` and `old_ln`/`new_ln` are defined at
  `crates/codescope-core/src/git.rs:356-415`.
- `crates/codescope-analysis/src/mapper.rs:103-108` instead chooses between
  `is_pure_deletion()` and `zero_based_span(hunk.new_start, hunk.new_len)`.
- `crates/codescope-analysis/src/mapper.rs:110-162` performs containment/intersection/gap
  mapping over that complete new-side envelope. It never consults `hunk.lines`.
- `crates/codescope-analysis/src/mapper.rs:260-265` confirms that the full header count is
  converted directly to the target range.

There is a second manifestation of the same mistake. `Hunk::is_pure_deletion()` is only
`new_len == 0` (`crates/codescope-core/src/git.rs:312-319`). A deletion-only edit surrounded
by ordinary unified-diff context has a nonzero `new_len`, so `mapper.rs:103` sends it down
the new-side worktree path. The side must come from each `DiffLineKind`, not from header
lengths.

### `attachment.go` confirmation

The reported first hunk is `@@ -19,20 +19,21 @@`. Its unchanged leading context is the tail
of `RootfsAttachmentConfig`; the first actual change is the following
`PreparedRootfsAttachment` doc comment. The full-envelope intersection at
`mapper.rs:121-159` therefore includes `RootfsAttachmentConfig` even though no `+` or `-`
line belongs to it.

Likewise, the hunk beginning around old line 85/new line 87 carries the unchanged tail of
`NewRootfsAttachment` as context before the changed `OpenRootfsAttachment` declaration.
The envelope makes `NewRootfsAttachment` a target. A run mapper sees only the changed
`OpenRootfsAttachment` declaration. These are exactly the two reported false positives.

This is also why changing Git to `-U0` is not the right fix. Context is useful for the diff
pane and AI hunk summaries. It is simply not semantic change evidence.

## All consumers and blast points

The following inventory is exhaustive for direct `MappedHunk`/`HunkMapping` use at this
HEAD. The later rows consume the aggregated `ChangedSymbolInfo` confidence rather than the
mapping record itself.

| Layer | Touchpoint | What it consumes |
|---|---|---|
| Core domain | `crates/codescope-core/src/mapping.rs:11-78` | `MappingConfidence`, `ApproxReason`, and serializable `HunkMapping { hunk, targets, confidence }`. The target revision is currently implicit. |
| Core serde contract | `crates/codescope-core/tests/serde_roundtrips.rs:206-237` | All confidence variants and a `HunkMapping` struct literal. |
| Mapper API | `crates/codescope-analysis/src/mapper.rs:20-91`; re-export at `src/lib.rs:38` | Produces one `MappedHunk` per input hunk today; `map_changes*` exposes the core records. |
| Per-file mapping | `crates/codescope-analysis/src/changes.rs:69-87` | `file_mappings` forwards detailed mappings for orchestration/backend output. |
| Symbol aggregation | `crates/codescope-analysis/src/changes.rs:97-151` | Iterates every target, hunk id, confidence, and the one hunk-wide `signature_touch`. It infers a base-tree id from `confidence == DeletedHunkBaseMapped` at lines 121-138. |
| Confidence/hunk merge | `crates/codescope-analysis/src/changes.rs:317-395` | Worst confidence wins; repeated `HunkId`s are deduplicated at lines 368-371; signature flags are ORed. |
| Engine | `crates/codescope-analysis/src/engine.rs:29-47,188-203,270-360` | Stores `Vec<MappedHunk>` in `FileAnalysis`, and independently builds the aggregated changed-symbol list. There is no hunk-id uniqueness lookup. |
| Non-interactive JSON | `crates/codescope/src/backend.rs:375-413` | Flattens `HunkMapping` plus `signature_touch` into `files[].mappings`. |
| Digest tier 1 | `crates/codescope-analysis/src/digest.rs:148-223,388-437` | Copies the aggregated symbol set and confidence, rendering `~`/`?`. It does not consume `MappedHunk` directly. |
| TUI conversion | `crates/codescope/src/dispatcher.rs:1365-1437` | Builds rows from aggregated symbols, sets `changed_symbol_count = symbols.len()` at line 1398, and converts confidence to `""`, `~`, or `?` at lines 1419-1423. |
| TUI rendering | `crates/codescope-tui/src/render.rs:659-709` | Draws the already-converted marker. It never reads `FileAnalysis.mappings`. |
| Interpretation | `crates/codescope/src/dispatcher.rs:1503-1517` | Uses `ChangedSymbol.hunks.len()` and `ChangedSymbolInfo.signature_touch`; repeated run evidence must still count as one Git hunk. |
| Impact graph | `crates/codescope-analysis/src/graph.rs:40-65` | Consumes the corrected changed-symbol set but ignores mapping confidence. |

Important current contracts in tests:

- Almost every mapper test indexes `maps[0]`, and `mapper.rs:509-515` assumes output order is
  one-to-one with input hunk order.
- The mapper helper at `mapper.rs:313-321` and changes helper at `changes.rs:457-465` build
  header-only hunks with `lines: Vec::new()`.
- `crates/codescope-analysis/tests/pipeline.rs:88-95` has a header/body count mismatch, and
  its missing-base deletion at lines 182-198 has an empty body.

Those fixtures must be made evidence-complete. A fallback to header spans solely to keep
header-only tests passing would preserve the production bug.

## Recommended changed-run algorithm

### 1. Extract only changed evidence

Walk `hunk.lines` in body order. Emit a run only for `Add` or `Del`:

- an `Add` run contains consecutive `Add` records with consecutive `new_ln` values;
- a `Del` run contains consecutive `Del` records with consecutive `old_ln` values;
- `Context`, a kind switch, a coordinate gap, or end of hunk closes the current run;
- assign a stable zero-based `run_index` in body order; and
- convert the selected side's 1-based first/last coordinate to a zero-based inclusive
  `LineRange`.

`Context` is a separator only. It is never included in a run, never expands a run, never
selects a target, and never touches a signature.

Production parsing guarantees the coordinates. A malformed programmatic/serialized hunk
with a missing coordinate should fail closed as an unmapped run or a traceable mapping
note. A body with no `Add`/`Del` records produces no hunk-derived symbol mapping. Do not
reconstruct evidence from `old_start/old_len/new_start/new_len`.

For a deletion fallback when no base tree exists, retain the new-side cursor while walking
the hunk (or derive it from adjacent new-side records). That gives each deletion run its
own insertion anchor. Do not reuse the complete hunk's insertion point for several
disjoint deletion runs.

### 2. Map each side against its own tree

For each run, independently and deterministically:

- **Add:** map its `new_ln` span against the worktree tree.
- **Del with base:** map its `old_ln` span against the base tree.
- **Del without base:** use that run's worktree insertion anchor and the existing nearest
  surviving-symbol fallback. This stays approximate.

A replacement therefore produces at least one old-side run and one new-side run. An
ordinary deletion-only edit with `-U3` context still produces an old-side run even though
`hunk.new_len > 0`. Two additions separated by context in one Git hunk map separately and
can target different symbols without making the context between them evidence.

### 3. Select semantic frontier targets, not ancestors

Apply containment/intersection to the changed run, not the Git hunk. For hierarchical
symbols, resolve changed lines to the deepest owning nodes and keep the minimal semantic
frontier:

- a line in a nested field maps to that field;
- siblings genuinely touched by one run can both be targets;
- do not add their parent merely because its extent contains both children; and
- include a parent only when changed evidence hits the parent's own declaration/selection,
  its own body, or its own attached doc-comment/gap evidence.

A practical implementation is to partition a run by deepest single-line ownership, merge
adjacent pieces with the same owner, and prune an ancestor when all of its apparent overlap
is accounted for by descendant pieces. This avoids turning a multi-field edit into a parent
modification while still allowing a changed class/struct declaration to appear alongside a
changed child.

`HunkSpansSymbols` remains useful only when one actual changed run genuinely crosses
several semantic targets or an ambiguous gap. Context-separated edits do not earn that
reason. A whole added symbol or whole added file can still produce exact targets because
the added lines really cover those symbols.

### 4. Treat gaps conservatively

Keep the existing real association for a changed run outside a symbol range:

- nearest credible symbol within `GAP_ATTACH_LINES` ->
  `Approximate(DocCommentOrGap)`;
- no credible symbol -> `Unmapped`, with no target; and
- a doc-comment run never becomes exact merely because unchanged declaration/body context
  is in the same Git hunk.

Imports must remain file-level and therefore unmapped. There is one architectural limit to
state explicitly: symbol ranges plus distance alone cannot always distinguish a first
symbol's doc comment from a nearby import/prelude line. A blind `nearest_within` rule cannot
guarantee both requirements. The conservative mapper-only policy is to keep a non-comment
pre-first-symbol/prelude run unmapped and attach only a credible comment/inter-symbol gap.
The durable language-neutral design is an optional lexical region classification from the
language adapter (`Import/Prelude`, `Comment`, `Other`) rather than hard-coding Go syntax in
the mapper. Tests should include an import close enough that a blind nearest-symbol rule
would fail.

### 5. Aggregate old and new evidence

`changes.rs` already has most of the right structural logic:

- `ordered_keys` recursively produces `(qualified name, kind)` at lines 278-295;
- `aggregate_base_target` folds a base target to a worktree survivor at lines 207-243;
- the one-side-only tree sweep at lines 153-188 emits true additions/deletions; and
- the aggregator deduplicates the same `HunkId` for one symbol at lines 368-371.

Retain all four. Change only the evidence entering them and make the mapped tree side
explicit. Do not infer target ownership from an approximation reason. For every old-side
base target:

1. resolve its exact qualified name and kind in the base tree;
2. if that key exists in the worktree, record a modification on the worktree id;
3. otherwise record a deletion on the base id.

The recursive key is what keeps `Config.Field` distinct from another `Field`, even when LSP
path ids shift across revisions. The structural sweep remains necessary for side-only
symbols that no run can map cleanly. It must not manufacture `Modified` parents; it should
only add `Added`/`Deleted` keys that exist on one side.

## API evolution for multiple runs

### Recommended: flat run records with a repeated `HunkId`

The least disruptive honest model is one mapping record per changed run. Keep the existing
public vector APIs and evolve the records rather than replace the pipeline:

```rust
pub enum ChangedSide { Old, New }

pub struct HunkMapping {
    pub hunk: HunkId,              // Git hunk provenance
    pub run_index: u32,             // stable within that hunk
    pub side: ChangedSide,          // Del/old or Add/new evidence
    pub range: LineRange,           // on that side, zero based
    pub mapped_revision: Revision,  // namespace of targets: Base or Worktree
    pub targets: Vec<SymbolId>,
    pub confidence: MappingConfidence,
}

pub struct MappedHunk {
    pub mapping: HunkMapping,
    pub signature_touches: Vec<SymbolId>, // target-specific, same tree namespace
}
```

The name `MappedHunk` can remain for compatibility, with its documentation changed to “one
changed-run mapping from a hunk.” Records are ordered by `(hunk.index, run_index)`. An
unmapped real run is retained with empty targets. A hunk body with no changed run emits no
record. `ChangedSymbol.hunks` remains `Vec<HunkId>` and continues to deduplicate run
provenance.

Both `side` and `mapped_revision` matter. A `Del` run normally maps to `Base`, but the
missing-base fallback maps that old-side evidence approximately to a `Worktree` id.
`changes.rs` must branch on `mapped_revision`, not on
`Approximate(DeletedHunkBaseMapped)`.

Target-specific signature ids are safer than the current hunk-wide boolean. A single Git
hunk can contain a signature edit for A and a body edit for B. Passing one boolean to every
target, as `changes.rs:121-149` does today, cannot represent that truth.

### Why not union all runs into today's fields?

Do not union base-tree and worktree ids in one `targets` vector. Tree-local ids such as
`"1/0"` can collide, and one hunk-wide confidence cannot say which run was exact, gap
attached, base mapped, or unmapped. It would also keep `signature_touch` ambiguous.

A nested `HunkMapping { hunk, runs: Vec<RunMapping> }` is also sound and preserves exactly
one outer record per Git hunk. It is the better shape only if external clients require that
cardinality. No current workspace consumer does. It requires a larger rewrite of the core
serde shape, `changes.rs`, backend JSON, and mapper tests. The flat form uses the existing
`Vec` at every layer and makes backend rows auditable with `run_index`, side, and range.

### Blast radius

The honest flat change is small but source-breaking where `HunkMapping` is constructed:

- core type/docs and `serde_roundtrips.rs`;
- mapper construction, all mapper fixtures, and the public function cardinality docs;
- `changes.rs` side selection and target-specific signature propagation;
- backend mapping JSON/view (several rows can share a hunk id, and new fields are exposed);
- the pure pipeline fixture; and
- `docs/research/03-change-mapping.md` plus the architecture summary.

`FileAnalysis`, `AnalysisSnapshot`, `ChangedSymbol`, dispatcher rows, digest symbols, graph,
and TUI snapshot types need no shape change. No new `MappingConfidence` variant is needed.
If compatibility with serialized old mappings matters, version the backend schema; a
serde default for an unknown side/revision would be ambiguous and is worse than a clear
break at this pre-1.0 stage.

## Confidence and `signature_touch` semantics

### Confidence

- **Exact:** actual changed lines are contained by the directly targeted worktree symbol,
  or actually add the complete symbol. Unchanged context never helps establish Exact.
- **Approximate(DocCommentOrGap):** an actual changed gap/doc-comment run is credibly
  attached within the threshold, or a deletion fallback attaches to a survivor without a
  base tree.
- **Approximate(DeletedHunkBaseMapped):** an actual `Del` run maps through the base tree,
  whether or not its Git hunk also contains context or `Add` runs.
- **Approximate(HunkSpansSymbols):** one actual changed run truly spans several targets.
- **Unmapped:** an actual run has no symbol association, including imports. It has no target
  and therefore creates no `ChangedSymbol` row.
- **Context-only overlap:** creates no run, target, confidence contribution, or symbol row.
  The independent structural sweep can still emit a genuinely side-only symbol.

Keep the current conservative worst-wins merge (`changes.rs:317-367`). Thus a replacement
with an exact new-side run and a base-mapped deleted run normally renders `~`: deleted-code
association is still approximate evidence. This is not a regression to hide. Repeated runs
for the same symbol keep one Git `HunkId`. An unmapped import run has no target, so it does
not downgrade an unrelated mapped symbol.

Equal-rank approximation reasons are currently first-wins. If the reason is important to
JSON clients, define a fixed precedence or retain run evidence; do not let refactoring order
silently choose it. The TUI only needs the `~` marker.

### Signature touch

Calculate signature touch from actual changed ranges on the tree used for that run:

- an `Add` run intersects a worktree target's `selection` -> touch that target;
- a `Del` run intersects a base target's `selection` -> carry the touch when folding to the
  qualified worktree survivor;
- declaration lines present only as `Context` never set it;
- doc-comment/gap associations do not set it; and
- signature evidence does not lower an otherwise exact mapping's confidence.

Store the touched target ids per run. At aggregation, OR only the flag belonging to the
current target. This preserves a signature edit for A without labeling B in the same Git
hunk as a signature edit.

## UI, backend, and AI inheritance

### Interactive TUI

1. `AnalysisEngine::analyze_changed_file` calls `changed_symbols_detailed` at
   `engine.rs:230-265`.
2. The result is cached as `FileSemanticResult.changed`.
3. `dispatcher.rs:1375-1399` converts that exact vector to symbol rows and uses its length
   for `changed_symbol_count`.
4. `dispatcher.rs:1419-1433` derives the `~` marker from each corrected record.
5. `codescope-tui/src/render.rs:659-709` draws the supplied count and marker.

Therefore `RootfsAttachmentConfig` and `NewRootfsAttachment` disappear automatically. A
legitimate doc/deletion/gap association remains and still displays `~`.

### Non-interactive backend

The eager engine path calls the same aggregator at `engine.rs:188-203`. `backend analyze`
exposes `snap.changed` and `snap.digest()` at `backend.rs:197-211`; `backend digest` uses the
same snapshot at lines 214-222. The backend and TUI do not have separate symbol mappers.

### AI digest tier 1

- `AnalysisSnapshot::digest` passes `self.changed` at `engine.rs:96-106`.
- The lazy TUI AI path collects each Ready file's `res.changed` at
  `dispatcher.rs:604-625`.
- `digest.rs:189-223` copies those records into tier 1.
- `digest.rs:408-437` renders the symbol and its confidence marker.

Tier 1 therefore needs no filtering change. The corrected set also reduces false changed
nodes before `build_impact_graph` queries them.

One adjacent issue is deliberately separate: digest tier-2 diagnostic filtering still adds
complete new-side hunk envelopes at `digest.rs:232-265`. Tier 1 is fixed by mapper/changes,
but a diagnostic that touches only hunk context can still enter tier 2. Tier 3 should remain
one summary per Git hunk. If run-accurate diagnostics are desired, tier 2 should use `Add`
runs; it should not change tier-3 hunk identity.

## Deterministic fixture plan — exactly 16 cases

All fixtures should use hand-built `SymbolTree`s and coordinate-complete `DiffLine` bodies.
A helper should derive/validate `old_len` and `new_len` from `Context + Del` and
`Context + Add`, so tests cannot repeat the current inconsistent header-only fixtures.
No fixture uses a live API, live LSP, or the `platform-2` worktree.

1. **Added body line.** A function declaration is context; one `Add` line is inside its
   body. Expect one new-side run, that function only, `Exact`, `signature_touch=false`.
2. **Deletion-only edit with `-U3` context and a base tree.** `new_len > 0`, but the only
   changed line is `Del` inside a surviving function. Expect an old/base run, fold to the
   worktree function as `Modified`, `DeletedHunkBaseMapped`; context neighbors are absent.
3. **Deletion-only edit without a base tree.** Put the deletion between context lines inside
   a surviving function. Expect the run-local insertion fallback, one approximate worktree
   target, and no header-wide targets.
4. **Body replacement.** Consecutive `Del` lines followed by consecutive `Add` lines inside
   one surviving function. Expect two run records with one `HunkId`, one aggregated symbol,
   one hunk in its provenance, and conservative approximate confidence from the old side.
5. **Two edit islands in one Git hunk.** Add inside `A`, then `Context` covering nearby `B`,
   then add inside `C`. Expect two ordered runs and changed symbols `A` and `C` only; `B`
   must not appear and neither run becomes `HunkSpansSymbols`.
6. **Actual signature replacement.** Replace `A`'s declaration line while the same hunk also
   edits `B`'s body. Expect target-specific signature touch on `A` only.
7. **Signature present only as context.** Add a body line while the declaration/selection
   line is hunk context. Expect `signature_touch=false`.
8. **Doc-comment edit.** Replace/add a comment immediately above a symbol while its
   declaration is context. Expect that symbol with `DocCommentOrGap`, `~`, and no signature
   touch; no adjacent context-only symbol.
9. **Real inter-symbol gap edit.** Change a short annotation/comment gap within the threshold.
   Verify the documented below-first/above fallback deterministically and keep the result
   approximate rather than dropping it.
10. **Import/prelude edit.** Put an added import close enough to a first declaration that a
    blind nearest-symbol heuristic would attach it. Expect an unmapped run and an empty
    changed-symbol set when the two trees are otherwise identical.
11. **Whole symbol addition.** A worktree-only top-level symbol, including its declaration
    and body, is added next to an unchanged context symbol. Expect only the new symbol as
    `Added/Exact`; the context symbol is absent.
12. **Whole symbol deletion with context.** A base-only symbol is deleted next to an
    unchanged survivor. Expect only the removed base symbol as `Deleted`/base-approximate;
    the survivor is absent.
13. **Whole file addition.** Multiple roots plus nested fields, all represented by `Add`
    lines. Expect every worktree key exactly once as `Added/Exact`; parents appear because
    their own declarations are actually new.
14. **Whole file deletion.** Multiple base roots plus nested fields, all represented by
    `Del` lines. Expect every base key exactly once as `Deleted` with base-mapped
    approximation and no worktree targets.
15. **Nested-field matrix.** In one stable parent, modify `Config.Keep`, delete
    `Config.Old`, and add `Config.New`. Expect qualified child records
    `Config.Keep` (modified), `Config.Old` (deleted), and `Config.New` (added), with no
    `Config` modified row unless its own declaration/doc line is also changed.
16. **`attachment.go`-shaped integration fixture.** Store static old/new symbol trees and
    literal `DiffLine`s for: (a) unchanged trailing context from
    `RootfsAttachmentConfig` before changed `PreparedRootfsAttachment`/state fields, and
    (b) unchanged trailing context from `NewRootfsAttachment` before the changed
    `OpenRootfsAttachment` signature. Assert the complete positive set (including intended
    nested fields), then assert that `RootfsAttachmentConfig` and
    `NewRootfsAttachment` are absent. Do not make this only a negative assertion: an empty
    mapper must fail the fixture.

Also update existing tests rather than retaining a second header-envelope behavior. Public
`map_changes*` cardinality assertions should use `(hunk.index, run_index)`; changed-symbol
assertions should continue to count unique Git hunks.

## Documentation updates required with the implementation

`docs/research/03-change-mapping.md` currently codifies the faulty envelope algorithm:

- lines 31-53 choose one `target_range` per hunk and select pure deletion by count;
- lines 55-60 describe hunk-spanning behavior rather than changed-run behavior;
- lines 87-97 show one targets/confidence pair per hunk; and
- lines 102-108 repeat worktree-hunk/base-pure-deletion and nearest-gap rules.

Replace that pseudocode with run extraction, explicit old/new tree selection, context
exclusion, frontier targeting, and old-target folding. Update
`docs/architecture.md:36-38` from “new-side hunks / pure deletions” to “new-side Add runs /
old-side Del runs.” Clarify the `Hunk::is_pure_deletion` documentation so callers do not
confuse an empty header side with a deletion-only edit island.
