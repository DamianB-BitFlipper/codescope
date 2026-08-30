# Review 06 — Performance & Concurrency

Reviewer scope: epoch/supersede correctness, debounce, blocking calls on the runtime,
backpressure, watch latest-wins, lock granularity. Code read against
`docs/architecture.md` (decisions 4, 5) and `docs/research/06-reactive-architecture.md`.
Verified with `cargo check --workspace` (clean); no code was modified.

## Summary

The channel topology matches the researched design (per-subsystem mpsc → one dispatcher →
`watch<UiSnapshot>` → TUI `select!{biased;}`), git runs on `tokio::process`, and the LSP
client's internal locking is sound. But the **concurrency architecture described in
architecture decision 4 and research 06 §3 was not implemented**: the dispatcher awaits
git, the full LSP analysis pipeline, and even the AI network request **inline** in its
event loop. There are no spawned jobs, no `CancellationToken`s outside LSP teardown, and
no apply-time epoch re-check anywhere in the binary. Epoch/supersede correctness holds
only *vacuously*, because nothing runs concurrently — and the price is that one slow
gopls call (60 s first-request timeout) or one AI request (worst case ~10 min with
retries × tool turns) freezes all repo-change processing. Around that central defect:
startup blocks on gopls initialize before the TUI appears, queued change events replay
the full pipeline N times (no coalescing, no per-file cache, eager per-symbol
call-hierarchy fan-out), the tree watcher counts `.git/**` and ignored build artifacts as
relevant, the FSEvents loss safety net (30 s reconcile poll / focus-gained check) is
missing, and `GitRepo::fingerprint` — built for cheap change detection — is dead code.

Numbers for one Branch-scope refresh of a changeset with F files / S changed symbols
(all sequential, all blocking the dispatcher):
- git subprocesses: ~6–14 fixed (status ×3, `infer_base` ×2–3, diff, …) + F×`git show`;
- LSP traffic: per file 1–2 didClose/didOpen cycles + 1–2 `documentSymbol`; per symbol
  ~2 more full-text reopen cycles + 4 requests (prepare/incoming, prepare/outgoing).
  F=10, S=30 ⇒ ≈140 LSP requests + ≈70 full-text `didOpen`s per refresh, repeated from
  scratch on every debounced event.

## Findings

### H1 — Dispatcher awaits everything inline: no spawned jobs, no cancellation, no apply-time epoch gate
- **Severity: high**
- **Where:** `crates/codescope/src/dispatcher.rs:88-115` (`handle`), `:118-135`
  (`refresh` awaits git + `engine.refresh`), `:151-183` (`refresh_ai` awaits
  `ai.request_plan` inline), `:471-489` (`run` loop, nothing spawned);
  contract it violates: `docs/architecture.md` decision 4,
  `docs/research/06-reactive-architecture.md` §3, and the AI crate's own docs
  `crates/codescope-ai/src/service.rs:17-18,128-131` ("callers `tokio::spawn` the future
  and apply the outcome at their epoch gate").
- **What:** `Dispatcher::handle` is the only consumer of both event channels, and it
  awaits the entire git→analysis→AI pipeline before reading the next event. `refresh_ai`
  awaits the AI HTTP loop (default 20 s/request timeout, 2 retries with backoff, up to
  `max_tool_calls + 2 = 10` turns ⇒ theoretical bound ≈10 minutes, realistic 5–30 s).
  `refresh` awaits up to ~140 sequential LSP requests (10 s steady timeout each, 60 s
  for the first — `crates/codescope-lsp/src/gopls.rs:33-35`). While any of this runs,
  `RepoChanged` events and user work actions (including `AiToggle`) just queue.
  There is no `tokio::spawn` for jobs, no `CancellationToken` anywhere in the binary
  (only inside LSP client teardown), and the promised apply-time epoch re-check does not
  exist: dispatcher.rs:172 applies `self.ai_rows = Some((epoch, ...))` without comparing
  against `self.epoch` (the comment at dispatcher.rs:148-150 says the gate "lives in the
  caller that awaits the spawned request — see `run`", but `run` contains no such logic).
- **Why it matters:** The epoch mechanism is the project's core correctness story
  ("AI responses must never overwrite newer state"). Today it cannot be violated only
  because nothing is concurrent — and that serialization is itself the defect: the app
  stops reacting to repository changes for the full duration of any slow subsystem call.
  It is also a trap: the moment anyone makes `refresh_ai` spawned (as three separate doc
  comments instruct), the missing apply-time check becomes a live stale-overwrite bug.
- **Suggested fix:** Implement decision 4 as designed: spawn analysis and AI work as
  jobs that capture `epoch`, deliver results back to the dispatcher through its own mpsc
  (a `JobDone { epoch, payload }` event), drop results whose epoch no longer matches,
  and store a `CancellationToken` per job so a newer event can supersede the older job.

### H2 — Startup blocks on gopls spawn + initialize before the TUI exists
- **Severity: high**
- **Where:** `crates/codescope/src/main.rs:57-68` (awaits `LanguageService::start`
  before `run_with_terminal` at `:97`); `crates/codescope-lsp/src/gopls.rs:57-90`
  (`start` performs the full `initialize` handshake, `FIRST_REQUEST_TIMEOUT = 60 s` at
  `:33`); first snapshot only after a full refresh: `crates/codescope/src/dispatcher.rs:477`.
- **What:** The comment at main.rs:57 claims "Start the language server in the
  background; the UI is usable before it is ready", but the code awaits spawn +
  `initialize` before the terminal is even initialized. After the TUI starts, the first
  published snapshot additionally waits for the *entire* initial analysis (git + all
  per-file LSP work) because `refresh` publishes only at the end (dispatcher.rs:118-135);
  `UiSnapshot.refreshing` is hard-coded `false` in `build_snapshot_with`
  (dispatcher.rs:216), so no in-flight indicator can ever show after boot.
- **Why it matters:** On a real Go repo, gopls initialize plus the first
  `documentSymbol` (60 s timeout budget) means seconds-to-a-minute of blank terminal /
  frozen placeholder. This directly contradicts the progressive-loading design
  (research 06 §4: T0 git info <100 ms first frame, T1 gopls in background with an
  "indexing…" indicator).
- **Suggested fix:** Start the TUI immediately; spawn `LanguageService::start` and hand
  the engine to the dispatcher via an event when ready (`LsStatus::Starting` already
  exists for exactly this). Publish a git-only snapshot first (T0), then re-publish as
  analysis lands; set `refreshing`/status before starting slow work.

### H3 — Refresh amplification: no event coalescing, no per-file cache, eager per-symbol fan-out
- **Severity: high**
- **Where:** `crates/codescope/src/dispatcher.rs:478-488` (one event = one full
  pipeline; queued events processed one by one, no drain/coalesce);
  `crates/codescope-analysis/src/engine.rs:133-146` (per-file loop, no caching, all
  sequential); `crates/codescope-analysis/src/graph.rs:54-104` (2–3 relationship queries
  per changed symbol, each preceded by a full didClose/didOpen re-sync —
  `crates/codescope-lsp/src/gopls.rs:142-150` via `prepare_call_hierarchy`
  `:518` and `implementations` `:465`); `crates/codescope/src/watcher.rs:51`
  (`try_send` silently drops when the 64-slot queue is full, no drop counter).
- **What:** Research 06 §4 prescribed a per-file `(path, content_hash)` cache that
  survives epoch bumps, and lazy call-hierarchy for the *selected* symbol only (T3).
  Neither exists: every `RepoChanged` re-opens and re-queries every changed file and
  re-runs incoming/outgoing call hierarchy for every changed symbol. Because refreshes
  are typically slower than the 300 ms debounce window, events queue (up to 64) while a
  refresh runs, and the dispatcher then replays the full pipeline once per queued event
  — even though only the last result can matter (each replay also bumps the epoch and
  resets `ai_status`). Within one refresh, the same file is reopened with full text 2×
  per changed symbol (once per prepare call), forcing gopls to re-diff its overlay
  repeatedly.
- **Why it matters:** This is the difference between "usable on the test fixture" and
  "usable on a real repository". A 10-file/30-symbol branch diff costs ≈140 sequential
  LSP requests per refresh; during editor/build churn the dispatcher can fall minutes
  behind doing 100% redundant work, with the UI silently showing stale state (and the
  silent `try_send` drops mean nobody can even observe the backlog).
- **Suggested fix:** (1) After `events.recv()`, drain the queue with `try_recv` and
  coalesce all pending `RepoChanged` into one refresh. (2) Add the researched
  content-hash cache for symbol trees; skip didOpen + documentSymbol for unchanged
  hashes. (3) Sync each file at most once per refresh, and defer call-hierarchy to the
  selected symbol (or at least cap/parallelize it). (4) Count and log dropped events.

### M1 — Tree watcher treats `.git/**` and ignored artifacts as relevant
- **Severity: med**
- **Where:** `crates/codescope/src/watcher.rs:26` (recursive watch of `toplevel`, which
  contains `.git/` in normal repos), `:60-63` (`WatchKind::Tree => true` — every path is
  relevant).
- **What:** Every git operation fires twice: once via the 100 ms git-dir watcher and
  again via the 300 ms tree watcher (the `.git` events are inside the toplevel and pass
  the `Tree => true` filter), producing two epoch bumps and two full refreshes per
  commit/checkout. Git-ignored churn (build outputs, editor temp files, `.DS_Store`)
  also triggers full refreshes, although the repo state codescope renders cannot have
  changed. The privacy/ignore layers (research 07) are applied to diff paths but not to
  watch events.
- **Why it matters:** Multiplies H3. Running `go build` or a test suite inside the repo
  turns the watcher into a refresh storm generator at the debounce rate.
- **Suggested fix:** In the tree watcher, ignore paths under the resolved git dir, and
  filter through the same ignore stack used for diff paths (`ignore` crate is already a
  workspace dependency). Optionally gate refreshes on `GitRepo::fingerprint` (see M2).

### M2 — No FSEvents-loss safety net; `fingerprint()` is dead code; watcher errors swallowed
- **Severity: med**
- **Where:** `crates/codescope/src/watcher.rs:48` (`let Ok(events) = result else
  {{ return }}` — debouncer/watcher errors dropped); no 30 s reconcile poll anywhere in
  `crates/codescope/src/main.rs`; `Event::FocusGained` ignored in
  `crates/codescope-tui/src/run.rs:50`; `crates/codescope-git/src/repo.rs:367-371`
  (`fingerprint` doc says "the dispatcher uses it to detect repo-state generations" —
  no caller outside tests).
- **What:** Research 06 §2/§7 required a slow safety net (≈30 s poll + focus-gained
  check) because FSEvents is lossy under load, plus a drop counter on the fs path.
  None of this exists, and watcher-side errors (including overflow notifications) are
  silently discarded. `fingerprint()` was built (and tested) precisely to make such
  reconciliation cheap, then never wired in — every accepted event bumps the epoch even
  when the repo state is byte-identical.
- **Why it matters:** A missed event leaves the UI permanently stale until an unrelated
  change arrives; users of a "live" tool will trust what they see. The dead fingerprint
  also means spurious events do full pipelines for nothing.
- **Suggested fix:** Add the periodic + focus-gained reconcile using
  `fingerprint()`; only bump the epoch when the fingerprint actually changed; log
  dropped/errored watch batches.

### M3 — Stale AI pane: `ai_rows` epoch ignored at render; `RefreshGit` reuses the epoch
- **Severity: med**
- **Where:** `crates/codescope/src/dispatcher.rs:44` (`ai_rows: Option<(Epoch, ...)>`),
  `:172` (stored with epoch), `:225` (`if let Some((_, rows, title))` — epoch discarded),
  `:100` (`Action::RefreshGit => self.refresh()` without `epoch.next()`).
- **What:** After a repo change bumps the epoch and analysis is recomputed, the semantic
  pane still renders the previous epoch's AI plan (`ai_generated: true`) instead of the
  fresh deterministic impact view; the stored epoch is never compared. `AiStatus` is
  reset to `Idle`, so the status bar and the pane disagree about freshness, and the pane
  carries no staleness cue. Separately, a manual `RefreshGit` recomputes analysis under
  the *same* epoch, so "epoch" no longer uniquely identifies the fact set a plan was
  validated against.
- **Why it matters:** Decision 4's intent is that stale AI output is dropped or clearly
  demoted at apply/render time. Rendering an old plan over new facts (hunks, symbols may
  have moved or vanished) quietly breaks the honesty contract the validator enforces.
- **Suggested fix:** In `panes()`, compare the stored epoch with `self.epoch`; on
  mismatch fall back to `impact_pane` or render the AI pane with an explicit "stale
  (epoch N)" note. Bump the epoch for any refresh that re-reads the repo.

### M4 — Blocking `std::fs` reads on the async runtime in the gopls adapter
- **Severity: med**
- **Where:** `crates/codescope-lsp/src/gopls.rs:144` (`sync_worktree`), `:187`
  (`diagnostics`), `:258` (`base_document_symbols`), `:284` (`symbol_tree` — re-reads the
  file `sync_worktree` just read), `:364, :465, :518, :545` (per-query re-reads), `:571`
  (`location_from_wire` — reads the whole file **per result item** of
  references/implementation responses), `:631` (goto links).
- **What:** All file content is read with synchronous `std::fs::read_to_string` inside
  async methods running on the tokio runtime, without `spawn_blocking`. The worst shape
  is `location_from_wire`: a references result with 100 locations re-reads up to 100
  files (unbounded size, no cache) synchronously on a worker thread — and since the
  dispatcher is single-flighted, this latency is fully user-visible.
- **Why it matters:** Blocking reads stall runtime workers (cold page cache, network
  filesystems, very large generated Go files), and the repeated full-file reads are
  O(results × file-size) redundant work per query.
- **Suggested fix:** Use `tokio::fs::read_to_string` (or one `spawn_blocking` batch),
  and cache file text per refresh (a simple `HashMap<Utf8PathBuf, Arc<str>>` scoped to
  the refresh) so each file is read once for sync + conversions.

### M5 — Redundant git work inside a single refresh (status ×3, base inference ×2–3)
- **Severity: med**
- **Where:** `crates/codescope/src/dispatcher.rs:119-120` (`repo_context()` then
  `changeset()`), `crates/codescope-analysis/src/engine.rs:126` (`repo_context()`
  again), `crates/codescope-git/src/repo.rs:154-165` and `:305-306` (each of
  `repo_context` and `changeset(Branch)` runs `status_snapshot` + `infer_base`;
  `infer_base` alone is up to ~5 subprocess probes, `:172-241`).
- **What:** One `RepoChanged` spawns `git status` three times and re-infers the base
  two to three times before any semantic work starts — ~6–14 subprocess spawns, all
  sequential and all inside the dispatcher's critical path.
- **Why it matters:** 100–300 ms of fixed overhead per refresh on a warm repo (more on
  large repos), multiplied by the replay behavior in H3.
- **Suggested fix:** Compute `RepoContext` once per refresh and pass it into
  `changeset`/`engine.refresh` (the engine already receives the changeset; give it the
  context too, or let `GitRepo` cache the status snapshot per call generation).

### M6 — TUI input path can block on the bounded action channel
- **Severity: med**
- **Where:** `crates/codescope-tui/src/run.rs:72` (`let _ = tx.send(action).await`
  inside the select loop), channel size 64 at `crates/codescope/src/main.rs:88`.
- **What:** Work actions are sent with an awaited `send` on a bounded(64) channel. If
  the dispatcher is wedged in a long inline await (H1) and the user holds a work key
  (`R` autorepeat ≈2.5 s fills 64 slots), the entire TUI loop suspends inside `send` —
  no rendering, and no `q`/ctrl-c handling until the dispatcher drains.
- **Why it matters:** Research 06 §5 explicitly warns against small bounded channels on
  the UI path where backpressure can couple the UI to a busy consumer; this is that
  coupling in the TUI→dispatcher direction. The freeze also hides the quit path.
- **Suggested fix:** Use `try_send` and surface "busy" in the status line (or coalesce
  duplicate pending work actions). Quit must never depend on channel capacity.

### M7 — Watch latest-wins clobbers local view state; scope switching never reaches the dispatcher
- **Severity: med** (functional wiring; overlaps another review topic — listed here
  because the mechanism is snapshot-overwrite over the watch channel)
- **Where:** `crates/codescope-tui/src/run.rs:69-75` (only
  `RefreshGit|AiToggle|AiRefresh` are forwarded), `crates/codescope-tui/src/app.rs:110-115`
  (`set_scope` mutates only the local snapshot copy), `crates/codescope/src/dispatcher.rs:66`
  (`scope` fixed to `Branch` at construction) and `:111` (`_ => {}` — scope actions never
  handled).
- **What:** Scope keys (`s`/`u`/`B`/`S`) flip a label in the local `UiSnapshot` clone;
  the dispatcher's `self.scope` stays `Branch` forever, so the data never changes, and
  the next published snapshot (any watch update) silently reverts the label. The same
  overwrite resets user expand/collapse state (`FileRow.expanded` is rebuilt `true`,
  dispatcher.rs:309) and the current-hunk position on every publish.
- **Why it matters:** `watch` latest-wins is the right primitive for dispatcher-owned
  state, but view-local mutations of that same object make the UI lie (scope label shows
  `staged`, data is branch) and make every refresh destructive to UI state.
- **Suggested fix:** Forward scope actions to the dispatcher (bump epoch or at least
  refresh under the new scope); keep view-local state (expansion, selection, hunk) out
  of `UiSnapshot` or merge it on `App::update` instead of overwriting.

### M8 — Refresh errors publish nothing: NoBase repos hang on the boot placeholder
- **Severity: med** (liveness as perceived by the user; overlaps wiring review)
- **Where:** `crates/codescope/src/dispatcher.rs:119-120` (`?` on `repo_context` /
  `changeset` skips `publish_with`), `crates/codescope-git/src/repo.rs:306`
  (`GitError::NoBase` for Branch scope with no inferable base), run loop swallows the
  error at `crates/codescope/src/dispatcher.rs:481,485`.
- **What:** On a local-only repository sitting on `main` with no remote, `infer_base`
  returns `None`, `changeset(Branch)` errors, `refresh` aborts before publishing, and
  the UI shows "scanning repository…" (the boot placeholder, `refreshing: true`)
  forever, re-failing identically on every watch event.
- **Why it matters:** The app appears hung on a perfectly ordinary repo shape; no error
  ever reaches the UI (`UiSnapshot.message` is always empty — dispatcher.rs:214).
- **Suggested fix:** Publish a git-only snapshot with the error in `message` on any
  refresh failure (repo context usually succeeds even when the changeset fails); for
  Branch-with-no-base, degrade to an empty changeset with an explanatory note.

### M9 — Linked worktrees: shared refs live in the unwatched common dir
- **Severity: med** (narrow population, systematic staleness)
- **Where:** `crates/codescope/src/watcher.rs:27` (watches `repo.git_dir()` only);
  `crates/codescope-git/src/repo.rs:127-131` exposes `common_dir()` unused by the
  watcher.
- **What:** In a linked worktree, `git_dir` is `<main>/.git/worktrees/<name>` (HEAD,
  index), while `refs/`, `packed-refs`, `FETCH_HEAD` live in the common dir. Research 06
  §2 says to watch "the resolved git dir(s)". Ref updates (fetch, branch moves from
  another worktree, gc producing packed-refs) never fire an event; with the safety poll
  also missing (M2), ahead/behind and base data stay stale indefinitely.
- **Suggested fix:** Watch `common_dir` too when it differs from `git_dir` (same filter).

### L1 — Unconditional ~30 fps render loop with a full snapshot clone per frame
- **Severity: low**
- **Where:** `crates/codescope-tui/src/run.rs:32` (33 ms interval), `:35`
  (`terminal.draw(...)` every loop iteration; `&app.snapshot.clone()` — a gratuitous
  deep clone of every diff/file/semantic row, ~30×/s even when idle).
- **What:** Research 04/06 said render only on snapshot change or animation tick need;
  here the tick alone forces a rebuild+diff every 33 ms forever, and each frame deep-
  clones the snapshot (for a 5k-row diff that is ~150k string allocations/s). The clone
  is unnecessary: `render(frame, &app, &app.snapshot)` is two shared borrows and
  compiles as-is.
- **Suggested fix:** Drop the clone; render on demand (dirty flag set by key/snapshot
  arms) and keep the tick only while a spinner is actually animating.

### L2 — LSP reader loop answers server→client requests inline behind the shared writer lock
- **Severity: low**
- **Where:** `crates/codescope-lsp/src/client.rs:505-530` (`reader_loop` →
  `handle_message(..).await` → `answer_server_request` at `:119-146` → `send_frame`
  which takes the writer `AsyncMutex` at `:102-115`).
- **What:** While the reader task awaits the writer lock (held by a large in-flight
  `didOpen`, e.g. a >64 KiB generated file, with gopls slow to drain stdin), it is not
  draining stdout; response frames back up in the pipe and pending requests can hit
  their 10 s timeout spuriously. A full deadlock additionally requires the server to
  couple its stdin reads to stdout writes — unlikely with gopls, but the coupling is
  structural.
- **Suggested fix:** Route outgoing frames through a dedicated writer task fed by an
  mpsc (reader then never blocks on the writer), or spawn the reply.

### L3 — gopls overlay window and versions map: correctness relies on external serialization
- **Severity: low** (latent; today the single dispatcher serializes everything)
- **Where:** `crates/codescope-lsp/src/gopls.rs:153-176` (`reopen` holds the global
  `versions` `AsyncMutex` across didClose/didOpen — fine), `:250-275`
  (`base_document_symbols`: the base overlay is open across an *unlocked* window between
  the two `reopen` calls).
- **What:** A concurrent `document_symbols(file)` during that window would read the
  base overlay content but label the tree `Revision::Worktree`. All engine methods are
  `&self` and `Send`, so nothing in the type system prevents the parallel per-file
  refresh that H3's fix would naturally introduce.
- **Suggested fix:** When parallelizing, key a per-file async lock covering the whole
  overlay round-trip (the global map already exists to hang it from), and document the
  invariant.

### L4 — Concurrency docs/comments describe machinery that does not exist
- **Severity: low** (doc drift that misleads maintainers)
- **Where:** `docs/architecture.md` decision 4 ("AtomicU64 epoch … CancellationToken per
  job") vs. `crates/codescope/src/dispatcher.rs:38` (plain `Epoch` field; no tokens);
  `crates/codescope-core/src/epoch.rs:7-10` ("every async job captures the epoch at
  spawn"); `crates/codescope/src/dispatcher.rs:148-150` ("see `run`" — no gate in `run`);
  `crates/codescope-ai/src/service.rs:17-18,128-131` ("callers `tokio::spawn`").
- **What:** Four places assert the spawn/supersede design; none of it is implemented.
  A plain `u64` is *fine* under the single-writer actor — but the comments promise a
  different system, which is exactly how the missing epoch gate (H1) ships unnoticed.
- **Suggested fix:** Either implement decision 4 (preferred, see H1) or rewrite the
  comments to state the actual serialization model and its consequences.

## Verdict

**fix-first.** The channel topology, git subprocess hygiene, debounce windows, and the
LSP client internals are solid and match the research. But the heart of the researched
reactive architecture — spawned, cancellable, epoch-gated jobs behind a responsive
dispatcher — is unimplemented, and its absence makes the app freeze-prone (H1, H2) and
quadratically wasteful under real editing load (H3, M1, M5). No redesign is needed: the
design documents already describe the correct target; H1–H3 plus M2/M3 are the work.
Counts: 3 high, 9 medium, 4 low.
