//! Shared implementation for conventional stdio language-server adapters.
//!
//! Server descriptors own process startup, project-root selection, document language,
//! file ownership, and initialization options. This session owns the reusable protocol
//! behavior: overlays, capability-gated queries, position conversion, caches, diagnostics,
//! and shutdown.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use codescope_core::{
    Diagnostic, Evidence, FeatureSet, FileId, Location, Position, Revision, SymbolKind, SymbolNode,
    SymbolRef, SymbolTree, SyntaxToken,
};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::capabilities::{parse_text_document_sync, require, resolve_features};
use crate::client::{LspClient, ShutdownOutcome};
use crate::content_cache::{DocumentSnapshot, OpenDocumentState, SymbolTreeCache};
use crate::encoding::{PositionEncoding, line_at, position_from_wire, position_to_wire};
use crate::error::{LspError, SemanticError};
use crate::options::LanguageServiceOptions;
use crate::semantic_tokens::{
    SemanticTokenLegend, client_capability as semantic_tokens_capability,
    decode as decode_semantic_tokens, legend_from_capabilities,
};
use crate::uri::{path_from_uri, uri_from_path};

/// Deadline for the first request while the server indexes its workspace.
const FIRST_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Steady-state request deadline.
const STEADY_TIMEOUT: Duration = Duration::from_secs(60);
/// A prepared hierarchy item can become stale while a server finishes indexing.
const CONTENT_MODIFIED_RETRIES: u32 = 2;

/// Configuration supplied by a conventional language-server adapter.
pub trait StandardAdapter: Send + Sync + 'static {
    /// Stable server label used for transport errors and tracing.
    const SERVER_NAME: &'static str;
    /// LSP language identifier sent with `textDocument/didOpen`.
    const LANGUAGE_ID: &'static str;
    /// Source extensions owned by the adapter, without leading dots.
    const FILE_EXTENSIONS: &'static [&'static str];
    /// Whether the server publishes `textDocument/publishDiagnostics` notifications.
    const PUSH_DIAGNOSTICS: bool = true;

    /// Resolve the project root passed through `rootUri` and `workspaceFolders`.
    fn project_root(repo_root: &Utf8Path) -> Result<Utf8PathBuf, SemanticError>;

    /// Construct the stdio server command.
    fn command(
        repo_root: &Utf8Path,
        project_root: &Utf8Path,
        options: LanguageServiceOptions,
    ) -> Command;

    /// Server-specific initialize payload.
    fn initialization_options(options: LanguageServiceOptions) -> Value;
}

/// Reusable semantic session for a [`StandardAdapter`].
#[derive(Debug)]
pub struct StandardLspService {
    client: LspClient,
    /// Absolute git/repository toplevel. Repo-relative [`FileId`]s are interpreted
    /// against this path.
    repo_root: Utf8PathBuf,
    /// The language-specific project root passed to the server.
    project_root: Utf8PathBuf,
    /// LSP language identifier used for opened documents.
    language_id: &'static str,
    /// File extensions owned by this adapter instance.
    file_extensions: &'static [&'static str],
    features: FeatureSet,
    encoding: PositionEncoding,
    /// Server-specific index-to-name mapping negotiated for semantic tokens.
    semantic_token_legend: Option<SemanticTokenLegend>,
    /// Open document versions and content identities by absolute path.
    documents: Mutex<HashMap<Utf8PathBuf, OpenDocumentState>>,
    /// Content-addressed symbol trees survive repository epochs without becoming stale.
    symbol_cache: Mutex<SymbolTreeCache>,
    /// Monotonic request counter to distinguish the slow first request.
    request_count: AtomicU64,
}

impl StandardLspService {
    /// Spawn and initialize the server described by `A`.
    pub async fn start<A: StandardAdapter>(
        repo_root: &Utf8Path,
        options: LanguageServiceOptions,
    ) -> Result<Self, SemanticError> {
        let options = options.normalized();
        let project_root = A::project_root(repo_root)?;
        let command = A::command(repo_root, &project_root, options);
        let client = LspClient::spawn(command, A::SERVER_NAME)?;

        let root_uri = uri_from_path(&project_root)?;
        let semantic_tokens = semantic_tokens_capability();
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri.as_str(),
            "workspaceFolders": [{ "uri": root_uri.as_str(), "name": project_root.file_name().unwrap_or("workspace") }],
            "capabilities": {
                "general": { "positionEncodings": ["utf-8", "utf-16"] },
                "textDocument": {
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                    "callHierarchy": { "dynamicRegistration": false },
                    "typeHierarchy": { "dynamicRegistration": false },
                    "publishDiagnostics": { "relatedInformation": true },
                    "semanticTokens": semantic_tokens
                },
                "workspace": { "workspaceFolders": true, "symbol": {} }
            },
            "initializationOptions": A::initialization_options(options)
        });
        let init = client
            .request("initialize", params, FIRST_REQUEST_TIMEOUT)
            .await?;
        let _ = parse_text_document_sync(&init["capabilities"]);
        let mut features = resolve_features(&init["capabilities"])?;
        let semantic_token_legend = legend_from_capabilities(&init["capabilities"]);
        if semantic_token_legend.is_none() {
            features.set(
                codescope_core::Feature::SemanticTokens,
                codescope_core::Availability::Unsupported,
            );
        }
        if A::PUSH_DIAGNOSTICS {
            features.set(
                codescope_core::Feature::PushDiagnostics,
                codescope_core::Availability::Supported,
            );
        }
        let encoding =
            PositionEncoding::from_response_value(init["capabilities"].get("positionEncoding"));
        client.notify("initialized", json!({})).await?;

        tracing::info!(repo_root = %repo_root, project_root = %project_root, server = A::SERVER_NAME, ?encoding, "language-server session initialized");
        Ok(StandardLspService {
            client,
            repo_root: repo_root.to_path_buf(),
            project_root,
            language_id: A::LANGUAGE_ID,
            file_extensions: A::FILE_EXTENSIONS,
            features,
            encoding,
            semantic_token_legend,
            documents: Mutex::new(HashMap::new()),
            symbol_cache: Mutex::new(SymbolTreeCache::default()),
            request_count: AtomicU64::new(0),
        })
    }

    /// Capabilities resolved at initialize.
    #[must_use]
    pub fn features(&self) -> &FeatureSet {
        &self.features
    }

    /// Absolute repository root that [`FileId`]s are relative to.
    #[must_use]
    pub fn repo_root(&self) -> &Utf8Path {
        &self.repo_root
    }

    /// Language-specific project root passed through `rootUri`.
    #[must_use]
    pub fn project_root(&self) -> &Utf8Path {
        &self.project_root
    }

    /// `true` while the server process is alive.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.client.is_alive()
    }

    /// Absolute path of a repo-relative file id.
    #[must_use]
    pub fn abs_path(&self, file: &FileId) -> Utf8PathBuf {
        self.repo_root.join(file.as_path())
    }

    /// Repo-relative [`FileId`] for an absolute path inside the repo root.
    #[must_use]
    pub fn file_id(&self, abs: &Utf8Path) -> Option<FileId> {
        abs.strip_prefix(&self.repo_root)
            .ok()
            .map(|rel| FileId::new_unchecked(rel.to_path_buf()))
    }

    fn timeout(&self) -> Duration {
        if self.request_count.fetch_add(1, Ordering::SeqCst) == 0 {
            FIRST_REQUEST_TIMEOUT
        } else {
            STEADY_TIMEOUT
        }
    }

    /// Ensure the server has the current disk content of `file` as an open document.
    async fn sync_worktree(&self, file: &FileId) -> Result<DocumentSnapshot, SemanticError> {
        let abs = self.abs_path(file);
        let text = std::fs::read_to_string(&abs).map_err(|source| SemanticError::FileRead {
            path: abs.clone(),
            source,
        })?;
        let snapshot = DocumentSnapshot::new(abs, text);
        self.sync_content(&snapshot.abs, &snapshot.text, snapshot.hash)
            .await?;
        Ok(snapshot)
    }

    /// Close the overlay for `abs` if one is open.
    async fn close(&self, abs: &Utf8Path) -> Result<(), SemanticError> {
        let mut documents = self.documents.lock().await;
        if documents.remove(abs).is_some() {
            let uri = uri_from_path(abs)?;
            self.client
                .notify(
                    "textDocument/didClose",
                    json!({ "textDocument": { "uri": uri.as_str() } }),
                )
                .await?;
        }
        Ok(())
    }

    /// Synchronize a full-text overlay without close/open churn.
    async fn sync_content(
        &self,
        abs: &Utf8Path,
        text: &str,
        hash: u64,
    ) -> Result<bool, SemanticError> {
        let uri = uri_from_path(abs)?;
        let mut documents = self.documents.lock().await;
        if let Some(state) = documents.get_mut(abs) {
            if state.hash == hash {
                return Ok(false);
            }
            state.version = state.version.saturating_add(1);
            state.hash = hash;
            self.client
                .notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": {
                            "uri": uri.as_str(),
                            "version": state.version,
                        },
                        "contentChanges": [{ "text": text }]
                    }),
                )
                .await?;
            return Ok(true);
        }
        self.client
            .notify(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri.as_str(),
                        "languageId": self.language_id,
                        "version": 1,
                        "text": text,
                    }
                }),
            )
            .await?;
        documents.insert(abs.to_path_buf(), OpenDocumentState { version: 1, hash });
        Ok(true)
    }

    /// Current push-diagnostics for `file`, converted to utf-8 positions.
    #[must_use]
    pub fn diagnostics(&self, file: &FileId) -> Vec<Diagnostic> {
        let abs = self.abs_path(file);
        let Ok(uri) = uri_from_path(&abs) else {
            return Vec::new();
        };
        let Ok(text) = std::fs::read_to_string(&abs) else {
            return Vec::new();
        };
        self.client
            .diagnostics(uri.as_str())
            .iter()
            .map(|d| {
                let mut core = Diagnostic::from_lsp(file.clone(), d);
                core.range = self.range_from_wire(&text, d.range);
                core
            })
            .collect()
    }

    // -- position conversion (wire utf-8 <-> internal utf-8) ------------------

    fn pos_to_wire(&self, text: &str, pos: Position) -> lsp_types::Position {
        let line = line_at(text, pos.line).unwrap_or("");
        position_to_wire(
            line,
            lsp_types::Position::new(pos.line, pos.col),
            self.encoding,
        )
    }

    fn pos_from_wire(&self, text: &str, pos: lsp_types::Position) -> Position {
        let line = line_at(text, pos.line).unwrap_or("");
        let p = position_from_wire(line, pos, self.encoding);
        Position::new(p.line, p.character)
    }

    fn range_from_wire(&self, text: &str, range: lsp_types::Range) -> codescope_core::LineRange {
        let s = self.pos_from_wire(text, range.start);
        let e = self.pos_from_wire(text, range.end);
        codescope_core::LineRange::new(s.line, s.col, e.line, e.col)
    }

    // -- queries ----------------------------------------------------------------

    /// Hierarchical symbol tree of the worktree content of `file`.
    #[tracing::instrument(err, skip(self))]
    pub async fn document_symbols(
        &self,
        file: &FileId,
    ) -> Result<Evidence<SymbolTree>, SemanticError> {
        require(&self.features, codescope_core::Feature::DocumentSymbols)?;
        let snapshot = self.sync_worktree(file).await?;
        if let Some(tree) =
            self.symbol_cache
                .lock()
                .await
                .get(&snapshot.abs, Revision::Worktree, snapshot.hash)
        {
            return Ok(tree);
        }
        let uri = uri_from_path(&snapshot.abs)?;
        let result = self
            .client
            .request(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": uri.as_str() } }),
                self.timeout(),
            )
            .await?;
        let tree = self.symbol_tree(
            file.clone(),
            Revision::Worktree,
            result,
            &snapshot.text,
            &snapshot.abs,
        )?;
        self.symbol_cache.lock().await.insert(
            snapshot.abs,
            Revision::Worktree,
            snapshot.hash,
            tree.clone(),
        );
        Ok(tree)
    }

    /// Symbol tree of `content` as a temporary overlay (base-revision analysis).
    #[tracing::instrument(err, skip(self, content))]
    pub async fn base_document_symbols(
        &self,
        file: &FileId,
        content: &str,
    ) -> Result<Evidence<SymbolTree>, SemanticError> {
        require(&self.features, codescope_core::Feature::DocumentSymbols)?;
        let abs = self.abs_path(file);
        let base_hash = xxhash_rust::xxh3::xxh3_64(content.as_bytes());
        if let Some(tree) = self
            .symbol_cache
            .lock()
            .await
            .get(&abs, Revision::Base, base_hash)
        {
            return Ok(tree);
        }
        let was_open = self.documents.lock().await.contains_key(&abs);
        let disk = if was_open {
            std::fs::read_to_string(&abs)
                .ok()
                .map(|text| DocumentSnapshot::new(abs.clone(), text))
        } else {
            None
        };
        self.sync_content(&abs, content, base_hash).await?;
        let uri = uri_from_path(&abs)?;
        let result = self
            .client
            .request(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": uri.as_str() } }),
                self.timeout(),
            )
            .await;
        let restore = match (was_open, &disk) {
            (true, Some(snapshot)) => self
                .sync_content(&abs, &snapshot.text, snapshot.hash)
                .await
                .map(|_| ()),
            _ => self.close(&abs).await,
        };
        let result = result?;
        restore?;
        let tree = self.symbol_tree(file.clone(), Revision::Base, result, content, &abs)?;
        self.symbol_cache
            .lock()
            .await
            .insert(abs, Revision::Base, base_hash, tree.clone());
        Ok(tree)
    }

    /// Semantic syntax tokens for the current worktree content of `file`.
    #[tracing::instrument(err, skip(self))]
    pub async fn semantic_tokens(
        &self,
        file: &FileId,
    ) -> Result<Evidence<Vec<SyntaxToken>>, SemanticError> {
        require(&self.features, codescope_core::Feature::SemanticTokens)?;
        let legend = self
            .semantic_token_legend
            .as_ref()
            .ok_or(SemanticError::Unsupported(
                codescope_core::Feature::SemanticTokens,
            ))?;
        let snapshot = self.sync_worktree(file).await?;
        let uri = uri_from_path(&snapshot.abs)?;
        let result = self
            .client
            .request(
                "textDocument/semanticTokens/full",
                json!({ "textDocument": { "uri": uri.as_str() } }),
                self.timeout(),
            )
            .await?;
        Ok(Evidence::complete(decode_semantic_tokens(
            result,
            &snapshot.text,
            self.encoding,
            legend,
        )?))
    }

    /// Semantic syntax tokens for exact snapshot `content` in a temporary overlay.
    #[tracing::instrument(err, skip(self, content))]
    pub async fn semantic_tokens_for_content(
        &self,
        file: &FileId,
        content: &str,
    ) -> Result<Evidence<Vec<SyntaxToken>>, SemanticError> {
        require(&self.features, codescope_core::Feature::SemanticTokens)?;
        let legend = self
            .semantic_token_legend
            .as_ref()
            .ok_or(SemanticError::Unsupported(
                codescope_core::Feature::SemanticTokens,
            ))?;
        let abs = self.abs_path(file);
        let base_hash = xxhash_rust::xxh3::xxh3_64(content.as_bytes());
        let was_open = self.documents.lock().await.contains_key(&abs);
        let disk = if was_open {
            std::fs::read_to_string(&abs)
                .ok()
                .map(|text| DocumentSnapshot::new(abs.clone(), text))
        } else {
            None
        };
        self.sync_content(&abs, content, base_hash).await?;
        let uri = uri_from_path(&abs)?;
        let result = self
            .client
            .request(
                "textDocument/semanticTokens/full",
                json!({ "textDocument": { "uri": uri.as_str() } }),
                self.timeout(),
            )
            .await;
        let restore = match (was_open, &disk) {
            (true, Some(snapshot)) => self
                .sync_content(&abs, &snapshot.text, snapshot.hash)
                .await
                .map(|_| ()),
            _ => self.close(&abs).await,
        };
        let result = result?;
        restore?;
        Ok(Evidence::complete(decode_semantic_tokens(
            result,
            content,
            self.encoding,
            legend,
        )?))
    }

    fn symbol_tree(
        &self,
        file: FileId,
        revision: Revision,
        result: Value,
        text: &str,
        abs: &Utf8Path,
    ) -> Result<Evidence<SymbolTree>, SemanticError> {
        match serde_json::from_value::<Option<lsp_types::DocumentSymbolResponse>>(result) {
            Ok(Some(lsp_types::DocumentSymbolResponse::Nested(symbols))) => {
                let symbols = symbols
                    .into_iter()
                    .map(|s| self.doc_symbol_from_wire(text, s))
                    .collect();
                let mut tree = SymbolTree::from_document_symbols(file, revision, symbols);
                tree.sort_recursive();
                Ok(Evidence::complete(tree))
            }
            Ok(Some(lsp_types::DocumentSymbolResponse::Flat(infos))) => {
                let mut roots: Vec<SymbolNode> = Vec::new();
                for (i, info) in infos.into_iter().enumerate() {
                    if path_from_uri(&info.location.uri).ok().as_ref() != Some(&abs.to_path_buf()) {
                        continue;
                    }
                    let range = self.range_from_wire(text, info.location.range);
                    roots.push(SymbolNode {
                        id: codescope_core::SymbolId::new(i.to_string()),
                        name: info.name,
                        detail: None,
                        kind: SymbolKind::from(info.kind),
                        range,
                        selection: range,
                        children: Vec::new(),
                    });
                }
                let mut tree = SymbolTree::new(file, revision, roots);
                tree.sort_recursive();
                Ok(Evidence::partial(
                    tree,
                    vec![
                        "server returned flat SymbolInformation; nested symbols unavailable"
                            .to_string(),
                    ],
                ))
            }
            Ok(None) => Ok(Evidence::complete(SymbolTree::new(
                file,
                revision,
                Vec::new(),
            ))),
            Err(e) => Err(LspError::Protocol(format!("documentSymbol response: {e}")).into()),
        }
    }

    fn doc_symbol_from_wire(
        &self,
        text: &str,
        sym: lsp_types::DocumentSymbol,
    ) -> lsp_types::DocumentSymbol {
        let mut sym = sym;
        sym.range = self.range_lsp_from_wire(text, sym.range);
        sym.selection_range = self.range_lsp_from_wire(text, sym.selection_range);
        sym.children = sym.children.map(|c| {
            c.into_iter()
                .map(|s| self.doc_symbol_from_wire(text, s))
                .collect()
        });
        sym
    }

    fn range_lsp_from_wire(&self, text: &str, r: lsp_types::Range) -> lsp_types::Range {
        lsp_types::Range::new(
            {
                let p = self.pos_from_wire(text, r.start);
                lsp_types::Position::new(p.line, p.col)
            },
            {
                let p = self.pos_from_wire(text, r.end);
                lsp_types::Position::new(p.line, p.col)
            },
        )
    }

    /// Reference sites of the symbol at `pos`.
    #[tracing::instrument(err, skip(self))]
    pub async fn references(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<Location>>, SemanticError> {
        require(&self.features, codescope_core::Feature::References)?;
        let snapshot = self.sync_worktree(file).await?;
        let uri = uri_from_path(&snapshot.abs)?;
        let wire = self.pos_to_wire(&snapshot.text, pos);
        let result = self
            .client
            .request(
                "textDocument/references",
                json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": { "line": wire.line, "character": wire.character },
                    "context": { "includeDeclaration": true }
                }),
                self.timeout(),
            )
            .await?;
        let locations: Vec<Location> = match serde_json::from_value::<
            Option<Vec<lsp_types::Location>>,
        >(result)
        {
            Ok(Some(locs)) => locs
                .into_iter()
                .filter_map(|l| self.location_from_wire(l))
                .collect(),
            Ok(None) => Vec::new(),
            Err(e) => return Err(LspError::Protocol(format!("references response: {e}")).into()),
        };
        Ok(Evidence::complete(locations))
    }

    /// Callers of the symbol at `pos`.
    pub async fn incoming_calls(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        require(
            &self.features,
            codescope_core::Feature::CallHierarchyIncoming,
        )?;
        let Some(result) = self
            .call_hierarchy_request(file, pos, "callHierarchy/incomingCalls")
            .await?
        else {
            return Ok(Evidence::complete(Vec::new()));
        };
        let refs = match serde_json::from_value::<Option<Vec<lsp_types::CallHierarchyIncomingCall>>>(
            result,
        ) {
            Ok(Some(calls)) => calls
                .into_iter()
                .filter_map(|c| self.call_item_to_ref(c.from))
                .collect(),
            Ok(None) => Vec::new(),
            Err(e) => return Err(LspError::Protocol(format!("incomingCalls response: {e}")).into()),
        };
        Ok(Evidence::complete(refs))
    }

    /// Callees of the symbol at `pos`.
    pub async fn outgoing_calls(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        require(
            &self.features,
            codescope_core::Feature::CallHierarchyOutgoing,
        )?;
        let Some(result) = self
            .call_hierarchy_request(file, pos, "callHierarchy/outgoingCalls")
            .await?
        else {
            return Ok(Evidence::complete(Vec::new()));
        };
        let refs = match serde_json::from_value::<Option<Vec<lsp_types::CallHierarchyOutgoingCall>>>(
            result,
        ) {
            Ok(Some(calls)) => calls
                .into_iter()
                .filter_map(|c| self.call_item_to_ref(c.to))
                .collect(),
            Ok(None) => Vec::new(),
            Err(e) => return Err(LspError::Protocol(format!("outgoingCalls response: {e}")).into()),
        };
        Ok(Evidence::complete(refs))
    }

    /// Implementations of the symbol at `pos` (`textDocument/implementation`).
    pub async fn implementations(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        require(&self.features, codescope_core::Feature::Implementation)?;
        let snapshot = self.sync_worktree(file).await?;
        let uri = uri_from_path(&snapshot.abs)?;
        let wire = self.pos_to_wire(&snapshot.text, pos);
        let result = self
            .client
            .request(
                "textDocument/implementation",
                json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": { "line": wire.line, "character": wire.character }
                }),
                self.timeout(),
            )
            .await?;
        let refs = self.goto_response_to_refs(result, "implementation")?;
        Ok(Evidence::complete(refs))
    }

    /// Supertypes of the type symbol at `pos`.
    ///
    /// Standard adapters currently leave advertised type-hierarchy enrichment unknown.
    pub async fn type_supertypes(
        &self,
        _file: &FileId,
        _pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        require(&self.features, codescope_core::Feature::TypeHierarchySuper)?;
        Ok(Evidence::unknown(Vec::new()))
    }

    /// Subtypes of the type symbol at `pos`.
    ///
    /// Standard adapters currently leave advertised type-hierarchy enrichment unknown.
    pub async fn type_subtypes(
        &self,
        _file: &FileId,
        _pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        require(&self.features, codescope_core::Feature::TypeHierarchySub)?;
        // Known adapters do not advertise typeHierarchy today. If a future server adds it,
        // return Unknown rather than fabricating a complete-empty result.
        Ok(Evidence::unknown(Vec::new()))
    }

    /// Hover text for the symbol at `pos`.
    pub async fn hover(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Option<String>, SemanticError> {
        require(&self.features, codescope_core::Feature::Hover)?;
        let snapshot = self.sync_worktree(file).await?;
        let uri = uri_from_path(&snapshot.abs)?;
        let wire = self.pos_to_wire(&snapshot.text, pos);
        let result = self
            .client
            .request(
                "textDocument/hover",
                json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": { "line": wire.line, "character": wire.character }
                }),
                self.timeout(),
            )
            .await?;
        let hover: Option<lsp_types::Hover> = serde_json::from_value(result)
            .map_err(|e| LspError::Protocol(format!("hover response: {e}")))?;
        Ok(hover.map(|h| hover_text(&h.contents)))
    }

    // -- prepare helpers ---------------------------------------------------------

    async fn call_hierarchy_request(
        &self,
        file: &FileId,
        pos: Position,
        method: &'static str,
    ) -> Result<Option<Value>, SemanticError> {
        for attempt in 0..=CONTENT_MODIFIED_RETRIES {
            let item = match self.prepare_call_hierarchy(file, pos).await {
                Ok(Some(item)) => item,
                Ok(None) => return Ok(None),
                Err(error) if attempt < CONTENT_MODIFIED_RETRIES && is_content_modified(&error) => {
                    tokio::time::sleep(content_modified_backoff(attempt)).await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            match self
                .client
                .request(method, json!({ "item": item }), self.timeout())
                .await
            {
                Ok(result) => return Ok(Some(result)),
                Err(error)
                    if attempt < CONTENT_MODIFIED_RETRIES
                        && matches!(error, LspError::Response { code: -32801, .. }) =>
                {
                    tokio::time::sleep(content_modified_backoff(attempt)).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
        unreachable!("content-modified retry loop always returns on its final attempt")
    }

    async fn prepare_call_hierarchy(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Option<lsp_types::CallHierarchyItem>, SemanticError> {
        let snapshot = self.sync_worktree(file).await?;
        let uri = uri_from_path(&snapshot.abs)?;
        let wire = self.pos_to_wire(&snapshot.text, pos);
        let result = self
            .client
            .request(
                "textDocument/prepareCallHierarchy",
                json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": { "line": wire.line, "character": wire.character }
                }),
                self.timeout(),
            )
            .await?;
        match serde_json::from_value::<Option<Vec<lsp_types::CallHierarchyItem>>>(result) {
            Ok(Some(items)) => Ok(items.into_iter().next()),
            Ok(None) => Ok(None),
            Err(e) => Err(LspError::Protocol(format!("prepareCallHierarchy: {e}")).into()),
        }
    }

    // -- wire → domain conversion ------------------------------------------------

    fn location_from_wire(&self, loc: lsp_types::Location) -> Option<Location> {
        let abs = path_from_uri(&loc.uri).ok()?;
        let file = self.file_id(&abs)?;
        let text = std::fs::read_to_string(&abs).ok()?;
        Some(Location {
            file,
            range: self.range_from_wire(&text, loc.range),
        })
    }

    fn call_item_to_ref(&self, item: lsp_types::CallHierarchyItem) -> Option<SymbolRef> {
        let abs = path_from_uri(&item.uri).ok()?;
        let file = self.file_id(&abs)?;
        let text = std::fs::read_to_string(&abs).ok()?;
        Some(SymbolRef {
            file,
            name: item.name,
            kind: SymbolKind::from(item.kind),
            range: Some(self.range_from_wire(&text, item.range)),
            selection: Some(self.range_from_wire(&text, item.selection_range)),
        })
    }

    fn goto_response_to_refs(
        &self,
        result: Value,
        what: &str,
    ) -> Result<Vec<SymbolRef>, SemanticError> {
        let parsed: Option<lsp_types::GotoDefinitionResponse> = serde_json::from_value(result)
            .map_err(|e| LspError::Protocol(format!("{what} response: {e}")))?;
        let mut out = Vec::new();
        let mut push_loc = |loc: lsp_types::Location| {
            if let Some(location) = self.location_from_wire(loc) {
                out.push(SymbolRef {
                    file: location.file,
                    name: format!("{}:{}", location.range.start_line, location.range.start_col),
                    kind: SymbolKind::Unknown,
                    range: Some(location.range),
                    selection: None,
                });
            }
        };
        match parsed {
            Some(lsp_types::GotoDefinitionResponse::Scalar(l)) => push_loc(l),
            Some(lsp_types::GotoDefinitionResponse::Array(ls)) => {
                for l in ls {
                    push_loc(l);
                }
            }
            Some(lsp_types::GotoDefinitionResponse::Link(ls)) => {
                for l in ls {
                    let abs = match path_from_uri(&l.target_uri) {
                        Ok(a) => a,
                        Err(_) => continue,
                    };
                    if let Some(file) = self.file_id(&abs) {
                        let Ok(text) = std::fs::read_to_string(&abs) else {
                            continue;
                        };
                        let range = self.range_from_wire(&text, l.target_range);
                        let selection = self.range_from_wire(&text, l.target_selection_range);
                        out.push(SymbolRef {
                            file,
                            name: format!("{}:{}", selection.start_line, selection.start_col),
                            kind: SymbolKind::Unknown,
                            range: Some(range),
                            selection: Some(selection),
                        });
                    }
                }
            }
            None => {}
        }
        Ok(out)
    }

    /// `true` for source files owned by this adapter instance.
    #[must_use]
    pub fn handles(&self, file: &FileId) -> bool {
        file.extension()
            .is_some_and(|extension| self.file_extensions.contains(&extension))
    }

    /// Graceful teardown.
    pub async fn shutdown(self) {
        let _outcome: ShutdownOutcome = self.client.shutdown().await;
    }
}

fn is_content_modified(error: &SemanticError) -> bool {
    matches!(
        error,
        SemanticError::Client(LspError::Response { code: -32801, .. })
    )
}

fn content_modified_backoff(attempt: u32) -> Duration {
    Duration::from_millis(50_u64 << attempt)
}

fn hover_text(contents: &lsp_types::HoverContents) -> String {
    match contents {
        lsp_types::HoverContents::Scalar(lsp_types::MarkedString::String(s)) => s.clone(),
        lsp_types::HoverContents::Scalar(lsp_types::MarkedString::LanguageString(ls)) => {
            ls.value.clone()
        }
        lsp_types::HoverContents::Markup(m) => m.value.clone(),
        lsp_types::HoverContents::Array(items) => items
            .iter()
            .map(|item| match item {
                lsp_types::MarkedString::String(s) => s.clone(),
                lsp_types::MarkedString::LanguageString(ls) => ls.value.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}
