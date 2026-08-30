# Review 10 — Multi-language (gopls multi-root, rust-analyzer) + ancestor_branches perf fix

Scope: working-tree diff vs `origin/main` (7b30a4c). Reviewed only the three described changes:
(1) multi-root gopls + language detection, (2) rust-analyzer adapter, (3) the
`ancestor_branches` for-each-ref rewrite. All other modified files were verified to be
rustfmt/import-order noise (token-stream comparison; no semantic change).

Verification performed against real tools on this machine: gopls v0.21.0 and
rust-analyzer 1.96.0 (live `initialize` handshakes, reference queries on scratch
workspaces), plus scratch git repos replaying the exact `for-each-ref` invocation.

---

## F1 — HIGH: ancestor ordering inverted — auto base picks the FARTHEST ancestor; picker order reversed

`crates/codescope-git/src/repo.rs:341-376` (`ancestor_branches`) now returns candidates
**nearest-first**: `--sort=-committerdate` is descending, and for a merged ref the tip *is*
the merge-base, so the newest-committerdate ref comes first. Empirically (scratch repo
X ← A ← B, branches `x`, `a`, HEAD=`b`):

```
refs/heads/b ... (current, filtered)
refs/heads/a ... (nearest — FIRST)
refs/heads/x ... (farthest — LAST)
```

Both consumers still assume the OLD order (oldest-first):

- `nearest_ancestor` (repo.rs:320-326) does `ancestors.pop()` → takes the **last** element,
  now the *farthest* ancestor. In the scenario above the inferred base is `x`, violating the
  invariant documented at repo.rs:214-216 ("for X <- A <- B, the base of B is A, not X") and
  the fn's own doc (repo.rs:318-319 "most recent common commit").
- `base_candidates` (repo.rs:293-295) does `.into_iter().rev()` under the now-stale comment
  "ancestor_branches returns oldest-merge-base first; reverse so the picker lists the nearest
  ancestor first" → the base picker lists ancestors farthest-first.

Impact: on any repo with ≥2 ancestor branches with distinct tips and no upstream configured,
Branch scope silently diffs against the *oldest* reachable branch tip (in a 5200-ref repo:
some years-old ref) — a huge, wrong changeset with a plausible-looking base label. This is
the default path (`infer_base` repo.rs:217 prefers nearest_ancestor over origin/HEAD and
guesses).

Why 405 tests pass: `git_repo.rs:397-429` and `:431-465` only ever construct a single
ancestor candidate or two refs pointing at the same commit, so pop-vs-first is invisible.

The stale doc block was left in place and now contradicts itself: repo.rs:328-329 (old:
"most recent merge-base LAST (so `pop` gives the nearest)") sits directly above the new
lines 330-335 ("nearest first").

Fix (small):
- `nearest_ancestor`: `Ok(ancestors.into_iter().next().map(...))`.
- `base_candidates`: drop `.rev()` (order is already nearest-first as the picker wants).
- Delete the stale doc lines 328-329.
- Add a regression test: three branches X ← A ← B, assert inferred base ref is `A` and
  `base_candidates` lists `A` before `X`.

## F2 — VERIFIED OK: the review-09 F3 empty-diff guard is preserved

Checked all three F3 cases (docs/review/09-recent-changes.md) empirically against the new
one-shot command:

- backup branch created at HEAD → listed by `--merged HEAD` but filtered by the
  `tip == head` check (repo.rs:367);
- pushed same-name remote branch at HEAD → same filter;
- descendant branch (mb == HEAD) → not listed at all (`--merged` lists only refs reachable
  *from* HEAD).

For a merged ref, merge-base(HEAD, ref) == ref tip, so the old `mb == HEAD` exclusion is
exactly equivalent to the new `tip == head` check on the retained set. `status.oid` is the
full 40-hex oid (porcelain v2 `branch.oid`) and `%(objectname)` is full hex — comparison
sound. Unborn HEAD is guarded before the command runs (repo.rs:337-339; `--merged HEAD`
would otherwise error). Empty for-each-ref output is fine (`stdout_trimmed` returns "" →
zero candidates).

Sort-key equivalence also holds: the old code ordered by merge-base committer time
(`show -s --format=%ct <mb>`); for merged refs tip == mb, so `--sort=-committerdate` uses
the same timestamp — only the *direction* changed (which is F1).

## F3 — MEDIUM: fork-sharing branches silently vanish from the base picker

Old `ancestor_branches` included any ref with *some* merge-base ≠ HEAD (doc: "or shares a
recent fork"): an integration branch that advanced past the fork point (e.g. `develop`,
`release/1.2`, or `main` after new commits) appeared with its fork point as the base.
`--merged HEAD` lists strict ancestors only — verified: a `main` advanced past the fork is
absent from the output.

`base_candidates` (repo.rs:279-316) only rescues advanced integration branches named
`origin/main`, `origin/master`, `main`, `master` (guess list, repo.rs:300). Repos based on
`develop`/`trunk`/release branches lose those picker entries entirely, and its doc ("All
plausible base branches") was not updated. `infer_base` is less affected (upstream /
origin/HEAD / guesses still cover the common cases).

Assessment: an intentional-looking narrowing (review-09 F3 explicitly offered "require
mb == candidate tip" as an option) with an unstated cost. Fix: document the narrowed
semantics in `base_candidates`, and/or run a bounded merge-base pass for a small set of
well-known integration names (`develop`, `trunk`, `release/*` tips capped at N refs) so the
picker keeps them.

## F4 — LOW (latent): rust_analyzer type_subtypes fabricates complete-empty evidence when the capability is advertised

`crates/codescope-lsp/src/rust_analyzer.rs:534-541`: after `require(...TypeHierarchySub)?`
succeeds the method returns `Ok(Evidence::complete(Vec::new()))` **without any wire
request**. That branch is reachable *only* when the server advertises
`typeHierarchyProvider` — precisely the case where returning "complete, zero subtypes" is
fabricated evidence. The analysis layer uses `type_subtypes` as the implementations
fallback (codescope-analysis/src/source.rs:84-87), so it would silently drop impact edges.

Today this is unreachable: verified by a live initialize handshake that rust-analyzer 1.96.0
(with `typeHierarchy` client capability offered) does **not** advertise
`typeHierarchyProvider`, so `require` returns `Unsupported` as the doc comment claims. The
live test (tests/rust_analyzer_live.rs:56-60, 92-105) pins this and will fail loudly if a
future rust-analyzer starts advertising it — good tripwire, wrong fallback behavior behind
it.

Fix: replace the body with an unconditional
`Err(SemanticError::Unsupported(Feature::TypeHierarchySub))` (matching the doc comment), or
implement the real `typeHierarchy/subtypes` round-trip like gopls.rs:544-582.

## F5 — LOW: gopls workspace-folder collapse condition is broader than its comment; leans on gopls ≥0.15 zero-config

`crates/codescope-lsp/src/gopls.rs:64-68`: the comment says "If the repo root itself has a
go.work", but the condition `go_roots.iter().any(|r| r == repo_root)` also fires when the
root has only a `go.mod`. A repo with a root module **plus** independent nested modules
(tools/, examples/) then collapses `workspaceFolders` to `[repo_root]`, dropping the nested
module folders this change was meant to load.

Verified live against gopls v0.21.0: zero-config gopls still serves the nested module in
that collapsed configuration (references + clean diagnostics on `tools/main.go`), so this
is not a defect with the supported gopls. With pre-0.15 gopls the nested modules would
silently degrade to "no package" behavior while `covers()` (gopls.rs:160-163) still returns
true, i.e. `handles()` claims ownership it can't serve.

Fix: make the condition match the comment (`if repo_root.join("go.work").exists()`), or
update the comment and document the gopls version floor the collapse relies on.

## F6 — LOW: workspaceFolders URI failure becomes an empty-string URI

`crates/codescope-lsp/src/gopls.rs:74-82`: `uri_from_path(dir).map(...).unwrap_or_else(|_|
String::new())` would send `{ "uri": "", "name": ... }` to the server. Unreachable in
practice (folders are absolute UTF-8 paths from the walker), but a `filter_map` that drops
the folder (with a `tracing::warn!`) is strictly better than a malformed folder entry.

## F7 — VERIFIED OK: repo-relative abs_path/file_id mapping; no single-module regression

- Both adapters interpret `FileId` against the **repo toplevel** (gopls.rs:146-157,
  rust_analyzer.rs:141-153); `main.rs:64,99` passes `repo.toplevel()`; codescope-git paths
  are toplevel-relative; the engine routes via `handles` (engine.rs:208). Cross-module
  result locations (references/call-hierarchy URIs in *other* modules) map back through
  `file_id()` to repo-relative ids uniformly — correct for multi-root.
- `covers()` (gopls.rs:160-163) correctly refuses `.go` files outside every loaded module
  folder (multi-root mode), preventing "No packages found" churn; in collapsed mode it
  covers everything under the root, consistent with the single workspace folder.
- Single-module path: go.mod at the toplevel produces `go_roots == [repo_root]` and an
  initialize payload identical to the old code (same rootUri, same single workspace folder,
  `current_dir` now repo_root — equal in that case). No regression. Repos whose only module
  lives in a subdirectory previously *failed to start* (`find_module_root` walked upward
  only → NoRoot → git-only mode); they now work — an improvement, not a regression.
- Asymmetry (informational): `RustAnalyzerService::handles` (rust_analyzer.rs:669-671) has
  no `covers()` analogue, so `.rs` files outside the chosen cargo root are still routed to
  rust-analyzer (detached-file behavior). Acceptable for the prototype; note it.
- Mixed-repo tie-breaking is Go-wins by design (service.rs:38-48): in a Go+Rust repo, `.rs`
  files are skipped by the engine with a note rather than analyzed. Documented; fine for
  "one server per session" scope.

## F8 — LOW: rust_project_root first-match nondeterminism and substring [workspace] check

`crates/codescope-lsp/src/detect.rs:128-161`: the first `Cargo.toml` whose text contains the
substring `[workspace]` wins, and `ignore::WalkBuilder` yields directories in unsorted OS
order. A repo with a second detached workspace manifest — the common `fuzz/Cargo.toml` with
an empty `[workspace]` table — can anchor rust-analyzer at `fuzz/` instead of the real root
depending on readdir order. `contains("[workspace]")` also matches commented-out tables.
Prototype-acceptable (the docstring admits the simplification), but prefer shallowest-path
preference among `[workspace]` hits (sort candidates by component count) or `cargo
locate-project --workspace`.

## F9 — INFO: go_module_folders may load fixture modules as workspace folders

`detect.rs:97-119` collects every non-gitignored `go.mod`/`go.work` under the root. Go
tooling repos routinely commit `testdata/**/go.mod` fixtures; each becomes a gopls workspace
folder (the collapse in F5 doesn't apply when the root has no marker). Standard `go mod
vendor` output contains no go.mod files, so vendoring is safe. Consider skipping `testdata`
directories or capping the folder count with a note.

## F10 — TRIVIAL: stale NoRoot message; Display-string coupling

- `error.rs:87`: "no Go module (go.mod) found at or above {0}" — detection now searches
  *under* the root and `go.work` also counts. Reword.
- `dispatcher.rs:147` matches `reason.contains("no supported language detected")` against
  `SemanticError::NoSupportedLanguage`'s Display text (error.rs:90-92). Works, but a typed
  signal (e.g. a dedicated DispatchEvent variant) would not silently break on rewording.

## Encoding & capability gating (focus items) — verified OK

- rust-analyzer: offers `["utf-8","utf-16"]` (rust_analyzer.rs:81); live handshake shows
  rust-analyzer 1.96 selects `positionEncoding: "utf-8"` → `PositionEncoding::Utf8` →
  identity conversions, matching the module doc. If an older r-a omits the field,
  `from_response_value` (encoding.rs) correctly falls back to the spec-default utf-16.
- gopls v0.21 omits `positionEncoding` entirely (verified) → utf-16 default → conversion
  helpers do real utf-16↔utf-8 work; unchanged from before this diff.
- Every rust-analyzer query path calls `require()` before the wire (documentSymbol:269,291;
  references:403; call hierarchy:440,475; implementation:510; hover:549; subtypes:539 — see
  F4 for the subtypes wrinkle). Hover is advertised by r-a (verified) and the Gopls service
  variant hard-returns `Unsupported` (service.rs:186-199) — currently no runtime callers, so
  no user-facing effect.

---

## Summary

| # | Severity | Where | Finding |
|---|----------|-------|---------|
| F1 | HIGH | repo.rs:293-295, 320-326, 341-376 | Ordering inverted: `pop()`/`.rev()` now select/list the farthest ancestor |
| F3 | MEDIUM | repo.rs:279-316 | Advanced integration branches (develop/trunk/release) silently dropped from picker |
| F4 | LOW (latent) | rust_analyzer.rs:534-541 | Supported-capability path fabricates complete-empty subtypes |
| F5 | LOW | gopls.rs:64-68 | Collapse fires on root go.mod too; relies on gopls ≥0.15 zero-config |
| F6 | LOW | gopls.rs:74-82 | Empty-string URI on (unreachable) workspace-folder URI failure |
| F8 | LOW | detect.rs:128-161 | rust_project_root first-match nondeterminism; substring `[workspace]` check |
| F9 | INFO | detect.rs:97-119 | testdata fixture modules become workspace folders |
| F10 | TRIVIAL | error.rs:87; dispatcher.rs:147 | Stale NoRoot wording; Display-string coupling |

Verified-good: F3(review-09) empty-diff guard preserved (`--merged` + `tip == head`);
one-subprocess perf shape; repo-relative FileId mapping consistent across both adapters and
multi-root results; single-module gopls path byte-identical initialize; subdir-module repos
now start (previously NoRoot); utf-8 negotiation + identity conversions for rust-analyzer;
capability gating before wire on all query paths; multi-root workspaceFolders accepted by
gopls 0.21 (live tests cover go.work-root and mixed-repo shapes).

**Verdict: request changes — F1 is a user-visible inversion of base auto-detection on the
default path, introduced by the perf rewrite and invisible to the current test matrix. The
fix is ~3 lines plus a regression test. Everything else is low/latent and can follow.**
