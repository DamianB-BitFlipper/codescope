//! The compact AI prompt payload: a 5-tier change digest (research 05 §4).
//!
//! Assembled in priority order — changed symbols > diagnostics > hunk summaries > 1-hop
//! relationships > repo sketch — with per-tier caps at build time and token-budget-aware
//! truncation ([`ChangeDigest::truncate_to_budget`]) that removes low-priority content
//! first and **never drops tiers 1–2**.
//!
//! Token counts use the standard ~4-chars-per-token heuristic ([`estimate_tokens`]); the
//! digest is honest about what it cut via [`ChangeDigest::notes`].

use std::collections::BTreeMap;

use codescope_core::{
    ChangeKind, ChangeScope, ChangeSet, Completeness, Diagnostic, DiagnosticSeverity, Evidence,
    FileId, ImpactGraph, LineRange, MappingConfidence, RelationKind, RepoContext, SymbolKind,
};

use crate::changes::ChangedSymbolInfo;

/// Tier-1 cap: changed symbols (research 05 §4).
pub const MAX_DIGEST_SYMBOLS: usize = 50;
/// Tier-2 cap: diagnostics touching changed ranges.
pub const MAX_DIGEST_DIAGNOSTICS: usize = 30;
/// Tier-3 cap: hunk summaries.
pub const MAX_DIGEST_HUNKS: usize = 40;
/// Tier-4 cap: 1-hop relationship lines.
pub const MAX_DIGEST_RELATIONS: usize = 100;
/// Maximum diagnostic message length carried into the digest.
pub const MAX_DIAGNOSTIC_MESSAGE_CHARS: usize = 160;
/// Default token budget for the rendered digest (~4–8k target).
pub const DIGEST_DEFAULT_TOKEN_BUDGET: usize = 8_000;
/// Hard token cap; budgets above this are clamped.
pub const DIGEST_HARD_TOKEN_CAP: usize = 12_000;

/// Estimate the token count of `text` (~4 characters per token, rounded up).
#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// Tier 1: one changed symbol.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DigestSymbol {
    /// Repo-relative file.
    pub file: FileId,
    /// Qualified symbol name (`Greeter.Name`, `(Greeter).Hello`).
    pub name: String,
    /// Symbol kind.
    pub kind: SymbolKind,
    /// Added / modified / deleted.
    pub change_kind: ChangeKind,
    /// Mapping confidence (rendered as ``~``/``?`` markers).
    pub confidence: MappingConfidence,
    /// `true` when a hunk touched the symbol's selection (signature-ish change).
    pub signature_touch: bool,
    /// Symbol detail (signature) when the language server supplied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Diagnostics whose range intersects the symbol's extent (worktree symbols only).
    pub diagnostic_count: usize,
}

/// Tier 2: one diagnostic touching a changed range.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DigestDiagnostic {
    /// Repo-relative file.
    pub file: FileId,
    /// Zero-based start line of the diagnostic range.
    pub line: u32,
    /// Severity.
    pub severity: DiagnosticSeverity,
    /// Diagnostic code, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Message, truncated to [`MAX_DIAGNOSTIC_MESSAGE_CHARS`].
    pub message: String,
}

/// Tier 3: one hunk summary (header + counts + short previews, never full bodies).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DigestHunk {
    /// Repo-relative file.
    pub file: FileId,
    /// Zero-based hunk index within the file's diff.
    pub index: u32,
    /// Reconstructed `@@ -a,b +c,d @@ section` header.
    pub header: String,
    /// Count of `+` lines.
    pub added: usize,
    /// Count of `-` lines.
    pub deleted: usize,
    /// First (≤2) old-side (`-`) lines.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub old_preview: Vec<String>,
    /// First (≤2) new-side (`+`) lines.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub new_preview: Vec<String>,
}

/// Tier 4: one 1-hop relationship line (name-only endpoints).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DigestRelation {
    /// Source node id (`file:fq_name`).
    pub from: String,
    /// Relation kind.
    pub kind: RelationKind,
    /// Target node id (`file:fq_name`).
    pub to: String,
    /// `true` when the source node is itself a changed symbol.
    pub from_changed: bool,
    /// `true` when the target node is itself a changed symbol.
    pub to_changed: bool,
}

/// Tier 5: shallow repo sketch (top-level dirs of changed files).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RepoSketch {
    /// HEAD description (branch name / detached sha / unborn).
    pub head: String,
    /// Base ref the branch scope compares against, when inferred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
    /// `(top-level dir, changed-file count)` in path order; `.` for repo-root files.
    #[serde(default)]
    pub dirs: Vec<(String, usize)>,
}

/// The 5-tier compact change description sent to the AI (research 05 §4).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChangeDigest {
    /// Which change scope this digest describes.
    pub scope: ChangeScope,
    /// Tier 1 (required, never dropped): changed symbols.
    pub changed_symbols: Vec<DigestSymbol>,
    /// Tier 2 (never dropped): diagnostics touching changed ranges, errors first.
    pub diagnostics: Vec<DigestDiagnostic>,
    /// Tier 3: hunk summaries.
    pub hunks: Vec<DigestHunk>,
    /// Tier 4: 1-hop relationships.
    pub relations: Vec<DigestRelation>,
    /// Tier 5: repo sketch.
    pub repo: RepoSketch,
    /// Caveats: build-time cap truncations, impact-graph evidence notes, budget cuts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Build a [`ChangeDigest`] from analysis results (applies the per-tier caps).
///
/// `changed` supplies tier 1 in the given order; `changeset` supplies tier 3;
/// `graph` (with its [`Evidence`] honesty metadata) supplies tier 4; `diagnostics` are
/// filtered to those touching changed ranges for tier 2.
#[must_use]
pub fn change_digest(
    changed: &[ChangedSymbolInfo],
    changeset: &ChangeSet,
    graph: &Evidence<ImpactGraph>,
    diagnostics: &[Diagnostic],
    repo_ctx: &RepoContext,
) -> ChangeDigest {
    let mut notes = Vec::new();

    let changed_symbols = build_symbols(changed, diagnostics, &mut notes);
    let digest_diagnostics = build_diagnostics(changed, changeset, diagnostics, &mut notes);
    let hunks = build_hunks(changeset, &mut notes);
    let relations = build_relations(graph, &mut notes);
    let repo = build_sketch(changeset, repo_ctx);

    let digest = ChangeDigest {
        scope: changeset.scope,
        changed_symbols,
        diagnostics: digest_diagnostics,
        hunks,
        relations,
        repo,
        notes,
    };
    tracing::debug!(
        symbols = digest.changed_symbols.len(),
        diagnostics = digest.diagnostics.len(),
        hunks = digest.hunks.len(),
        relations = digest.relations.len(),
        est_tokens = digest.estimated_tokens(),
        "built change digest"
    );
    digest
}

fn build_symbols(
    changed: &[ChangedSymbolInfo],
    diagnostics: &[Diagnostic],
    notes: &mut Vec<String>,
) -> Vec<DigestSymbol> {
    if changed.len() > MAX_DIGEST_SYMBOLS {
        notes.push(format!(
            "changed symbols truncated: {MAX_DIGEST_SYMBOLS} of {}",
            changed.len()
        ));
    }
    changed
        .iter()
        .take(MAX_DIGEST_SYMBOLS)
        .map(|info| {
            let diagnostic_count = if info.revision == codescope_core::Revision::Base {
                0 // deleted symbols: base-revision ranges never match live diagnostics
            } else {
                diagnostics
                    .iter()
                    .filter(|d| d.file == info.file && d.range.intersects_lines(&info.range))
                    .count()
            };
            DigestSymbol {
                file: info.file.clone(),
                name: info.name.clone(),
                kind: info.kind,
                change_kind: info.record.change_kind,
                confidence: info.record.confidence,
                signature_touch: info.signature_touch,
                detail: info.detail.clone(),
                diagnostic_count,
            }
        })
        .collect()
}

fn build_diagnostics(
    changed: &[ChangedSymbolInfo],
    changeset: &ChangeSet,
    diagnostics: &[Diagnostic],
    notes: &mut Vec<String>,
) -> Vec<DigestDiagnostic> {
    // Changed ranges per file: worktree symbol extents + new-side hunk spans.
    let mut ranges: BTreeMap<&FileId, Vec<LineRange>> = BTreeMap::new();
    for info in changed {
        if info.revision != codescope_core::Revision::Base {
            ranges.entry(&info.file).or_default().push(info.range);
        }
    }
    let file_ids: Vec<(FileId, Vec<LineRange>)> = changeset
        .files
        .iter()
        .map(|fc| {
            let id = FileId::new_unchecked(fc.path.clone());
            let spans = fc
                .hunks
                .iter()
                .filter(|h| h.new_len > 0)
                .map(|h| {
                    let start = h.new_start.saturating_sub(1);
                    LineRange::from_line_span(start, start + h.new_len.saturating_sub(1))
                })
                .collect::<Vec<_>>();
            (id, spans)
        })
        .collect();

    let touches_changed = |d: &Diagnostic| -> bool {
        if let Some(spans) = ranges.get(&d.file) {
            if spans.iter().any(|r| r.intersects_lines(&d.range)) {
                return true;
            }
        }
        file_ids
            .iter()
            .any(|(id, spans)| *id == d.file && spans.iter().any(|r| r.intersects_lines(&d.range)))
    };

    let mut touching: Vec<&Diagnostic> =
        diagnostics.iter().filter(|d| touches_changed(d)).collect();
    // Errors first (severity derives Ord with Error as the least value), then file/line.
    touching.sort_by_key(|d| (d.severity, d.file.clone(), d.range.start_line));
    if touching.len() > MAX_DIGEST_DIAGNOSTICS {
        notes.push(format!(
            "diagnostics truncated: {MAX_DIGEST_DIAGNOSTICS} of {}",
            touching.len()
        ));
    }
    touching
        .into_iter()
        .take(MAX_DIGEST_DIAGNOSTICS)
        .map(|d| DigestDiagnostic {
            file: d.file.clone(),
            line: d.range.start_line,
            severity: d.severity,
            code: d.code.clone(),
            message: truncate_chars(&d.message, MAX_DIAGNOSTIC_MESSAGE_CHARS),
        })
        .collect()
}

fn build_hunks(changeset: &ChangeSet, notes: &mut Vec<String>) -> Vec<DigestHunk> {
    let total: usize = changeset.files.iter().map(|f| f.hunks.len()).sum();
    if total > MAX_DIGEST_HUNKS {
        notes.push(format!("hunks truncated: {MAX_DIGEST_HUNKS} of {total}"));
    }
    let mut out = Vec::new();
    'files: for fc in &changeset.files {
        for (index, hunk) in fc.hunks.iter().enumerate() {
            if out.len() >= MAX_DIGEST_HUNKS {
                break 'files;
            }
            let section = hunk
                .section
                .as_deref()
                .map(|s| format!(" {s}"))
                .unwrap_or_default();
            let old_preview: Vec<String> = hunk
                .lines
                .iter()
                .filter(|l| l.kind == codescope_core::DiffLineKind::Del)
                .take(2)
                .map(|l| l.text.clone())
                .collect();
            let new_preview: Vec<String> = hunk
                .lines
                .iter()
                .filter(|l| l.kind == codescope_core::DiffLineKind::Add)
                .take(2)
                .map(|l| l.text.clone())
                .collect();
            out.push(DigestHunk {
                file: FileId::new_unchecked(fc.path.clone()),
                index: index as u32,
                header: format!(
                    "@@ -{},{} +{},{} @@{section}",
                    hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
                ),
                added: hunk.count_added(),
                deleted: hunk.count_deleted(),
                old_preview,
                new_preview,
            });
        }
    }
    out
}

fn build_relations(graph: &Evidence<ImpactGraph>, notes: &mut Vec<String>) -> Vec<DigestRelation> {
    if graph.completeness != Completeness::Complete {
        notes.push(format!(
            "impact graph completeness: {:?}",
            graph.completeness
        ));
    }
    for note in &graph.notes {
        notes.push(format!("impact graph: {note}"));
    }
    let g = &graph.value;
    if g.edges.len() > MAX_DIGEST_RELATIONS {
        notes.push(format!(
            "relations truncated: {MAX_DIGEST_RELATIONS} of {}",
            g.edges.len()
        ));
    }
    let changed = |id: &str| g.node(id).is_some_and(|n| n.change.is_some());
    g.edges
        .iter()
        .take(MAX_DIGEST_RELATIONS)
        .map(|e| DigestRelation {
            from: e.from.clone(),
            kind: e.kind,
            to: e.to.clone(),
            from_changed: changed(&e.from),
            to_changed: changed(&e.to),
        })
        .collect()
}

fn build_sketch(changeset: &ChangeSet, repo_ctx: &RepoContext) -> RepoSketch {
    let mut dirs: BTreeMap<String, usize> = BTreeMap::new();
    for fc in &changeset.files {
        let top = fc
            .path
            .components()
            .next()
            .map(|c| c.as_str().to_string())
            .filter(|_| fc.path.parent().is_some_and(|p| !p.as_str().is_empty()))
            .unwrap_or_else(|| ".".to_string());
        *dirs.entry(top).or_insert(0) += 1;
    }
    RepoSketch {
        head: repo_ctx.head.to_string(),
        base_ref: repo_ctx.base.as_ref().map(|b| b.ref_name.clone()),
        dirs: dirs.into_iter().collect(),
    }
}

impl ChangeDigest {
    /// Render the digest as compact, deterministic prompt text.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(4096);
        out.push_str("# change digest\n");
        out.push_str(&format!(
            "repo: head={} base={} scope={:?} dirs:",
            self.repo.head,
            self.repo.base_ref.as_deref().unwrap_or("(none)"),
            self.scope,
        ));
        if self.repo.dirs.is_empty() {
            out.push_str(" (none)");
        }
        out.push('\n');
        for (dir, count) in &self.repo.dirs {
            out.push_str(&format!("- {dir} ({count} changed files)\n"));
        }

        out.push_str(&format!(
            "## changed symbols ({})\n",
            self.changed_symbols.len()
        ));
        for s in &self.changed_symbols {
            let marker = match s.confidence {
                MappingConfidence::Exact => "",
                MappingConfidence::Approximate(_) => "~",
                MappingConfidence::Unmapped => "?",
            };
            let sig = if s.signature_touch {
                " [signature]"
            } else {
                ""
            };
            let diag = if s.diagnostic_count > 0 {
                format!(" ({} diagnostics)", s.diagnostic_count)
            } else {
                String::new()
            };
            let detail = s
                .detail
                .as_deref()
                .map(|d| format!(" `{d}`"))
                .unwrap_or_default();
            out.push_str(&format!(
                "- {marker}{:?} {:?} {}:{}{sig}{detail}{diag}\n",
                s.change_kind, s.kind, s.file, s.name
            ));
        }

        out.push_str(&format!("## diagnostics ({})\n", self.diagnostics.len()));
        for d in &self.diagnostics {
            let code = d
                .code
                .as_deref()
                .map(|c| format!(" [{c}]"))
                .unwrap_or_default();
            out.push_str(&format!(
                "- {:?} {}:{}{code} {}\n",
                d.severity, d.file, d.line, d.message
            ));
        }

        out.push_str(&format!("## hunks ({})\n", self.hunks.len()));
        for h in &self.hunks {
            out.push_str(&format!(
                "- {}#h{} {} (+{}/-{})\n",
                h.file, h.index, h.header, h.added, h.deleted
            ));
            for l in &h.old_preview {
                out.push_str(&format!("  -| {l}\n"));
            }
            for l in &h.new_preview {
                out.push_str(&format!("  +| {l}\n"));
            }
        }

        out.push_str(&format!("## relations ({})\n", self.relations.len()));
        for r in &self.relations {
            let fc = if r.from_changed { " (changed)" } else { "" };
            let tc = if r.to_changed { " (changed)" } else { "" };
            out.push_str(&format!("- {}{fc} -{:?}-> {}{tc}\n", r.from, r.kind, r.to));
        }

        if !self.notes.is_empty() {
            out.push_str("## notes\n");
            for n in &self.notes {
                out.push_str(&format!("- {n}\n"));
            }
        }
        out
    }

    /// Estimated token count of [`ChangeDigest::render`] output.
    #[must_use]
    pub fn estimated_tokens(&self) -> usize {
        estimate_tokens(&self.render())
    }

    /// Trim the digest until it fits `budget_tokens` (clamped to
    /// [`DIGEST_HARD_TOKEN_CAP`]): repo sketch dirs first, then relations, then hunks —
    /// bottom of each tier first. Tiers 1–2 are never dropped (research 05 §4); if they
    /// alone exceed the budget a note records the overflow.
    pub fn truncate_to_budget(&mut self, budget_tokens: usize) {
        let budget = budget_tokens.min(DIGEST_HARD_TOKEN_CAP);
        if self.estimated_tokens() <= budget {
            return;
        }
        // Measure the *final* state each round: cut notes are part of the rendered text,
        // so they are written before measuring and rewritten as counts grow.
        let base_notes = self.notes.len();
        let mut cut = [0usize; 3];
        loop {
            self.notes.truncate(base_notes);
            for (n, tier) in [
                (cut[0], "repo sketch dirs"),
                (cut[1], "relations"),
                (cut[2], "hunks"),
            ] {
                if n > 0 {
                    self.notes.push(format!("budget cut {n} {tier}"));
                }
            }
            if self.estimated_tokens() <= budget {
                break;
            }
            if self.repo.dirs.pop().is_some() {
                cut[0] += 1;
            } else if self.relations.pop().is_some() {
                cut[1] += 1;
            } else if self.hunks.pop().is_some() {
                cut[2] += 1;
            } else {
                // Tiers 1–2 (plus fixed headers) alone exceed the budget: keep them.
                self.notes.push(format!(
                    "digest exceeds budget of {budget} tokens even after truncation"
                ));
                break;
            }
        }
        tracing::debug!(
            budget,
            est = self.estimated_tokens(),
            "truncated digest to budget"
        );
    }
}

/// Truncate to at most `max` characters, appending `…` when cut.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use codescope_core::{
        BaseInfo, BaseSource, ChangedSymbol, DiffLine, EntityRef, FileChange, FileStatus,
        HeadState, Hunk, ImpactEdge, ImpactNode, Oid, Revision, SymbolId,
    };

    fn info(file: &str, name: &str, change: ChangeKind, start: u32, end: u32) -> ChangedSymbolInfo {
        ChangedSymbolInfo {
            file: FileId::new(file).unwrap(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            detail: Some(format!("func {name}()")),
            range: LineRange::new(start, 0, end, 1),
            selection: LineRange::new(start, 5, start, 8),
            revision: Revision::Worktree,
            record: ChangedSymbol::new(
                SymbolId::new("0"),
                change,
                vec![],
                MappingConfidence::Exact,
            ),
            signature_touch: false,
        }
    }

    fn changeset() -> ChangeSet {
        ChangeSet::new(
            ChangeScope::Unstaged,
            vec![FileChange {
                path: Utf8PathBuf::from("pkg/main.go"),
                old_path: None,
                status: FileStatus::Modified,
                hunks: vec![Hunk {
                    old_start: 10,
                    old_len: 2,
                    new_start: 10,
                    new_len: 3,
                    section: Some("func main()".to_string()),
                    lines: vec![
                        DiffLine::context(10, 10, "ctx"),
                        DiffLine::del(11, "old line"),
                        DiffLine::add(11, "new line one"),
                        DiffLine::add(12, "new line two"),
                    ],
                }],
                binary: false,
            }],
        )
    }

    fn repo_ctx() -> RepoContext {
        RepoContext {
            toplevel: Utf8PathBuf::from("/repo"),
            head: HeadState::Branch("feature".to_string()),
            upstream: None,
            base: Some(BaseInfo {
                source: BaseSource::Upstream,
                ref_name: "origin/main".to_string(),
                merge_base: Oid::new("abc123"),
            }),
        }
    }

    fn graph() -> Evidence<ImpactGraph> {
        let mut g = ImpactGraph::new();
        g.add_node(ImpactNode {
            id: "pkg/main.go:main".to_string(),
            entity: EntityRef::for_symbol(FileId::new("pkg/main.go").unwrap(), "main", None),
            change: Some(ChangeKind::Modified),
            diagnostic_severity: None,
        });
        g.add_node(ImpactNode {
            id: "pkg/greet.go:Hello".to_string(),
            entity: EntityRef::for_symbol(FileId::new("pkg/greet.go").unwrap(), "Hello", None),
            change: None,
            diagnostic_severity: None,
        });
        g.add_edge(ImpactEdge {
            from: "pkg/greet.go:Hello".to_string(),
            to: "pkg/main.go:main".to_string(),
            kind: RelationKind::Calls,
        });
        Evidence::partial(
            g,
            vec!["call hierarchy timed out for one symbol".to_string()],
        )
    }

    fn diag(file: &str, line: u32, severity: DiagnosticSeverity, message: &str) -> Diagnostic {
        Diagnostic {
            file: FileId::new(file).unwrap(),
            range: LineRange::new(line, 0, line, 5),
            severity,
            code: Some("E100".to_string()),
            message: message.to_string(),
            source: Some("compiler".to_string()),
        }
    }

    #[test]
    fn digest_assembles_all_five_tiers() {
        let changed = vec![info("pkg/main.go", "main", ChangeKind::Modified, 8, 20)];
        let diagnostics = vec![
            diag(
                "pkg/main.go",
                11,
                DiagnosticSeverity::Warning,
                "shadowed var",
            ),
            diag("pkg/main.go", 9, DiagnosticSeverity::Error, "type mismatch"),
            diag(
                "pkg/other.go",
                3,
                DiagnosticSeverity::Error,
                "unrelated file",
            ),
        ];
        let d = change_digest(&changed, &changeset(), &graph(), &diagnostics, &repo_ctx());

        // Tier 1: symbol with diagnostic count (two diagnostics intersect main's 8-20 range).
        assert_eq!(d.changed_symbols.len(), 1);
        assert_eq!(d.changed_symbols[0].diagnostic_count, 2);
        assert_eq!(d.changed_symbols[0].name, "main");

        // Tier 2: only diagnostics touching changed ranges, errors first.
        assert_eq!(d.diagnostics.len(), 2);
        assert_eq!(d.diagnostics[0].severity, DiagnosticSeverity::Error);
        assert_eq!(d.diagnostics[0].line, 9);

        // Tier 3: hunk summary with previews and counts.
        assert_eq!(d.hunks.len(), 1);
        let h = &d.hunks[0];
        assert_eq!(h.header, "@@ -10,2 +10,3 @@ func main()");
        assert_eq!((h.added, h.deleted), (2, 1));
        assert_eq!(h.old_preview, vec!["old line"]);
        assert_eq!(h.new_preview, vec!["new line one", "new line two"]);

        // Tier 4: relation with changed-endpoint annotation.
        assert_eq!(d.relations.len(), 1);
        assert!(d.relations[0].to_changed);
        assert!(!d.relations[0].from_changed);

        // Tier 5: repo sketch.
        assert_eq!(d.repo.head, "feature");
        assert_eq!(d.repo.base_ref.as_deref(), Some("origin/main"));
        assert_eq!(d.repo.dirs, vec![("pkg".to_string(), 1)]);

        // Evidence notes surface.
        assert!(d.notes.iter().any(|n| n.contains("Partial")));
        assert!(d.notes.iter().any(|n| n.contains("timed out")));

        // Render mentions the key facts.
        let text = d.render();
        assert!(text.contains("pkg/main.go:main"));
        assert!(text.contains("@@ -10,2 +10,3 @@"));
        assert!(text.contains("-Calls->"));
    }

    #[test]
    fn diagnostic_messages_are_truncated() {
        let long = "x".repeat(500);
        let changed = vec![info("pkg/main.go", "main", ChangeKind::Modified, 8, 20)];
        let d = change_digest(
            &changed,
            &changeset(),
            &Evidence::complete(ImpactGraph::new()),
            &[diag("pkg/main.go", 10, DiagnosticSeverity::Error, &long)],
            &repo_ctx(),
        );
        assert_eq!(
            d.diagnostics[0].message.chars().count(),
            MAX_DIAGNOSTIC_MESSAGE_CHARS
        );
        assert!(d.diagnostics[0].message.ends_with('…'));
    }

    #[test]
    fn caps_apply_and_are_noted() {
        let changed: Vec<ChangedSymbolInfo> = (0..60)
            .map(|i| info("pkg/main.go", &format!("f{i}"), ChangeKind::Modified, i, i))
            .collect();
        let mut g = ImpactGraph::new();
        for i in 0..120 {
            g.add_edge(ImpactEdge {
                from: format!("a{i}"),
                to: format!("b{i}"),
                kind: RelationKind::Calls,
            });
        }
        let d = change_digest(
            &changed,
            &changeset(),
            &Evidence::complete(g),
            &[],
            &repo_ctx(),
        );
        assert_eq!(d.changed_symbols.len(), MAX_DIGEST_SYMBOLS);
        assert_eq!(d.relations.len(), MAX_DIGEST_RELATIONS);
        assert!(d
            .notes
            .iter()
            .any(|n| n.contains("changed symbols truncated: 50 of 60")));
        assert!(d
            .notes
            .iter()
            .any(|n| n.contains("relations truncated: 100 of 120")));
    }

    #[test]
    fn budget_truncation_drops_low_tiers_first_and_keeps_tiers_1_and_2() {
        let changed = vec![info("pkg/main.go", "main", ChangeKind::Modified, 8, 20)];
        let mut g = ImpactGraph::new();
        for i in 0..50 {
            g.add_edge(ImpactEdge {
                from: format!("pkg/caller_with_a_long_name_{i}.go:CallerFunctionNumber{i}"),
                to: "pkg/main.go:main".to_string(),
                kind: RelationKind::Calls,
            });
        }
        let diagnostics = vec![diag("pkg/main.go", 10, DiagnosticSeverity::Error, "broken")];
        let mut d = change_digest(
            &changed,
            &changeset(),
            &Evidence::complete(g),
            &diagnostics,
            &repo_ctx(),
        );
        let before = d.estimated_tokens();
        assert!(before > 200);

        d.truncate_to_budget(200);
        assert!(d.estimated_tokens() <= 200);
        // Tiers 1–2 survive.
        assert_eq!(d.changed_symbols.len(), 1);
        assert_eq!(d.diagnostics.len(), 1);
        // Low-priority tiers shrank, and the cuts are recorded.
        assert!(d.relations.len() < 50);
        assert!(d.repo.dirs.is_empty());
        assert!(d.notes.iter().any(|n| n.contains("budget cut")));
    }

    #[test]
    fn budget_truncation_is_noop_when_within_budget() {
        let changed = vec![info("pkg/main.go", "main", ChangeKind::Modified, 8, 20)];
        let mut d = change_digest(
            &changed,
            &changeset(),
            &Evidence::complete(ImpactGraph::new()),
            &[],
            &repo_ctx(),
        );
        let before = d.clone();
        d.truncate_to_budget(DIGEST_DEFAULT_TOKEN_BUDGET);
        assert_eq!(d, before);
    }

    #[test]
    fn budget_overflow_of_protected_tiers_is_noted() {
        // 50 symbols with long names: tiers 1–2 alone exceed a tiny budget.
        let changed: Vec<ChangedSymbolInfo> = (0..50)
            .map(|i| {
                info(
                    "pkg/main.go",
                    &format!("a_rather_long_function_name_number_{i}"),
                    ChangeKind::Modified,
                    i,
                    i,
                )
            })
            .collect();
        let mut d = change_digest(
            &changed,
            &changeset(),
            &Evidence::complete(ImpactGraph::new()),
            &[],
            &repo_ctx(),
        );
        d.truncate_to_budget(50);
        assert_eq!(d.changed_symbols.len(), 50); // never dropped
        assert!(d.notes.iter().any(|n| n.contains("exceeds budget")));
    }

    #[test]
    fn hard_cap_clamps_budget() {
        let changed = vec![info("pkg/main.go", "main", ChangeKind::Modified, 8, 20)];
        let mut d = change_digest(
            &changed,
            &changeset(),
            &Evidence::complete(ImpactGraph::new()),
            &[],
            &repo_ctx(),
        );
        // A huge budget is clamped to the hard cap — digest is far below it → no cuts.
        d.truncate_to_budget(usize::MAX);
        assert!(d.notes.iter().all(|n| !n.contains("budget cut")));
    }

    #[test]
    fn root_level_files_sketch_as_dot() {
        let cs = ChangeSet::new(
            ChangeScope::Staged,
            vec![FileChange {
                path: Utf8PathBuf::from("main.go"),
                old_path: None,
                status: FileStatus::Modified,
                hunks: vec![],
                binary: false,
            }],
        );
        let d = change_digest(
            &[],
            &cs,
            &Evidence::complete(ImpactGraph::new()),
            &[],
            &repo_ctx(),
        );
        assert_eq!(d.repo.dirs, vec![(".".to_string(), 1)]);
    }

    #[test]
    fn token_estimate_heuristic() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }
}
