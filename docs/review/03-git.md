# Review 03 — Git correctness (`codescope-git`)

Scope: porcelain v2 `-z` parsing (rename 2-token, unmerged 3-stage), unified-diff hunk
parsing (`,1` omission, len-0 sides, `\` markers, truncation), the read-only guarantee,
base inference, and worktree handling. Method: read every module in
`crates/codescope-git` plus the consumers (`crates/codescope/src/dispatcher.rs`,
`watcher.rs`), ran `cargo test -p codescope-git` (all green), and verified every claimed
defect empirically against git 2.50.1 — including end-to-end through the public
`GitRepo::changeset` API via a scratch out-of-tree probe crate (no project files were
modified).

## Summary

The core parsers are correct and carefully built. Verified against real git output:
porcelain v2 `-z` rename records consume exactly two NUL tokens (new path first), unmerged
`u` records use the 3-stage 10-field layout (`splitn(11).nth(10)`), headers are
NUL-terminated and parsed as documented; hunk headers normalize the omitted `,1`, handle
len-0 sides (`-0,0` / `+N,0`), drop `\ No newline` markers mid- and end-hunk without
counting them, and hard-error on truncated hunk bodies. Base inference implements the
research-02 chain exactly (upstream → origin/HEAD → guess → fork-point) with a merge-base
gate per candidate, and the unborn/detached/bare edge cases are right. Worktree discovery
is correct for main, linked, and relative-path worktrees (relative `--git-common-dir`
output is joined against the toplevel cwd, which matches where the command runs).

The defects are all at the *hardening boundary* the crate explicitly claims to own
(`lib.rs:5-8`, `runner.rs:1-11`): specific user configs and one inherited env var still
reach the parser, and each of them either hard-fails the entire changeset or silently
skews output. Worst is config-independent: one non-UTF-8 (e.g. Latin-1) text file near a
change makes every `changeset()` of that scope return `Err(NonUtf8)` forever — and
`dispatcher.rs::refresh()` aborts on that error, so the app never shows a fresh snapshot
again. All fixes are one-line flag/env additions except the non-UTF-8 one (lossy decode of
hunk *content* while keeping strict paths).

Everything below was reproduced with concrete commands; parser-level cases were also
confirmed through `GitRepo::changeset` end-to-end.

## Findings

### 1. One non-UTF-8 text file fails the whole changeset (and wedges refresh)

- **Severity: high**
- `crates/codescope-git/src/repo.rs:312,318,327` (`out.stdout_utf8("diff…")?`),
  `crates/codescope-git/src/runner.rs:129-133`
- What: `git diff` copies file content bytes verbatim into hunk bodies. A changed-or-near
  text file in a legacy encoding (Latin-1 `0xE9` etc.) is *text* to git (numstat `1 1`,
  not binary), so its raw bytes land in stdout, `stdout_utf8` fails, and `changeset()`
  returns `Err(NonUtf8)` for the **entire scope**. Verified end-to-end: a one-byte
  `caf\xe9` context line → `ERR: non-UTF-8 output from git (diff)`.
- Why it matters: it is input-dependent (no exotic config needed), all-or-nothing, and
  persistent — `dispatcher.rs::refresh()` propagates the error, so every subsequent
  refresh fails while the file stays modified; the user sees a permanently stale/empty
  view. Old repos with ISO-8859-1 READMEs/fixtures are common. The same strictness on
  *paths* (camino + `utf8_token`, `status.rs:120-124`) is a reasonable design line, but
  content should not share it.
- Suggested fix: parse the diff from `&[u8]` (or `String::from_utf8_lossy` per hunk-body
  line) so content is lossy-decoded while structural lines (headers, paths) stay strict.
  Alternatively catch the failure per `diff --git` section and degrade that one file to
  `binary: true`-style "no hunks" with a note, keeping the rest of the changeset.

### 2. `diff.suppressBlankEmpty = true` breaks every hunk containing an empty context line

- **Severity: medium**
- `crates/codescope-git/src/diff.rs:213-218` (`_ =>` arm rejects `""`),
  `crates/codescope-git/src/runner.rs:18-35` (`CONFIG_OVERRIDES` doesn't pin it)
- What: with this (documented, GNU-diff-compat) config set in the user's global config,
  git emits empty context lines as truly empty lines — no leading space. Verified:
  `\n \n` becomes `\n\n` in `od -c`, and end-to-end
  `ERR: malformed unified diff: unexpected line inside hunk "@@ -2,4 +2,4 @@ a": ""`.
  Since blank lines separate functions in virtually all code, any real diff with ≥3 lines
  of context hits this.
- Why it matters: total, hard failure of the crate's main job for affected users, and the
  crate's stated thesis is exactly this class of config hardening (`runner.rs:9-10`
  "user config … corrupts machine output").
- Suggested fix: add `-c diff.suppressBlankEmpty=false` to `CONFIG_OVERRIDES`. Optionally
  also accept an empty line inside a hunk body as context-of-empty when counts are unmet
  (belt and suspenders; GNU patch accepts this form too).

### 3. `diff.submodule = log|diff` breaks changeset parsing

- **Severity: medium**
- `crates/codescope-git/src/repo.rs:21-29` (`DIFF_FLAGS` has no `--submodule=short`),
  `crates/codescope-git/src/diff.rs:60-63` (only the `Submodule …` *first* line is skipped)
- What: with `diff.submodule=log`, a gitlink pointer change renders as
  `Submodule vendor/sub c7a150b..67e9781:` followed by `  > subject` lines; the first
  line is skipped but the indented ones hit "unexpected top-level line". Verified
  end-to-end: `ERR: … unexpected top-level line: "  > s2"`. With `diff.submodule=diff`
  the hardened invocation fails differently (verified: inner diff emits a literal
  `(diff failed)` top-level line under the crate's env, likely interacting with the
  `-c diff.external=` override; in a plain shell the inner diff parses but would attribute
  `vendor/sub/f.txt` — a submodule-internal path — to the parent repo, which is silently
  wrong).
- Why it matters: `diff.submodule=log` is a popular quality-of-life setting; any repo
  with a moved submodule pointer then fails the whole scope (same blast radius as #1).
- Suggested fix: append `--submodule=short` to `DIFF_FLAGS` (explicit flag beats config;
  keeps the verified gitlink section format the parser already handles).

### 4. Read-only guarantee gap: `core.fsmonitor = true` spawns a daemon that writes under `.git`

- **Severity: medium**
- `crates/codescope-git/src/lib.rs:7-8` ("nothing under `.git/` is ever written"),
  `crates/codescope-git/src/runner.rs:18-35,55-63`
- What: on repos with `core.fsmonitor=true` (a common macOS/large-repo perf setting), the
  crate's hardened `git status` implicitly starts `fsmonitor--daemon`. Verified: after one
  hardened status call, `.git/fsmonitor--daemon.ipc` (socket) and a `fsmonitor--daemon`
  cookie dir appear, and `git fsmonitor--daemon status` reports it watching the repo.
  `--no-optional-locks` / `GIT_OPTIONAL_LOCKS=0` do not cover this path.
- Why it matters: the headline read-only guarantee is factually overstated: a "read-only"
  inspection tool leaves a running background process and new files in `.git`. It is not
  corruption (git designed it), but it is exactly the class of side effect the crate
  promises not to have, and the `read_only_guarantee_smoke` test (git_repo.rs:529-549)
  only checks index bytes so it cannot catch it.
- Suggested fix: add `-c core.fsmonitor=false` to `CONFIG_OVERRIDES` (pure stat-cache; no
  semantic change to status output), and soften/scope the lib.rs claim.

### 5. `fingerprint()` misses the most common repo transition: another edit to an already-modified file

- **Severity: medium**
- `crates/codescope-git/src/repo.rs:367-397` (doc: "Changes whenever HEAD, the index, or
  the worktree status changes — the dispatcher uses it to detect repo-state generations")
- What: the fingerprint hashes HEAD + `ls-files --stage` + porcelain v2 bytes. Porcelain
  v2 ordinary records contain modes and HEAD/index OIDs only — **no worktree content
  hash** (git never hashes the worktree for status). Verified: editing an already-`.M`
  file a second time leaves both `status --porcelain=v2 -z` and `ls-files --stage -z`
  byte-identical, so the fingerprint does not change while the changeset content does.
- Why it matters: the documented purpose (repo-state generation detection for the
  dispatcher) silently fails for consecutive saves of one file — the single most frequent
  event in an editing session. Today the binary drives refreshes from `notify` watcher
  events and `fingerprint()` is only exercised by tests, so nothing is broken *yet*; it
  is a public API whose contract invites a stale-UI bug the moment someone uses it for
  dedup/short-circuiting.
- Suggested fix: fold worktree evidence into the hash (e.g. lstat size+mtime of paths that
  porcelain reports changed/untracked — two extra stats per dirty file), or re-document it
  as "index/HEAD/status-shape fingerprint, insensitive to worktree content".

### 6. Inherited `GIT_DIFF_OPTS` (and friends) survive env hardening

- **Severity: low**
- `crates/codescope-git/src/runner.rs:55-63` (env_remove list)
- What: git documents that `GIT_DIFF_OPTS` **takes precedence over the command line**.
  Verified: `GIT_DIFF_OPTS=-u10` turns the crate's `-U3` hunk `@@ -12,7 +12,7 @@` into
  `@@ -5,21 +5,21 @@` (end-to-end: parse still succeeds, hunks silently 3× wider, nearby
  hunks merge). Also not cleared: `GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES`,
  `GIT_COMMON_DIR`, `GIT_NAMESPACE`, `GIT_CONFIG_PARAMETERS`/`GIT_CONFIG_COUNT` — the same
  redirect-the-repo class as the already-removed `GIT_DIR`.
- Why it matters: wider hunks degrade hunk→symbol mapping precision downstream
  (codescope-analysis assumes `-U3` margins); the object/commondir vars can silently point
  reads at a different repository, defeating `runner.rs`'s own rationale for removing
  `GIT_DIR`.
- Suggested fix: extend `env_remove` with the six vars above.

### 7. `log.showSignature = true` corrupts `branch_commits` output

- **Severity: low**
- `crates/codescope-git/src/repo.rs:269-289` (`log --oneline --no-decorate`, then
  `split_once(' ')` per line)
- What: with signed commits plus this config, `--oneline` stdout gains verification text.
  Verified (ssh-signed commit): stdout becomes `b07e118 No signature\nsecond\n` — the
  parser then yields a bogus `CommitSummary{oid:"b07e118", subject:"No signature"}` plus
  `{oid:"second", subject:""}`. Silently wrong data, no error.
- Why it matters: commit list in the UI shows garbage for signed-commit workflows; also
  the only place in the crate where corrupted output is *accepted* rather than rejected.
- Suggested fix: pass `--no-show-signature` (available since git 2.10) in
  `branch_commits`.

### 8. Gitlink↔file conversion yields two `FileChange`s with the same path

- **Severity: low**
- `crates/codescope-git/src/diff.rs:47-73` (`parse_all`, one `FileChange` per section),
  `crates/codescope-git/src/repo.rs:344-345` (sort keeps both)
- What: replacing a submodule with a regular file (or vice versa) makes git emit **two**
  `diff --git a/vendor/sub b/vendor/sub` sections (verified: `deleted file mode 160000`
  then `new file mode 100644` with content hunks). The parser correctly produces
  Gitlink-deleted + Added-with-hunks, but the changeset now has duplicate `path` keys.
- Why it matters: `ChangeSet::find_file` (core/git.rs) returns the first match and
  `HunkId{file,index}` assumes per-path uniqueness — `vendor/sub#h0` resolves to the
  hunk-less Gitlink entry, so AI `focused_diff` references and hunk lookups can miss.
  Rare event, but silent ambiguity when it happens.
- Suggested fix: merge same-path sections into one `FileStatus::TypeChanged` entry
  carrying the content hunks (or document that paths are non-unique and make `find_file`
  callers hunk-aware).

### 9. Upstream-gone is reported as ahead 0 / behind 0

- **Severity: low**
- `crates/codescope-git/src/repo.rs:155-162` (`status.ahead_behind.unwrap_or((0, 0))`)
- What: when a configured upstream's remote-tracking ref is deleted (pruned), porcelain
  still prints `# branch.upstream origin/main` but omits `# branch.ab` (verified). The
  code then fabricates `Upstream{ahead:0, behind:0}` — indistinguishable from "in sync".
- Why it matters: status-bar honesty; the crate otherwise goes out of its way
  (Evidence/completeness) to avoid asserting facts it doesn't have. Base inference is
  unaffected (the dead upstream fails its merge-base gate and falls through — correct).
- Suggested fix: make ahead/behind optional in `Upstream` (or skip `upstream` when
  `branch.ab` is absent) and render "gone" like git's own `[gone]` marker.

### 10. `base_file_content` with a directory path returns a `tree …` listing as file content

- **Severity: low**
- `crates/codescope-git/src/repo.rs:353-366`
- What: `git show HEAD:dir` exits 0 and prints `tree HEAD:dir\n\nf.txt\n` (verified);
  the function returns that as `Some(content)`. Current callers only pass file paths
  taken from changesets, so this is latent.
- Why it matters: a future caller (e.g. deletion-overlay code resolving an old_path that
  became a directory) would feed a fake "file" into symbol mapping with no error.
- Suggested fix: reject tree output (`git cat-file -e <rev>:<path>` + type check, or
  cheap guard: treat output starting with `tree <spec>\n` as `Ok(None)`).

### 11. Unknown porcelain record tags are a hard error (forward-compat)

- **Severity: low**
- `crates/codescope-git/src/status.rs:154-158`
- What: `parse_status_z` errors on any record whose first byte isn't `#/1/2/u/?/!`.
  Unknown *headers* are skipped gracefully (status.rs:194 comment), but a future git
  adding a record type (as `?`→`!` and `u` once were added) fails the whole status parse,
  which fails `repo_context`/`changeset` for every scope.
- Why it matters: strictness is the right default for records the crate *does* consume,
  but the asymmetry vs headers means new-git users get a total failure instead of a
  degraded view. (Git does add porcelain v2 fields via new headers more often than new
  records, so this is genuinely low.)
- Suggested fix: skip-with-`tracing::warn!` on unknown record tags, keeping hard errors
  for malformed *known* records.

## Worktree note (cross-crate, informational)

`GitRepo` itself is worktree-correct — verified: discovery from main, linked, and
`--relative-paths` worktrees resolves `toplevel`/`git_dir`/`common_dir` correctly
(`repo.rs:60-118`; relative `--git-common-dir` is joined against the toplevel the command
ran in), and status/diff against a linked worktree use its per-worktree HEAD/index. But
`crates/codescope/src/watcher.rs:25-27` watches only `repo.git_dir()`; in a *linked*
worktree, shared state (`refs/`, `packed-refs`, `FETCH_HEAD`) lives under
`repo.common_dir()`, so ref updates from fetches or commits made in sibling worktrees
won't trigger `RepoChanged` there. `GitRepo` exposes `common_dir()` precisely for this —
the watcher should watch both when they differ. Belongs to the wiring review, flagged
here for completeness.

## Positive verifications (no findings)

- Porcelain `-z`: headers NUL-terminated; rename `2` record = record + second NUL token
  (new first, orig second), spaces in both paths preserved; `u` record path extracted
  after 10 fixed fields; `?` untracked; `!` skipped; `(initial)`/`(detached)` handled;
  `branch.ab +A -B` signs stripped. All confirmed against live `od -c` output
  (status.rs:133-262).
- Hunks: `,1` omission → len 1 (`diff.rs:346-351`); `-0,0`/`+N,0` len-0 sides with
  correct `is_pure_addition/deletion`; `\` metadata lines counted as nothing, mid- and
  end-hunk; body terminates exactly at count satisfaction; EOF inside a body →
  `ParseDiff("truncated hunk …")` (`diff.rs:188-224`); section text after the closing
  `@@` trimmed and optional. Mid-*header* truncation degrades to a hunk-less file rather
  than an error, which is fine because git can't exit 0 on a half-written patch and
  non-zero exits already error at `require_success`.
- Read-only: `--no-optional-locks` + `GIT_OPTIONAL_LOCKS=0` verified to keep a stat-dirty
  index byte-identical across hardened `status` and `diff` (50-file probe); all commands
  used are read commands; `kill_on_drop` leaves no lock files because none are taken.
- Base inference: chain order and per-candidate merge-base gate verified by the crate's
  own integration tests (upstream → originhead → guess → forkpoint); unborn → `Ok(None)`;
  `main` never uses itself as fork-point base; merge-base exit 1 (no ancestor) and 128
  (unknown ref) both correctly treated as "candidate unusable" rather than errors.
  Fork-point on git 2.50 falls back to the ref tip when the reflog is empty, so the
  missing plain-merge-base final fallback matters less than research 02 implied.
- `git diff --cached` on an unborn HEAD diffs against the empty tree (claim at
  repo.rs:296-297 verified, rc=0).
- `base_file_content` stderr matching is stable under the forced `LC_ALL=C`
  (`does not exist in` / `exists on disk, but not in` both verified verbatim).

## Verdict

**fix-first.** The parsing core, base inference, and worktree discovery are solid and
match verified git behavior, but findings 1–3 each turn one ordinary repository state
(legacy-encoded file, common user config) into a permanent whole-scope failure of the
crate's primary function, and 1 needs no special config at all. Findings 2, 3, 4, 6, 7
are one-line hardening additions in `runner.rs`/`DIFF_FLAGS`; finding 1 needs a small
parser change (bytes in, lossy content out). None require redesign.
