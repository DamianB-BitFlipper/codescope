//! Scripted fake AI provider: an OpenAI-compatible chat-completions HTTP server.
//!
//! [`ScriptedProvider`] binds a raw [`tokio::net::TcpListener`] on `127.0.0.1:0` — no web
//! framework — and answers each incoming HTTP request by consuming the next
//! [`AiScriptStep`] from its queue (research 08 §3):
//!
//! - [`AiScriptStep::valid_plan`] — a schema-valid `submit_visualization_plan` tool call
//!   built from [`codescope_core`] plan types, echoing **any** epoch you script (so stale
//!   / arbitrary-epoch handling is testable);
//! - [`AiScriptStep::hallucinated_plan`] — same shape, but every entity points at files
//!   and symbols that exist nowhere (the validator must drop them);
//! - [`AiScriptStep::malformed_json`] — a tool call whose `arguments` string is not valid
//!   JSON;
//! - [`AiScriptStep::RateLimited`] — HTTP 429 with a `Retry-After` header;
//! - [`AiScriptStep::Hang`] — accepts the request and never responds until the provider
//!   is aborted/dropped;
//! - [`AiScriptStep::Raw`] — full control over status/content-type/body.
//!
//! Every request is recorded ([`ScriptedProvider::requests`]) for assertions on what the
//! client actually sent. When the script runs dry the provider answers `500` with an
//! explanatory JSON error rather than panicking.

use crate::error::{Result, TestutilError};
use codescope_core::{Epoch, EntityRef, FileId, FormKind, PlanNode, PlanNodeChange, VisualizationPlan, VizForm};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

/// Tool name the provider's plan responses call (research 05).
pub const PLAN_TOOL_NAME: &str = "submit_visualization_plan";

/// Model name reported in fake completions.
pub const FAKE_MODEL: &str = "codescope-fake";

/// Fixed `created` timestamp in fake completions: 2026-01-01T00:00:00Z.
pub const FAKE_CREATED: u64 = 1_767_225_600;

/// Cap on inbound HTTP heads (64 KiB).
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// Cap on inbound HTTP bodies (16 MiB).
const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

/// One scripted provider behavior, consumed per incoming request (FIFO).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AiScriptStep {
    /// `200`: completion whose single tool call carries `plan` (serialized) as the
    /// `arguments` string of [`PLAN_TOOL_NAME`].
    ToolCallPlan {
        /// The plan JSON value to serialize into `function.arguments`.
        plan: Value,
    },
    /// `200`: tool call whose `arguments` string is sent **verbatim** — use for malformed
    /// JSON arguments.
    ToolCallRaw {
        /// Raw `function.arguments` string.
        arguments: String,
    },
    /// `200`: assistant message with plain text `content` and no tool call (a provider
    /// that ignored `tool_choice`).
    AssistantText {
        /// The message content.
        content: String,
    },
    /// `429 Too Many Requests` with a `Retry-After` header.
    RateLimited {
        /// Value of the `Retry-After` header, in seconds.
        retry_after_secs: u64,
    },
    /// Arbitrary response (malformed bodies, wrong content types, 5xx, …).
    Raw {
        /// HTTP status code.
        status: u16,
        /// `Content-Type` header value.
        content_type: String,
        /// Response body.
        body: String,
    },
    /// Accept the request, record it, and never respond. The connection stays open until
    /// the provider is aborted or dropped ("hang until abort").
    Hang,
}

impl AiScriptStep {
    /// A tool-call step from a typed plan. Fails only if the plan fails to serialize
    /// (structurally impossible for [`VisualizationPlan`], but surfaced honestly).
    pub fn from_plan(plan: &VisualizationPlan) -> Result<Self> {
        Ok(AiScriptStep::ToolCallPlan {
            plan: serde_json::to_value(plan)?,
        })
    }

    /// A schema-valid plan over real fixture entities, echoing `epoch` (script an old
    /// epoch to test stale-plan handling).
    pub fn valid_plan(epoch: Epoch) -> Result<Self> {
        Self::from_plan(&sample_plan(epoch))
    }

    /// A schema-valid plan whose entities are hallucinated (nonexistent files/symbols);
    /// the validation boundary must drop or reject them.
    pub fn hallucinated_plan(epoch: Epoch) -> Result<Self> {
        Self::from_plan(&hallucinated_sample_plan(epoch))
    }

    /// A tool call whose `arguments` string is truncated, syntactically invalid JSON.
    #[must_use]
    pub fn malformed_json() -> Self {
        AiScriptStep::ToolCallRaw {
            arguments: r#"{"plan_version":1,"epoch":"#.to_string(),
        }
    }
}

/// A well-formed sample [`VisualizationPlan`] whose entities are real files/symbols of the
/// [`go_fixture`](crate::go_fixture) (the feature-branch changes), echoing `epoch`.
#[must_use]
pub fn sample_plan(epoch: Epoch) -> VisualizationPlan {
    let mut plan = VisualizationPlan::new(epoch, "What changed on feature/api-changes?");
    let middleware = PlanNode::new("n1", "LoggingMiddleware", PlanNodeChange::Added)
        .with_entity(EntityRef::for_symbol(
            FileId::new_unchecked(crate::go_fixture::MIDDLEWARE_FILE),
            "LoggingMiddleware",
            None,
        ));
    let postgres_get = PlanNode::new("n2", "(PostgresRepo).Get", PlanNodeChange::Modified)
        .with_entity(EntityRef::for_symbol(
            FileId::new_unchecked(crate::go_fixture::POSTGRES_FILE),
            "(PostgresRepo).Get",
            None,
        ));
    plan.forms.push(VizForm {
        kind: FormKind::ImpactSummary,
        title: "Feature branch impact".to_string(),
        summary: "Adds request logging middleware; hardens PostgresRepo.Get.".to_string(),
        nodes: vec![middleware, postgres_get],
        edges: Vec::new(),
    });
    plan
}

/// Like [`sample_plan`] but with entities that resolve to nothing anywhere in the fixture.
#[must_use]
pub fn hallucinated_sample_plan(epoch: Epoch) -> VisualizationPlan {
    let mut plan = VisualizationPlan::new(epoch, "What changed on feature/api-changes?");
    let ghost = PlanNode::new("n1", "QuantumFluxHandler", PlanNodeChange::Modified).with_entity(
        EntityRef::for_symbol(
            FileId::new_unchecked("internal/api/quantum_flux.go"),
            "QuantumFluxHandler",
            None,
        ),
    );
    plan.forms.push(VizForm {
        kind: FormKind::ImpactSummary,
        title: "Imaginary impact".to_string(),
        summary: "References entities that do not exist.".to_string(),
        nodes: vec![ghost],
        edges: Vec::new(),
    });
    plan
}

/// One recorded inbound HTTP request.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordedRequest {
    /// HTTP method (`POST`, …).
    pub method: String,
    /// Request path (`/v1/chat/completions`, …).
    pub path: String,
    /// Headers, names lowercased.
    pub headers: BTreeMap<String, String>,
    /// Raw body.
    pub body: String,
}

impl RecordedRequest {
    /// The body parsed as JSON, when it is JSON.
    #[must_use]
    pub fn body_json(&self) -> Option<Value> {
        serde_json::from_str(&self.body).ok()
    }
}

struct ProviderState {
    steps: Mutex<VecDeque<AiScriptStep>>,
    requests: Mutex<Vec<RecordedRequest>>,
    calls: AtomicU64,
}

/// The scripted fake AI provider server. Aborts its listener (and any hung connections)
/// on [`ScriptedProvider::abort`] or drop.
#[derive(Debug)]
pub struct ScriptedProvider {
    addr: SocketAddr,
    state: Arc<ProviderState>,
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for ProviderState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderState")
            .field("calls", &self.calls.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl ScriptedProvider {
    /// Bind `127.0.0.1:0` and start serving `steps`. Must be called inside a tokio
    /// runtime.
    pub async fn start(steps: impl IntoIterator<Item = AiScriptStep>) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| TestutilError::Net(format!("bind 127.0.0.1:0: {e}")))?;
        let addr = listener
            .local_addr()
            .map_err(|e| TestutilError::Net(format!("local_addr: {e}")))?;
        let state = Arc::new(ProviderState {
            steps: Mutex::new(steps.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            calls: AtomicU64::new(0),
        });
        let loop_state = Arc::clone(&state);
        let task = tokio::spawn(async move {
            // Dropping the JoinSet (when this task is aborted) aborts every open
            // connection, which is what ends `Hang` steps.
            let mut connections = JoinSet::new();
            loop {
                while connections.try_join_next().is_some() {}
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        tracing::debug!(%peer, "fake-ai: connection accepted");
                        let conn_state = Arc::clone(&loop_state);
                        connections.spawn(async move {
                            if let Err(e) = handle_connection(stream, conn_state).await {
                                tracing::debug!(error = %e, "fake-ai: connection ended with error");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "fake-ai: accept failed");
                    }
                }
            }
        });
        tracing::info!(%addr, "fake-ai provider listening");
        Ok(ScriptedProvider { addr, state, task })
    }

    /// Bound address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Base URL, e.g. `http://127.0.0.1:PORT` (append `/v1` per client convention).
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Full chat-completions endpoint URL.
    #[must_use]
    pub fn chat_completions_url(&self) -> String {
        format!("{}/v1/chat/completions", self.base_url())
    }

    /// Append a step to the script queue (usable while serving).
    pub fn push_step(&self, step: AiScriptStep) {
        lock_ignore_poison(&self.state.steps).push_back(step);
    }

    /// Number of unconsumed script steps.
    #[must_use]
    pub fn remaining_steps(&self) -> usize {
        lock_ignore_poison(&self.state.steps).len()
    }

    /// Snapshot of every request received so far (including hung ones).
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        lock_ignore_poison(&self.state.requests).clone()
    }

    /// Stop the listener and abort all open connections (ends `Hang` steps).
    pub fn abort(&self) {
        self.task.abort();
    }
}

impl Drop for ScriptedProvider {
    fn drop(&mut self) {
        self.abort();
    }
}

/// Serve exactly one HTTP request on `stream` according to the next script step.
async fn handle_connection(mut stream: TcpStream, state: Arc<ProviderState>) -> Result<()> {
    let request = match read_request(&mut stream).await {
        Ok(req) => req,
        Err(e) => {
            let body = json!({"error": {"message": format!("bad request: {e}"), "type": "bad_request"}});
            write_http(&mut stream, 400, "Bad Request", &[], "application/json", body.to_string().as_bytes()).await?;
            return Err(e);
        }
    };
    tracing::debug!(method = %request.method, path = %request.path, "fake-ai: request");
    lock_ignore_poison(&state.requests).push(request);
    let step = lock_ignore_poison(&state.steps).pop_front();
    let call = state.calls.fetch_add(1, Ordering::Relaxed);

    match step {
        Some(AiScriptStep::ToolCallPlan { plan }) => {
            let completion = tool_call_completion(call, plan.to_string());
            write_json(&mut stream, 200, "OK", &completion).await
        }
        Some(AiScriptStep::ToolCallRaw { arguments }) => {
            let completion = tool_call_completion(call, arguments);
            write_json(&mut stream, 200, "OK", &completion).await
        }
        Some(AiScriptStep::AssistantText { content }) => {
            let message = json!({"role": "assistant", "content": content});
            let completion = chat_completion(call, message, "stop");
            write_json(&mut stream, 200, "OK", &completion).await
        }
        Some(AiScriptStep::RateLimited { retry_after_secs }) => {
            let body = json!({
                "error": {"message": "rate limited by script", "type": "rate_limit_exceeded"}
            });
            let headers = [("Retry-After".to_string(), retry_after_secs.to_string())];
            write_http(&mut stream, 429, "Too Many Requests", &headers, "application/json", body.to_string().as_bytes()).await
        }
        Some(AiScriptStep::Raw { status, content_type, body }) => {
            write_http(&mut stream, status, "Scripted", &[], &content_type, body.as_bytes()).await
        }
        Some(AiScriptStep::Hang) => {
            tracing::debug!("fake-ai: hanging per script (until provider abort)");
            std::future::pending::<()>().await;
            Ok(())
        }
        None => {
            let body = json!({
                "error": {"message": "fake-ai script exhausted: no step for this request", "type": "script_exhausted"}
            });
            write_json(&mut stream, 500, "Internal Server Error", &body).await
        }
    }
}

/// OpenAI-shaped completion with a single [`PLAN_TOOL_NAME`] tool call.
fn tool_call_completion(call: u64, arguments: String) -> Value {
    let message = json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [{
            "id": format!("call_fake_{call}"),
            "type": "function",
            "function": {"name": PLAN_TOOL_NAME, "arguments": arguments}
        }]
    });
    chat_completion(call, message, "tool_calls")
}

/// OpenAI-shaped chat completion envelope.
fn chat_completion(call: u64, message: Value, finish_reason: &str) -> Value {
    json!({
        "id": format!("chatcmpl-fake-{call}"),
        "object": "chat.completion",
        "created": FAKE_CREATED,
        "model": FAKE_MODEL,
        "choices": [{
            "index": 0,
            "message": message,
            "logprobs": null,
            "finish_reason": finish_reason
        }],
        "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
    })
}

// ---------------------------------------------------------------------------
// minimal HTTP/1.1 plumbing (deliberately no web framework)
// ---------------------------------------------------------------------------

/// Read one HTTP/1.1 request (head + `Content-Length` body) from `stream`.
async fn read_request(stream: &mut TcpStream) -> Result<RecordedRequest> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let head_end = loop {
        if let Some(pos) = find_head_end(&buf) {
            break pos;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(TestutilError::Protocol("http head exceeds 64KiB".to_string()));
        }
        let mut chunk = [0u8; 4096];
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| TestutilError::Protocol(format!("read: {e}")))?;
        if n == 0 {
            return Err(TestutilError::Protocol(
                "connection closed before full http head".to_string(),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let (method, path, headers) = parse_head(&head)?;

    let content_length: u64 = match headers.get("content-length") {
        Some(v) => v
            .parse()
            .map_err(|e| TestutilError::Protocol(format!("bad content-length {v:?}: {e}")))?,
        None => 0,
    };
    if content_length > MAX_BODY_BYTES {
        return Err(TestutilError::Protocol(format!(
            "http body of {content_length} bytes exceeds cap"
        )));
    }

    let mut body = buf[head_end + 4..].to_vec();
    while (body.len() as u64) < content_length {
        let mut chunk = [0u8; 4096];
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| TestutilError::Protocol(format!("read body: {e}")))?;
        if n == 0 {
            return Err(TestutilError::Protocol(
                "connection closed before full body".to_string(),
            ));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length as usize);

    Ok(RecordedRequest {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// Locate the `\r\n\r\n` head terminator.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Parse an HTTP/1.1 head into (method, path, lowercased headers).
fn parse_head(head: &str) -> Result<(String, String, BTreeMap<String, String>)> {
    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| TestutilError::Protocol("empty http head".to_string()))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| TestutilError::Protocol("missing http method".to_string()))?
        .to_string();
    let path = parts
        .next()
        .ok_or_else(|| TestutilError::Protocol("missing http path".to_string()))?
        .to_string();
    let mut headers = BTreeMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    Ok((method, path, headers))
}

async fn write_json(stream: &mut TcpStream, status: u16, reason: &str, body: &Value) -> Result<()> {
    write_http(stream, status, reason, &[], "application/json", body.to_string().as_bytes()).await
}

/// Write a complete HTTP/1.1 response and close the write side.
async fn write_http(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    extra_headers: &[(String, String)],
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (name, value) in extra_headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let io_err = |e: std::io::Error| TestutilError::Protocol(format!("write: {e}"));
    stream.write_all(head.as_bytes()).await.map_err(io_err)?;
    stream.write_all(body).await.map_err(io_err)?;
    stream.flush().await.map_err(io_err)?;
    stream.shutdown().await.map_err(io_err)?;
    Ok(())
}

fn lock_ignore_poison<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codescope_core::{MAX_FORMS_PER_PLAN, MAX_FORM_NODES, PLAN_VERSION};

    #[test]
    fn sample_plan_is_schema_valid_and_echoes_epoch() {
        let plan = sample_plan(Epoch(42));
        assert_eq!(plan.plan_version, PLAN_VERSION);
        assert_eq!(plan.epoch, Epoch(42));
        assert!(plan.forms.len() <= MAX_FORMS_PER_PLAN);
        assert!(plan.forms.iter().all(|f| f.nodes.len() <= MAX_FORM_NODES));
        // Round-trips through the exact wire shape the AI layer parses.
        let value = serde_json::to_value(&plan).unwrap();
        let back: VisualizationPlan = serde_json::from_value(value).unwrap();
        assert_eq!(back, plan);
    }

    #[test]
    fn valid_and_hallucinated_plans_differ_in_entities() {
        let AiScriptStep::ToolCallPlan { plan: valid } =
            AiScriptStep::valid_plan(Epoch(1)).unwrap()
        else {
            panic!("expected ToolCallPlan");
        };
        let AiScriptStep::ToolCallPlan { plan: ghost } =
            AiScriptStep::hallucinated_plan(Epoch(1)).unwrap()
        else {
            panic!("expected ToolCallPlan");
        };
        let valid_files: Vec<&str> = valid["forms"][0]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["entity"]["file"].as_str().unwrap())
            .collect();
        assert!(valid_files.contains(&crate::go_fixture::MIDDLEWARE_FILE));
        assert_eq!(
            ghost["forms"][0]["nodes"][0]["entity"]["file"],
            "internal/api/quantum_flux.go"
        );
    }

    #[test]
    fn malformed_json_step_is_actually_malformed() {
        let AiScriptStep::ToolCallRaw { arguments } = AiScriptStep::malformed_json() else {
            panic!("expected ToolCallRaw");
        };
        assert!(serde_json::from_str::<Value>(&arguments).is_err());
    }

    #[test]
    fn script_steps_serde_roundtrip() {
        let steps = vec![
            AiScriptStep::valid_plan(Epoch(3)).unwrap(),
            AiScriptStep::malformed_json(),
            AiScriptStep::AssistantText { content: "hi".to_string() },
            AiScriptStep::RateLimited { retry_after_secs: 2 },
            AiScriptStep::Raw {
                status: 503,
                content_type: "text/plain".to_string(),
                body: "nope".to_string(),
            },
            AiScriptStep::Hang,
        ];
        let json = serde_json::to_string(&steps).unwrap();
        let back: Vec<AiScriptStep> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, steps);
    }

    #[test]
    fn completion_envelope_shape() {
        let completion = tool_call_completion(7, "{}".to_string());
        assert_eq!(completion["object"], "chat.completion");
        assert_eq!(completion["created"], FAKE_CREATED);
        assert_eq!(completion["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            completion["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            PLAN_TOOL_NAME
        );
        assert_eq!(
            completion["choices"][0]["message"]["tool_calls"][0]["id"],
            "call_fake_7"
        );
    }

    #[test]
    fn parse_head_extracts_request_line_and_headers() {
        let head = "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Length: 12\r\nAuthorization: Bearer k";
        let (method, path, headers) = parse_head(head).unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/v1/chat/completions");
        assert_eq!(headers["content-length"], "12");
        assert_eq!(headers["authorization"], "Bearer k");
        assert!(parse_head("").is_err());
        assert!(parse_head("GET").is_err());
    }

    #[test]
    fn find_head_end_positions() {
        assert_eq!(find_head_end(b"ab\r\n\r\ncd"), Some(2));
        assert_eq!(find_head_end(b"abcd"), None);
    }
}
