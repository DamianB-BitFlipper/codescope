# Review 23 — changed-run mapping re-review

Reviewed `bc5b6e4` plus the review-22 fix commit `cfdf9a8` statically. Per the
request, I did not run Cargo tests or Clippy. I changed no Rust source; this review file is
the only artifact.

**Counts:** BLOCKER 0 · MAJOR 3 · MINOR 3 · NIT 0

The namespace fix (M1) and recursive Added sweep (M5) are correct. The mixed-hunk part of
the anchor fix is also correct, and far-away imports still remain unmapped. The refactor is
not ready yet: the semantic frontier is used only on the new/worktree side, the new
`spans_child_boundary` approximation misses internal uncovered lines, and the anchor
initializer moved pure deletions one line backward.

## BLOCKER

None.

## MAJOR

1. **`crates/codescope-analysis/src/mapper.rs:280-304`; `crates/codescope-analysis/src/changes.rs:121-136`; `crates/codescope-analysis/src/changes.rs:813-842` — old/base runs still bypass the semantic frontier, so an ordinary sibling-field replacement reports the parent.**
   `map_run_worktree` uses `deepest_frontier`, but `map_run_base` still calls
   `base.find_smallest_containing(target)` and then falls back to intersected *roots*. For a
   Del run spanning two sibling fields, the smallest container is their struct. Aggregation
   folds that base struct onto the surviving worktree struct as `Modified`; the matching Add
   run maps to the two fields, leaving the unwanted parent in the final set. The new
   `sibling_field_edit_maps_to_fields_not_parent` test cannot catch this: its body contains
   only Add lines, its base has only one of the two fields, and it never asserts that
   `Greeter` is absent. Apply the same frontier partition to base runs, retaining
   `Revision::Base` and `DeletedHunkBaseMapped`, and test a coordinate-complete Del+Add
   replacement with both siblings on both sides and an exact negative assertion for the
   parent.

2. **`crates/codescope-analysis/src/mapper.rs:134-140,169-173`; `crates/codescope-core/src/git.rs:273-277,327-335` — the M3 initializer fixes mixed hunks but regresses every nonzero pure-deletion anchor by one line.**
   For a hunk with a nonempty new side, the cursor before its first body line is correctly
   `new_start - 1`; after a Context/Add at one-based `N`, assigning `last_new = N` correctly
   anchors a later deletion island at the following line. But Git gives a `new_len == 0`
   header different semantics: `new_start` is the line *after which* content was removed,
   and `Hunk::insertion_point_zero_based()` therefore returns `new_start`, not
   `new_start - 1`. The unconditional `saturating_sub(1)` now maps `@@ ... +10,0 @@` at
   zero-based line 9 rather than 10. At a boundary between `Prev` ending on 9 and `Next`
   starting on 10, the deletion attaches to `Prev` instead of `Next`. Initialize from
   `new_start` when `new_len == 0`, and from `new_start - 1` otherwise. The broad `main`
   ranges in `pure_deletion_without_base_attaches_to_surviving_container` and
   `baseless_deletion_aggregates_onto_worktree_survivor` mask this regression.

3. **`crates/codescope-analysis/src/mapper.rs:201-238,256-267` — `spans_child_boundary` does not test whether children actually cover the run, which can drop parent-owned evidence and falsely return `Exact`.**
   The comments promise union coverage, but the implementation checks only whether the run
   begins before the first intersected child or ends after the last. For children `A=12..14`
   and `B=18..20`, a changed run `12..20` makes both checks false even though changed lines
   `15..17` are covered by neither child. The parent is omitted. Because the run fully
   contains the two emitted child ranges, the rule at lines 259-260 then labels the
   incomplete frontier `Exact`. Merge/clip the intersecting child intervals and retain the
   parent whenever any line in `target ∩ node.range` is uncovered (or perform the promised
   line-ownership partition). Add a fixture with an internal gap between siblings. The
   exact-confidence rule is otherwise sound for a complete frontier.

## MINOR

1. **`crates/codescope-analysis/src/mapper.rs:251-255,283-304`; `crates/codescope-analysis/src/changes.rs:121-147,212-243` — base-side signature touches are still discarded during aggregation.**
   New-side frontier targets now compute selection intersection independently and the
   worktree branch propagates it correctly. A contained base run also computes a touched
   base id, but `aggregate_base_target` accepts no touch flag and hard-codes `false` for both
   a survivor and a deletion; the multi-target base path computes no touches at all. Thus an
   old declaration edit folded onto a survivor can still be described as body-only. Pass
   the target-specific flag through the base fold and compute it for every base-frontier
   target.

2. **`crates/codescope-analysis/src/mapper.rs:137-163` — the malformed-coordinate fix flushes valid evidence, but zero and maximum coordinates still underflow/overflow.**
   `coord - 1` panics/wraps for zero and `r.range.end_line + 1` does so at `u32::MAX`.
   `Hunk`/`DiffLine` are public and deserializable, so these values are constructible outside
   the Git parser. Use checked arithmetic and reject/trace the malformed record. This is the
   unresolved arithmetic half of review 22 MINOR 2.

3. **`crates/codescope-analysis/src/mapper.rs:788-807`; `crates/codescope-analysis/src/changes.rs:789-842`; `crates/codescope-analysis/tests/pipeline.rs:68-95,188-207` — the regression tests do not establish several properties their names/comments claim.**
   The sibling test has no Del run and no parent-absence assertion. The replacement mapper
   test checks namespaces but not aggregation/deduplication and declares 3/3 header counts
   for a 2/2 body. There is no boundary-sensitive pure-deletion anchor test or mixed-hunk
   deletion-island test. The pipeline's missing-base deletion still has an empty body and
   explicitly permits no mapping. These gaps let MAJOR 1 and MAJOR 2 coexist with the green
   suite.

## NIT

None in the reviewed fix paths.

## Prior-MAJOR verification

- **M1 — resolved.** `RunMapping.revision` is copied to `HunkMapping`; successful base
  targets are `Base`, while survivor/gap fallback targets are `Worktree`; aggregation now
  branches on `mapped_revision`. A baseless Del therefore records a worktree `Modified`
  survivor instead of being dropped. `replacement_maps_both_sides_of_the_run` verifies the
  normal Base/Worktree pair, and `baseless_deletion_aggregates_onto_worktree_survivor`
  verifies final aggregation (subject to the separate anchor bug above).
- **M2 — only partially resolved.** On the new side, two adjacent sibling-field lines map to
  the fields without the parent; a one-field run maps only that field; and a struct's own
  declaration line maps to the struct. Base-side runs and internal child gaps remain wrong
  as described in MAJOR 1 and MAJOR 3.
- **M3 — only partially resolved.** `last_new = nl` correctly means the next slot for a
  mixed-hunk deletion island. Pure-deletion initialization regressed as described in MAJOR
  2.
- **M4 — no regression within the acknowledged distance-only design.** A prelude/import run
  farther than `GAP_ATTACH_LINES` reaches `map_gap` and stays `Unmapped`; the existing
  far-from-symbol fixture still exercises that path. Close imports remain the accepted
  lexical-classification follow-up.
- **M5 — resolved.** The worktree-only sweep again walks every recursive key and calls
  `record_if_absent`; whole-struct/whole-file additions therefore retain parents,
  intermediate containers, and leaves as `Added`.
