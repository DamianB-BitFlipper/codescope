//! Scriptable fake LSP server for negative-path client tests (research 08 §3).
//!
//! A hand-rolled stdio JSON-RPC server: it speaks the LSP base protocol
//! (`Content-Length` framed JSON) over any `AsyncRead`/`AsyncWrite` pair, so tests can run
//! it in-process over [`tokio::io::duplex`] ([`spawn_in_process`]) or as a real subprocess
//! over stdio (the `fake-lsp` binary → [`FakeLspServer::serve_stdio`]).
//!
//! Scriptable behavior:
//!
//! - canned `initialize` result, including empty (`{}`) or literal-`null` capabilities;
//! - canned `textDocument/documentSymbol` (or any method) results from provided JSON;
//! - `textDocument/publishDiagnostics` pushes triggered by a configurable inbound method;
//! - `-32601 MethodNotFound` for any unscripted request (matches verified gopls behavior);
//! - malformed frames: invalid JSON bodies, over-declared `Content-Length`, truncated
//!   streams;
//! - a configurable per-response delay and a "never answer `shutdown`" mode for
//!   kill-path tests.
//!
//! The server records every inbound message; fetch them with [`FakeLspServer::received`].

use crate::error::{Result, TestutilError};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Hard cap on inbound frame bodies (16 MiB), guarding against absurd `Content-Length`.
pub const MAX_FRAME_BYTES: u64 = 16 * 1024 * 1024;

/// JSON-RPC `MethodNotFound` error code (what gopls returns for unknown methods).
pub const METHOD_NOT_FOUND: i64 = -32601;

/// How the fake server answers one request method.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScriptedResponse {
    /// Well-formed JSON-RPC success frame carrying `value` as the result.
    Result {
        /// The `result` payload.
        value: Value,
    },
    /// Well-formed JSON-RPC error frame.
    Error {
        /// JSON-RPC error code (e.g. [`METHOD_NOT_FOUND`]).
        code: i64,
        /// Error message.
        message: String,
    },
    /// Correctly framed (`Content-Length` matches) but the body is not valid JSON.
    MalformedJson {
        /// Raw body bytes to send.
        body: String,
    },
    /// Frame whose `Content-Length` declares `excess` more bytes than are actually sent.
    /// A spec-following client blocks (or times out) waiting for the remainder.
    WrongContentLength {
        /// Raw body bytes to send.
        body: String,
        /// Extra bytes the header lies about.
        excess: u64,
    },
    /// Write a frame header + `body`, then close the connection mid-stream (truncated
    /// stream: the header declares `declared_len` but the stream ends after `body`).
    TruncateAndClose {
        /// Raw body bytes to send before closing.
        body: String,
        /// Length the header declares.
        declared_len: u64,
    },
    /// Never respond to this request (for shutdown-timeout / client-kill tests).
    Ignore,
}

impl ScriptedResponse {
    /// A success response with `value`.
    #[must_use]
    pub fn result(value: Value) -> Self {
        ScriptedResponse::Result { value }
    }

    /// A `-32601 method not found` error, as gopls sends for unknown methods.
    #[must_use]
    pub fn method_not_found() -> Self {
        ScriptedResponse::Error {
            code: METHOD_NOT_FOUND,
            message: "method not found".to_string(),
        }
    }

    /// A correctly framed but syntactically invalid JSON body.
    #[must_use]
    pub fn malformed_json() -> Self {
        ScriptedResponse::MalformedJson {
            body: r#"{"jsonrpc":"2.0","id":"#.to_string(),
        }
    }
}

/// One diagnostics push: full `textDocument/publishDiagnostics` params.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DiagnosticsPush {
    /// Complete params object (`{"uri": …, "diagnostics": […]}`).
    pub params: Value,
}

/// Configuration for one fake-server session. Serializable so the `fake-lsp` binary can
/// load a script file.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FakeLspConfig {
    /// Full JSON result for `initialize` (an `InitializeResult`-shaped value).
    pub initialize_result: Value,
    /// Per-method scripted responses; any request method not present here (and not
    /// `initialize`/`shutdown`) gets [`ScriptedResponse::method_not_found`].
    #[serde(default)]
    pub responses: BTreeMap<String, ScriptedResponse>,
    /// Delay applied before **every** response frame, in milliseconds.
    #[serde(default)]
    pub response_delay_ms: u64,
    /// Diagnostics pushed (once, in order) after the first inbound message whose method
    /// equals [`FakeLspConfig::diagnostics_trigger`].
    #[serde(default)]
    pub diagnostics: Vec<DiagnosticsPush>,
    /// Inbound method that triggers the diagnostics pushes. Default: `initialized`.
    pub diagnostics_trigger: String,
    /// When `false`, `shutdown` requests are ignored (never answered) so clients must
    /// escalate to killing the process. Default: `true`.
    pub respond_to_shutdown: bool,
}

impl Default for FakeLspConfig {
    fn default() -> Self {
        FakeLspConfig {
            initialize_result: gopls_like_initialize_result(),
            responses: BTreeMap::new(),
            response_delay_ms: 0,
            diagnostics: Vec::new(),
            diagnostics_trigger: "initialized".to_string(),
            respond_to_shutdown: true,
        }
    }
}

impl FakeLspConfig {
    /// Config advertising a gopls-like capability set (see
    /// [`gopls_like_initialize_result`]).
    #[must_use]
    pub fn gopls_like() -> Self {
        FakeLspConfig::default()
    }

    /// Config whose `initialize` result advertises **no** capabilities
    /// (`"capabilities": {}`) — every optional feature must degrade to unavailable.
    #[must_use]
    pub fn empty_capabilities() -> Self {
        FakeLspConfig::default().with_capabilities(json!({}))
    }

    /// Config whose `initialize` result carries a literal `"capabilities": null` — a
    /// hostile server; clients must not panic.
    #[must_use]
    pub fn null_capabilities() -> Self {
        FakeLspConfig::default().with_capabilities(Value::Null)
    }

    /// Replace the whole `initialize` result value.
    #[must_use]
    pub fn with_initialize_result(mut self, result: Value) -> Self {
        self.initialize_result = result;
        self
    }

    /// Replace only the `capabilities` field of the `initialize` result.
    #[must_use]
    pub fn with_capabilities(self, capabilities: Value) -> Self {
        self.with_initialize_result(json!({
            "capabilities": capabilities,
            "serverInfo": {"name": "codescope-fake-lsp", "version": "0"}
        }))
    }

    /// Script the response for one request method.
    #[must_use]
    pub fn with_response(mut self, method: impl Into<String>, response: ScriptedResponse) -> Self {
        self.responses.insert(method.into(), response);
        self
    }

    /// Serve `symbols` (a `DocumentSymbol[]` JSON value) for
    /// `textDocument/documentSymbol`.
    #[must_use]
    pub fn with_document_symbols(self, symbols: Value) -> Self {
        self.with_response(
            "textDocument/documentSymbol",
            ScriptedResponse::result(symbols),
        )
    }

    /// Delay every response by `delay`.
    #[must_use]
    pub fn with_response_delay(mut self, delay: Duration) -> Self {
        self.response_delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
        self
    }

    /// Push these `publishDiagnostics` params after the trigger method arrives.
    #[must_use]
    pub fn with_diagnostics(mut self, params: Value) -> Self {
        self.diagnostics.push(DiagnosticsPush { params });
        self
    }

    /// Change the diagnostics trigger method (default `initialized`).
    #[must_use]
    pub fn with_diagnostics_trigger(mut self, method: impl Into<String>) -> Self {
        self.diagnostics_trigger = method.into();
        self
    }

    /// Ignore `shutdown` requests so the client's kill-escalation path can be tested.
    #[must_use]
    pub fn with_shutdown_ignored(mut self) -> Self {
        self.respond_to_shutdown = false;
        self
    }
}

/// A gopls-like `InitializeResult` JSON value: UTF-16 positions plus the semantic
/// providers codescope relies on (research 01 matrix).
#[must_use]
pub fn gopls_like_initialize_result() -> Value {
    json!({
        "capabilities": {
            "positionEncoding": "utf-16",
            "textDocumentSync": {"openClose": true, "change": 2},
            "definitionProvider": true,
            "referencesProvider": true,
            "implementationProvider": true,
            "documentSymbolProvider": true,
            "callHierarchyProvider": true,
            "hoverProvider": true,
            "workspace": {"workspaceFolders": {"supported": true}}
        },
        "serverInfo": {"name": "codescope-fake-lsp", "version": "0"}
    })
}

/// One inbound client message, as recorded by the server.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReceivedMessage {
    /// `method` field (empty string when absent — e.g. a client response frame).
    pub method: String,
    /// Request `id` (`None` for notifications).
    pub id: Option<Value>,
    /// `params` field.
    pub params: Option<Value>,
}

impl ReceivedMessage {
    /// `true` when the message was a request (has an id).
    #[must_use]
    pub fn is_request(&self) -> bool {
        self.id.is_some()
    }
}

/// The scriptable fake LSP server. Cheap to clone; clones share the received-message log.
#[derive(Debug, Clone)]
pub struct FakeLspServer {
    config: Arc<FakeLspConfig>,
    received: Arc<Mutex<Vec<ReceivedMessage>>>,
}

impl FakeLspServer {
    /// Create a server with `config`.
    #[must_use]
    pub fn new(config: FakeLspConfig) -> Self {
        FakeLspServer {
            config: Arc::new(config),
            received: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Snapshot of every message received so far.
    #[must_use]
    pub fn received(&self) -> Vec<ReceivedMessage> {
        lock_ignore_poison(&self.received).clone()
    }

    /// Serve one session over an arbitrary transport until `exit`, EOF, or a scripted
    /// truncation. Returns the full received-message log.
    pub async fn serve<R, W>(&self, reader: R, writer: W) -> Result<Vec<ReceivedMessage>>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut reader = BufReader::new(reader);
        let mut writer = writer;
        let mut diagnostics_sent = false;

        loop {
            let Some(bytes) = read_frame(&mut reader).await? else {
                tracing::debug!("fake-lsp: client closed the stream");
                return Ok(self.received());
            };
            let message: Value = match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(e) => {
                    // The *client* sent garbage; report it loudly — this fake exists to
                    // misbehave on the write side, never to tolerate broken clients.
                    return Err(TestutilError::Protocol(format!(
                        "fake-lsp received invalid JSON from client: {e}"
                    )));
                }
            };

            let method = message
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let id = message.get("id").cloned().filter(|v| !v.is_null());
            let params = message.get("params").cloned();
            tracing::debug!(%method, ?id, "fake-lsp: received");
            lock_ignore_poison(&self.received).push(ReceivedMessage {
                method: method.clone(),
                id: id.clone(),
                params,
            });

            if let Some(id) = id {
                // Request.
                let response = self.response_for(&method);
                self.delay().await;
                match self.write_response(&mut writer, &id, response).await? {
                    SessionControl::Continue => {}
                    SessionControl::Close => return Ok(self.received()),
                }
            } else if method == "exit" {
                tracing::debug!("fake-lsp: exit notification, ending session");
                return Ok(self.received());
            }

            if !diagnostics_sent
                && method == self.config.diagnostics_trigger
                && !self.config.diagnostics.is_empty()
            {
                diagnostics_sent = true;
                self.delay().await;
                for push in &self.config.diagnostics {
                    let note = json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": push.params,
                    });
                    write_frame(&mut writer, &serde_json::to_vec(&note)?).await?;
                }
                tracing::debug!(
                    count = self.config.diagnostics.len(),
                    "fake-lsp: pushed diagnostics"
                );
            }
        }
    }

    /// Serve one session on this process's stdio (used by the `fake-lsp` binary).
    pub async fn serve_stdio(&self) -> Result<Vec<ReceivedMessage>> {
        self.serve(tokio::io::stdin(), tokio::io::stdout()).await
    }

    /// Resolve the scripted response for `method`.
    fn response_for(&self, method: &str) -> ScriptedResponse {
        if let Some(scripted) = self.config.responses.get(method) {
            return scripted.clone();
        }
        match method {
            "initialize" => ScriptedResponse::result(self.config.initialize_result.clone()),
            "shutdown" if self.config.respond_to_shutdown => {
                ScriptedResponse::result(Value::Null)
            }
            "shutdown" => ScriptedResponse::Ignore,
            _ => ScriptedResponse::method_not_found(),
        }
    }

    async fn delay(&self) {
        if self.config.response_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.config.response_delay_ms)).await;
        }
    }

    /// Write `response` for request `id`. Returns whether the session must close.
    async fn write_response<W: AsyncWrite + Unpin>(
        &self,
        writer: &mut W,
        id: &Value,
        response: ScriptedResponse,
    ) -> Result<SessionControl> {
        match response {
            ScriptedResponse::Result { value } => {
                let body = json!({"jsonrpc": "2.0", "id": id, "result": value});
                write_frame(writer, &serde_json::to_vec(&body)?).await?;
            }
            ScriptedResponse::Error { code, message } => {
                let body = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": code, "message": message},
                });
                write_frame(writer, &serde_json::to_vec(&body)?).await?;
            }
            ScriptedResponse::MalformedJson { body } => {
                write_frame(writer, body.as_bytes()).await?;
            }
            ScriptedResponse::WrongContentLength { body, excess } => {
                let declared = body.len() as u64 + excess;
                write_raw_frame(writer, declared, body.as_bytes()).await?;
            }
            ScriptedResponse::TruncateAndClose { body, declared_len } => {
                write_raw_frame(writer, declared_len, body.as_bytes()).await?;
                writer.shutdown().await.map_err(io_protocol)?;
                return Ok(SessionControl::Close);
            }
            ScriptedResponse::Ignore => {
                tracing::debug!("fake-lsp: ignoring request per script");
            }
        }
        Ok(SessionControl::Continue)
    }
}

enum SessionControl {
    Continue,
    Close,
}

/// Handle to an in-process fake server running over a duplex pipe.
#[derive(Debug)]
pub struct InProcessLsp {
    /// The client end of the pipe (read = server→client, write = client→server).
    pub client_io: tokio::io::DuplexStream,
    /// The running server (shares the received-message log).
    pub server: FakeLspServer,
    /// Join handle of the serving task; resolves when the session ends.
    pub handle: tokio::task::JoinHandle<Result<Vec<ReceivedMessage>>>,
}

/// Spawn `config` as an in-process server over [`tokio::io::duplex`]. Must be called from
/// within a tokio runtime.
#[must_use]
pub fn spawn_in_process(config: FakeLspConfig) -> InProcessLsp {
    let (client_io, server_io) = tokio::io::duplex(MAX_FRAME_BYTES as usize / 256);
    let server = FakeLspServer::new(config);
    let task_server = server.clone();
    let handle = tokio::spawn(async move {
        let (read, write) = tokio::io::split(server_io);
        task_server.serve(read, write).await
    });
    InProcessLsp {
        client_io,
        server,
        handle,
    }
}

// ---------------------------------------------------------------------------
// framing (also useful for hand-rolled test clients)
// ---------------------------------------------------------------------------

/// Read one LSP base-protocol frame; `Ok(None)` on clean EOF at a frame boundary.
pub async fn read_frame<R>(reader: &mut BufReader<R>) -> Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let mut content_length: Option<u64> = None;
    let mut first = true;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.map_err(io_protocol)?;
        if n == 0 {
            if first && content_length.is_none() {
                return Ok(None); // clean EOF between frames
            }
            return Err(TestutilError::Protocol(
                "stream ended inside frame headers".to_string(),
            ));
        }
        first = false;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break; // end of headers
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                let len = value.trim().parse::<u64>().map_err(|e| {
                    TestutilError::Protocol(format!("bad Content-Length {value:?}: {e}"))
                })?;
                content_length = Some(len);
            }
        }
    }
    let len = content_length
        .ok_or_else(|| TestutilError::Protocol("frame without Content-Length".to_string()))?;
    if len > MAX_FRAME_BYTES {
        return Err(TestutilError::Protocol(format!(
            "frame of {len} bytes exceeds cap {MAX_FRAME_BYTES}"
        )));
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await.map_err(io_protocol)?;
    Ok(Some(buf))
}

/// Write one well-formed frame around `body`.
pub async fn write_frame<W>(writer: &mut W, body: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    write_raw_frame(writer, body.len() as u64, body).await
}

/// Write a frame whose header declares `declared_len` regardless of `body.len()` — the
/// building block for the malformed-frame responses.
async fn write_raw_frame<W>(writer: &mut W, declared_len: u64, body: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let header = format!("Content-Length: {declared_len}\r\n\r\n");
    writer
        .write_all(header.as_bytes())
        .await
        .map_err(io_protocol)?;
    writer.write_all(body).await.map_err(io_protocol)?;
    writer.flush().await.map_err(io_protocol)?;
    Ok(())
}

fn io_protocol(e: std::io::Error) -> TestutilError {
    TestutilError::Protocol(format!("transport i/o: {e}"))
}

fn lock_ignore_poison<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_roundtrip() {
        let (mut a, b) = tokio::io::duplex(4096);
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"x"}"#;
        write_frame(&mut a, body).await.unwrap();
        drop(a);
        let mut reader = BufReader::new(b);
        let frame = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(frame, body);
        assert!(read_frame(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn read_frame_rejects_missing_content_length() {
        let (mut a, b) = tokio::io::duplex(4096);
        a.write_all(b"X-Whatever: 1\r\n\r\n").await.unwrap();
        drop(a);
        let mut reader = BufReader::new(b);
        let err = read_frame(&mut reader).await.unwrap_err();
        assert!(err.to_string().contains("Content-Length"));
    }

    #[tokio::test]
    async fn read_frame_rejects_oversized_frames() {
        let (mut a, b) = tokio::io::duplex(4096);
        let header = format!("Content-Length: {}\r\n\r\n", MAX_FRAME_BYTES + 1);
        a.write_all(header.as_bytes()).await.unwrap();
        drop(a);
        let mut reader = BufReader::new(b);
        let err = read_frame(&mut reader).await.unwrap_err();
        assert!(err.to_string().contains("exceeds cap"));
    }

    #[test]
    fn config_builders_compose() {
        let config = FakeLspConfig::gopls_like()
            .with_document_symbols(json!([{"name": "main"}]))
            .with_response("textDocument/hover", ScriptedResponse::method_not_found())
            .with_response_delay(Duration::from_millis(7))
            .with_diagnostics(json!({"uri": "file:///x.go", "diagnostics": []}))
            .with_diagnostics_trigger("textDocument/didOpen")
            .with_shutdown_ignored();
        assert_eq!(config.response_delay_ms, 7);
        assert!(!config.respond_to_shutdown);
        assert_eq!(config.diagnostics_trigger, "textDocument/didOpen");
        assert!(config.responses.contains_key("textDocument/documentSymbol"));
        // Serializable for the fake-lsp binary script file.
        let json = serde_json::to_string(&config).unwrap();
        let back: FakeLspConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn null_and_empty_capability_presets() {
        assert_eq!(
            FakeLspConfig::null_capabilities().initialize_result["capabilities"],
            Value::Null
        );
        assert_eq!(
            FakeLspConfig::empty_capabilities().initialize_result["capabilities"],
            json!({})
        );
        let gopls = gopls_like_initialize_result();
        assert_eq!(gopls["capabilities"]["positionEncoding"], "utf-16");
    }
}
