//! Orchestration: change-set in, epoch-tagged analysis out.
//!
//! [`AnalysisEngine::refresh`] runs the full pipeline for one [`ChangeSet`]:
//!
//! 1. repo context (HEAD / upstream / inferred base),
//! 2. per changed file: worktree symbol tree from the language service, plus a
//!    base-revision overlay tree (`git show` content) when the scope has a base and the
//!    file needs one (research 03),
//! 3. pure hunk→symbol mapping + per-symbol aggregation ([`crate::changes`]),
//! 4. 1-hop impact graph over the service ([`crate::graph`]), diagnostics annotated.
//!
//! Per-file semantic failures degrade to file-level notes instead of failing the pass;
//! only git/context failures are hard errors. Results carry the caller's [`Epoch`] so the
//! dispatcher can drop stale snapshots at apply time (research 06).

use codescope_core::{
    ChangeScope, ChangeSet, ChangedSymbol, Diagnostic, Epoch, Evidence, FileChange, FileId,
    FileStatus, HeadState, ImpactGraph, RepoContext, SymbolTree,
};
use codescope_git::GitRepo;

use crate::changes::{changed_symbols_detailed, file_mappings, ChangedSymbolInfo};
use crate::digest::{change_digest, ChangeDigest};
use crate::error::AnalysisError;
use crate::graph::{annotate_diagnostics, build_impact_graph};
use crate::mapper::MappedHunk;
use crate::source::SemanticSource;

/// Per-file analysis artifacts (trees, per-hunk mappings, degradation notes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAnalysis {
    /// Repo-relative path (the change's current path).
    pub file: FileId,
    /// Git status of the file in this change-set.
    pub status: FileStatus,
    /// Worktree symbol tree (`None` for deleted files or when the query failed).
    pub worktree: Option<SymbolTree>,
    /// Base-revision overlay tree (`None` when unavailable).
    pub base: Option<SymbolTree>,
    /// Per-hunk mappings (empty when the file was skipped).
    pub mappings: Vec<MappedHunk>,
    /// Why parts of this file's analysis are missing/degraded.
    pub notes: Vec<String>,
}

/// One epoch-tagged analysis result for a change-set.
#[derive(Debug, Clone, PartialEq)]
pub struct AnalysisSnapshot {
    /// The repo-state generation this snapshot was computed against.
    pub epoch: Epoch,
    /// Repo context at refresh time.
    pub repo_ctx: RepoContext,
    /// The change-set that was analysed.
    pub changeset: ChangeSet,
    /// Per-file artifacts, in change-set order.
    pub files: Vec<FileAnalysis>,
    /// All changed symbols across files, in change-set order.
    pub changed: Vec<ChangedSymbolInfo>,
    /// The 1-hop impact graph with its honesty metadata.
    pub graph: Evidence<ImpactGraph>,
    /// Diagnostics for the analysed files (from the push cache), post annotation.
    pub diagnostics: Vec<Diagnostic>,
}

impl AnalysisSnapshot {
    /// The plain [`ChangedSymbol`] records (domain view of [`AnalysisSnapshot::changed`]).
    #[must_use]
    pub fn changed_records(&self) -> Vec<ChangedSymbol> {
        self.changed.iter().map(|c| c.record.clone()).collect()
    }

    /// Build the 5-tier AI digest for this snapshot (apply
    /// [`ChangeDigest::truncate_to_budget`] before prompting).
    #[must_use]
    pub fn digest(&self) -> ChangeDigest {
        change_digest(
            &self.changed,
            &self.changeset,
            &self.graph,
            &self.diagnostics,
            &self.repo_ctx,
        )
    }
}

/// The analysis orchestrator: a semantic source (language service) plus a git repo.
#[derive(Debug)]
pub struct AnalysisEngine<S> {
    svc: S,
    repo: GitRepo,
}

impl<S: SemanticSource> AnalysisEngine<S> {
    /// Create an engine over a semantic source and a discovered repository.
    #[must_use]
    pub fn new(svc: S, repo: GitRepo) -> Self {
        AnalysisEngine { svc, repo }
    }

    /// The semantic source.
    #[must_use]
    pub fn svc(&self) -> &S {
        &self.svc
    }

    /// Consume the engine and return the semantic service (for graceful shutdown).
    pub fn into_service(self) -> S {
        self.svc
    }

    /// The git repository.
    #[must_use]
    pub fn repo(&self) -> &GitRepo {
        &self.repo
    }

    /// Run the full analysis pipeline for `changeset`, tagging results with `epoch`.
    ///
    /// Hard errors are git-level only (context queries); per-file semantic failures
    /// degrade to notes on the affected [`FileAnalysis`].
    #[tracing::instrument(
        skip(self, changeset),
        fields(scope = ?changeset.scope, files = changeset.len(), %epoch),
        err
    )]
    pub async fn refresh(
        &self,
        changeset: &ChangeSet,
        epoch: Epoch,
    ) -> Result<AnalysisSnapshot, AnalysisError> {
        let repo_ctx = self.repo.repo_context().await?;
        let base_spec = base_revspec(changeset.scope, &repo_ctx);

        let mut files = Vec::with_capacity(changeset.len());
        let mut changed = Vec::new();
        let mut diagnostics = Vec::new();

        for fc in &changeset.files {
            let analysis = self.analyse_file(fc, base_spec.as_deref()).await?;
            if analysis.worktree.is_some() {
                diagnostics.extend(self.svc.diagnostics(&analysis.file));
            }
            changed.extend(changed_symbols_detailed(
                analysis.worktree.as_ref(),
                analysis.base.as_ref(),
                fc,
            ));
            files.push(analysis);
        }

        let mut graph = build_impact_graph(&changed, &self.svc).await;
        annotate_diagnostics(&mut graph.value, &diagnostics);

        tracing::info!(
            files = files.len(),
            changed = changed.len(),
            graph_nodes = graph.value.node_count(),
            graph_edges = graph.value.edge_count(),
            "analysis refresh complete"
        );
        Ok(AnalysisSnapshot {
            epoch,
            repo_ctx,
            changeset: changeset.clone(),
            files,
            changed,
            graph,
            diagnostics,
        })
    }

    /// Fetch trees and compute mappings for one file; semantic failures become notes.
    async fn analyse_file(
        &self,
        fc: &FileChange,
        base_spec: Option<&str>,
    ) -> Result<FileAnalysis, AnalysisError> {
        let file = FileId::new(fc.path.clone())?;
        let mut notes = Vec::new();

        if fc.binary || matches!(fc.status, FileStatus::Gitlink | FileStatus::Unmerged) {
            notes.push(format!("skipped semantic analysis ({:?}/binary)", fc.status));
            return Ok(FileAnalysis {
                file,
                status: fc.status,
                worktree: None,
                base: None,
                mappings: Vec::new(),
                notes,
            });
        }

        // Language-ownership routing: don't send files the language service doesn't own
        // (e.g. README.md, go.sum, YAML) to gopls as `languageId: "go"`.
        if !self.svc.handles(&file) {
            notes.push("not owned by the language service; git-only".to_string());
            return Ok(FileAnalysis {
                file,
                status: fc.status,
                worktree: None,
                base: None,
                mappings: Vec::new(),
                notes,
            });
        }

        // Worktree tree (the language server reads from disk).
        let worktree = if fc.status == FileStatus::Deleted {
            None
        } else {
            match self.svc.document_symbols(&file).await {
                Ok(ev) => {
                    for note in &ev.notes {
                        notes.push(format!("worktree symbols: {note}"));
                    }
                    Some(ev.value)
                }
                Err(err) => {
                    tracing::warn!(%file, error = %err, "worktree document symbols failed");
                    notes.push(format!("worktree symbols unavailable: {err}"));
                    None
                }
            }
        };

        // Base overlay tree (research 03: pure deletions + symbol add/remove detection).
        let base = match base_spec {
            Some(spec) if needs_base_tree(fc) => self.base_tree(fc, spec, &mut notes).await,
            Some(_) => None,
            None => {
                if needs_base_tree(fc) {
                    notes.push("no base revision available for base-tree overlay".to_string());
                }
                None
            }
        };

        let mappings = file_mappings(worktree.as_ref(), base.as_ref(), fc);
        Ok(FileAnalysis {
            file,
            status: fc.status,
            worktree,
            base,
            mappings,
            notes,
        })
    }

    /// Base-revision overlay tree for `fc` (uses the pre-rename path when present).
    async fn base_tree(
        &self,
        fc: &FileChange,
        base_spec: &str,
        notes: &mut Vec<String>,
    ) -> Option<SymbolTree> {
        let base_path = fc.old_path.as_deref().unwrap_or(&fc.path);
        let content = match self.repo.base_file_content(base_spec, base_path).await {
            Ok(Some(content)) => content,
            Ok(None) => {
                notes.push(format!("{base_path} does not exist at {base_spec}"));
                return None;
            }
            Err(err) => {
                tracing::warn!(file = %base_path, error = %err, "base content unavailable");
                notes.push(format!("base content unavailable: {err}"));
                return None;
            }
        };
        let base_file = FileId::new_unchecked(base_path.to_path_buf());
        match self.svc.base_document_symbols(&base_file, &content).await {
            Ok(ev) => {
                for note in &ev.notes {
                    notes.push(format!("base symbols: {note}"));
                }
                Some(ev.value)
            }
            Err(err) => {
                tracing::warn!(file = %base_file, error = %err, "base document symbols failed");
                notes.push(format!("base symbols unavailable: {err}"));
                None
            }
        }
    }
}

/// The revision whose content is the "old side" of hunks for `scope` (research 02):
/// merge-base for `Branch`, `HEAD` for `Staged`, the index (`:0`) for `Unstaged`.
fn base_revspec(scope: ChangeScope, repo_ctx: &RepoContext) -> Option<String> {
    match scope {
        ChangeScope::Branch => repo_ctx
            .base
            .as_ref()
            .map(|b| b.merge_base.as_str().to_string()),
        ChangeScope::Staged => match repo_ctx.head {
            HeadState::Unborn => None,
            _ => Some("HEAD".to_string()),
        },
        ChangeScope::Unstaged => Some(":0".to_string()),
    }
}

/// Whether analysis wants a base overlay tree for this file (deletion mapping and
/// symbol add/remove detection). Added/untracked/copied files have no base version.
fn needs_base_tree(fc: &FileChange) -> bool {
    match fc.status {
        FileStatus::Modified
        | FileStatus::Deleted
        | FileStatus::Renamed { .. }
        | FileStatus::TypeChanged => true,
        FileStatus::Added
        | FileStatus::Copied { .. }
        | FileStatus::Untracked
        | FileStatus::Unmerged
        | FileStatus::Gitlink => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{Reply, ScriptedSource};
    use camino::Utf8PathBuf;
    use codescope_core::{
        Availability, ChangeKind, Feature, FeatureSet, LineRange, Position, Revision, SymbolId,
        SymbolKind, SymbolNode, SymbolRef,
    };
    use std::time::Duration;

    const MEMSTORE: &str = "internal/store/memstore.go";
    const HEALTH: &str = "internal/api/health.go";

    fn node(id: &str, name: &str, kind: SymbolKind, start: u32, end: u32) -> SymbolNode {
        SymbolNode {
            id: SymbolId::new(id),
            name: name.to_string(),
            detail: None,
            kind,
            range: LineRange::new(start, 0, end, 1),
            selection: LineRange::new(start, 5, start, 5 + name.len() as u32),
            children: Vec::new(),
        }
    }

    /// Symbol tree matching the fixture's *edited* memstore.go (nil-guard in Get).
    fn memstore_worktree() -> SymbolTree {
        SymbolTree::new(
            FileId::new(MEMSTORE).unwrap(),
            Revision::Worktree,
            vec![
                node("0", "MemoryRepo", SymbolKind::Struct, 3, 5),
                node("1", "NewMemoryRepo", SymbolKind::Function, 8, 10),
                node("2", "(MemoryRepo).Get", SymbolKind::Method, 13, 22),
                node("3", "(MemoryRepo).Save", SymbolKind::Method, 25, 28),
                node("4", "(MemoryRepo).Delete", SymbolKind::Method, 31, 34),
            ],
        )
    }

    /// Symbol tree matching the *index* content (pre-edit, no nil-guard).
    fn memstore_base() -> SymbolTree {
        SymbolTree::new(
            FileId::new(MEMSTORE).unwrap(),
            Revision::Base,
            vec![
                node("0", "MemoryRepo", SymbolKind::Struct, 3, 5),
                node("1", "NewMemoryRepo", SymbolKind::Function, 8, 10),
                node("2", "(MemoryRepo).Get", SymbolKind::Method, 13, 19),
                node("3", "(MemoryRepo).Save", SymbolKind::Method, 22, 25),
                node("4", "(MemoryRepo).Delete", SymbolKind::Method, 28, 31),
            ],
        )
    }

    fn scripted() -> ScriptedSource {
        let mut features = FeatureSet::new();
        features.set(Feature::CallHierarchyIncoming, Availability::Supported);
        features.set(Feature::CallHierarchyOutgoing, Availability::Supported);
        features.set(Feature::Implementation, Availability::Supported);
        let mut svc = ScriptedSource {
            features,
            ..ScriptedSource::default()
        };
        svc.trees
            .insert(FileId::new(MEMSTORE).unwrap(), memstore_worktree());
        svc.base_trees
            .insert(FileId::new(MEMSTORE).unwrap(), memstore_base());
        // Staged scope resolves the rename to the pre-rename path at HEAD.
        svc.base_trees.insert(
            FileId::new("internal/store/memory.go").unwrap(),
            memstore_base(),
        );
        // Callers of (MemoryRepo).Get (selection starts at 13:5).
        svc.incoming.insert(
            (FileId::new(MEMSTORE).unwrap(), Position::new(13, 5)),
            Reply::Ok(codescope_core::Evidence::complete(vec![SymbolRef {
                file: FileId::new("internal/service/service.go").unwrap(),
                name: "(Service).GetUser".to_string(),
                kind: SymbolKind::Method,
            }])),
        );
        svc
    }

    async fn fixture_engine() -> (tempfile::TempDir, AnalysisEngine<ScriptedSource>) {
        let tmp = tempfile::tempdir().unwrap();
        codescope_testutil::copy_fixture_into(tmp.path()).unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let repo = GitRepo::discover(&root).await.unwrap();
        (tmp, AnalysisEngine::new(scripted(), repo))
    }

    #[tokio::test]
    async fn refresh_unstaged_produces_epoch_tagged_snapshot() {
        let (_tmp, engine) = fixture_engine().await;
        let changeset = tokio::time::timeout(
            Duration::from_secs(30),
            engine.repo().changeset(ChangeScope::Unstaged),
        )
        .await
        .expect("changeset timed out")
        .unwrap();
        assert!(changeset.find_file(camino::Utf8Path::new(MEMSTORE)).is_some());
        assert!(changeset.find_file(camino::Utf8Path::new(HEALTH)).is_some());

        let snap = tokio::time::timeout(
            Duration::from_secs(30),
            engine.refresh(&changeset, Epoch(7)),
        )
        .await
        .expect("refresh timed out")
        .expect("refresh failed");

        assert_eq!(snap.epoch, Epoch(7));
        assert_eq!(snap.files.len(), changeset.len());

        // memstore.go: worktree + base (index overlay) trees, mapped hunks.
        let mem = snap.files.iter().find(|f| f.file.as_path() == MEMSTORE).unwrap();
        assert!(mem.worktree.is_some());
        assert!(mem.base.is_some(), "index overlay tree expected: {:?}", mem.notes);
        assert!(!mem.mappings.is_empty());

        // The nil-guard edit lands on (MemoryRepo).Get.
        let get = snap
            .changed
            .iter()
            .find(|c| c.name == "(MemoryRepo).Get")
            .expect("Get should be a changed symbol");
        assert_eq!(get.record.change_kind, ChangeKind::Modified);
        assert_eq!(get.revision, Revision::Worktree);

        // health.go is untracked and unscripted: degraded with a note, not fatal.
        let health = snap.files.iter().find(|f| f.file.as_path() == HEALTH).unwrap();
        assert!(health.worktree.is_none());
        assert!(!health.notes.is_empty());

        // Graph got the scripted caller edge and kept change annotations.
        let get_id = format!("{MEMSTORE}:(MemoryRepo).Get");
        assert!(snap.graph.value.contains_edge(
            "internal/service/service.go:(Service).GetUser",
            &get_id,
            codescope_core::RelationKind::Calls
        ));
        assert_eq!(
            snap.graph.value.node(&get_id).unwrap().change,
            Some(ChangeKind::Modified)
        );

        // Digest builds from the snapshot and mentions the changed symbol.
        let digest = snap.digest();
        assert!(digest.render().contains("(MemoryRepo).Get"));
        assert_eq!(digest.scope, ChangeScope::Unstaged);
    }

    #[tokio::test]
    async fn refresh_staged_uses_head_as_base() {
        let (_tmp, engine) = fixture_engine().await;
        let changeset = tokio::time::timeout(
            Duration::from_secs(30),
            engine.repo().changeset(ChangeScope::Staged),
        )
        .await
        .expect("changeset timed out")
        .unwrap();
        // Staged set: service.go edit + memory.go -> memstore.go rename.
        assert!(!changeset.is_empty());

        let snap = tokio::time::timeout(
            Duration::from_secs(30),
            engine.refresh(&changeset, Epoch(1)),
        )
        .await
        .expect("refresh timed out")
        .expect("refresh failed");
        assert_eq!(snap.epoch, Epoch(1));

        // The staged rename pairs memstore.go with its pre-rename path; the base
        // overlay is requested for old_path at HEAD (content exists), but the worktree
        // tree is scripted while service.go is not — both degrade to notes, never errors.
        let renamed = snap
            .files
            .iter()
            .find(|f| f.file.as_path() == MEMSTORE)
            .expect("renamed file analysed");
        assert!(matches!(renamed.status, FileStatus::Renamed { .. }));
        assert!(renamed.base.is_some(), "base tree via old path: {:?}", renamed.notes);
    }

    #[test]
    fn base_revspec_per_scope() {
        let ctx = |base: Option<codescope_core::BaseInfo>, head: HeadState| RepoContext {
            toplevel: Utf8PathBuf::from("/repo"),
            head,
            upstream: None,
            base,
        };
        let base_info = codescope_core::BaseInfo {
            source: codescope_core::BaseSource::Upstream,
            ref_name: "origin/main".to_string(),
            merge_base: codescope_core::Oid::new("cafe12"),
        };
        assert_eq!(
            base_revspec(
                ChangeScope::Branch,
                &ctx(Some(base_info.clone()), HeadState::Branch("x".into()))
            ),
            Some("cafe12".to_string())
        );
        assert_eq!(
            base_revspec(ChangeScope::Branch, &ctx(None, HeadState::Branch("x".into()))),
            None
        );
        assert_eq!(
            base_revspec(ChangeScope::Staged, &ctx(None, HeadState::Branch("x".into()))),
            Some("HEAD".to_string())
        );
        assert_eq!(base_revspec(ChangeScope::Staged, &ctx(None, HeadState::Unborn)), None);
        assert_eq!(
            base_revspec(ChangeScope::Unstaged, &ctx(None, HeadState::Unborn)),
            Some(":0".to_string())
        );
    }

    #[test]
    fn needs_base_tree_matrix() {
        let fc = |status: FileStatus| FileChange {
            path: Utf8PathBuf::from("a.go"),
            old_path: None,
            status,
            hunks: vec![],
            binary: false,
        };
        assert!(needs_base_tree(&fc(FileStatus::Modified)));
        assert!(needs_base_tree(&fc(FileStatus::Deleted)));
        assert!(needs_base_tree(&fc(FileStatus::Renamed { score: 90 })));
        assert!(needs_base_tree(&fc(FileStatus::TypeChanged)));
        assert!(!needs_base_tree(&fc(FileStatus::Added)));
        assert!(!needs_base_tree(&fc(FileStatus::Untracked)));
        assert!(!needs_base_tree(&fc(FileStatus::Copied { score: 90 })));
        assert!(!needs_base_tree(&fc(FileStatus::Gitlink)));
    }
}
