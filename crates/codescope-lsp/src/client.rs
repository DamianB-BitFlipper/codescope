//! Generic LSP client: stdio JSON-RPC transport over `tokio::process`.
//!
//! Responsibilities (research 01 / architecture decision 2):
//!
//! - `Content-Length` framing via [`crate::framing`]; malformed frames are
//!   logged and skipped, never fatal.
//! - Request-id matching with support for out-of-order responses.
//! - Server notifications: `textDocument/publishDiagnostics` is cached per
//!   file URI (gopls is push-only, quirk 6); everything else is traced.
//! - Server→client requests get safe default replies (`workspace/configuration`
//!   → `[null, …]`, `client/registerCapability` → `null`, …) so servers like
//!   gopls never hang waiting on us.
//! - Graceful teardown: `shutdown` request, `exit` notification, then a
//!   5-second wait before the process is killed.
//! - The last lines of the server's stderr are retained and embedded in
//!   spawn/exit errors.
//!
//! The client is transport only: it speaks [`serde_json::Value`]. Position
//! encoding conversion and capability gating live above it
//! ([`crate::service`] / [`crate::gopls`]).

use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::error::LspError;
use crate::framing::{encode_frame, FrameDecoder, FrameEvent};
use crate::jsonrpc::{self, Incoming, RequestId, ResponseError};

/// How long the process may take to exit after `shutdown` + `exit` before it
/// is killed.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// Deadline for the `shutdown` request itself during teardown.
const SHUTDOWN_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Number of stderr lines retained for error reporting.
const STDERR_TAIL_LINES: usize = 40;

type BoxWriter = Box<dyn AsyncWrite + Send + Unpin>;
type BoxReader = Box<dyn AsyncRead + Send + Unpin>;

/// Failure delivered to a pending request when no normal response arrives.
#[derive(Debug, Clone)]
enum PendingFailure {
    /// JSON-RPC error object from the server.
    Response(ResponseError),
    /// Server exited / stdout closed before responding.
    Exited { stderr_tail: String },
}

type PendingSender = oneshot::Sender<Result<Value, PendingFailure>>;

struct Inner {
    /// Displayed program name (for error messages/tracing).
    program: String,
    /// Server stdin; `None` once teardown started (closing it signals EOF).
    writer: AsyncMutex<Option<BoxWriter>>,
    pending: Mutex<HashMap<i64, PendingSender>>,
    next_id: AtomicI64,
    /// Push-diagnostics cache, keyed by the URI string exactly as the server
    /// sent it. Each publish replaces the previous entry for that URI.
    diagnostics: Mutex<HashMap<String, Vec<lsp_types::Diagnostic>>>,
    stderr_tail: Mutex<VecDeque<String>>,
    alive: AtomicBool,
}

impl Inner {
    fn stderr_tail_string(&self) -> String {
        match self.stderr_tail.lock() {
            Ok(tail) => {
                if tail.is_empty() {
                    "<empty>".to_string()
                } else {
                    tail.iter().cloned().collect::<Vec<_>>().join("\n")
                }
            }
            Err(_) => "<poisoned>".to_string(),
        }
    }

    fn fail_all_pending(&self) {
        let drained: Vec<PendingSender> = match self.pending.lock() {
            Ok(mut pending) => pending.drain().map(|(_, tx)| tx).collect(),
            Err(_) => Vec::new(),
        };
        let tail = self.stderr_tail_string();
        for tx in drained {
            let _ = tx.send(Err(PendingFailure::Exited {
                stderr_tail: tail.clone(),
            }));
        }
    }

    async fn send_frame(&self, message: &Value) -> Result<(), LspError> {
        let body = serde_json::to_vec(message)
            .map_err(|e| LspError::Protocol(format!("cannot serialize message: {e}")))?;
        let frame = encode_frame(&body);
        let mut guard = self.writer.lock().await;
        let Some(writer) = guard.as_mut() else {
            return Err(LspError::ServerExited {
                stderr_tail: self.stderr_tail_string(),
            });
        };
        writer.write_all(&frame).await?;
        writer.flush().await?;
        Ok(())
    }

    /// Default replies for server→client requests so the server never blocks
    /// on us. Unknown methods are answered with `-32601 MethodNotFound`.
    async fn answer_server_request(&self, id: RequestId, method: &str, params: &Value) {
        let reply = match method {
            // gopls asks for per-section config; `null` per item selects the
            // server defaults.
            "workspace/configuration" => {
                let len = params
                    .get("items")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                jsonrpc::response_ok(&id, Value::Array(vec![Value::Null; len]))
            }
            "client/registerCapability" | "client/unregisterCapability" => {
                jsonrpc::response_ok(&id, Value::Null)
            }
            "window/workDoneProgress/create" => jsonrpc::response_ok(&id, Value::Null),
            "window/showMessageRequest" => jsonrpc::response_ok(&id, Value::Null),
            "workspace/applyEdit" => {
                jsonrpc::response_ok(&id, serde_json::json!({ "applied": false }))
            }
            other => {
                tracing::debug!(method = other, "rejecting unknown server->client request");
                jsonrpc::response_err(&id, -32601, "method not supported by codescope")
            }
        };
        if let Err(error) = self.send_frame(&reply).await {
            tracing::warn!(%error, method, "failed to answer server->client request");
        }
    }

    fn handle_notification(&self, method: &str, params: Value) {
        match method {
            "textDocument/publishDiagnostics" => {
                match serde_json::from_value::<lsp_types::PublishDiagnosticsParams>(params) {
                    Ok(p) => {
                        let uri = p.uri.as_str().to_string();
                        let count = p.diagnostics.len();
                        if let Ok(mut cache) = self.diagnostics.lock() {
                            cache.insert(uri.clone(), p.diagnostics);
                        }
                        tracing::debug!(uri, count, "cached push diagnostics");
                    }
                    Err(error) => {
                        tracing::warn!(%error, "malformed publishDiagnostics params");
                    }
                }
            }
            "window/logMessage" | "window/showMessage" => {
                tracing::debug!(method, ?params, "server message");
            }
            other => {
                tracing::trace!(method = other, "ignoring server notification");
            }
        }
    }

    fn handle_response(&self, id: RequestId, result: Result<Value, ResponseError>) {
        let RequestId::Number(id) = id else {
            tracing::warn!(?id, "response with non-numeric id (we never send those)");
            return;
        };
        let sender = match self.pending.lock() {
            Ok(mut pending) => pending.remove(&id),
            Err(_) => None,
        };
        match sender {
            Some(tx) => {
                let _ = tx.send(result.map_err(PendingFailure::Response));
            }
            None => {
                // Late (post-timeout) or duplicate response: drop it.
                tracing::debug!(id, "response for unknown request id; dropping");
            }
        }
    }

    async fn handle_message(&self, body: &[u8]) {
        let value: Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(error) => {
                tracing::warn!(%error, "frame body is not valid JSON; skipping");
                return;
            }
        };
        match jsonrpc::classify(value) {
            Ok(Incoming::Response { id, result }) => self.handle_response(id, result),
            Ok(Incoming::Notification { method, params }) => {
                self.handle_notification(&method, params);
            }
            Ok(Incoming::ServerRequest { id, method, params }) => {
                self.answer_server_request(id, &method, &params).await;
            }
            Err(reason) => {
                tracing::warn!(reason, "unclassifiable JSON-RPC message; skipping");
            }
        }
    }
}

/// Handle to the underlying server process (absent for in-memory test streams).
enum ServerHandle {
    Child(Child),
    #[cfg(test)]
    Streams,
}

/// Outcome of [`LspClient::shutdown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownOutcome {
    /// The server exited on its own within the grace period.
    Graceful,
    /// The server had to be killed after [`SHUTDOWN_GRACE`].
    Killed,
}

/// Stdio JSON-RPC client for one language-server process.
///
/// Cheap to share behind `&self`; all methods take shared references. Consume
/// with [`LspClient::shutdown`] for a clean teardown.
pub struct LspClient {
    inner: Arc<Inner>,
    handle: ServerHandle,
    cancel: CancellationToken,
    reader_task: JoinHandle<()>,
    stderr_task: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for LspClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspClient")
            .field("program", &self.inner.program)
            .field("alive", &self.is_alive())
            .finish_non_exhaustive()
    }
}

impl LspClient {
    /// Spawn `command` with piped stdio and start the reader/stderr tasks.
    ///
    /// The command's stdin/stdout/stderr configuration is overridden;
    /// `kill_on_drop` is enabled as a last-resort safety net (prefer
    /// [`LspClient::shutdown`]).
    pub fn spawn(mut command: Command, program: impl Into<String>) -> Result<Self, LspError> {
        let program = program.into();
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|source| LspError::Spawn {
            program: program.clone(),
            source,
        })?;

        let stdin = child.stdin.take().ok_or_else(|| LspError::Protocol(
            "child stdin not captured".to_string(),
        ))?;
        let stdout = child.stdout.take().ok_or_else(|| LspError::Protocol(
            "child stdout not captured".to_string(),
        ))?;
        let stderr = child.stderr.take();

        tracing::info!(program, pid = child.id(), "language server spawned");
        Ok(Self::assemble(
            Box::new(stdin),
            Box::new(stdout),
            stderr.map(|e| Box::new(e) as BoxReader),
            ServerHandle::Child(child),
            program,
        ))
    }

    /// Build a client over in-memory streams (unit tests only).
    #[cfg(test)]
    pub(crate) fn from_streams(
        writer: impl AsyncWrite + Send + Unpin + 'static,
        reader: impl AsyncRead + Send + Unpin + 'static,
    ) -> Self {
        Self::assemble(
            Box::new(writer),
            Box::new(reader),
            None,
            ServerHandle::Streams,
            "test-server".to_string(),
        )
    }

    fn assemble(
        writer: BoxWriter,
        reader: BoxReader,
        stderr: Option<BoxReader>,
        handle: ServerHandle,
        program: String,
    ) -> Self {
        let inner = Arc::new(Inner {
            program,
            writer: AsyncMutex::new(Some(writer)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            diagnostics: Mutex::new(HashMap::new()),
            stderr_tail: Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)),
            alive: AtomicBool::new(true),
        });
        let cancel = CancellationToken::new();

        let reader_task = tokio::spawn(reader_loop(Arc::clone(&inner), reader, cancel.clone()));
        let stderr_task =
            stderr.map(|s| tokio::spawn(stderr_loop(Arc::clone(&inner), s, cancel.clone())));

        LspClient {
            inner,
            handle,
            cancel,
            reader_task,
            stderr_task,
        }
    }

    /// `false` once the server exited, closed stdout, or teardown started.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.inner.alive.load(Ordering::SeqCst)
    }

    /// Last captured stderr lines (joined), for diagnostics/error context.
    #[must_use]
    pub fn stderr_tail(&self) -> String {
        self.inner.stderr_tail_string()
    }

    /// Cached diagnostics for a file URI (as pushed by the server, **wire
    /// encoding** — callers convert positions). Empty when none were pushed.
    #[must_use]
    pub fn diagnostics(&self, uri: &str) -> Vec<lsp_types::Diagnostic> {
        self.inner
            .diagnostics
            .lock()
            .map(|cache| cache.get(uri).cloned().unwrap_or_default())
            .unwrap_or_default()
    }

    /// URIs with at least one cached diagnostic.
    #[must_use]
    pub fn diagnostic_uris(&self) -> Vec<String> {
        self.inner
            .diagnostics
            .lock()
            .map(|cache| {
                cache
                    .iter()
                    .filter(|(_, v)| !v.is_empty())
                    .map(|(k, _)| k.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Send a request and await its response (out-of-order safe).
    ///
    /// On timeout the pending entry is removed and a later response is
    /// logged + dropped.
    #[tracing::instrument(level = "debug", skip(self, params), fields(program = %self.inner.program))]
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, LspError> {
        if !self.is_alive() {
            return Err(LspError::ServerExited {
                stderr_tail: self.inner.stderr_tail_string(),
            });
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        if let Ok(mut pending) = self.inner.pending.lock() {
            pending.insert(id, tx);
        } else {
            return Err(LspError::Protocol("pending map poisoned".to_string()));
        }

        let message = jsonrpc::request(id, method, &params);
        if let Err(error) = self.inner.send_frame(&message).await {
            if let Ok(mut pending) = self.inner.pending.lock() {
                pending.remove(&id);
            }
            return Err(error);
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(PendingFailure::Response(err)))) => {
                if err.code == -32601 {
                    Err(LspError::MethodNotFound {
                        method: method.to_string(),
                    })
                } else {
                    Err(LspError::Response {
                        code: err.code,
                        message: err.message,
                    })
                }
            }
            Ok(Ok(Err(PendingFailure::Exited { stderr_tail }))) => {
                Err(LspError::ServerExited { stderr_tail })
            }
            Ok(Err(_recv)) => Err(LspError::ServerExited {
                stderr_tail: self.inner.stderr_tail_string(),
            }),
            Err(_elapsed) => {
                if let Ok(mut pending) = self.inner.pending.lock() {
                    pending.remove(&id);
                }
                tracing::warn!(method, ?timeout, "request timed out");
                Err(LspError::Timeout {
                    method: method.to_string(),
                    after: timeout,
                })
            }
        }
    }

    /// Send a notification (no response expected).
    #[tracing::instrument(level = "debug", skip(self, params), fields(program = %self.inner.program))]
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        self.inner
            .send_frame(&jsonrpc::notification(method, &params))
            .await
    }

    /// Graceful teardown: `shutdown` request → `exit` notification → close
    /// stdin → wait up to [`SHUTDOWN_GRACE`] → kill.
    ///
    /// Never returns an error for an uncooperative server (it gets killed);
    /// errors are reserved for local task failures.
    pub async fn shutdown(mut self) -> ShutdownOutcome {
        tracing::debug!(program = %self.inner.program, "shutting down language server");
        if let Err(error) = self
            .request("shutdown", Value::Null, SHUTDOWN_REQUEST_TIMEOUT)
            .await
        {
            tracing::debug!(%error, "shutdown request failed (continuing teardown)");
        }
        if let Err(error) = self.notify("exit", Value::Null).await {
            tracing::debug!(%error, "exit notification failed (continuing teardown)");
        }
        self.inner.alive.store(false, Ordering::SeqCst);
        // Close stdin: many servers treat EOF as a hard exit signal.
        self.inner.writer.lock().await.take();
        self.cancel.cancel();
        self.inner.fail_all_pending();

        let outcome = match &mut self.handle {
            ServerHandle::Child(child) => {
                match tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await {
                    Ok(Ok(status)) => {
                        tracing::info!(%status, "language server exited");
                        ShutdownOutcome::Graceful
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "wait for language server failed; killing");
                        let _ = child.start_kill();
                        ShutdownOutcome::Killed
                    }
                    Err(_elapsed) => {
                        tracing::warn!(
                            "language server did not exit within {SHUTDOWN_GRACE:?}; killing"
                        );
                        if let Err(error) = child.kill().await {
                            tracing::warn!(%error, "kill failed");
                        }
                        ShutdownOutcome::Killed
                    }
                }
            }
            #[cfg(test)]
            ServerHandle::Streams => ShutdownOutcome::Graceful,
        };

        self.reader_task.abort();
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
        outcome
    }
}

async fn reader_loop(inner: Arc<Inner>, mut reader: BoxReader, cancel: CancellationToken) {
    let mut decoder = FrameDecoder::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = tokio::select! {
            () = cancel.cancelled() => break,
            read = reader.read(&mut buf) => match read {
                Ok(0) => {
                    tracing::info!(program = %inner.program, "server stdout closed");
                    break;
                }
                Ok(n) => n,
                Err(error) => {
                    tracing::warn!(%error, program = %inner.program, "stdout read failed");
                    break;
                }
            },
        };
        for event in decoder.feed(&buf[..n]) {
            match event {
                FrameEvent::Message(body) => inner.handle_message(&body).await,
                FrameEvent::Skipped { reason } => {
                    tracing::warn!(reason, "skipped malformed frame");
                }
            }
        }
    }
    inner.alive.store(false, Ordering::SeqCst);
    inner.fail_all_pending();
}

async fn stderr_loop(inner: Arc<Inner>, stderr: BoxReader, cancel: CancellationToken) {
    let mut lines = BufReader::new(stderr).lines();
    loop {
        let line = tokio::select! {
            () = cancel.cancelled() => break,
            line = lines.next_line() => match line {
                Ok(Some(line)) => line,
                Ok(None) | Err(_) => break,
            },
        };
        tracing::trace!(program = %inner.program, line = %line, "server stderr");
        if let Ok(mut tail) = inner.stderr_tail.lock() {
            if tail.len() == STDERR_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::DuplexStream;

    /// Test harness: the "server" side of two duplex pipes.
    struct FakeServer {
        /// Server reads client requests from here.
        stdin: DuplexStream,
        /// Server writes responses into here.
        stdout: DuplexStream,
        decoder: FrameDecoder,
        /// Messages decoded from a previous read but not yet consumed.
        /// A single `read` can carry several frames; extras must be queued,
        /// not dropped (dropping them hangs the next `recv` forever).
        inbox: VecDeque<Value>,
    }

    fn client_and_server() -> (LspClient, FakeServer) {
        let (client_writer, server_stdin) = tokio::io::duplex(64 * 1024);
        let (server_stdout, client_reader) = tokio::io::duplex(64 * 1024);
        let client = LspClient::from_streams(client_writer, client_reader);
        (
            client,
            FakeServer {
                stdin: server_stdin,
                stdout: server_stdout,
                decoder: FrameDecoder::new(),
                inbox: VecDeque::new(),
            },
        )
    }

    impl FakeServer {
        /// Read one framed JSON message from the client.
        async fn recv(&mut self) -> Value {
            if let Some(msg) = self.inbox.pop_front() {
                return msg;
            }
            let mut buf = [0u8; 4096];
            loop {
                let n = self.stdin.read(&mut buf).await.unwrap();
                assert!(n > 0, "client closed stdin unexpectedly");
                for event in self.decoder.feed(&buf[..n]) {
                    if let FrameEvent::Message(body) = event {
                        self.inbox.push_back(serde_json::from_slice(&body).unwrap());
                    }
                }
                if let Some(msg) = self.inbox.pop_front() {
                    return msg;
                }
            }
        }

        async fn send(&mut self, message: &Value) {
            let body = serde_json::to_vec(message).unwrap();
            self.stdout.write_all(&encode_frame(&body)).await.unwrap();
            self.stdout.flush().await.unwrap();
        }

        async fn send_raw(&mut self, bytes: &[u8]) {
            self.stdout.write_all(bytes).await.unwrap();
            self.stdout.flush().await.unwrap();
        }
    }

    #[tokio::test]
    async fn matches_out_of_order_responses() {
        let (client, mut server) = client_and_server();
        let client = Arc::new(client);

        let c1 = Arc::clone(&client);
        let req_a = tokio::spawn(async move {
            c1.request("query/a", json!({"q": "a"}), Duration::from_secs(5))
                .await
        });
        let c2 = Arc::clone(&client);
        let req_b = tokio::spawn(async move {
            c2.request("query/b", json!({"q": "b"}), Duration::from_secs(5))
                .await
        });

        let first = server.recv().await;
        let second = server.recv().await;
        // Reply in reverse arrival order.
        for msg in [&second, &first] {
            let id = msg["id"].as_i64().unwrap();
            let method = msg["method"].as_str().unwrap().to_string();
            server
                .send(&json!({"jsonrpc":"2.0","id":id,"result":{"echo":method}}))
                .await;
        }

        let res_a = req_a.await.unwrap().unwrap();
        let res_b = req_b.await.unwrap().unwrap();
        assert_eq!(res_a, json!({"echo":"query/a"}));
        assert_eq!(res_b, json!({"echo":"query/b"}));
    }

    #[tokio::test]
    async fn skips_malformed_frames_and_recovers() {
        let (client, mut server) = client_and_server();

        let request = tokio::spawn({
            let client = Arc::new(client);
            let c = Arc::clone(&client);
            async move {
                c.request("ping", Value::Null, Duration::from_secs(5))
                    .await
            }
        });

        let msg = server.recv().await;
        let id = msg["id"].as_i64().unwrap();
        // Garbage first, then a valid response in the same stream.
        server.send_raw(b"not a header at all\r\n\r\n").await;
        server
            .send(&json!({"jsonrpc":"2.0","id":id,"result":"pong"}))
            .await;

        assert_eq!(request.await.unwrap().unwrap(), json!("pong"));
    }

    #[tokio::test]
    async fn caches_push_diagnostics_per_file_and_replaces() {
        let (client, mut server) = client_and_server();
        let uri = "file:///w/main.go";

        server
            .send(&json!({
                "jsonrpc":"2.0",
                "method":"textDocument/publishDiagnostics",
                "params":{"uri":uri,"diagnostics":[
                    {"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":4}},
                     "message":"first"}
                ]}
            }))
            .await;
        // Wait until the cache is populated (reader task is async).
        for _ in 0..100 {
            if !client.diagnostics(uri).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let diags = client.diagnostics(uri);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "first");
        assert_eq!(client.diagnostic_uris(), vec![uri.to_string()]);

        // A new publish replaces the previous set (LSP semantics).
        server
            .send(&json!({
                "jsonrpc":"2.0",
                "method":"textDocument/publishDiagnostics",
                "params":{"uri":uri,"diagnostics":[]}
            }))
            .await;
        for _ in 0..100 {
            if client.diagnostics(uri).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(client.diagnostics(uri).is_empty());
        assert!(client.diagnostic_uris().is_empty());
    }

    #[tokio::test]
    async fn answers_workspace_configuration_with_nulls() {
        let (_client, mut server) = client_and_server();

        server
            .send(&json!({
                "jsonrpc":"2.0",
                "id":"cfg-1",
                "method":"workspace/configuration",
                "params":{"items":[{"section":"gopls"},{"section":"other"}]}
            }))
            .await;

        let reply = server.recv().await;
        assert_eq!(reply["id"], json!("cfg-1"));
        assert_eq!(reply["result"], json!([null, null]));
    }

    #[tokio::test]
    async fn rejects_unknown_server_request_with_method_not_found() {
        let (_client, mut server) = client_and_server();
        server
            .send(&json!({
                "jsonrpc":"2.0","id":5,"method":"workspace/weirdThing","params":{}
            }))
            .await;
        let reply = server.recv().await;
        assert_eq!(reply["id"], json!(5));
        assert_eq!(reply["error"]["code"], json!(-32601));
    }

    #[tokio::test]
    async fn timeout_removes_pending_and_late_response_is_dropped() {
        let (client, mut server) = client_and_server();
        let client = Arc::new(client);

        let err = client
            .request("slow/thing", Value::Null, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(matches!(err, LspError::Timeout { .. }));

        // The request reached the server; reply late — must be dropped silently.
        let msg = server.recv().await;
        let id = msg["id"].as_i64().unwrap();
        server
            .send(&json!({"jsonrpc":"2.0","id":id,"result":"late"}))
            .await;

        // Client still fully functional for the next request.
        let c = Arc::clone(&client);
        let next = tokio::spawn(async move {
            c.request("fast/thing", Value::Null, Duration::from_secs(5))
                .await
        });
        let msg = server.recv().await;
        let id = msg["id"].as_i64().unwrap();
        server
            .send(&json!({"jsonrpc":"2.0","id":id,"result":"ok"}))
            .await;
        assert_eq!(next.await.unwrap().unwrap(), json!("ok"));
    }

    #[tokio::test]
    async fn server_error_response_maps_to_lsp_error() {
        let (client, mut server) = client_and_server();
        let client = Arc::new(client);
        let c = Arc::clone(&client);
        let req = tokio::spawn(async move {
            c.request("bad/request", Value::Null, Duration::from_secs(5))
                .await
        });
        let msg = server.recv().await;
        let id = msg["id"].as_i64().unwrap();
        server
            .send(&json!({"jsonrpc":"2.0","id":id,
                "error":{"code":-32602,"message":"invalid params"}}))
            .await;
        let err = req.await.unwrap().unwrap_err();
        assert!(
            matches!(err, LspError::Response { code: -32602, ref message } if message == "invalid params")
        );

        // -32601 maps to the dedicated MethodNotFound variant.
        let c = Arc::clone(&client);
        let req = tokio::spawn(async move {
            c.request("missing/method", Value::Null, Duration::from_secs(5))
                .await
        });
        let msg = server.recv().await;
        let id = msg["id"].as_i64().unwrap();
        server
            .send(&json!({"jsonrpc":"2.0","id":id,
                "error":{"code":-32601,"message":"unknown"}}))
            .await;
        assert!(matches!(
            req.await.unwrap().unwrap_err(),
            LspError::MethodNotFound { ref method } if method == "missing/method"
        ));
    }

    #[tokio::test]
    async fn server_exit_fails_pending_requests() {
        let (client, server) = client_and_server();
        let client = Arc::new(client);
        let c = Arc::clone(&client);
        let req = tokio::spawn(async move {
            c.request("hang/forever", Value::Null, Duration::from_secs(30))
                .await
        });
        // Give the request a moment to be registered, then "crash" the server.
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(server);

        let err = req.await.unwrap().unwrap_err();
        assert!(matches!(err, LspError::ServerExited { .. }));
        assert!(!client.is_alive());

        // Further requests fail fast.
        let err = client
            .request("anything", Value::Null, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(matches!(err, LspError::ServerExited { .. }));
    }
}

