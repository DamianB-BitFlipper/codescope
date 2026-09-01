# Review 26 — stacked-branch base inference

Read-only Git-graph and UX design for codescope at
`080380c451cbd3516c24a12d4a0ed7d378363778`. I did not change Rust source, run a Cargo
command, or create, delete, or move a Git ref. This document is the only file I wrote.

## Decision

Enumerate local and remote-tracking refs in one object-agnostic command, canonicalize and
deduplicate their tip OIDs, and then use one streaming topological walk to filter strict ancestors
and rank them:

```text
git for-each-ref \
  --format=%(refname)%00%(refname:short)%00%(objectname)%00%(symref) \
  refs/heads refs/remotes

git rev-list --topo-order <captured-head-oid>
```

The first discovered tip encountered by `rev-list --topo-order` is an exact nearest candidate under
the required reachability definition. It is not a committer-date guess. For the reported stack,
that candidate is the remote-only `origin/feature/pr-4682-rootfs-2-vmhostd-nbd-ownership`, not the
farther local rootfs-1 branch.

An upstream whose merge base is `HEAD` must be discarded, not retained as a last-resort base. A
meaningful upstream remains the first choice. If every candidate is empty or invalid, return
`RepoContext.base = None`; do not manufacture an empty comparison.

Use this top-bar wording:

```text
codescope  <repo>  <checked-out-branch>  base: <comparison-base>
```

It is explicit, and the existing narrow-layout policy can drop the final `base: ...` span before it
drops the checked-out branch. The picker title is exactly
`comparison base (selected: <comparison-base>)`, never `current`.

## Confirmed defect and current paths

The root cause in `crates/codescope-git/src/repo.rs` matches the report:

- `ancestor_branches` invokes one `for-each-ref` at `repo.rs:381-391`, but its only namespace
  argument is `refs/heads` at `:388`. The comments at `:367-371` explicitly exclude remote-tracking
  refs. A remote-only stacked parent therefore cannot enter inference or the ancestor picker tier.
- The same call sorts by `--sort=-committerdate` at `repo.rs:386`. `nearest_ancestor` takes the first
  item at `:348-359`, so rebases, cherry-picks, and clock skew can choose a farther graph ancestor.
- `base_candidates` pushes the configured upstream at `repo.rs:298-306` without checking whether
  its merge base equals the captured HEAD. The inference path notices that condition at `:204-219`,
  but then returns the empty upstream anyway when it finds no *local* ancestor at `:220-225`.
- The current picker tiers and conventional branch list are at `repo.rs:291-343`.
- The dispatcher already prepends `(auto / inferred)` at
  `crates/codescope/src/dispatcher.rs:336-340`; that behavior should remain.
- The top bar documents and renders `{branch} ◂ {base}` at
  `crates/codescope-tui/src/render.rs:194-240`. Its width fallback already drops the base before the
  branch at `:233-263`. The picker says `current` at `render.rs:1771-1782`.

The data model already keeps the chosen ref and its merge base separate
(`crates/codescope-core/src/git.rs:109-118`), so this does not need a change to diff semantics.

## Real repository verification

The shared `platform-2` worktree moved after the bug was observed. At review time it was checked out
on `improvement/pr-4682-integrate-1-durable-action-ownership`; the rootfs refs had been rewritten,
and a local rootfs-2 ref now existed. The reproduction state is still present in the real object
database and reflogs:

| role at the reproduction time | OID | evidence |
|---|---|---|
| rootfs-1 local | `210294afd1a456dcdde4a9b895599987bbf482af` | `show-ref --verify refs/heads/feature/pr-4682-rootfs-1-fenced-attachments` |
| remote-only rootfs-2 | `8f10aa2052880557a89abc7d5684c36d259a658c` | `origin/...rootfs-2` reflog at 2026-08-31 09:04; the local branch reflog starts with creation later, at 10:18 |
| rootfs-3 local and upstream | `3534afde0977a88620fc421e11c7c708bbd211f2` | local and `origin/...rootfs-3` reflogs at 09:04/09:05 |

Read-only graph checks against those OIDs give:

```text
git merge-base 8f10aa2052 3534afde09
8f10aa2052880557a89abc7d5684c36d259a658c

git merge-base --is-ancestor 8f10aa2052 3534afde09
# exit 0

git rev-list --count 8f10aa2052..3534afde09   # 1
git rev-list --count 210294afd1..8f10aa2052   # 6
git rev-list --count 210294afd1..3534afde09   # 7
```

The reproduction rootfs-3 commit's sole parent is `8f10aa2052`. Thus rootfs-2 is a strict ancestor
one commit behind rootfs-3; rootfs-1 is six commits behind rootfs-2 and seven behind rootfs-3. The
rootfs-3 remote-tracking upstream was exactly rootfs-3, so it was an empty comparison. This explains
both the reported seven-commit view and the correct one-commit delta.

For performance context, the live repository currently has 1,311 local plus remote-tracking refs.
For the current rootfs-3 tip, the one-shot merged-ref query returns 62 rows; after removing
HEAD-equivalent rows, 60 labels share only 33 unique tip OIDs. Tip dedup is material here.

## Candidate discovery

### One namespace scan

Change `ancestor_branches` (`repo.rs:362-416`) to pass **both** namespace patterns to the same
`for-each-ref` process: `refs/heads` and `refs/remotes`. Remove `--merged` and the date sort. Do not
loop over refs and do not run `merge-base`, `show`, or `rev-parse` per row. Plain enumeration lets the
application deduplicate tip OIDs before the one graph walk; retaining `--merged` would make Git do
reachability work before that dedup and then repeat the walk for ranking.

The NUL-delimited fields make parsing explicit. Ref names cannot contain NUL, and the parser no
longer depends on whitespace splitting. Parse each newline-terminated record as:

1. full ref name;
2. display/short name;
3. raw tip object OID;
4. symbolic-ref target.

Reject a record when a field is missing or the OID is malformed. Do not request `%(objecttype)`:
that would dereference every object before tip dedup and can make a stray non-commit/broken ref fail
the scan. A missing-object or non-commit OID simply never appears in the commit-only `rev-list`
stream, so it is ignored without another Git process. Git may also warn and omit a syntactically
broken ref; log the warning and retain every complete valid record. Never retry a bad ref on its own.
A command-wide failure remains an error rather than silently becoming an empty picker.

### Symbolic aliases and strictness

Skip every record with a non-empty `%(symref)`, not only names ending in `/HEAD`, and retain the
`/HEAD` suffix check as a defensive guard for a dangling symbolic alias. This removes
`refs/remotes/origin/HEAD` and any other symbolic alias without a second `symbolic-ref` process.
The existing explicit `origin/HEAD` fallback may still resolve its real target later when needed.

Filter `tip_oid == captured_head_oid` before canonicalization. This removes:

- the checked-out local branch;
- its same-tip upstream, such as `origin/rootfs-3`;
- any other local or remote alias at HEAD.

The remaining raw rows are only *possible* candidates. The `rev-list` membership match below admits
only commit tips reachable from the captured HEAD. Because HEAD-equivalent tips were removed first,
every match is a strict ancestor and its merge base with HEAD is the tip itself. Unreachable sibling,
descendant, missing-object, and non-commit refs never match. No per-candidate merge-base call is
needed.

## Canonicalization and deduplication

Keep presentation identity separate from graph identity.

### Presentation policy

For `refs/heads/P`, the logical branch path is `P`. For `refs/remotes/R/P`, it is also `P`, with
remote provenance `R`. Only collapse records with the **same logical path and same tip OID**:

- `refs/heads/feature/a` plus `refs/remotes/origin/feature/a` at the same OID becomes one
  `feature/a` entry; the local spelling wins;
- a remote-only `origin/feature/a` remains selectable;
- local and remote refs with the same logical path but different tips both remain;
- `main` and `release/1` at the same OID both remain because their logical paths differ;
- two remote-only refs keep their remote-qualified identities unless a matching local ref wins.

This is deliberately not `HashSet<Oid>` presentation dedup. Sharing a tip does not prove that two
unrelated branch names have the same user meaning. If a configured upstream is merged into a local
twin during canonicalization, retain its upstream-priority flag/source while using the local label;
`RepoContext.upstream` still records the configured remote name.

### Graph-work policy

After presentation canonicalization, group all remaining entries by `tip_oid`. The graph ranker sees
one OID per group. When a group is emitted, expand its retained labels with local first, then
`origin/`, then other remotes and lexical name. This avoids duplicate graph work while preserving
unrelated same-tip choices in the picker.

Use a canonical seen key, not the current `HashSet<String>` at `repo.rs:297`. The same policy must
also prevent duplicate inferred/upstream/ancestor/conventional entries across picker tiers.

## Exact graph ranking

Let `E` be the deduplicated possible tip OIDs and `H` the captured HEAD OID. Stream:

```text
git rev-list --topo-order H
```

and perform an O(1) hash lookup for each output OID. A match both proves that the raw ref tip is a
commit reachable from `H` and emits its group in graph order; remove the group from the lookup map.
`--topo-order` guarantees that no parent is emitted before all of its children. Therefore, if the
first matched tip `A` were an ancestor of another eligible candidate `C`, Git would have emitted
`C` before `A`. That contradicts `A` being the first match. The first match is consequently a
strict ancestor that is not itself an ancestor of a closer eligible candidate, exactly the stated
rule. On `X <- A <- B(HEAD)`, the matches are `A`, then `X`.

The complete matched sequence is a linear extension of reachability: a descendant candidate always
precedes its ancestor. Incomparable candidates on two merge lanes have no graph-defined winner.
Either may be inferred; tests should assert that the result is maximal, not impose a commit-date
winner. Alias selection within one tip group uses the stable namespace/name rule above.

### Why not the alternatives

- `git merge-base --independent <tips...>` returns exactly the maximal frontier, so it is correct for
  finding *a* nearest candidate. It does not rank the remaining picker candidates, requires every
  tip on argv (an `ARG_MAX` concern with many refs), and Git must reduce the whole set before it can
  answer. It is more work than needed for the first match.
- Pairwise `merge-base --is-ancestor` checks are an O(refs²) subprocess design and are rejected.
  Refining only a committer-date prefix is not correct because the true nearest candidate can be
  outside that prefix.
- One topological `rev-list` has no candidate argv and visits graph commits once. With the one-shot
  ref scan, total work is O(refs + walked commits) and subprocess count is constant, rather than
  depending on the number of refs.

### Streaming and bounds

The current runner captures all output to completion (`crates/codescope-git/src/runner.rs:78-109`).
Add a hardened streaming-stdout path that preserves the same environment/config, drains stderr, and
can intentionally terminate and reap `rev-list`.

Use these explicit bounds:

```text
MAX_ANCESTOR_PICKER_ENTRIES = 256
MAX_RANK_COMMITS_AFTER_FIRST = 50_000
```

Inference has no pre-first-match cap: stopping before the first match could silently return the
wrong base. It terminates as soon as the first tip group matches, so its answer remains exact. The
picker continues from that exact first match until it has all known tips, 256 ancestor entries, or
has examined 50,000 additional commits. It then omits unranked far ancestors and logs/marks the list
as truncated; it must not append them in a guessed date order. Conventional branches are a fixed
small tier and may still be appended. Thus the cap limits optional picker refinement, never base
correctness.

## Inference, empty comparisons, and picker order

Introduce one meaningful-base predicate for automatic and picker candidates:

```text
merge_base(candidate, captured_head) exists && merge_base != captured_head
```

Apply it before insertion into `out` or `seen`.

1. If the configured upstream is meaningful, preserve current priority and return it first.
2. If its merge base equals HEAD, discard it completely and continue; do not execute the fallback at
   `repo.rs:220-225`.
3. Choose the first graph-ranked strict ancestor, local or remote.
4. Only then try `origin/HEAD`, conventional remote defaults, and local fork points. Apply the same
   empty guard to each result.
5. If nothing qualifies, return `None`. `changeset(ChangeScope::Branch)` already turns that into
   `GitError::NoBase` at `repo.rs:488-491`; staged, unstaged, and working scopes remain available.

`base_candidates` should build the same canonical ranked result rather than independently
recreating a different priority chain. The displayed order is:

1. `(auto / inferred)` — still prepended by the dispatcher;
2. the actual inferred candidate (a meaningful upstream when present, otherwise the nearest strict
   ancestor; remote-only is allowed);
3. remaining graph-ranked ancestor entries, with local labels preferred for twins;
4. conventional integration branches by logical name (`main`, `master`, `develop`, `trunk`,
   `release`), after canonical dedup and the meaningful-base guard.

For the regression fixture this is `(auto / inferred)`, `origin/...rootfs-2`, farther local
`...rootfs-1`, then any qualifying conventional integration branches. An empty
`origin/...rootfs-3` appears nowhere.

## UI wording and diff-direction invariant

Render the full left group as:

```text
codescope  platform  rootfs-3  base: origin/rootfs-2
```

This form is preferable to `rootfs-2 -> rootfs-3` in the current layout because branch and base
remain separate spans. At a narrow width, drop the whole `base: ...` span first and retain
`rootfs-3`; never leave a dangling arrow or show the base as if it were the checked-out branch.
When no base exists, use `base: none` if the span fits.

The picker block title is:

```text
comparison base (selected: origin/...rootfs-2)
```

and, for an honest no-base state, `comparison base (selected: none)`. The selected marker still
marks the actual comparison ref; `current` is not used because it is confused with the checked-out
branch.

These are presentation-only changes. Resolve the automatic base once per pipeline and pass that
same `BaseInfo.merge_base` into repository context and branch changeset construction. Today
`run_pipeline` obtains context and then calls a changeset API that infers again
(`crates/codescope/src/dispatcher.rs:1280-1286`); a ref movement between those calls could make the
label/overlay describe a different base from the diff. Removing that duplicate resolution is both a
performance improvement and an invariant guard. The diff command itself remains based on `HEAD` as
required.

Preserve all of the following:

- automatic branch scope constructs `<merge-base>...HEAD` at `repo.rs:488-495`;
- an explicit picker base constructs the same direction at `repo.rs:584-593`;
- the merge base is the old/base side and HEAD is the new/checked-out side;
- `DiffLine.old_ln` belongs to deleted/base lines and `new_ln` to added/HEAD lines
  (`crates/codescope-core/src/git.rs:368-413`);
- branch semantic overlay content comes from the merge-base OID, not the display ref
  (`crates/codescope-analysis/src/engine.rs:400-408`);
- changed-symbol revision ownership stays on the worktree/HEAD side.

Do not change the range to `HEAD...<base>`, swap the hunk sides, load the overlay from the remote tip
instead of `BaseInfo.merge_base`, or reinterpret the label arrow as a Git command direction.

## Exactly 24 test cases

The existing stacked/twin tests at `crates/codescope-git/tests/git_repo.rs:431-541`, fully-pushed
tests at `git_repo.rs:721-746` and `repo.rs:766-840`, top-bar tests at
`render.rs:2035-2078`, and picker test at `render.rs:2854-2865` need updates. Add or revise exactly
these cases:

1. **Historical regression:** `rootfs-1 <- origin/rootfs-2 <- rootfs-3`, with rootfs-2 remote-only
   and `origin/rootfs-3 == HEAD`; infer rootfs-2, report one branch commit, and omit both rootfs-1's
   seven-commit diff and the empty upstream.
2. **One-shot discovery contract:** a command-recording test asserts one `for-each-ref` invocation
   contains both `refs/heads` and `refs/remotes`, and no subprocess is launched per ref.
3. **Any remote namespace:** a remote-only strict parent under `upstream/feature/a` (not `origin`)
   remains eligible and can win.
4. **Symbolic aliases:** `origin/HEAD` and another remote symbolic alias that point at eligible
   commits are both omitted, while their real target refs remain.
5. **Broken refs:** a dangling/broken remote ref and a malformed record are ignored with a warning;
   a valid remote-only parent in the same scan still wins and no per-ref retry occurs.
6. **Same-tip twin:** `feature/a` and `origin/feature/a` at the same OID produce one picker entry,
   spelled `feature/a`; inference uses that canonical entry.
7. **Moved twin:** local `feature/a` and `origin/feature/a` at different OIDs both remain; neither is
   collapsed merely because the logical path matches.
8. **Unrelated same-tip names:** `main` and `release/1` at one OID both remain adjacent/selectable,
   while instrumentation shows their tip is ranked once.
9. **All HEAD aliases:** the current local branch, its remote upstream, and an unrelated same-tip
   local/remote ref are all excluded from inference and the picker.
10. **Adversarial timestamps:** in `X <- A <- B(HEAD)`, give X a newer committer date than A; A must
    still win.
11. **Long stacked chain:** several comparable candidates with reversed/skewed dates appear in
    descendant-before-ancestor order; the first is the direct stacked parent.
12. **Merge DAG:** with incomparable eligible tips on two merged lanes, the chosen tip is maximal;
    no test claims that timestamp makes one incomparable lane graph-nearer.
13. **Meaningful upstream:** an upstream behind HEAD (`merge_base != HEAD`) remains inferred first,
    ahead of another strict ancestor, and keeps upstream source priority.
14. **Empty upstream with a parent:** a same-tip upstream is absent from both inference and picker;
    the nearest remote-only strict ancestor replaces it.
15. **Honest no-base:** a same-tip upstream with no other meaningful candidate yields
    `RepoContext.base == None`, only the auto picker row, and `Branch` returns `NoBase` (also retain
    the unborn-HEAD no-base behavior).
16. **Invalid upstream:** a missing or unrelated-history upstream is skipped and a valid strict
    ancestor is still inferred; if none exists, the result is no-base rather than an error-shaped
    fake candidate.
17. **Conventional tier:** qualifying `main/master/develop/trunk/release` refs follow every ranked
    ancestor, canonical twins do not duplicate, and any conventional ref whose merge base is HEAD
    is omitted.
18. **Scale and bounds:** thousands of local/remote labels with many duplicate tips use one ref scan
    plus one topological walk, never O(refs²) commands; inference remains exact and picker
    refinement stops at the documented 256-entry/50,000-post-first-commit bounds.
19. **Dispatcher order:** the snapshot list is exactly auto first, inferred remote ancestor second,
    farther local ancestors next, and conventional branches last; selecting auto clears an
    override and reruns inference.
20. **Canonical upstream label:** a meaningful `origin/feature/a` upstream with a same-tip local
    twin is one local-spelled picker entry but retains upstream priority and the configured remote
    in `RepoContext.upstream`.
21. **Wide top bar:** render `rootfs-3  base: origin/rootfs-2`; assert the old `◂` wording is absent
    and the checked-out branch/base roles are unambiguous.
22. **Narrow top bar:** across the width sweep, `base: ...` elides before the checked-out branch and
    before the reserved LSP/AI state; no dangling arrow/label remains.
23. **Picker wording:** the title is exactly `comparison base (selected: origin/rootfs-2)` (or
    `selected: none`), contains no `current:`, and the marker is on the selected comparison base.
24. **Diff invariant:** for both inferred and explicit bases, assert the executed range remains
    `<merge-base>...HEAD`, deleted text/`old_ln` comes from the base, added text/`new_ln` comes from
    HEAD, the overlay is loaded from the merge-base OID, and changed-symbol revision ownership stays
    worktree/HEAD.

No tests were run during this review.
