# Review 22 — changed-run symbol mapping

Reviewed the full `git show bc5b6e4` diff against the contract in
`docs/review/20-symbol-mapping.md`. This was a static audit. Per the
request, I did not run Cargo tests or Clippy. I did not change Rust source; this review
file is my only artifact.

**Counts:** BLOCKER 0 · MAJOR 5 · MINOR 4 · NIT 1

The refactor does extract ordered Add/Del runs and keeps Context out of direct mapping.
The flat multi-record API also reaches the aggregator and backend without a remaining
one-record-per-hunk consumer. It is not ready to ship, however. Baseless deletion targets
are labeled as base ids and dropped, deletion anchors are off by one in mixed hunks, and
the old whole-range container algorithm still reports parents instead of the nested
semantic frontier. Imports can still become symbol edits, and the structural sweep now
omits real added containers.

## BLOCKER

None.

## MAJOR

1. **`crates/codescope-analysis/src/mapper.rs:83-100,243-286`; `crates/codescope-analysis/src/changes.rs:121-137` — old-side worktree fallback targets are mislabeled as base ids and can disappear or resolve to an unrelated symbol.**
   `mapped_revision` is derived only from `run.side`, so every `Old` run is stamped
   `Revision::Base`. But `map_run_base` returns a worktree target when `base` is absent,
   and also falls through to that worktree fallback when a supplied base tree has no
   target. With `base == None`, aggregation sees `Base` and immediately skips the target.
   Thus a coordinate-complete deletion-only change can appear in `map_changes()` but
   produce no `ChangedSymbol`; in a baseless replacement, the Add evidence survives as
   `Exact` while the approximate Del evidence is silently lost. With a present but
   nonmatching base, a colliding tree-local id can instead resolve to an unrelated base
   node. Backend JSON can consequently claim `mapped_revision: "base"` while `base` is
   `null`. **Fix:** carry the actual target namespace in `RunMapping` and copy it into
   `HunkMapping`; successful base lookup returns `Base`, while a missing-base survivor
   fallback returns `Worktree`. When a base tree exists but has no credible target, retain
   the old-side run as `Unmapped` rather than switching trees. Add aggregation tests for a
   pure Del and a replacement with no base, asserting `Old + Worktree`, one deduplicated
   hunk, and worst-wins approximate confidence.

2. **`crates/codescope-analysis/src/mapper.rs:188-238,243-265` — the mapper still selects one container for a whole run instead of the deepest semantic frontier.**
   `find_smallest_containing(target)` returns immediately. A two-line replacement of
   adjacent `Config.A` and `Config.B` fields is contained by `Config`, while neither child
   contains the complete run, so both the Del and Add records target only `Config`.
   Existing children cannot be recovered by the side-only tree sweep, and the parent is
   falsely reported as modified. The fallback intersection paths inspect only roots, so a
   parent-declaration-plus-child run also loses the child. A nested child doc comment or
   gap that lies inside the parent's extent is likewise classified as an exact parent
   edit. **Fix:** partition each run by recursive deepest line ownership, merge equal
   owners, and retain the minimal target frontier. Prune an ancestor when descendants
   account for its apparent overlap; retain it only when changed evidence reaches its own
   declaration/selection or genuinely parent-owned body. Apply the same helper to base
   runs and add adjacent-sibling modification and nested-gap fixtures.

3. **`crates/codescope-analysis/src/mapper.rs:130-168` — the worktree anchor for baseless deletion islands is off by one.**
   After a Context/Add at 1-based new line `N`, `last_new = N - 1` points at the line just
   consumed, but the deletion insertion slot is the next zero-based index, `N`. The
   initial value is wrong in the other direction for a mixed hunk that starts with Del:
   `insertion_point_zero_based()` is defined for an empty new side, while the cursor before
   the first line of a nonempty new side is `new_start - 1`. At a symbol boundary either
   error can attach the deletion to the preceding/following symbol rather than the symbol
   at the run's own anchor. **Fix:** track the next new-side cursor: initialize from the
   first valid new-side record (or use `new_start - 1` for a nonempty side and `new_start`
   for an empty side), capture it before a Del run, and advance it to `new_ln` after each
   valid Add/Context. Test a mixed hunk starting with Del, a context-bounded boundary Del,
   and two separated deletion islands.

4. **`crates/codescope-analysis/src/mapper.rs:111-118,267-320` — import/prelude runs still attach to nearby symbols.**
   `ChangeRun` retains only side, range, and anchor, so `map_gap` cannot distinguish a doc
   comment from an import. It blindly attaches any unowned run within three lines to a
   top-level symbol; the base path does the same through `nearest_within`. An import two
   lines above the first declaration therefore reports that declaration as approximately
   changed, contrary to the required file-level `Unmapped` result. **Fix:** preserve a
   language-neutral lexical region (`Import/Prelude`, `Comment`, `Other`) with each run
   and gate gap attachment on credible comment/gap evidence. Until adapters supply that
   classification, fail conservatively for unclassified pre-first-symbol runs instead of
   using blind proximity. Test both added and deleted imports close enough that the old
   nearest-symbol rule would attach them.

5. **`crates/codescope-analysis/src/changes.rs:152-179` — the new Added sweep suppresses real one-side-only ancestors and intermediate containers.**
   Every entry in `added` has a `(qualified name, kind)` that is absent from the base, so
   it is a real added symbol. Dropping an entry merely because another added name starts
   with `"{name}."` is not parent-noise suppression. For a newly added
   `Outer.Inner.Field`, a whole-symbol run normally maps `Outer`; the sweep then skips
   `Outer.Inner` because `Outer.Inner.Field` exists, and only the leaf is recovered. The
   string-prefix check is not tree ancestry and can also conflate unrelated top-level LSP
   names that contain dots. An existing parent that only gained a child was never in
   `added` in the first place because its key exists in the base. **Fix:** restore the
   recursive sweep over every worktree-only key and use `record_if_absent`. Prevent false
   `Modified` parents in the semantic-frontier mapper, not by deleting genuine `Added`
   records. Add a whole nested-symbol addition fixture that asserts every recursive key.

## MINOR

1. **`crates/codescope-analysis/src/mapper.rs:189-235,246-265`; `crates/codescope-analysis/src/changes.rs:121-149,223-254` — target-specific `signature_touch` is still lost on several paths.**
   Multi-target and partial-overlap branches return no touched ids even when changed lines
   intersect a target's selection. A contained base run does compute the touched base id,
   but `aggregate_base_target` has no touch argument and hard-codes `false` for both the
   surviving and deleted entries. Therefore an old-side declaration edit folded by
   `(name, kind)` can be described as implementation-only, and a run can touch A's
   signature without recording it. **Fix:** after final target selection, compute
   `selection.intersects_lines(run.range)` independently for every target and pass
   `m.signature_touches.contains(target)` through base folding. Gap-only evidence will
   remain false naturally. Add the required A-signature/B-body and old-declaration
   survivor cases.

2. **`crates/codescope-analysis/src/mapper.rs:130-175` — malformed coordinates erase earlier valid evidence and can underflow.**
   On a changed line with a missing selected coordinate, `cur = None` discards the valid
   run accumulated before it instead of closing that run. For example,
   `[Add(new=10), Add(new=None)]` emits nothing. `nl - 1`, `coord - 1`, and the adjacency
   `end_line + 1` are also unchecked; zero or maximum coordinates are constructible via
   the public structs/serde and can panic in checked builds or wrap. The malformed line
   produces neither an unmapped record nor a traceable note. **Fix:** flush the current
   valid run before rejecting malformed evidence, use checked arithmetic and kind/side
   coordinate validation, and return or trace an explicit extraction issue. Add
   missing-first/middle/last, zero-coordinate, and coordinate-gap tests.

3. **`crates/codescope-core/src/mapping.rs:82-110`; `crates/codescope-core/tests/serde_roundtrips.rs:221-235` — required side/namespace fields have ambiguous serde defaults that do not provide compatibility.**
   Missing `side` silently becomes `New` and missing `mapped_revision` becomes `Worktree`,
   even though these fields are independent and semantically required (`Old + Worktree`
   is valid for a baseless fallback). Old records still fail to deserialize because the
   new `range` field has no default, so the defaults do not actually preserve the old
   schema; they only accept partially new records with potentially false meanings. The
   struct roundtrip covers only `New + Worktree`. **Fix:** make this an explicit breaking
   or versioned schema and remove the ambiguous defaults, or implement a real legacy
   migration. Add exact JSON/roundtrip cases for `Old + Base` and `Old + Worktree`, plus a
   backend case with repeated hunk ids and per-row signature targets.

4. **`crates/codescope-analysis/tests/pipeline.rs:68-95,103-125,182-202`; `crates/codescope-analysis/src/mapper.rs:683-741` — the integration/regression fixtures do not exercise the claimed run semantics.**
   The greet replacement header says 11 old/21 new lines but its body has one Add and no
   Del; structural diffing, not replacement mapping, supplies the deleted/added symbols.
   The missing-base deletion has an empty body and explicitly accepts `maps.is_empty()`, so
   a test named “survives missing base” cannot catch MAJOR 1. The main replacement asserts
   `Exact`, which currently passes because its approximate old fallback is mislabeled and
   skipped; correct aggregation is approximate. Several new mapper fixtures also give
   header counts unrelated to their bodies. **Fix:** use a builder that derives and
   validates header counts from coordinate-complete bodies. Assert exact
   `(hunk.index, run_index, side, range, mapped_revision, targets)` rows and aggregated
   unique-hunk behavior, and add the review-20 nested/import/signature/anchor/malformed
   matrix rather than weakening expectations to permit no record.

## NIT

1. **`crates/codescope-analysis/src/mapper.rs:1-7,46-50,63`; `crates/codescope-core/src/mapping.rs:42-48,66-67,93`; `docs/research/03-change-mapping.md:98-119` — public documentation still describes the replaced hunk/pure-deletion contract.**
   The mapper docs still infer target ownership from confidence and speak of one flag per
   hunk; core comments still say import blocks attach and `HunkSpansSymbols` uses a common
   ancestor; the research type sketch still shows the old three-field `HunkMapping`, and
   its recommendations revert to new-side hunks/pure deletions and approximate imports.
   The required updates to `docs/architecture.md:36-37` and the clarification at
   `crates/codescope-core/src/git.rs:312-315` were also omitted. **Fix:** consistently
   document Add/New and Del/Old runs, `(hunk, run_index)` identity, explicit target
   revision, repeated backend rows, frontier targets, and unmapped imports.

## Verified behavior

- For well-formed positive coordinates, Add uses `new_ln`, Del uses `old_ln`, and the
  1-based to 0-based inclusive run ranges are correct.
- Context, a kind switch, and a coordinate gap close a run. Context itself creates no
  mapping and cannot set `signature_touch`.
- Output order is deterministic: input hunk order followed by body run order, with a
  zero-based `run_index` per hunk. Target/hunk aggregation deduplicates repeated `HunkId`s.
- A normal replacement emits separate Old and New records. Successful base targets fold
  onto a surviving worktree symbol by qualified `(name, kind)`.
- The recursive deletion sweep and whole-file-added path without a base remain intact.
- No workspace consumer still assumes one mapping record per hunk. The engine stores the
  flat vector, aggregation iterates every row, and the backend preserves the row order and
  exposes `run_index`, `side`, `range`, `mapped_revision`, and `signature_touches`.
