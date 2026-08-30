# Review 09 — Recent changes (key chain / Anthropic client / pickers / Working scope / base override)

Scope: ONLY the change set `git diff HEAD~1` + working tree (AI key chain & provider-inferred
base URL, native Anthropic envelope, `set_model`/`list_models`, TUI model picker `m` and base
picker `b`, `Working` scope `w`, nearest-ancestor default base, dispatcher wiring for
`BaseLoaded`/`ModelsLoaded` and `base_override` threading). Method: read every changed file plus
its consumers; compared against the pre-change dispatcher (`git show HEAD~1:...`); verified the
`%(refname:short)` behavior for `origin/HEAD` and the `list_models` URL derivation empirically.
Prior reviews (01–08) cover the rest of the codebase.

## Summary

The core flows are right: the key-chain resolution and provider-inferred base URL are correct and
well-tested; the Anthropic envelope mapping (system hoisting, `tool_use`/`tool_result` conversion,
role-merge for strict alternation, `tool_choice: any`, string-arguments round-trip) is correct for
every message sequence the service loop can actually produce; picker selection/forwarding is
race-safe against stale lists (`.get()` + empty-name guards on both sides); the base override
threads coherently through `repo_context_with_base` → `run_pipeline` → `refresh_with_ctx` →
snapshot; and the `Working` scope diff (`git diff HEAD` + unmerged marking + untracked append,
old side `HEAD`) is correct and integration-tested.

The defects cluster in three places: the **Anthropic `/models` URL is wrong** (and the default
model is a Prime-only id, so an `ANTHROPIC_API_KEY`-only setup cannot work out of the box, not
even via the picker); the **nearest-ancestor heuristic accepts refs whose merge-base is HEAD
itself** (pushed same-name remote branches, backup/descendant branches → silently empty branch
diff); and the **same-epoch refresh race** (pre-existing) now covers the new base-pick path.

## Findings

### F1 — HIGH: Anthropic `list_models` drops `/v1` from the URL

`crates/codescope-ai/src/client.rs:277-284`. The endpoint is `{base}/chat/completions`
(OpenAI-compatible, two path segments) or `{base}/messages` (Anthropic, one segment), but the
code strips **two** segments unconditionally (`rsplit_once` at client.rs:279 and again at
client.rs:283). For Anthropic this turns `https://api.anthropic.com/v1/messages` into
`GET https://api.anthropic.com/models` (missing `/v1`) → 404. The dispatcher swallows the error
(`crates/codescope/src/dispatcher.rs:219` `unwrap_or_default`), so the model picker silently
shows "no models loaded". No test covers the URL derivation (client tests stop at the endpoint
join, client.rs:759-762).

Fix: derive per provider — strip one segment for `ProviderKind::Anthropic`, two for
`OpenAiCompatible` — or store `base_url` on the client and join `{base}/models` directly.

### F2 — HIGH (Anthropic) / MEDIUM (OpenAI): `DEFAULT_MODEL` is a Prime-only id

`crates/codescope-ai/src/config.rs:37` (`openai/gpt-5-mini`) is applied regardless of which key
was found (config.rs:156-158). That id is only valid on Prime Inference. With `OPENAI_API_KEY`
the base is `api.openai.com` but the model string is not an OpenAI id; with `ANTHROPIC_API_KEY`
every `POST /v1/messages` 404s on the model. Combined with F1 the Anthropic path is unusable
in-TUI: default model invalid **and** the picker cannot fetch alternatives. The provider-inferred
base URL (the point of this change) is undermined by a provider-blind default model.

Fix: make the default model follow `KeySource` (e.g. `gpt-5-mini` for OpenAI, a
`claude-*-latest` alias for Anthropic, keep `openai/gpt-5-mini` for Prime), mirroring the
`default_base` selection at config.rs:141-145.

### F3 — MEDIUM-HIGH: nearest-ancestor accepts refs whose merge-base is HEAD → empty branch diff

`crates/codescope-git/src/repo.rs:344-355`. `ancestor_branches` never checks that a candidate is
a *strict* ancestor: the tip is parsed and discarded (`_tip`, repo.rs:346) and there is no
`merge_base == HEAD` exclusion. Any ref whose merge-base with HEAD **is HEAD itself** carries the
newest possible commit timestamp and therefore wins `nearest_ancestor` (repo.rs:320 `pop()`):

- the current branch pushed to `origin/<branch>` **without upstream tracking** (only the exact
  local name is excluded, repo.rs:347);
- a backup/experiment branch created at HEAD (`git branch backup`);
- any descendant branch.

Result: the default base becomes that ref and the Branch scope renders an **empty changeset**
(`mb...HEAD` where `mb == HEAD`) with a plausible-looking base label — silent, wrong, and the
default path. The same refs also pollute `base_candidates` (repo.rs:293).

Fix: skip candidates whose merge-base equals `status.oid`; additionally exclude
`*/{current_branch}` remote-tracking names. If "ancestor" is meant literally, require
`mb == candidate tip` (tip is an ancestor of HEAD), which also removes the stacked-sibling
mislabeling (a branch forked mid-way off the current branch shows up under the sibling's name).

### F4 — MEDIUM: same-epoch refresh race now covers base picks

`crates/codescope/src/dispatcher.rs:271-293` — `spawn_refresh` tags jobs with the *current*
epoch without bumping it; the apply gate (dispatcher.rs:320) only rejects mismatched epochs.
Two action-triggered refreshes (epoch unchanged — only `RepoChanged` bumps, dispatcher.rs:162)
therefore race: **last to finish wins**. Pre-existing for scope switches, but this change adds
`set_base` (dispatcher.rs:254-261) where the window is real (merge-base + full branch diff +
analysis). Pick base A then base B quickly → if A's pipeline lands last, the published snapshot
shows A's diff/base while `self.base_override` is B; `build_snapshot` then mixes stale panes with
current `self.scope` (dispatcher.rs:385). Self-heals only on the next refresh.

Fix: bump the epoch in `spawn_refresh` (cheapest, and consistent with "one epoch per accepted
change"), or gate `on_analysis_done` on `(epoch, scope, base_override)` captured at spawn time.

### F5 — MEDIUM: an invalid base override wedges every subsequent refresh

`crates/codescope/src/dispatcher.rs:438` routes **all** scopes through
`repo_context_with_base(base_override)`; `crates/codescope-git/src/repo.rs:162-171` hard-errors
(`GitError::NoBase`) when the override no longer yields a merge base (branch deleted, remote
pruned, history rewritten). Because the override is never cleared on failure
(dispatcher.rs:331-333 only sets a message), every refresh — including `RepoChanged` and
non-Branch scopes that don't need a base — fails until the user picks a new valid base. The
picker offers no "(auto)" entry to return to inference (render.rs:472-520), and `set_base`
ignores empty names (dispatcher.rs:255).

Fix: on `NoBase` with an override set, drop the override and re-run inference (surface a
message); add an "(auto / inferred)" row to the base picker; skip the base computation for
non-Branch scopes.

### F6 — MEDIUM (perf): ancestor scan = 2 git subprocesses per ref, per refresh

`crates/codescope-git/src/repo.rs:333-355`: for every candidate ref, one `git merge-base`
(repo.rs:351) plus one `git show -s --format=%ct` (repo.rs:353), sequentially awaited. This runs
inside `infer_base` (repo.rs:217) → `repo_context` → **every** pipeline run (watcher events,
scope switches, `R`) whenever no upstream is configured, and again in `base_candidates` when the
picker opens. A repo with a few hundred remote-tracking branches pays hundreds of subprocess
round-trips per refresh.

Fix: batch — e.g. one `git for-each-ref` + a single `git rev-list --topo-order` walk, or
`xargs`-style `git merge-base` batching; at minimum cache results keyed on (HEAD oid, refs
digest).

### F7 — LOW-MEDIUM: `origin/HEAD` enters candidates as a ref literally named `origin`

`crates/codescope-git/src/repo.rs:347`. `%(refname:short)` renders `refs/remotes/origin/HEAD`
as `origin` (verified empirically with git 2.x), so the `ends_with("/HEAD")` guard never fires.
A pseudo-candidate named `origin` (duplicate of the default branch tip) appears in the picker
list and in ancestor inference, and can become the displayed base name.

Fix: emit `%(refname) %(symref)` and skip entries with a non-empty symref (or skip
`refs/remotes/*/HEAD` before shortening).

### F8 — LOW-MEDIUM: picker candidate ordering contradicts the documented order

`crates/codescope-git/src/repo.rs:276-277` promises "ancestor branches (most recent common
commit first)", but `ancestor_branches` sorts ascending (repo.rs:358, oldest merge-base first —
only `pop()` at repo.rs:320 relies on that) and `base_candidates` consumes it in that order
(repo.rs:293). The picker therefore lists the *farthest* ancestor first and the default/nearest
base last among ancestors.

Fix: `for b in self.ancestor_branches(&status).await?.into_iter().rev()`.

### F9 — LOW: `branch_changeset_with_base` skips the path sort

`crates/codescope-git/src/repo.rs:509` builds the `ChangeSet` directly from
`parse_unified_diff`, bypassing the `files.sort_by(path)` every other scope gets
(repo.rs:492). Git output is usually path-ordered, but rename pairing can reorder entries, so
override-based views may order files differently from inferred-base views.

Fix: sort before `ChangeSet::new` (share the tail of `changeset()`).

### F10 — LOW: stale `CODESCOPE_AI_API_KEY` in a user-facing message

`crates/codescope/src/dispatcher.rs:188` still says "AI not configured (set
CODESCOPE_AI_API_KEY)" — an env var this change set removed. The sibling messages
(dispatcher.rs:212, dispatcher.rs:232) were updated.

Fix: "set PRIME_API_KEY / OPENAI_API_KEY / ANTHROPIC_API_KEY".

### F11 — LOW: `Working` scope errors on unborn HEAD

`crates/codescope-git/src/repo.rs:471` runs `git diff HEAD`, which fails in a repo with no
commits, so pressing `w` in a fresh repo yields "analysis failed: …". `Staged` deliberately
supports unborn HEAD (repo.rs:417-418) and the engine side already handles it
(`crates/codescope-analysis/src/engine.rs:305-307` returns `None`); only the git side errors.

Fix: on unborn HEAD, diff against the empty tree (`git diff $(git hash-object -t tree
/dev/null)`) or fall back to index+worktree composition.

### F12 — INFO: Anthropic envelope details (mapping itself verified correct)

- `crates/codescope-ai/src/client.rs:560`: `max_tokens` hardcoded to 4096 — a large plan can
  truncate mid-`tool_use` (Anthropic then returns incomplete `input` → plan parse failure).
  Consider a larger cap or config knob. The OpenAI path sets no ceiling.
- `client.rs:660-668`: `parse_anthropic_response` rebuilds the assistant echo from tool calls
  only, dropping text blocks — fidelity loss between turns; would become a hard API error only
  if extended thinking were ever enabled.
- `client.rs:338-347`: `anthropic-version` is attached only when a key exists (and silently
  dropped, along with auth, if the key has non-header-safe bytes).
- `client.rs:678-690`: Anthropic `/v1/models` is paginated (default page ~20); `has_more` is
  ignored, so long model lists truncate. Moot until F1 is fixed.
- `crates/codescope-ai/src/config.rs:213`: `provider()` matches `anthropic.com` anywhere in the
  URL, not just the host (docstring says host). Self-inflicted misconfig only.

Positively verified: system hoisting, `tool` → user/`tool_result` conversion, same-role merge
(consecutive tool results merge into one user turn, as Anthropic requires), `tool_choice:
{type:any}` ≙ `required`, object/string arguments round-trip through the echo path, and the
service loop (`crates/codescope-ai/src/service.rs:142-198`) never emits user text after tool
results, so the tool_result-ordering constraint cannot be violated.

### F13 — INFO: picker ergonomics / shared limiter

- `crates/codescope-ai/src/client.rs:286`: `list_models` draws from the same 10-rpm token bucket
  as plan requests — repeatedly opening the picker can throttle the next `A` refresh.
- `crates/codescope/src/dispatcher.rs:219` swallows list errors; `render.rs:486` then shows
  "fetching base candidates…" indefinitely even after an empty `BaseLoaded([])` arrived.
- The model picker opens with the cursor at row 0 rather than the current model
  (`crates/codescope-tui/src/app.rs:122-125` resets `model_sel`).
- Modals swallow Ctrl-C (`crates/codescope-tui/src/action.rs:101-116` precede the CONTROL branch
  at action.rs:119) — same pattern as the pre-existing help modal, but a global Ctrl-C escape
  would be safer.

## Verified correct (no action)

- Key chain `PRIME > OPENAI > ANTHROPIC`, empty-as-unset, `api_key_env` hard error, literal-key
  rejection, redacting Debug — all covered by tests (`config.rs` tests).
- `set_model`/`model()` interior mutability: single `Mutex<String>`, no stale copies anywhere
  (service delegates, `build_snapshot` reads live via `a.model()`); a mid-plan model switch just
  changes the model between turns, which providers accept.
- Picker forwarding: Enter resolves the name in `run.rs::dispatch` from
  `app.snapshot.available_*[sel]` via `.get()` (stale/short lists → silent close, no panic);
  empty names guarded in both `run.rs` and `Dispatcher::set_base`; `step()` handles empty lists;
  render clamps the highlight index.
- Base override threading: `repo_context_with_base` → `BaseSource::Override` → `run_pipeline`
  (`dispatcher.rs:431-452`) → `refresh_with_ctx` → snapshot `repo_ctx`; top bar prefers the
  authoritative `base_ref` with a pending-override fallback (`dispatcher.rs:374-382`), covered by
  dispatcher and render tests.
- `Working` scope: `git diff HEAD` (staged+unstaged) + unmerged marking + untracked append
  matches the Unstaged pattern; old side `HEAD` in `base_revspec` is right; integration test
  covers the staged/unstaged/untracked combination; scope cycle and labels updated everywhere
  (no non-exhaustive matches remain).
