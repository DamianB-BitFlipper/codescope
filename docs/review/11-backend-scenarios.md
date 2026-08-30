# Review 11 — JSON backend, branch-scope fallback, PendingScope guard, scenario library

Scope: the uncommitted working tree vs `origin/main`, limited to the described change set:
(1) `codescope scan|changeset|analyze|digest|bases` JSON backend
(`crates/codescope/src/backend.rs`, `crates/codescope/tests/backend.rs`, `main.rs` wiring),
(2) bugfixes — branch-scope working-tree fallback (`crates/codescope-git/src/repo.rs`) and
the scope-flicker fix (`crates/codescope-tui/src/run.rs` `PendingScope`, dispatcher
forwarding), (3) the scenario library (`crates/codescope-testutil/src/scenarios.rs`,
`crates/codescope/tests/scenarios.rs`).

Verification performed: `cargo test -p codescope --test scenarios` (17 pass),
`--test backend` (14 pass), `cargo test -p codescope-git --lib` (36 pass, incl. the new
fallback regression test), `cargo test -p codescope-tui --lib run::` (2 pass); plus live
probes of the compiled binary against five scratch repos (scenario-shaped, truly
fully-pushed dirty, net-empty-diff-ahead dirty, single-branch, clean fully-pushed).

Direct answers to the review questions:

- **Does the fallback mislabel a clean branch?** No. Fully-pushed + clean worktree stays
  `files: []` (verified live). The mislabel risk is the *dirty* case — see F2.
- **Does it double-count?** No. The fallback wholesale-replaces an empty committed diff
  (`repo.rs:472-477`); it never merges, and `working_tree_files` dedupes untracked against
  diffed paths (`repo.rs:540-549`). A file staged *and* unstaged appears once (unit test
  `repo.rs:773-778` asserts exact once-per-path membership).
- **Can PendingScope wedge or clobber a real update?** No wedge, no clobber found — see F6
  for the argument and the residual (cosmetic, transient) label/data mismatch.
- **Do the scenarios assert real behavior or a tautology?** Mostly real, with two
  exceptions: F1 (the headline `branch_fully_pushed` scenario does not exercise the
  fallback) and F3 (branch-count expectations of `0` assert an error mask, not behavior).

---

## F1 — MEDIUM: `branch_fully_pushed` scenario never exercises the fallback it claims to cover

`crates/codescope-testutil/src/scenarios.rs:337-356`. The scenario commits `f.go` on
`feature` (line 342) **before** `SetUpstream { to: "main" }` (line 343), so `feature` is
1 ahead of its upstream — merge-base(main, HEAD) ≠ HEAD, i.e. the branch is *not* fully
pushed. The committed branch diff is `[f.go]` (non-empty), so the `files.is_empty()`
fallback (`repo.rs:472`) never fires. Verified live on an identically-shaped repo:

```
$ git rev-list --left-right --count main...HEAD   → 0 1   (one ahead)
$ codescope changeset . --scope branch --compact
{"scope":"branch","files":[{"path":"f.go",...}]}          (no util.go)
```

The comment at line 350 — "The dirty worktree file must surface in branch scope
(fallback)" — is false: the dirty `util.go` is **not** in the branch scope. The expected
count `scope_counts: Some((1, 0, 1, 1))` (line 351) passes because of the committed
`f.go`, not the fallback. Reverting the entire fallback leaves this scenario green; the
only real coverage for bugfix 2a is the unit test
`crates/codescope-git/src/repo.rs:740-803` (`branch_scope_includes_dirty_worktree_when_fully_pushed`).

Fix: drop the `Write f.go` / `AddAll` / `Commit "feature work"` steps so `feature` sits at
the upstream tip (merge-base == HEAD). The expected tuple stays `(1, 0, 1, 1)` but the
branch `1` is then `util.go` via the fallback — and dies if the fallback regresses.
Keeping the current shape *as well* (renamed `branch_ahead_and_dirty`, branch count 1 =
`f.go` only) would pin the complementary fact that a non-empty committed diff suppresses
the fallback.

## F2 — MEDIUM: fallback output is unmarked — JSON/digest consumers misattribute uncommitted changes to the branch

`crates/codescope-git/src/repo.rs:472-477` (Branch arm) and `:568-570`
(`branch_changeset_with_base`), surfaced verbatim by every backend subcommand
(`crates/codescope/src/backend.rs:153-246`). Verified live on a truly fully-pushed dirty
repo (merge-base == HEAD, one unstaged edit, one untracked file):

```
$ codescope changeset . --scope branch --compact
{"scope":"branch","files":[{"path":"brand_new.txt","status":"untracked",...},
                           {"path":"tracked.txt","status":"modified",...}]}
$ codescope scan . --compact       → "scopes":{"branch":2,...}   (no note)
$ codescope digest . --scope branch --text
  repo: head=main base=upstream scope=Branch ... (2 changed files)
```

Nothing in the output marks the substitution. `changeset` emits no repo context at all,
and `scan`'s `head` carries no oid to compare against `base.merge_base`, so a tool (or
LLM) cannot detect that the "branch" files are actually uncommitted working-tree state
with **zero commits ahead**. The digest — the AI prompt payload — makes the same false
claim. The TUI at least shows ahead/behind context in the top bar; the JSON contract
(backend.rs:7-13, "stable for LLM/tool consumers") does not.

Also, the trigger is broader than the doc comment suggests: any *net-empty* committed
diff falls back, not just merge-base == HEAD. Verified: a branch **2 commits ahead**
(edit + revert) with one untracked scratch file reports that file as the branch change.

Not a mislabel of clean branches (empty stays empty), and no double count (replacement,
not merge). But the silent substitution makes the branch scope's JSON semantically wrong
exactly where a consumer has no way to notice.

Fix: make the substitution explicit. Minimal: an `origin`/`fallback` field on
`ChangeSet` (`codescope-core/src/git.rs:151-158`, `#[serde(default)]` for compat), set in
the two fallback sites, surfaced as a `scan`/`digest` note and in the TUI scope label
("branch — showing uncommitted; branch diff empty"). Optionally tighten the trigger to
`merge_base == HEAD` (the motivating bug) so a net-empty-diff branch stays honest.

## F3 — LOW: scenario driver masks branch-scope errors — several `branch: 0` expectations assert the mask, not behavior

`crates/codescope/tests/scenarios.rs:69-73`: `let branch_n = branch.unwrap_or(0);`
converts **any** error — not just `GitError::NoBase` — to 0, asymmetrically with the
other three scopes (`.expect(...)`). For the 8 scenarios expecting `branch == 0`, most
have no inferable base at all, so the assertion "passes" on the masked `NoBase` error by
construction; a regression that made the branch changeset fail with, say, a diff-parse
error on `binary_change` or `crlf_file` would stay green.

Fix:

```rust
let branch_n = match branch {
    Ok(n) => n,
    Err(e) if e.is_no_base() => 0,   // GitError::is_no_base, error.rs:70-73
    Err(e) => panic!("{}: branch scope failed: {e}", s.name),
};
```

Better still, add `branch_unavailable: Option<bool>` to `Expect` so no-base scenarios
assert the error *positively* instead of relying on the 0-mask.

## F4 — LOW: `Step::AddRemoteAndPush` is latent-broken (and unused)

`crates/codescope-testutil/src/scenarios.rs:505-511`. Two defects: (a) the bare repo dir
is never created and `git -C <dir> init --bare` requires an existing dir — verified:
exit 128, "cannot change to ... No such file or directory" — and the failure is swallowed
by `.ok()`, so the subsequent `push` is what fails, confusingly; (b)
`root.with_file_name(format!("{remote}.git"))` (line 507) escapes the TempDir — for a
root of `/tmp/.tmpXYZ` the bare path is the **shared** `/tmp/{remote}.git`: cross-test
collision and never cleaned up. No scenario currently uses the step, so nothing fails
today — but it is the step a *real* fully-pushed scenario (F1) would want.

Fix: `let bare = root.join(format!("_{remote}.git"));` (inside the TempDir),
`git(&bare_parent-safe init)` via `git -C root init --bare _origin.git`-style or
`std::fs::create_dir_all(&bare)?` first, and drop the `.ok()`.

## F5 — LOW: `scan` notes use Debug scope names, diverging from the contract's lowercase spellings

`crates/codescope/src/backend.rs:172`: `format!("{scope:?} scope unavailable: {err}")`
emits "**Branch** scope unavailable: ...", while every machine-facing spelling in the
same document is lowercase (`scopes.branch`, `"scope":"branch"`). The backend test
(`tests/backend.rs:211-224`, assert at :221) only passes because `GitError::NoBase`'s *message text*
happens to contain lowercase "branch" ("no base ref could be inferred for the branch
scope") — verified live; a rewording of that error breaks the test for the wrong reason,
and a consumer matching documented scope names won't match the note.

Fix: emit the canonical name — e.g. add a `fn scope_name(ChangeScope) -> &'static str`
(or reuse `Scope::as_str` by iterating over `[Scope; 4]` instead of `[ChangeScope; 4]`)
and assert the exact prefix `"branch scope unavailable"` in the test.

## F6 — INFO: PendingScope guard is sound; residual (accepted) behaviors worth a comment

`crates/codescope-tui/src/run.rs:81-101` (guard), `:160-168` (record+forward),
`:63-65` (reconcile before `App::update`). Checked against the dispatcher:

- **No wedge.** The dispatcher adopts every forwarded scope unconditionally
  (`dispatcher.rs:180-190`, `set_scope:280-285` — no refusal path, no autonomous scope
  change anywhere) and stamps every publish with its current scope
  (`build_snapshot`, dispatcher.rs:418). Actions travel a FIFO `mpsc` (`main.rs:108`,
  cap 64) whose `send().await` cannot drop while the dispatcher lives; so the pending
  pick is always eventually published and confirmed, and `reconcile` clears. The only
  non-confirming path — `set_scope` no-op when the pick equals the dispatcher's scope
  (no publish) — leaves a pending whose patch is an identity until the next routine
  publish clears it. Watch-channel coalescing (latest-wins) can only skip stale
  intermediates, which is strictly helpful.
- **No clobber of a real update.** The dispatcher never publishes scope X with another
  scope's data: `spawn_refresh` clears `repo_ctx`/`changeset` before the synchronous
  `publish_refreshing` (dispatcher.rs:296-299), and stale `AnalysisDone` results are
  epoch-gated. Hence a snapshot matching the pending scope is always genuinely for that
  scope, and patching applies only to snapshots that predate the user's action. A newer
  user pick simply re-records pending.
- **Lockstep cycling.** `next_scope` (action.rs:166-173) and the dispatcher's cycle
  (dispatcher.rs:184-190) use the same order, and modals swallow scope keys
  (action.rs:96-118), so repeated `ScopeCycle` cannot desync the two sides.

Residual: `reconcile` patches **only** `snapshot.scope`, so during the pending window the
UI shows the picked label over the previous scope's files/counts (bounded by the
dispatcher's synchronous refreshing publish — typically one frame). That is the intended
trade (label stability beats data staleness) but deserves a sentence in the `PendingScope`
doc so a future reader doesn't "fix" it by dropping whole snapshots. The regression tests
(`run.rs:175-238`) assert the real reconcile/confirm protocol against a live channel —
not a tautology.

## F7 — INFO: backend contract wording — read-only and determinism caveats are inherited, not new

`crates/codescope/src/backend.rs:1-13`. The git layer is genuinely read-only-hardened
(`codescope-git/src/runner.rs:17-43`: `GIT_OPTIONAL_LOCKS=0` + `--no-optional-locks`,
`core.fsmonitor=false`; every command in `repo.rs` is a read). Two inherited caveats the
contract text slightly oversells for `analyze`/`digest`:

- `snapshot_for` (backend.rs:258) starts a real language server in the repo;
  rust-analyzer with empty `initializationOptions` (`codescope-lsp/src/rust_analyzer.rs:79-90`)
  runs cargo/flycheck by default and writes `target/` inside the repo. Pre-existing
  (shared with the TUI), but "read-only" for the CLI should be scoped to "never writes
  tracked files or git state", or backend runs should pass a no-flycheck config.
- Byte-determinism is tested for `changeset`/`scan` only (`tests/backend.rs:505-536`);
  `analyze` embeds live LSP diagnostics, which are timing/version dependent. The doc's
  "deterministic output" bullet is accurate for paths/timestamps but not byte-stability
  of `analyze` with a live server.

The `GitOnlySource` stub (backend.rs:492-577) is correct: `handles() == false` routes
every file to the engine's per-file degradation (`codescope-analysis/src/engine.rs:208`),
`FeatureSet::default()` advertises nothing, and the never-reached query methods fail fast
with the recorded reason. Verified live: git-only `analyze`/`digest` exit 0, `lsp: null`,
notes populated, digest hunks intact.

---

## Verdict

No HIGH-severity defect. The backend wiring, the fallback implementation, and the
PendingScope guard are correct as implemented; both bugfixes do fix their bugs (verified
by live probes and the new regression tests). Ship-blockers: none. Recommended before
relying on the JSON contract: F2 (mark the fallback in output — the only finding that
produces factually wrong machine-facing output) and F1 (make the named scenario actually
cover the fallback). F3-F5 are small test/polish fixes.
