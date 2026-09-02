# 08 — Test quality vs the validation list

Reviewer scope: test quality against the brief's validation list (terminal restore, LS shutdown,
malformed responses, unsupported caps, rapid changes, fixture states), isolation (no network/key),
flaky risks, and gaps. Verified by reading every test file in the workspace and running
`cargo test --workspace` (365 passed + 1 ignored, ~10 s warm, gopls + go 1.26 present on this
machine).

## Summary

The library crates are tested to a high standard. The git layer is exemplary: scratch repos with
pinned identity/dates and neutralized host config, parser negative tests (truncated rename/hunk,
quoted paths, unmerged, gitlink, binary), and a read-only smoke test asserting `.git/index` is
byte-identical after every query (`crates/codescope-git/tests/git_repo.rs:530`). The AI matrix
(429 + Retry-After, hang→timeout, circuit breaker + half-open probe, tool budget across turns,
outbound redaction, keyless providers, stale epoch) is exceptional
(`crates/codescope-ai/tests/scripted.rs`). LSP framing/decoding is hardened by 11 negative tests
(`crates/codescope-lsp/src/framing.rs`) and the client harness covers out-of-order responses,
garbage-frame recovery, timeout + late-response drop, and server-death fail-fast
(`crates/codescope-lsp/src/client.rs:557+`). Capability resolution covers hostile
null/empty/broken initialize results (`crates/codescope-lsp/src/capabilities.rs:147+`), and the
graph builder proves per-feature degradation with notes
(`crates/codescope-analysis/src/graph.rs:357`). Isolation is clean: fakes bind loopback only,
config tests inject an env closure instead of mutating process env, and live tests are
double-gated (`#[ignore]` + `CODESCOPE_LIVE=1`).

The problem is concentrated at the top of the stack. The binary crate (`crates/codescope`,
796 lines: dispatcher, watcher, terminal, main) has **zero tests**, the workspace `tests/`
directory is **empty** (architecture slice 6, "wiring → dispatcher, watchers, epoch flow", is
absent), and two items on the validation list — **terminal restore** and **rapid changes / epoch
coalescing** — have no automated coverage at all. Several purpose-built test assets (the
`fake-lsp` stdio binary, the `CODESCOPE_GOPLS` override hook, `portable-pty`/`insta`/`rstest`/
`assert_cmd` workspace dev-deps) exist but are wired to nothing.

Validation-list scorecard:

| Item | Status |
|---|---|
| terminal restore | **untested** (no pty test, no guard unit test) |
| LS shutdown | partial — graceful path live-only; kill-escalation path untested |
| malformed responses | good (framing + client + AI); two small client-side gaps |
| unsupported caps | good at unit level; no end-to-end or UI-note test |
| rapid changes | **untested** (no coalescing/debounce/epoch-supersede-loop test) |
| fixture states | covered (fixture + scratch repos, porcelain v2 shape asserted) |
| isolation (no network/key) | clean |

## Findings

### 1. Terminal restore has no test coverage; the planned pty harness was never built
- **Severity: high**
- **Where:** `crates/codescope/src/terminal.rs:12` (only 21 lines, no tests anywhere);
  `Cargo.toml:57` (`portable-pty = "0.9"` declared, used by no crate); no occurrence of
  `\x1b[?1049l`, `portable_pty`, or a hidden panic arm (`CODESCOPE_TEST_PANIC`) in the tree.
- **What:** Research 08 §4 and the validation list require a pty subprocess test: make the built
  binary panic, assert leave-alternate-screen/raw-mode teardown bytes and a non-zero exit.
  Nothing verifies `run_with_terminal` restores on the error path, and nothing verifies the
  ratatui panic hook survives the app's own hook/tracing setup. The binary also lacks the panic
  test arm the research proposed, so such a test cannot even be written without a code hook.
- **Why it matters:** A TUI that corrupts the user's terminal on panic is the single worst
  first-impression failure, and this is the one behavior explicitly named first in the
  validation list. The current code delegates entirely to `ratatui::init/restore`; that is a
  reasonable design, but it is an unverified assumption, and regressions (e.g. someone
  installing a later panic hook) would ship silently.
- **Suggested fix:** Add the `CODESCOPE_TEST_PANIC=1` arm in `main`, then a `pty_*` integration
  test in the binary crate using the already-declared `portable-pty`: spawn the binary in a pty
  against a fixture copy, trigger the panic, assert output contains `\x1b[?1049l` and exit
  status != 0. Keep it `linux+macos` as planned.

### 2. "Rapid changes" is untested: no epoch-coalescing/debounce test, and the dispatcher/watcher layer has zero tests
- **Severity: high**
- **Where:** `crates/codescope/src/dispatcher.rs` (577 lines, no `#[cfg(test)]`),
  `crates/codescope/src/watcher.rs:60` (`is_relevant` — pure, trivially testable, untested),
  workspace `tests/` directory empty; `grep tokio::time::pause` → no hits anywhere.
- **What:** Research 08 §4 requires: "rapid `didChange` v2..v6 within the debounce window;
  assert exactly one re-analysis at latest epoch and that stale results are dropped", driven by
  `tokio::time::{pause,advance}`. No such test exists. The epoch *primitives* are tested well
  below this layer (`AiOutcome::Stale` in `scripted.rs:424`, `stale_epoch_gates_everything` in
  the validator, `Epoch::next` in core), but the dispatcher behaviors those primitives exist
  for — epoch bump per `RepoChanged`, AI result superseding (`ai_rows` keyed by epoch),
  `SnapshotFacts` assembly, `plan_rows` tree flattening, burst coalescing through the two
  debounced watchers — are unverified. Note `Dispatcher::handle` awaits `refresh`/`refresh_ai`
  inline (`dispatcher.rs:166`), so the "stale result dropped at apply time" property of
  architecture decision 4 currently holds only by serialization; a test would pin whichever
  semantics are intended before anyone makes AI requests concurrent.
- **Why it matters:** This layer is the app: it is where stale-overwrite bugs, event storms, and
  lost refreshes live. It is also the only place the watch-channel topology and epoch gating
  compose. All 365 passing tests say nothing about it.
- **Suggested fix:** Give the binary crate dev-deps (`codescope-testutil`, `tempfile`) and add:
  (a) unit tests for `is_relevant`, `plan_rows`, `SnapshotFacts`, `status_badge`; (b) an
  integration test on a fixture copy driving `Dispatcher::handle` directly: N rapid
  `RepoChanged` events → assert final snapshot epoch = N and one publish per handled event;
  (c) an AI-supersede test with a `ScriptedProvider` plan for an old epoch → assert
  `AiStatus::Stale` and `ai_rows` not applied. The debouncer itself (notify) can stay untested;
  the dispatcher must not.

### 3. LS shutdown kill-escalation path is never exercised against the real client
- **Severity: med**
- **Where:** `crates/codescope-lsp/src/client.rs:453-497` (`shutdown`; `Killed` arm at 472-490);
  `crates/codescope-lsp/src/client.rs:494` (test-only `Streams` handle short-circuits to
  `Graceful`); `crates/codescope-testutil/src/bin/fake_lsp.rs:24` (stdio binary built for this,
  spawned by no test); `crates/codescope-testutil/tests/fake_servers.rs:184`
  (`lsp_ignored_shutdown_forces_kill_path` tests the **fake server** with a hand-rolled client
  that just drops a duplex pipe — it never touches `LspClient`).
- **What:** Research 08 §2: "Test both paths (fake server that ignores shutdown → client must
  SIGKILL within deadline)." The graceful path is covered only by `gopls_live.rs:97` (env-skipped
  without gopls). The `ShutdownOutcome::Killed` path — timeout on `shutdown`, `exit` notify,
  stdin close, `SHUTDOWN_GRACE` wait, `child.kill()` — has zero coverage on any machine. A
  contributing cause: `SHUTDOWN_REQUEST_TIMEOUT`/`SHUTDOWN_GRACE` are hardcoded 5 s consts
  (`client.rs:41-43`), so an honest kill-path test would take ~10 s; they are not injectable.
- **Why it matters:** This is exactly the code that prevents orphaned gopls processes eating CPU
  after codescope exits — a defect users notice and cannot diagnose. It contains subtle
  ordering (fail pending → close writer → cancel reader → wait/kill) that is easy to regress.
- **Suggested fix:** Make the two durations injectable (e.g. `ShutdownConfig` with test
  constructor). Add a test that spawns the existing `fake-lsp` binary
  (`cargo_bin`/`CARGO_BIN_EXE_fake-lsp` from a testutil integration test, or via
  `LspClient::spawn` in codescope-lsp with a dev-dependency on testutil) with
  `respond_to_shutdown=false`, then assert `shutdown().await == ShutdownOutcome::Killed` within
  the shrunk deadline and that the process is gone.

### 4. The scriptable fake LSP server is never wired to the real client/adapter; unsupported-caps degradation is untested end-to-end and un-rendered
- **Severity: med**
- **Where:** `crates/codescope-testutil/src/fake_lsp.rs:155` (`empty_capabilities`),
  `:222` (`with_shutdown_ignored`) — consumed only by testutil's own suite via a hand-rolled
  client (`tests/fake_servers.rs:89-118`); `crates/codescope-lsp/Cargo.toml` has no
  (dev-)dependency on testutil; `crates/codescope-lsp/src/gopls.rs:57` (`CODESCOPE_GOPLS`
  override that would let tests point the adapter at the fake binary — unused);
  `crates/codescope/src/dispatcher.rs:401` (the "partial: some relationships unavailable" note)
  and `crates/codescope-tui/src/render.rs:291` (note rendering) — no test ever renders a
  non-empty note (render tests all use `note: String::new()`, `tests/render.rs:49`).
- **What:** The validation item "unsupported caps → UI shows 'unavailable' affordances instead
  of errors" is covered piecewise: `resolve_features` unit tests (incl. broken-session
  null-caps), `require()` gating, and graph-builder note tests. But no test runs
  `initialize` → `FeatureSet` → query-gating through `GoplsService`/`LanguageService` against a
  server with minimal caps, and no test asserts the degradation note actually reaches the
  screen. The negative-response fakes (`MalformedJson`, `WrongContentLength`,
  `TruncateAndClose`) are likewise only ever aimed at a throwaway test client, not at
  `LspClient` (whose own harness covers similar ground, but e.g. a correctly-framed non-JSON
  body → `handle_message` skip path at `client.rs` `handle_message` has no direct test).
- **Why it matters:** The seam between capability resolution and the adapter's request methods is
  where "degrade, don't error" can silently regress; unit tests on `resolve_features` won't
  catch an adapter method that forgets its `require()` gate. The project built exactly the tool
  needed to test this and then didn't plug it in.
- **Suggested fix:** One integration test in codescope-lsp (dev-dep on testutil): set
  `CODESCOPE_GOPLS=$CARGO_BIN_EXE_fake-lsp` with an `empty_capabilities()` script file, start
  `LanguageService`, assert `SemanticError::BrokenSession`; a second script with only
  `documentSymbolProvider` → assert `document_symbols` works while `references`/`incoming_calls`
  return `Unsupported` without any wire traffic (fake's received log proves it). One render test
  with a non-empty `SemanticPane::note` asserting the `ⓘ` line appears.

### 5. Real-gopls coverage skips the Go fixture entirely; the fixture's cross-package semantics and the overlay path are never validated live
- **Severity: med**
- **Where:** `crates/codescope-lsp/tests/gopls_live.rs:51` (single-file inline module, not the
  fixture); covered live: `document_symbols`, `implementations`, `incoming_calls`, graceful
  `shutdown`. Not covered by any live test: `references`, `base_document_symbols` (the
  close/reopen overlay dance in `gopls.rs`, which `AnalysisEngine` depends on for base trees),
  `outgoing_calls`, `type_subtypes`, diagnostics push. The fixture's designed-for semantics —
  cross-package definition/references incl. `_test.go`, `Repository` with two impls
  (`crates/codescope-testutil/src/go_fixture.rs` module docs; research 08 §1.1 "must
  exercise") — are never queried through gopls; the fixture meets real LSP code only via the
  *scripted* source in `crates/codescope-analysis/src/engine.rs:409`.
- **What:** The most failure-prone gopls interactions (base-revision overlays on a renamed dirty
  file; references across packages and test files) have no test against the real server, warm
  or cold.
- **Why it matters:** Overlay mistakes (didOpen version bookkeeping, restoring worktree text)
  produce silently wrong base trees → wrong change-kind classification, and only a real gopls
  would catch encoding/URI/overlay mismatches. The fixture was purpose-built for this and is
  ready.
- **Suggested fix:** Extend the env-skipped live suite: copy the fixture
  (`copy_fixture_into`), start `LanguageService`, and assert (a)
  `base_document_symbols(memstore.go, <HEAD content>)` returns the pre-edit tree while a
  subsequent `document_symbols` still sees the worktree version; (b) `references` on
  `Repository.Get` spans store/service/store_test; (c) `implementations` on `Repository`
  returns both repos. Keep the 60 s first-request timeout.

### 6. Environment sensitivity: fixture Go checks fail instead of skipping on a pre-1.26 toolchain
- **Severity: low**
- **Where:** `crates/codescope-testutil/src/helpers.rs:23` (`require_go` probes only that
  `go version` runs), `crates/codescope-testutil/src/go_fixture.rs:325` (`go 1.26` in go.mod),
  `:279` (`GOTOOLCHAIN=local` — correct for network isolation, but it converts a version
  shortfall into a hard `go build` failure), `crates/codescope-testutil/tests/fixture.rs:139`
  (`fixture_typechecks_formats_and_tests_clean` → `unwrap()`).
- **What:** On a machine with go ≥ installed but < 1.26, the test fails rather than skipping,
  violating the suite's own env-skip convention. Also `GOPROXY` is not pinned to `off`; harmless
  today (zero deps) but a future fixture dep would silently enable network in "hermetic" runs.
- **Why it matters:** Red CI/dev runs for an environment reason erode trust in the suite's
  signal.
- **Suggested fix:** Have `require_go` parse the version and skip below the fixture's minimum
  (single source of truth for "1.26"), and add `("GOPROXY", "off")` to `run_go`'s env.

### 7. Minor timing-based flake risks (all bounded; none observed in runs)
- **Severity: low**
- **Where:** `crates/codescope-lsp/src/client.rs:836` (`server_exit_fails_pending_requests`
  sleeps a real 50 ms before dropping the transport; under an extreme scheduler stall the
  request could hit the already-closed pipe and surface as an I/O error instead of
  `ServerExited`, failing the `matches!`); `client.rs:696,715` (diagnostics cache polled up to
  500 ms); `crates/codescope-ai/tests/scripted.rs:253` (asserts elapsed ≥ 950 ms — safe lower
  bound — but also `< 5s`/`< 3s` upper bounds at `:256,:282` that a badly overloaded CI node
  could exceed); `scripted.rs:333` and `crates/codescope-ai/src/client.rs:575` (real sleeps for
  breaker cooldown — deterministic since sleep ≥ cooldown, just wall-clock cost). The
  `ScriptedProvider`/duplex fakes correctly use port 0 / in-memory pipes, so there are no port
  or path collisions.
- **What:** No test uses `tokio::time::pause` even where the research prescribed it; the suite
  instead relies on real timers with margins. Margins chosen are sane; the risk is small but
  nonzero on saturated CI.
- **Why it matters:** Intermittent failures in exactly the negative-path tests people already
  distrust ("it's just flaky") would mask real regressions.
- **Suggested fix:** For `server_exit_fails_pending_requests`, wait for the fake server to
  *receive* the request (it already can) instead of sleeping. Where retry/breaker delays are
  driven by `tokio::time::sleep` internally, add `start_paused = true` variants. Loosen the
  `< 3s/< 5s` ceilings or drop them (the lower bounds carry the assertion's meaning).

### 8. Test-plan drift: declared-but-unused dev tooling, leaked canonical fixture dir, untested TUI run loop
- **Severity: low**
- **Where:** `Cargo.toml:54-57` (`assert_cmd`, `insta`, `rstest`, `portable-pty` in workspace
  deps; zero consumers — `insta` snapshots and `rstest` parametrized fixture-state tests from
  research 08 §5 were never adopted, and there are no `assert_cmd` CLI tests for `--log-file`
  or non-repo exit); `crates/codescope-testutil/src/helpers.rs:41-56` (canonical
  fixture `TempDir` lives in a `static OnceLock` and is never dropped — each test-binary run
  leaks one `codescope-fixture-*` dir in `$TMPDIR`); `crates/codescope-tui/src/run.rs:24-70`
  (event loop and `dispatch` untested: quit path, work-action forwarding vs local apply; note
  the `changed.is_err() → continue` arm at `run.rs:55-58` busy-spins redraws if the dispatcher
  ever drops the sender early — a loop test would have surfaced that; the defect itself belongs
  to the reactive-architecture review).
- **What:** Hygiene-level mismatches between the written test plan and reality.
- **Why it matters:** Unused deps overstate the harness's capabilities to readers of the
  manifest; the leak is cosmetic but real; `dispatch()` is a 10-line pure-ish function guarding
  the TUI/dispatcher contract.
- **Suggested fix:** Either adopt the tools (pty test from finding 1 uses `portable-pty`;
  `assert_cmd` for CLI smoke) or delete the declarations. Unit-test `dispatch` with a bounded
  channel. Accept or document the fixture-dir leak.

## What the validation list *does* get, verified

- **Fixture states:** all six research §1.2 states are asserted somewhere real: committed base +
  2-commit divergent branch (`fixture.rs:114-125` via `rev-list --count` = 2), staged M + staged
  R100-with-unstaged-M combo entry + untracked, asserted against literal porcelain v2 output
  with an exact entry count (`fixture.rs:27-75`); staged **A** and pure/edited renames, deletes,
  binary, gitlink, unmerged, unborn, detached are covered in the scratch-repo suite
  (`git_repo.rs:272,218,250,320,336,438,144,132`). OID determinism is proven by rebuild-equality rather
  than hardcoded hashes (`fixture.rs:76-92` — exactly the research's recommendation).
- **Malformed responses:** decoder never panics and resyncs after garbage/oversized/lying
  headers (11 cases, `framing.rs:143+`); real client recovers mid-stream and drops late/unknown
  ids (`client.rs:657,757`); AI side covers malformed tool-args, text-instead-of-tool-call,
  5xx, non-JSON content types, and script exhaustion → explanatory 500
  (`fake_servers.rs:283-343`, `scripted.rs:201-232`).
- **Isolation:** fakes bind `127.0.0.1:0` or in-memory duplex only; no test needs a key
  (`keyless_local_provider_sends_no_authorization`, `scripted.rs:488`); env is injected, never
  mutated (`config.rs:307+`; no `set_var` in the tree); live tests are `#[ignore]` **and**
  env-gated (`live.rs:46`, `gopls_live` env-skips per research); `GOWORK=off`,
  `GOTOOLCHAIN=local`, pinned git identity + `GIT_CONFIG_NOSYSTEM`/`GIT_CONFIG_GLOBAL=null`
  keep host state out; the canonical fixture is copy-on-use, never mutated
  (`helpers.rs:64-77`).

## Verdict

**fix-first.** The seven library crates are ship-quality on this axis, but two named
validation-list items (terminal restore; rapid changes/epoch coalescing) have zero coverage, the
entire binary/wiring crate is untested, and the LS kill-escalation path is unreachable by the
current suite. Findings 1-3 are the gate; 4-5 should follow shortly after; 6-8 are cleanups.
