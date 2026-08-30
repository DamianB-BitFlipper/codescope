# 07 — Privacy & Safety (codescope)

Scope: secret exclusion, AI opt-in + key handling, read-only guarantees, temp files,
AI failure modes, recommended Rust types/defaults. Verified locally 2026-08 (macOS,
git 2.50.1, gopls 0.21.0); crate versions checked against crates.io / docs.rs.

## Verified facts

- `git status` supports `--no-optional-locks` (= `GIT_OPTIONAL_LOCKS=0`); without it,
  `git status` may refresh `.git/index` (a write into the repo). Verified in `git help`.
- gopls 0.21.0 source: default `ModFlag: "readonly"` (`internal/cache/session.go:223`);
  when it needs module metadata it runs `go` commands against a **temp** `go.mod`
  (`snapshot.go:380`, `inv.ModFile = filepath.Join(tempDir, ...)`). Its on-disk cache
  lives in `os.UserCacheDir()` (e.g. `~/Library/Caches/gopls`), never the repo.
- `ignore` 0.4.33 defaults (docs.rs source): `git_ignore`, `git_exclude`, `git_global`,
  `require_git` are all `true` — `.gitignore`, `.git/info/exclude`, and
  `core.excludesFile` are respected out of the box inside a git repo.
- `ignore::WalkBuilder::add_custom_ignore_filename` exists; custom files have **higher
  precedence than all other ignore files** (later names win).
- `secrecy` 0.10.3: `SecretString` = `SecretBox<str>`; `Display`/`Debug` redact;
  serde `Serialize` requires opting in via the `SerializableSecret` marker trait;
  access only via `ExposeSecret::expose_secret()`.
- Latest crates (crates.io): `ignore 0.4.33`, `secrecy 0.10.3`, `zeroize 1.9.0`,
  `governor 0.10.4`, `backon 1.6.0`, `tempfile 3.27`, `figment 0.10.19`,
  `etcetera 0.11`, `directories 6.0`, `notify 8.2.0` (9.x is RC).

## 1. Exclusion layers (deny always wins over allow)

Apply to every source of file content: walker, `git diff` paths, watcher events, AI excerpts.

1. **Git rules** via `ignore::WalkBuilder` defaults (gitignore + info/exclude + global).
2. **`.codescopeignore`** at repo root via `add_custom_ignore_filename(".codescopeignore")`
   (gitignore syntax; outranks layer 1, so users can also re-include with `!`).
3. **Built-in secrets denylist** — compiled-in, cannot be disabled by config.
   Match with `ignore::overrides::OverrideBuilder` (inverted: pattern ⇒ Deny) or a
   `globset 0.4` matcher against repo-relative paths (also match case-insensitively
   for the extension part):
   - `.env`, `.env.*`, `*.env`
   - `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.jks`, `*.keystore`, `*.ppk`, `*.asc`
   - `id_rsa*`, `id_dsa*`, `id_ecdsa*`, `id_ed25519*`, `*.pub` pairs' private halves
   - `**/credentials*`, `**/*credentials.json`, `**/.secrets/**`, `**/secrets.*`
   - `**/.aws/credentials`, `**/.ssh/**`, `**/.gnupg/**`
   - `**/.docker/config.json`, `**/.kube/config`, `**/kubeconfig*`
   - `**/.netrc`, `**/.npmrc`, `**/.pypirc`, `*.tfvars`, `**/.terraformrc`
   Keep the list curated; broad patterns (`*token*`) cause false positives on legit code.
4. **Content sniffing** (defense in depth, runs on any text about to leave the process):
   `regex 1` set for `-----BEGIN (RSA|EC|OPENSSH|PGP)? ?PRIVATE KEY`, `ghp_…`, `github_pat_…`,
   `AKIA[0-9A-Z]{16}`, `xox[baprs]-…`, `sk-(ant-)?[A-Za-z0-9_-]{20,}`,
   `AIza[0-9A-Za-z_-]{35}`, JWT-shaped `eyJ…`. Replace matches with `«redacted:<kind>»`
   and count them for the status line.

Pitfalls:
- **Tracked secrets still appear in `git diff`/`git show` output.** Denylist must filter
  diff *paths* (drop those hunks entirely), not just the directory walker.
- `git check-ignore` alternative: works (`git check-ignore --stdin -z`, exit 0 = ignored),
  matches git exactly, but per-repo subprocess + no enumeration. Use only as a debug/CI
  validation path; keep `ignore` as the primary engine.
- Symlink escape: before reading a worktree file, `canonicalize` and require the result to
  stay under the repo root (a repo symlink can point at `~/.ssh/id_rsa`). Base-revision
  content should come from `git cat-file` (symlink = blob containing the target path), which
  sidesteps this for historical reads.

## 2. AI opt-in configuration

- **Default: AI off.** No HTTP client is constructed while `ai.enabled = false`; the app is
  fully functional deterministically. No telemetry, ever.
- **Resolution order** (low → high priority, later wins): built-in defaults → user config
  (`~/.config/codescope/config.toml` via `etcetera 0.11`) → project config
  (`<repo>/.codescope.toml`) → env (`CODESCOPE_AI__ENABLED`, `…__MODEL`, …) → CLI flags
  (`--ai`, `--model`). Use `figment 0.10` (`Toml`, `Env::prefixed("CODESCOPE_").split("__")`,
  `Serialized::defaults`).
- **Keys:** config stores only `api_key_env` = *name* of the env var (e.g. `"OPENAI_API_KEY"`).
  The value is read with `std::env::var` at request time into `ApiKey(SecretString)` and never
  stored in the config struct, serialized, or logged. If a literal `api_key` field appears in
  any config file → hard error, refuse to enable AI (prevents keys committed into config).
- **Redaction:** `secrecy 0.10.3`. Hand-`Debug` for `AiConfig` printing only non-secret fields;
  never log `Authorization` headers, request bodies, or env values; scrub `base_url` query
  strings. `zeroize 1.9` for raw key bytes after the HTTP request is built.
- **What gets sent:** only repo-relative paths (never absolute — they leak username/home),
  changed hunks ± N context lines (default 3), symbol names, LSP diagnostics. Hard caps:
  `max_excerpt_bytes` (default 8 KiB/hunk), `max_request_bytes` (default 64 KiB). Layer-3/4
  exclusions applied *after* excerpt assembly, immediately before send.
- **Disclosure:** first enable (config or `--ai`) triggers a modal listing exactly what is
  sent, the caps, the endpoint (provider + base_url + model), and "keys are read from
  $<api_key_env> at request time only". Requires explicit confirm. Per-session status bar
  segment: `AI: off` (default) | `AI: on · anthropic/claude-… · 12 req · 3 redactions` |
  `AI: cooldown 42s` | `AI: unavailable (deterministic mode)`.

## 3. Read-only guarantees

- **Git:** allowlist of subcommands only: `rev-parse`, `status --porcelain=v2`, `diff`,
  `diff --cached`, `log`, `show`, `cat-file`, `ls-files`, `check-ignore`, `for-each-ref`.
  Spawn every git with `GIT_OPTIONAL_LOCKS=0` in env (index refresh is the one sneaky write);
  `-c gc.auto=0` is optional defense-in-depth. Never spawn: `add`, `commit`, `update-index`,
  `checkout`, `restore`, `stash`, `clean`, `gc`, `prune`, `fetch`, `push`, `config` (write),
  `worktree`, `apply`, `am`, `merge`. Centralize spawning in one module that enforces the
  allowlist — single audit point.
- **LSP:** request read-only methods only (definition, references, hover, documentSymbol,
  callHierarchy, diagnostics via pull). Never send `textDocument/formatting`, `codeAction`
  with edit execution, `workspace/executeCommand` that edits, and reply `Ok(None)`/ignore to
  server→client `workspace/applyEdit` requests. Verified: gopls reads the repo with
  `-mod=readonly` and writes only to the user cache dir — safe by default; do **not** set
  `GOFLAGS=-mod=mod` in the gopls environment.
- **Watcher:** `notify 8.2` is read-only. Skip events under `.git` and excluded paths;
  debounce 100–300 ms to avoid churn.
- **Temp files:** prefer **LSP overlays** (`didOpen` with base-revision text from
  `git show`) — zero temp files. If a real on-disk base snapshot is required (gopls
  cross-file analysis of an old revision), use `tempfile 3.27` `TempDir` under
  `dirs::cache_dir()/codescope/` (never inside the repo, even gitignored — avoids watcher
  loops and accidental `git add`); set `0700` on Unix; `TempDir` auto-deletes on drop; sweep
  stale `codescope-*` dirs at startup for crash residue.

## 4. Rate limiting & failure modes

- Local token bucket: `governor 0.10` `RateLimiter::direct(Quota::per_minute(nonzero(10)))`,
  burst 2; configurable via `ai.max_requests_per_minute`.
- Retry: `backon 1.6` `ExponentialBuilder::default().with_jitter().with_max_times(2)`;
  retry only 429 / 5xx / connect errors / timeouts; honor `Retry-After` (cap 30 s). Never
  retry 4xx (other than 429).
- Budgets: connect 5 s; per-request 20 s (`ai.request_timeout_ms`); whole-analysis 60 s;
  `tokio::time::timeout` at each layer. On budget exhaustion → deterministic-only result.
- Circuit breaker: 3 consecutive transport failures ⇒ provider `Cooldown` for 60 s, then a
  single probe. Any cooldown/unavailable state ⇒ deterministic-only mode + status-bar reason;
  no error dialogs that block the TUI.
- `AiStatus` for the UI:
  `Disabled | Ready | InFlight { n: u32 } | Cooldown { until: Instant } | Unavailable { reason: String }`.

## 5. Recommended decisions

1. `ignore 0.4.33` walker + `add_custom_ignore_filename(".codescopeignore")` + compiled-in
   `globset` secrets denylist + `regex` content sniffing; apply the same denylist to diff
   paths and watcher events. Deny always wins; denylist can't be turned off.
2. AI off by default; `figment 0.10` layering defaults < user cfg < project cfg < env < CLI.
3. Keys: `secrecy::SecretString`, env-var-name-only config (`api_key_env`), literal `api_key`
   in config = hard error, custom `Debug`, never log headers/bodies.
4. Git subprocess allowlist with `GIT_OPTIONAL_LOCKS=0`; LSP read-only method subset; overlays
   instead of temp files; `tempfile::TempDir` in user cache dir if a snapshot is unavoidable.
5. `governor` (10 rpm, burst 2) + `backon` (2 retries, jitter, honor Retry-After) + 5/20/60 s
   budgets + 3-strike 60 s circuit breaker; all failures degrade silently to deterministic
   mode with an `AiStatus` status-bar segment.
6. Enable-time disclosure modal + persistent per-session status (provider, model, request
   count, redaction count). No telemetry.
