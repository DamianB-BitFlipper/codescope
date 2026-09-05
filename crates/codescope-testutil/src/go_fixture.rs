//! Deterministic Go fixture repository for codescope tests.
//!
//! [`build_fixture`] regenerates — from scratch, never by hand-editing — a small Go module
//! (`fixture.example/codescopefx`) with a store → service → api call chain, a `Repository`
//! interface with two implementations, a two-commit feature branch, and a deliberately
//! dirty working state (staged edit, staged rename, unstaged edit, untracked file).
//!
//! # Determinism
//!
//! Every `git` invocation runs with fixed author/committer identity and dates
//! ([`FIXTURE_DATE_BASE`] onward, all on 2026-01-01) and with host git config neutralized
//! (`GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=<null device>`), so commit OIDs are
//! identical across rebuilds and machines (research 08 §1.3). Tests may assert on
//! [`FixtureInfo::head_prefix`] stability.
//!
//! # Layout and history
//!
//! ```text
//! main:                 base commit — go.mod, store/{store,memory,postgres,store_test}.go,
//!                       service/service.go, api/api.go, cmd/server/main.go
//! feature/api-changes:  commit 1 — adds internal/api/middleware.go (LoggingMiddleware)
//!                       commit 2 — edits PostgresRepo.Get body (empty-dsn guard)
//! working state:        staged   M  internal/service/service.go   (GetDisplayName fallback)
//!                       staged   R  internal/store/memory.go → memstore.go (git mv)
//!                       unstaged M  internal/store/memstore.go    (MemoryRepo.Get nil guard)
//!                       untracked   internal/api/health.go
//! ```
//!
//! `git status --porcelain=v2 --branch -uall` on the result (verified):
//!
//! ```text
//! # branch.head feature/api-changes
//! 1 M. … internal/service/service.go
//! 2 RM … R100 internal/store/memstore.go<TAB>internal/store/memory.go
//! ? internal/api/health.go
//! ```
//!
//! The rename and the unstaged `MemoryRepo.Get` edit target the same file, so they share
//! one porcelain entry: `R` on the staged side, `M` on the unstaged side.

use crate::error::{Result, TestutilError};
use codescope_core::Oid;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Go module path of the fixture.
pub const MODULE_PATH: &str = "fixture.example/codescopefx";

/// Feature branch the fixture leaves checked out.
pub const FIXTURE_BRANCH: &str = "feature/api-changes";

/// Base branch the feature branch diverged from.
pub const FIXTURE_BASE: &str = "main";

/// File carrying the **staged** modification (`UserService.GetDisplayName`).
pub const STAGED_MODIFIED_FILE: &str = "internal/service/service.go";

/// Pre-rename path of the staged rename (`git mv` source).
pub const RENAMED_FROM: &str = "internal/store/memory.go";

/// Post-rename path of the staged rename; also carries the **unstaged** modification
/// (`MemoryRepo.Get`).
pub const RENAMED_TO: &str = "internal/store/memstore.go";

/// The untracked file (valid Go, package `api`).
pub const UNTRACKED_FILE: &str = "internal/api/health.go";

/// File added by the first feature-branch commit.
pub const MIDDLEWARE_FILE: &str = "internal/api/middleware.go";

/// File whose `PostgresRepo.Get` body the second feature-branch commit edits.
pub const POSTGRES_FILE: &str = "internal/store/postgres.go";

/// Fixed author/committer name used for every commit.
pub const FIXTURE_IDENT_NAME: &str = "codescope fixture";

/// Fixed author/committer email used for every commit.
pub const FIXTURE_IDENT_EMAIL: &str = "fixture@codescope.invalid";

/// Author+committer date of the base commit.
pub const FIXTURE_DATE_BASE: &str = "2026-01-01T00:00:00Z";

/// Author+committer date of the first feature commit.
pub const FIXTURE_DATE_COMMIT1: &str = "2026-01-01T00:01:00Z";

/// Author+committer date of the second feature commit.
pub const FIXTURE_DATE_COMMIT2: &str = "2026-01-01T00:02:00Z";

/// Length of [`FixtureInfo::head_prefix`] in hex characters.
pub const HEAD_PREFIX_LEN: usize = 12;

/// Description of one freshly built fixture.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FixtureInfo {
    /// Absolute (or caller-relative) path of the fixture worktree root.
    pub root: PathBuf,
    /// Checked-out feature branch ([`FIXTURE_BRANCH`]).
    pub branch: String,
    /// Base branch ([`FIXTURE_BASE`]).
    pub base: String,
    /// First [`HEAD_PREFIX_LEN`] hex chars of the HEAD commit OID. Stable across rebuilds
    /// and machines thanks to the fixed commit metadata.
    pub head_prefix: Oid,
}

/// Remove a previously built fixture directory (recursively), if it exists.
///
/// Missing directories are not an error, so this is safe to call unconditionally before a
/// rebuild. [`build_fixture`] calls it internally.
pub fn reset_fixture(dir: impl AsRef<Path>) -> Result<()> {
    let dir = dir.as_ref();
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {
            tracing::debug!(dir = %dir.display(), "removed fixture dir");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(TestutilError::Io {
            path: dir.to_path_buf(),
            source,
        }),
    }
}

/// Build the fixture from scratch inside `dir` (any existing content is removed first).
///
/// Requires `git` on `PATH`; does **not** require `go` (run [`go_build`] separately to
/// typecheck). On success the worktree is left on [`FIXTURE_BRANCH`] with the dirty state
/// described in the module docs.
pub fn build_fixture(dir: impl AsRef<Path>) -> Result<FixtureInfo> {
    let root = dir.as_ref();
    tracing::info!(root = %root.display(), "building go fixture");
    reset_fixture(root)?;
    std::fs::create_dir_all(root).map_err(|source| TestutilError::Io {
        path: root.to_path_buf(),
        source,
    })?;

    // Baseline tree on `main`.
    write_file(root, "go.mod", GO_MOD)?;
    write_file(root, "internal/store/store.go", STORE_GO)?;
    write_file(root, RENAMED_FROM, MEMORY_GO)?;
    write_file(root, POSTGRES_FILE, POSTGRES_GO)?;
    write_file(root, "internal/store/store_test.go", STORE_TEST_GO)?;
    write_file(root, STAGED_MODIFIED_FILE, SERVICE_GO)?;
    write_file(root, "internal/api/api.go", API_GO)?;
    write_file(root, "cmd/server/main.go", MAIN_GO)?;

    git(root, None, &["init", "-q", "-b", FIXTURE_BASE])?;
    git(root, None, &["config", "user.name", FIXTURE_IDENT_NAME])?;
    git(root, None, &["config", "user.email", FIXTURE_IDENT_EMAIL])?;
    git(root, None, &["config", "commit.gpgsign", "false"])?;
    git(root, None, &["config", "tag.gpgsign", "false"])?;
    git(root, None, &["config", "core.autocrlf", "false"])?;
    git(root, None, &["add", "-A"])?;
    git(
        root,
        Some(FIXTURE_DATE_BASE),
        &[
            "commit",
            "-q",
            "-m",
            "base: store/service/api layers with Repository interface",
        ],
    )?;

    // Feature branch: two deterministic commits.
    git(root, None, &["checkout", "-q", "-b", FIXTURE_BRANCH])?;
    write_file(root, MIDDLEWARE_FILE, MIDDLEWARE_GO)?;
    git(root, None, &["add", MIDDLEWARE_FILE])?;
    git(
        root,
        Some(FIXTURE_DATE_COMMIT1),
        &["commit", "-q", "-m", "feature: add LoggingMiddleware"],
    )?;
    write_file(root, POSTGRES_FILE, POSTGRES_GO_COMMIT2)?;
    git(root, None, &["add", POSTGRES_FILE])?;
    git(
        root,
        Some(FIXTURE_DATE_COMMIT2),
        &[
            "commit",
            "-q",
            "-m",
            "feature: guard PostgresRepo.Get against empty dsn",
        ],
    )?;

    let head = git(root, None, &["rev-parse", "HEAD"])?;
    let head = head.trim();
    if head.len() < HEAD_PREFIX_LEN || !head.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(TestutilError::ToolOutput {
            tool: "git".to_string(),
            detail: format!("rev-parse HEAD returned {head:?}"),
        });
    }

    // Dirty working state (order matters: rename before the unstaged edit so the edit
    // lands in the post-rename file).
    write_file(root, STAGED_MODIFIED_FILE, SERVICE_GO_STAGED_EDIT)?;
    git(root, None, &["add", STAGED_MODIFIED_FILE])?;
    git(root, None, &["mv", RENAMED_FROM, RENAMED_TO])?;
    write_file(root, RENAMED_TO, MEMORY_GO_UNSTAGED_EDIT)?;
    write_file(root, UNTRACKED_FILE, HEALTH_GO)?;

    let info = FixtureInfo {
        root: root.to_path_buf(),
        branch: FIXTURE_BRANCH.to_string(),
        base: FIXTURE_BASE.to_string(),
        head_prefix: Oid::new(&head[..HEAD_PREFIX_LEN]),
    };
    tracing::info!(head_prefix = %info.head_prefix, "fixture built");
    Ok(info)
}

/// Run `go build ./...` in the fixture (hermetic: `GOWORK=off`, `GOTOOLCHAIN=local`).
///
/// Callers should skip (not fail) their test when [`crate::helpers::require_go`] returns
/// `None`.
pub fn go_build(root: impl AsRef<Path>) -> Result<()> {
    run_go(root.as_ref(), &["build", "./..."]).map(|_| ())
}

/// Run `go test ./...` in the fixture (hermetic; compiles and executes the fixture tests).
pub fn go_test(root: impl AsRef<Path>) -> Result<()> {
    run_go(root.as_ref(), &["test", "./..."]).map(|_| ())
}

/// Run `gofmt -l .` in the fixture and return the list of unformatted files (empty when
/// everything is gofmt-clean).
pub fn gofmt_unformatted(root: impl AsRef<Path>) -> Result<Vec<String>> {
    let out = run_tool(root.as_ref(), "gofmt", &["-l", "."], &[])?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

// ---------------------------------------------------------------------------
// process plumbing
// ---------------------------------------------------------------------------

/// Platform null device, used to neutralize the user's global git config.
fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

/// Run `git` inside `root` with fully pinned identity/config env. `date`, when given, sets
/// both `GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE`.
fn git(root: &Path, date: Option<&str>, args: &[&str]) -> Result<String> {
    let mut extra: Vec<(&str, &str)> = vec![
        ("GIT_AUTHOR_NAME", FIXTURE_IDENT_NAME),
        ("GIT_AUTHOR_EMAIL", FIXTURE_IDENT_EMAIL),
        ("GIT_COMMITTER_NAME", FIXTURE_IDENT_NAME),
        ("GIT_COMMITTER_EMAIL", FIXTURE_IDENT_EMAIL),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", null_device()),
        ("LC_ALL", "C"),
    ];
    if let Some(d) = date {
        extra.push(("GIT_AUTHOR_DATE", d));
        extra.push(("GIT_COMMITTER_DATE", d));
    }
    run_tool(root, "git", args, &extra)
}

/// Run `go` inside `root`, hermetically (no workspace files, no toolchain downloads).
fn run_go(root: &Path, args: &[&str]) -> Result<String> {
    run_tool(
        root,
        "go",
        args,
        &[("GOWORK", "off"), ("GOTOOLCHAIN", "local"), ("GOFLAGS", "")],
    )
}

/// Spawn `tool args…` in `root` with `extra_env`, capture output, error on non-zero exit.
fn run_tool(root: &Path, tool: &str, args: &[&str], extra_env: &[(&str, &str)]) -> Result<String> {
    let mut cmd = Command::new(tool);
    cmd.args(args).current_dir(root);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    tracing::debug!(tool, ?args, root = %root.display(), "running tool");
    let out = cmd.output().map_err(|source| TestutilError::Spawn {
        tool: tool.to_string(),
        source,
    })?;
    if !out.status.success() {
        return Err(TestutilError::ToolFailed {
            tool: tool.to_string(),
            args: args.join(" "),
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Write `content` at `root/rel`, creating parent directories.
fn write_file(root: &Path, rel: &str, content: &str) -> Result<()> {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| TestutilError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(&path, content).map_err(|source| TestutilError::Io { path, source })
}

// ---------------------------------------------------------------------------
// Go sources (verified: `go build ./...`, `go vet ./...`, `go test ./...` pass
// and `gofmt -l` is empty on the assembled fixture, including edited variants).
// ---------------------------------------------------------------------------

const GO_MOD: &str = r#"module fixture.example/codescopefx

go 1.26
"#;

const STORE_GO: &str = r#"// Package store provides user persistence backends.
package store

import "errors"

// ErrNotFound is returned when a user does not exist.
var ErrNotFound = errors.New("store: user not found")

// User is the domain entity persisted by a Repository.
type User struct {
	ID    int
	Name  string
	Email string
}

// Repository is the persistence contract for users.
type Repository interface {
	Get(id int) (User, error)
	Save(u User) error
	Delete(id int) error
}
"#;

const MEMORY_GO: &str = r#"package store

// MemoryRepo is an in-memory Repository backed by a map.
type MemoryRepo struct {
	users map[int]User
}

// NewMemoryRepo returns an empty in-memory repository.
func NewMemoryRepo() *MemoryRepo {
	return &MemoryRepo{users: make(map[int]User)}
}

// Get returns the stored user with the given id.
func (m *MemoryRepo) Get(id int) (User, error) {
	u, ok := m.users[id]
	if !ok {
		return User{}, ErrNotFound
	}
	return u, nil
}

// Save stores u, replacing any existing user with the same id.
func (m *MemoryRepo) Save(u User) error {
	m.users[u.ID] = u
	return nil
}

// Delete removes the user with the given id.
func (m *MemoryRepo) Delete(id int) error {
	delete(m.users, id)
	return nil
}
"#;

/// `MemoryRepo.Get` with a nil-map guard — the **unstaged** edit applied to
/// `internal/store/memstore.go` after the staged rename.
const MEMORY_GO_UNSTAGED_EDIT: &str = r#"package store

// MemoryRepo is an in-memory Repository backed by a map.
type MemoryRepo struct {
	users map[int]User
}

// NewMemoryRepo returns an empty in-memory repository.
func NewMemoryRepo() *MemoryRepo {
	return &MemoryRepo{users: make(map[int]User)}
}

// Get returns the stored user with the given id.
func (m *MemoryRepo) Get(id int) (User, error) {
	if m.users == nil {
		return User{}, ErrNotFound
	}
	u, ok := m.users[id]
	if !ok {
		return User{}, ErrNotFound
	}
	return u, nil
}

// Save stores u, replacing any existing user with the same id.
func (m *MemoryRepo) Save(u User) error {
	m.users[u.ID] = u
	return nil
}

// Delete removes the user with the given id.
func (m *MemoryRepo) Delete(id int) error {
	delete(m.users, id)
	return nil
}
"#;

const POSTGRES_GO: &str = r#"package store

import "fmt"

// PostgresRepo is a stub Repository that would talk to Postgres via a DSN.
// It performs no I/O in this fixture; every method fails deterministically.
type PostgresRepo struct {
	dsn string
}

// NewPostgresRepo returns a PostgresRepo for the given DSN.
func NewPostgresRepo(dsn string) *PostgresRepo {
	return &PostgresRepo{dsn: dsn}
}

// Get would load a user from Postgres.
func (p *PostgresRepo) Get(id int) (User, error) {
	return User{}, fmt.Errorf("postgres %q: get %d: %w", p.dsn, id, ErrNotFound)
}

// Save would persist a user to Postgres.
func (p *PostgresRepo) Save(u User) error {
	return fmt.Errorf("postgres %q: save %d: not implemented", p.dsn, u.ID)
}

// Delete would delete a user from Postgres.
func (p *PostgresRepo) Delete(id int) error {
	return fmt.Errorf("postgres %q: delete %d: not implemented", p.dsn, id)
}
"#;

/// `PostgresRepo.Get` with an empty-dsn guard — the second feature-branch commit.
const POSTGRES_GO_COMMIT2: &str = r#"package store

import "fmt"

// PostgresRepo is a stub Repository that would talk to Postgres via a DSN.
// It performs no I/O in this fixture; every method fails deterministically.
type PostgresRepo struct {
	dsn string
}

// NewPostgresRepo returns a PostgresRepo for the given DSN.
func NewPostgresRepo(dsn string) *PostgresRepo {
	return &PostgresRepo{dsn: dsn}
}

// Get would load a user from Postgres.
func (p *PostgresRepo) Get(id int) (User, error) {
	if p.dsn == "" {
		return User{}, fmt.Errorf("postgres: empty dsn: %w", ErrNotFound)
	}
	return User{}, fmt.Errorf("postgres %q: get %d: %w", p.dsn, id, ErrNotFound)
}

// Save would persist a user to Postgres.
func (p *PostgresRepo) Save(u User) error {
	return fmt.Errorf("postgres %q: save %d: not implemented", p.dsn, u.ID)
}

// Delete would delete a user from Postgres.
func (p *PostgresRepo) Delete(id int) error {
	return fmt.Errorf("postgres %q: delete %d: not implemented", p.dsn, id)
}
"#;

const STORE_TEST_GO: &str = r#"package store

import (
	"errors"
	"testing"
)

func TestMemoryRepoRoundTrip(t *testing.T) {
	repo := NewMemoryRepo()
	want := User{ID: 1, Name: "Ada", Email: "ada@example.com"}
	if err := repo.Save(want); err != nil {
		t.Fatalf("Save: %v", err)
	}
	got, err := repo.Get(1)
	if err != nil {
		t.Fatalf("Get: %v", err)
	}
	if got != want {
		t.Fatalf("Get = %+v, want %+v", got, want)
	}
}

func TestMemoryRepoGetMissing(t *testing.T) {
	repo := NewMemoryRepo()
	if _, err := repo.Get(42); !errors.Is(err, ErrNotFound) {
		t.Fatalf("Get(42) err = %v, want ErrNotFound", err)
	}
}
"#;

const SERVICE_GO: &str = r#"// Package service contains user-facing business logic on top of store.
package service

import (
	"strings"

	"fixture.example/codescopefx/internal/store"
)

// UserService exposes user operations over a store.Repository.
type UserService struct {
	repo store.Repository
}

// NewUserService returns a UserService using repo.
func NewUserService(repo store.Repository) *UserService {
	return &UserService{repo: repo}
}

// FormatName normalizes a user-visible name.
func FormatName(name string) string {
	return strings.TrimSpace(name)
}

// GetDisplayName loads the user and returns its formatted display name.
func (s *UserService) GetDisplayName(id int) (string, error) {
	u, err := s.repo.Get(id)
	if err != nil {
		return "", err
	}
	return FormatName(u.Name), nil
}

// Register persists a new user.
func (s *UserService) Register(u store.User) error {
	return s.repo.Save(u)
}
"#;

/// `UserService.GetDisplayName` with an anonymous-name fallback — the **staged** edit.
const SERVICE_GO_STAGED_EDIT: &str = r#"// Package service contains user-facing business logic on top of store.
package service

import (
	"strings"

	"fixture.example/codescopefx/internal/store"
)

// UserService exposes user operations over a store.Repository.
type UserService struct {
	repo store.Repository
}

// NewUserService returns a UserService using repo.
func NewUserService(repo store.Repository) *UserService {
	return &UserService{repo: repo}
}

// FormatName normalizes a user-visible name.
func FormatName(name string) string {
	return strings.TrimSpace(name)
}

// GetDisplayName loads the user and returns its formatted display name.
func (s *UserService) GetDisplayName(id int) (string, error) {
	u, err := s.repo.Get(id)
	if err != nil {
		return "", err
	}
	name := FormatName(u.Name)
	if name == "" {
		name = "anonymous"
	}
	return name, nil
}

// Register persists a new user.
func (s *UserService) Register(u store.User) error {
	return s.repo.Save(u)
}
"#;

const API_GO: &str = r#"// Package api exposes request handlers over the service layer.
package api

import (
	"fmt"

	"fixture.example/codescopefx/internal/service"
)

// Handler dispatches user requests to the service layer.
type Handler struct {
	svc *service.UserService
}

// NewHandler returns a Handler using svc.
func NewHandler(svc *service.UserService) *Handler {
	return &Handler{svc: svc}
}

// HandleGetUser renders the display name for the user with the given id.
func (h *Handler) HandleGetUser(id int) (string, error) {
	name, err := h.svc.GetDisplayName(id)
	if err != nil {
		return "", fmt.Errorf("get user %d: %w", id, err)
	}
	return "user: " + name, nil
}
"#;

/// `LoggingMiddleware` — added by the first feature-branch commit.
const MIDDLEWARE_GO: &str = r#"package api

import "log"

// LoggingMiddleware wraps next and logs every handled user id.
func LoggingMiddleware(next func(int) (string, error)) func(int) (string, error) {
	return func(id int) (string, error) {
		log.Printf("api: handling user %d", id)
		out, err := next(id)
		if err != nil {
			log.Printf("api: user %d failed: %v", id, err)
		}
		return out, err
	}
}
"#;

/// Untracked file: valid Go in package `api` so `go build ./...` keeps passing.
const HEALTH_GO: &str = r#"package api

// Health reports liveness for the api layer.
func Health() string {
	return "ok"
}
"#;

const MAIN_GO: &str = r#"// Command server wires a repository backend into the api handler.
package main

import (
	"flag"
	"fmt"
	"os"

	"fixture.example/codescopefx/internal/api"
	"fixture.example/codescopefx/internal/service"
	"fixture.example/codescopefx/internal/store"
)

func main() {
	backend := flag.String("repo", "memory", "repository backend: memory or postgres")
	dsn := flag.String("dsn", "postgres://localhost/codescopefx", "postgres DSN")
	flag.Parse()

	var repo store.Repository
	switch *backend {
	case "postgres":
		repo = store.NewPostgresRepo(*dsn)
	default:
		repo = store.NewMemoryRepo()
	}

	svc := service.NewUserService(repo)
	if err := svc.Register(store.User{ID: 1, Name: " Ada ", Email: "ada@example.com"}); err != nil {
		fmt.Fprintf(os.Stderr, "register: %v\n", err)
		os.Exit(1)
	}

	handler := api.NewHandler(svc)
	out, err := handler.HandleGetUser(1)
	if err != nil {
		fmt.Fprintf(os.Stderr, "get user: %v\n", err)
		os.Exit(1)
	}
	fmt.Println(out)
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edited_variants_differ_from_baselines() {
        assert_ne!(SERVICE_GO, SERVICE_GO_STAGED_EDIT);
        assert_ne!(MEMORY_GO, MEMORY_GO_UNSTAGED_EDIT);
        assert_ne!(POSTGRES_GO, POSTGRES_GO_COMMIT2);
        // The edits touch exactly the functions the task names.
        assert!(SERVICE_GO_STAGED_EDIT.contains(r#"name = "anonymous""#));
        assert!(MEMORY_GO_UNSTAGED_EDIT.contains("if m.users == nil {"));
        assert!(POSTGRES_GO_COMMIT2.contains(r#"if p.dsn == "" {"#));
    }

    #[test]
    fn go_sources_are_tab_indented_with_trailing_newline() {
        for (name, src) in [
            ("store.go", STORE_GO),
            ("memory.go", MEMORY_GO),
            ("memstore.go edit", MEMORY_GO_UNSTAGED_EDIT),
            ("postgres.go", POSTGRES_GO),
            ("postgres.go commit2", POSTGRES_GO_COMMIT2),
            ("store_test.go", STORE_TEST_GO),
            ("service.go", SERVICE_GO),
            ("service.go staged", SERVICE_GO_STAGED_EDIT),
            ("api.go", API_GO),
            ("middleware.go", MIDDLEWARE_GO),
            ("health.go", HEALTH_GO),
            ("main.go", MAIN_GO),
        ] {
            assert!(src.ends_with('\n'), "{name}: missing trailing newline");
            for line in src.lines() {
                assert!(
                    !line.starts_with("    "),
                    "{name}: space-indented line (gofmt wants tabs): {line:?}"
                );
            }
        }
    }

    #[test]
    fn baseline_main_does_not_reference_feature_branch_symbols() {
        // main.go is committed on `main`, before LoggingMiddleware exists.
        assert!(!MAIN_GO.contains("LoggingMiddleware"));
        assert!(MAIN_GO.contains("-repo") || MAIN_GO.contains(r#"flag.String("repo""#));
        assert!(MAIN_GO.contains("NewPostgresRepo"));
        assert!(MAIN_GO.contains("NewMemoryRepo"));
    }

    #[test]
    fn fixture_info_serde_roundtrip() {
        let info = FixtureInfo {
            root: PathBuf::from("/tmp/fx"),
            branch: FIXTURE_BRANCH.to_string(),
            base: FIXTURE_BASE.to_string(),
            head_prefix: Oid::new("0123456789ab"),
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: FixtureInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
        assert_eq!(back.head_prefix.as_str().len(), HEAD_PREFIX_LEN);
    }

    #[test]
    fn reset_fixture_is_idempotent_on_missing_dir() {
        let dir = std::env::temp_dir().join("codescope-testutil-definitely-missing-dir");
        let _ = std::fs::remove_dir_all(&dir);
        reset_fixture(&dir).unwrap();
        reset_fixture(&dir).unwrap();
    }
}
