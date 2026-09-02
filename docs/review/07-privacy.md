# Review 07 — Privacy & credentials

Scope: `codescope-ai` config/key handling, redaction, exclusion layers (.gitignore /
.codescopeignore / secret denylist / content sniffing), exactly what leaves the machine,
and the end-to-end read-only guarantee. Reviewed against `docs/architecture.md` (decision 7)
and `docs/research/07-privacy-safety.md`. Verified by reading source and running
`cargo test -p codescope-ai` (81 tests pass, live smoke ignored).

## Summary

Key handling is the strong part: keys are resolved from env only, wrapped in
`secrecy::SecretString`, never serialized, redacted by hand-written `Debug` impls on both
`AiConfig` and `AiClient`, sent once as a `set_sensitive` Authorization header, and every
error path is sanitized (`reqwest::Error::without_url`, no bodies/headers in logs). A
scripted-provider test asserts on the actual wire bytes that the absolute repo root is
stripped. The read-only guarantee also holds end-to-end: git runs a read-only command set
under `--no-optional-locks` + hardened env, the LSP client refuses `workspace/applyEdit`,
base revisions use in-memory overlays (zero temp files), and the only production write is
the opt-in `--log-file`. The single network egress in the whole workspace is the AI
chat-completions POST, and it fires only on an explicit `A` keypress.

The weak part is content exclusion. Architecture decision 7 claims a "4-layer exclusion
(git ignore rules < .codescopeignore < compiled denylist < content sniffing), applied to
diff paths too". Only layer 1 exists, and only implicitly (files are enumerated via git, so
ignored files never appear). `.codescopeignore`, the compiled secrets denylist, and content
sniffing are entirely absent — the `ignore` crate is declared in the workspace manifest but
consumed by no crate. Consequently, a changed tracked secret file (committed `.env`,
`*.pem`, `credentials.json`) or an inline credential in a changed code line goes into the
AI digest verbatim (up to 2 old + 2 new preview lines per hunk, up to 40 hunks). The
research doc's own top pitfall — "tracked secrets still appear in git diff output; the
denylist must filter diff paths" — is unmitigated. The first-enable disclosure modal is
also missing while AI silently auto-enables off ambient `OPENAI_API_KEY`/`PRIME_API_KEY`.

## What is actually sent to the provider (verified)

- One `POST {base_url}/chat/completions`, `stream: false`, built in
  `crates/codescope-ai/src/client.rs:262-291`. No other crate depends on `reqwest`; no
  telemetry anywhere.
- Body: model id, system prompt (static rules + epoch), and the rendered 5-tier digest
  (`crates/codescope/src/dispatcher.rs:162-167`):
  - tier 1: changed symbol names, kinds, signatures (`detail`), repo-relative files;
  - tier 2: diagnostics touching changed ranges (message truncated to 160 chars,
    `crates/codescope-analysis/src/digest.rs:285`);
  - tier 3: hunk headers (incl. git section heading) + **first ≤2 removed and ≤2 added raw
    source lines per hunk** (`digest.rs:290-330`);
  - tier 4: relationship edges (name-only); tier 5: HEAD/base ref names + top-level dirs.
- Tool loop: the binary wires `NoToolExecutor` (`dispatcher.rs:166`), so `get_hunk`/
  `get_file_outline`/… always return an error string to the model — today no file content
  beyond the digest previews can leave via tools.
- Redaction applied: absolute repo-root prefix stripped from digest and tool results
  (`crates/codescope-ai/src/service.rs:130`, `:229-233`, `:260-267`), asserted on the wire
  in `crates/codescope-ai/tests/scripted.rs:143-197` and for tool results at `:363`.

## Findings

### 1. Exclusion layers 2–4 (denylist / .codescopeignore / content sniffing) do not exist
- **Severity: high**
- **Where:** `Cargo.toml:41` (`ignore = "0.4.33"` declared, used by no crate — no
  `crates/*/Cargo.toml` references it); no hit for `codescopeignore`, denylist, or secret
  regexes anywhere in `crates/`; digest assembly `crates/codescope-analysis/src/digest.rs:290-330`
  (raw hunk lines in `old_preview`/`new_preview`); egress `crates/codescope/src/dispatcher.rs:162-167`.
- **What:** Architecture decision 7 and research 07 §1 specify four exclusion layers applied
  to every content source including diff paths. The code implements none of layers 2–4: no
  `.codescopeignore` support, no compiled-in secrets denylist (`.env`, `*.pem`, `id_rsa*`,
  `credentials*`, …), no content sniffing (`AKIA…`, `ghp_…`, private-key headers) and no
  redaction counting. Nothing filters `ChangeSet` paths before digest assembly.
- **Why it matters:** the digest carries verbatim source lines. A modified tracked secret
  file — the research doc's explicitly verified pitfall — or a changed line containing an
  inline API key is sent to the configured provider on `A`. The status line cannot show
  "3 redactions" because redaction (beyond repo-root stripping) never happens. This is the
  gap between the documented privacy contract and the shipped behavior.
- **Suggested fix:** implement layer 3 as a compiled `globset` denylist filtering
  `ChangeSet.files` (drop file's hunks, keep a "excluded: N files" note), honor a root
  `.codescopeignore` via the already-declared `ignore` crate, and add a small regex pass
  over the rendered digest immediately before send, replacing matches with
  `«redacted:<kind>»` and counting them. Mitigation lands entirely in
  `change_digest`/`AiService::request_plan`, so it is well-contained.

### 2. AI auto-enables from ambient env keys with no first-use disclosure
- **Severity: medium**
- **Where:** `crates/codescope-ai/src/config.rs:128` (`AiMode::Auto => key.is_some()`) with
  fallback chain `CODESCOPE_AI_API_KEY` → `PRIME_API_KEY` → `OPENAI_API_KEY`
  (`config.rs:288-296`); wiring `crates/codescope/src/main.rs:70-84`; keymap
  `crates/codescope-tui/src/action.rs:113-114`; silent toggle
  `crates/codescope/src/dispatcher.rs:101-108`; status label only `off/idle/…/✓`
  (`crates/codescope-tui/src/render.rs` `ai_label`).
- **What:** research 07 §2 requires a first-enable disclosure modal (what is sent, caps,
  provider/base_url/model, key source) with explicit confirm, plus a status segment with
  request/redaction counts. None of this exists. A developer with a globally exported
  `OPENAI_API_KEY` gets AI armed at startup; one `A` keypress sends repo-derived content
  (finding 1 unfiltered) with no indication of endpoint or payload.
- **Why it matters:** consent-by-ambient-env-var is the exact failure mode the research
  called out; `OPENAI_API_KEY` is extremely common in shells. This pre-automatic-generation
  review predates the current AI-required startup contract and explicit provider/model status.
- **Suggested fix:** first `AiRefresh` per session opens a confirm modal listing endpoint,
  model, key env-var name, and payload summary; render provider/model in the status bar
  when enabled. Alternatively require explicit `CODESCOPE_AI=on` when the key came from the
  generic `OPENAI_API_KEY` fallback.

### 3. Diagnostic messages can carry absolute non-repo paths into the digest
- **Severity: low**
- **Where:** `crates/codescope-analysis/src/digest.rs:271-286` (message passed through,
  only length-truncated); `crates/codescope-ai/src/service.rs:260-267` (`redact_repo_root`
  strips the repo toplevel prefix only).
- **What:** gopls/go-list diagnostics sometimes embed absolute paths outside the repo
  (module cache, GOROOT, `inconsistent vendoring in /Users/<name>/…`). Repo-root redaction
  does not touch them, so `$HOME`/username can reach the provider inside tier-2 messages.
- **Why it matters:** research 07 §2: "only repo-relative paths (never absolute — they leak
  username/home)". The digest's own file fields are repo-relative; the free-text message is
  the remaining hole.
- **Suggested fix:** scrub `$HOME` (and `GOMODCACHE`/`GOPATH` when resolvable) from
  diagnostic messages at digest build, or generalize `redact_repo_root` to also replace the
  user home dir with `~`.

### 4. Symlink-escape guard on worktree reads is missing
- **Severity: low**
- **Where:** `crates/codescope-lsp/src/gopls.rs:142-149` (`sync_worktree`:
  `std::fs::read_to_string(root.join(file))` follows symlinks), same pattern at
  `:184-199` (`diagnostics`), `:186+` and in `references`/hierarchy prep; no
  `canonicalize`-under-root check exists anywhere.
- **What:** research 07 §1 pitfall: a repo symlink `a.go -> ~/.ssh/id_rsa` (or any file
  outside the repo) is read and `didOpen`ed to gopls when it appears in a changeset.
  Content stays local (gopls), but symbol names/signatures parsed from an out-of-repo Go
  file can enter digest tier 1 and leave the machine when AI is used.
- **Why it matters:** codescope's use case includes inspecting unfamiliar branches — i.e.
  potentially attacker-authored trees. The stated guarantee ("repo-root-sandboxed") is not
  enforced at the one place worktree bytes are read.
- **Suggested fix:** in `sync_worktree` (single choke point), canonicalize `abs` and require
  `starts_with(canonicalized root)`, degrading to a per-file note otherwise.

### 5. `base_url` is logged verbatim; query-string credentials would leak to the log file
- **Severity: low**
- **Where:** `crates/codescope-ai/src/config.rs:178-186` (`tracing::info!(base_url = %base_url, …)`);
  `crates/codescope-ai/src/client.rs:184-196` (`Debug` prints full `endpoint`);
  log sink is opt-in `--log-file` (`crates/codescope/src/main.rs:36,109-121`).
- **What:** providers that key via query string (Azure-style `?api-key=…` in
  `CODESCOPE_AI_BASE_URL`) would have the credential written to the log. Research 07 §2
  says to scrub `base_url` query strings. Transport errors are already sanitized
  (`without_url`), so this is only the info-log/Debug path.
- **Why it matters:** `--log-file` output is exactly the artifact users paste into issues.
- **Suggested fix:** strip `?…` (or the whole query) from `base_url` before logging and in
  both hand-written `Debug` impls.

### 6. Git hardening is excellent, but there is no subcommand allowlist audit point
- **Severity: low**
- **Where:** `crates/codescope-git/src/runner.rs:46-71` (`GitCommand::new` accepts arbitrary
  args; env hardening + `--no-optional-locks` + `GIT_OPTIONAL_LOCKS=0` at `:47`, `:55-62`).
- **What:** research 07 §3 recommends a centralized allowlist as the single audit point.
  `GitCommand` is `pub(crate)`, and I verified every call site is read-only
  (`repo.rs`: `rev-parse`, `status`, `symbolic-ref --quiet`, `merge-base`, `log`, `diff`,
  `diff --cached`, `show`, `ls-files --stage`), so the guarantee currently holds by
  inspection — but nothing stops a future `cmd(&["stash", …])` from compiling.
- **Why it matters:** the read-only promise is the product's headline; it should be
  mechanically enforced, not review-enforced.
- **Suggested fix:** const allowlist of first tokens in `GitCommand::new` with a
  `debug_assert!`/hard error, plus a unit test.

### 7. Latent: `FileId` allows `..`, and ToolExecutor sandboxing is doc-comment-only
- **Severity: low**
- **Where:** `crates/codescope-core/src/file.rs:36-44` (only absolute paths rejected;
  `..` accepted), `:46` (`new_unchecked`); `crates/codescope-ai/src/tools.rs:186-199`
  (sandbox rules stated in the trait docs, no shared enforcement); currently inert because
  the binary passes `NoToolExecutor` (`crates/codescope/src/dispatcher.rs:166`).
- **What:** when a real `ToolExecutor` is wired (the design intends it), model-supplied
  `file` arguments like `"../../.ssh/config"` pass `FileId::new` and would resolve outside
  the repo via `root.join(file)` unless every implementer remembers the doc comment.
  The plan validator protects plan *entities* (must match known facts) but tool *arguments*
  are only checked by the executor.
- **Why it matters:** the privacy boundary for the future tool loop rests on an unenforced
  convention at the exact trust boundary (model-controlled input → filesystem).
- **Suggested fix:** reject `..` components in `FileId::new` (git never emits them), or ship
  a `sanitize_tool_path(root, arg)` helper next to the trait and use it in the first real
  executor.

### 8. Config-file layer (`.codescope.toml` / user config) is documented but never loaded
- **Severity: low**
- **Where:** `crates/codescope-ai/src/config.rs:79` (`from_env_with_file` — no production
  caller), `:220-247` (`AiFileConfig`); `crates/codescope/src/main.rs:73` uses
  `AiConfig::from_env()` only; `figment` declared unused (`Cargo.toml:45`); no
  `etcetera`/config-dir code exists.
- **What:** the research resolution order (defaults < user config < project config < env <
  CLI) is unimplemented; resolution is env-only. The `LiteralApiKeyInConfig` hard error
  (`config.rs:107-110`) — a genuine, well-tested protection — can never trigger because no
  file is ever parsed.
- **Why it matters:** privacy-wise this is conservative (fewer inputs), but the docs/module
  comments imply a file layer that a `.codescope.toml` author would silently not get,
  including its key-hygiene guarantees.
- **Suggested fix:** either wire project/user config loading through
  `from_env_with_file`, or mark the file layer as future work in the config module docs.

### 9. Minor payload-cap and watcher notes
- **Severity: low**
- **Where:** `crates/codescope/src/dispatcher.rs:162` renders the digest without calling
  `ChangeDigest::truncate_to_budget` (`crates/codescope-analysis/src/digest.rs:483`), which
  only tests call — the engine doc (`engine.rs:73`) says to apply it before prompting.
  Payload stays bounded anyway by build-time tier caps (50/30/40/100), so this is a missed
  belt-and-suspenders, and research's `max_excerpt_bytes`/`max_request_bytes` knobs don't
  exist. Separately, `crates/codescope/src/watcher.rs:26,62`: the tree watcher recursively
  watches the whole toplevel including `.git` (double-watched) and git-ignored dirs
  (`target/`, `node_modules/`) — `is_relevant` is `true` for every tree event. No egress,
  but ignore rules are not respected here (research 07 §3), causing refresh churn during
  builds.
- **Suggested fix:** call `truncate_to_budget(DIGEST_DEFAULT_TOKEN_BUDGET)` in
  `refresh_ai`; skip `.git` and ignored paths in the tree watcher.

## Read-only verification (no findings)

- Git: every spawn goes through `GitCommand` with `--no-optional-locks`,
  `GIT_OPTIONAL_LOCKS=0`, `GIT_TERMINAL_PROMPT=0`, inherited `GIT_DIR`/`GIT_WORK_TREE`/
  `GIT_INDEX_FILE`/`GIT_EXTERNAL_DIFF`/`GIT_PAGER` removed (`runner.rs:46-63`). Observed
  subcommand set is read-only (finding 6).
- LSP: request surface is documentSymbol/references/callHierarchy/typeHierarchy/
  implementation only; `workspace/applyEdit` answered `{"applied": false}`
  (`crates/codescope-lsp/src/client.rs:135-137`); unknown server requests get `-32601`;
  gopls spawned without `GOFLAGS` overrides (`gopls.rs:56-61`), keeping its default
  `-mod=readonly`.
- Base revisions come from `git show` into LSP overlays (`gopls.rs` `base_document_symbols`,
  `repo.rs:353-366`) — no temp files; production `tempfile` usage is tests/testutil only.
- Only production filesystem write: the opt-in `--log-file` (`main.rs:111`). Note: pointing
  it inside the repo creates a watcher feedback loop (log write → RepoChanged → refresh →
  log write); consider warning or defaulting to the cache dir.
- Untracked files are enumerated via `status --untracked-files=all` (ignored files are
  never listed by git, so `.gitignore`/`info/exclude`/global excludes are respected by
  construction); untracked content is never read into the digest — only paths.
- Env reads: AI key chain + `CODESCOPE_GOPLS` (program name) + test-only `CODESCOPE_LIVE`.
  Nothing logs env values; live smoke test is `#[ignore]`-gated.

## Verdict

**fix-first.** Key handling, redaction of the repo root, and the read-only guarantee are
solid and test-backed; deterministic (AI-off) operation is ship-ready. But the shipped AI
path contradicts architecture decision 7: without the secrets denylist / `.codescopeignore`
/ content sniffing (finding 1) and with silent auto-enable minus disclosure (finding 2),
changed secret material can leave the machine on a single keypress. Land findings 1 and 2
(both are localized: digest assembly + one modal) before enabling AI for real users; the
remaining findings are hardening.
