# 02 — Git change modeling (read-only)

Recovered from sub-agent `research-git` verified experiments (agent stalled in final writing step;
doc written by lead engineer from its complete outline + verified facts, git 2.50.1).

## Verdict: CLI subprocess (not git2, not gix)

- **git CLI** — recommended. Zero C build; behavior exactly matches the user's git (worktrees,
  submodules); stable machine formats (porcelain v2 + `-z`); the app already spawns gopls.
- git2 0.21 (libgit2 1.9.7): builds C code; rename-detection defaults and linked-worktree
  support differ from CLI git. Not recommended.
- gix 0.87: pure Rust, promising, but status/diff coverage still maturing. Revisit later.

## Command inventory (verified formats)

Repo anchor: `git rev-parse --show-toplevel` (paths in porcelain v2 and `diff --git a/...` are
repo-root relative; run commands from toplevel).

Status: `git --no-optional-locks status --porcelain=v2 --branch -z --untracked-files=all`
- Headers: `# branch.oid <sha>` (`(initial)` when unborn), `# branch.head <name>` or
  `(detached)`, `# branch.upstream <name>` (only when set), `# branch.ab +A -B` (only when set).
- Ordinary: `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>` — X=staged (index vs HEAD),
  Y=unstaged (worktree vs index).
- Rename/copy: `2 <XY> ... <R|C><score> <newPath><NUL><origPath>` (new first, old second).
- Unmerged: `u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>` (THREE stage modes/SHAs).
- Untracked: `? <path>`; ignored (with --ignored): `! <path>`.
- Without `-z`, non-ASCII paths are C-quoted octal (core.quotePath); ALWAYS use `-z`.
- Untracked dirs collapse to `? dir/` unless `--untracked-files=all` (use `all`; codescope
  needs per-file granularity for LSP mapping).

Diffs (pass `-M` explicitly even though diff.renames defaults to true since git 2.9):
- unstaged: `git diff -M -U3` (worktree vs index)
- staged: `git diff --cached -M -U3` (index vs HEAD)
- branch-committed: `git diff -M -U3 <merge-base>...HEAD`
- total-vs-base (worktree): `git diff -M -U3 <merge-base>` (all changes except untracked)
- Hunk header: `@@ -old_start[,old_len] +new_start[,new_len] @@ [section-context]`;
  `,1` is OMITTED when len==1; len==0 possible (pure add/delete side).
- Rename headers: `similarity index NN%`, `rename from`, `rename to`.
  PITFALL: pathspec filtering breaks rename pairing (a rename whose source is excluded by the
  pathspec shows as a plain new file) — diff without narrow pathspecs.
- Binary: `Binary files a/x and b/x differ`; numstat shows `-  -`. Mark binary, no hunks.
- Submodule: mode 160000, porcelain `sub` field `S..`, diff shows `Subproject commit` lines —
  no line hunks; represent as a gitlink change.
- Unmerged paths: `git diff` emits combined `diff --cc` — skip hunk parsing, mark Conflicted.

Base inference fallback chain (verified): `@{upstream}` →
`git symbolic-ref refs/remotes/origin/HEAD` → guess `origin/main` / `origin/master` →
`git merge-base --fork-point` (needs reflog). Then `mb = git merge-base <base> HEAD`.
Commits on branch: `git log --oneline <mb>..HEAD`. Ahead/behind from `# branch.ab`.

Untracked file content: read from the filesystem (no git object). Base-revision content:
`git show <base>:<path>`.

Empty repo: `# branch.oid (initial)`; `git rev-parse HEAD` fails → ChangeSet with Unborn base;
all files untracked.

## Environment hardening (verified)

- `git --no-optional-locks` / `GIT_OPTIONAL_LOCKS=0` — `git status` otherwise may rewrite
  .git/index. REQUIRED for the read-only guarantee.
- Never rely on tty color detection; pass `--no-color` (or set `GIT_CONFIG_PARAMETERS`).
- Exit codes: 0 ok; 128 fatal (not a repo / bad revision); `diff --quiet` 1 = changes present.

## Rust data model (recommendation)

```rust
pub struct RepoContext { pub toplevel: PathBuf, pub head: HeadState,
    pub upstream: Option<Upstream>, pub base: Option<BaseInfo> }
pub enum HeadState { Branch(String), Detached(Oid), Unborn }
pub struct Upstream { pub name: String, pub ahead: u32, pub behind: u32 }
pub struct BaseInfo { pub source: BaseSource /*Upstream|OriginHead|Guess|ForkPoint*/,
    pub ref_name: String, pub merge_base: Oid }

pub struct ChangeSet { pub scope: ChangeScope, pub files: Vec<FileChange> }
pub enum ChangeScope { Branch /*mb...HEAD*/, Staged, Unstaged } // untracked live in Unstaged set
pub struct FileChange { pub path: PathBuf, pub old_path: Option<PathBuf> /*rename*/,
    pub status: FileStatus, pub hunks: Vec<Hunk>, pub binary: bool }
pub enum FileStatus { Added, Modified, Deleted, Renamed{score:u8}, Copied{score:u8},
    TypeChanged, Unmerged, Untracked, Gitlink }
pub struct Hunk { pub old_start: u32, pub old_len: u32, pub new_start: u32, pub new_len: u32,
    pub section: Option<String> /*header context*/, pub lines: Vec<DiffLine> }
```

Scopes stay distinct: three ChangeSets (Branch, Staged, Unstaged[+untracked]) computed
independently; the UI switches scope, never merges them implicitly.

## Pitfalls (verified)

1. `,1` omitted in hunk headers; count 0 on the empty side of pure add/delete.
2. Pathspec filtering silently breaks rename pairing.
3. `stash pop` destroys index rename info (rename becomes delete+add) — renames are index-only
   (`git mv`, not worktree moves).
4. CRLF: with autocrlf, git compares filtered content; LSP reads disk bytes. Map LSP symbols to
   DISK content, git hunks to git's filtered view; note possible 1-line offset mismatch class.
5. Unmerged paths have a different porcelain field count (3 modes/SHAs) — parse separately.
6. `git diff` inside a subdir still yields repo-root-relative paths; always anchor at toplevel.
7. Untracked dirs collapse without `--untracked-files=all`.

## Recommended decisions

1. git CLI subprocess only, `-z` + porcelain v2, `--no-optional-locks` on every call.
2. Three independent ChangeSets; untracked folded into the Unstaged set with status Untracked.
3. Base inference fallback chain as above; surface the chosen base + source in the status bar.
4. Binary/submodule/unmerged files represented explicitly, never hunk-parsed.
5. Content needs: untracked → fs read; base revision → `git show`; worktree → disk (matches LSP).
