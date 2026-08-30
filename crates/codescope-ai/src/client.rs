//! Provider-neutral OpenAI-compatible chat-completions client (research 05 §5, 07 §4).
//!
//! One endpoint: `POST {base_url}/chat/completions` with `tool_choice: "required"` and the
//! `submit_visualization_plan` tool always attached, so every completion must end in a
//! tool call. Streaming is off (`"stream": false`): a plan is atomic — it must be complete
//! and validated before render.
//!
//! Local protections (all before/around the network call):
//!
//! - **token bucket** (`governor`, default 10 requests/minute) → [`AiError::Throttled`]
//!   without any I/O;
//! - **circuit breaker**: 3 transport failures (connect/timeout/5xx) within 60 s opens the
//!   circuit for 60 s → [`AiError::CircuitOpen`] without any I/O; one probe is allowed
//!   after cooldown;
//! - **per-request timeout** (config, default 20 s) → [`AiError::Timeout`];
//! - HTTP 429 → [`AiError::RateLimited`] carrying the parsed `Retry-After` (capped at
//!   30 s); it is *not* a circuit-breaker strike — the retry layer in
//!   [`AiService`](crate::AiService) honors it.
//!
//! Privacy: the `Authorization` header is built from the [`secrecy`]-wrapped key at request
//! time and marked sensitive; message/tool contents are never logged (counts only); reqwest
//! errors are sanitized with [`reqwest::Error::without_url`].

use crate::config::{AiConfig, ProviderKind};
use crate::error::AiError;
use crate::plan::plan_tool;
use crate::tools::{ToolDef, PLAN_TOOL_NAME};
use governor::clock::{Clock, QuantaClock};
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Cap applied to provider `Retry-After` values (research 07 §4).
pub const RETRY_AFTER_CAP: Duration = Duration::from_secs(30);

/// One chat message, stored as its OpenAI wire object.
///
/// A thin newtype over [`Value`] keeps the client wire-exact (assistant messages with tool
/// calls are echoed back verbatim in the tool loop) without modeling the whole OpenAI
/// surface.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct ChatMessage(Value);

impl ChatMessage {
    /// A `system` message.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        ChatMessage(json!({"role": "system", "content": content.into()}))
    }

    /// A `user` message.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        ChatMessage(json!({"role": "user", "content": content.into()}))
    }

    /// A `tool` result message answering `tool_call_id`.
    #[must_use]
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        ChatMessage(json!({
            "role": "tool",
            "tool_call_id": tool_call_id.into(),
            "content": content.into(),
        }))
    }

    /// An assistant message echoed verbatim (used to feed tool calls back into the
    /// conversation).
    #[must_use]
    pub fn assistant_raw(message: Value) -> Self {
        ChatMessage(message)
    }

    /// Borrow the wire object.
    #[must_use]
    pub fn as_value(&self) -> &Value {
        &self.0
    }
}

/// One tool call extracted from the completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawToolCall {
    /// Provider-assigned call id (echoed in the tool result message).
    pub id: String,
    /// Function name.
    pub name: String,
    /// Raw `function.arguments` JSON text (unparsed — may be malformed).
    pub arguments: String,
}

/// A parsed chat completion: the raw assistant message plus its tool calls.
///
/// The plan, when submitted, is the `submit_visualization_plan` call's arguments
/// ([`RawPlanResponse::plan_arguments`]); any other calls are read-only tool requests the
/// service must execute and answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPlanResponse {
    /// The assistant message exactly as received (echo it into the conversation when
    /// answering tool calls).
    pub message: Value,
    /// All tool calls in the message, in order.
    pub tool_calls: Vec<RawToolCall>,
    /// Model the provider reports having used.
    pub model: Option<String>,
}

impl RawPlanResponse {
    /// Arguments of the first `submit_visualization_plan` call, if present: the JSON plan
    /// text.
    #[must_use]
    pub fn plan_arguments(&self) -> Option<&str> {
        self.tool_calls
            .iter()
            .find(|c| c.name == PLAN_TOOL_NAME)
            .map(|c| c.arguments.as_str())
    }

    /// Tool calls other than plan submission (the read-only surface).
    pub fn read_only_calls(&self) -> impl Iterator<Item = &RawToolCall> {
        self.tool_calls.iter().filter(|c| c.name != PLAN_TOOL_NAME)
    }
}

/// Client knobs beyond [`AiConfig`], with production defaults; tests tighten/loosen them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiClientOptions {
    /// Token-bucket rate: requests per minute (research 07 §4: 10 rpm).
    pub requests_per_minute: u32,
    /// Token-bucket burst. Defaults to `requests_per_minute` so one full tool loop
    /// (≤ 8 tool turns + submission) fits a single burst.
    pub burst: u32,
    /// Circuit breaker: this many transport failures within [`Self::failure_window`] open
    /// the circuit.
    pub failure_threshold: u32,
    /// Sliding window for counting transport failures.
    pub failure_window: Duration,
    /// How long the circuit stays open before a probe is allowed.
    pub cooldown: Duration,
    /// TCP/TLS connect timeout (research 07 §4: 5 s).
    pub connect_timeout: Duration,
}

impl Default for AiClientOptions {
    fn default() -> Self {
        AiClientOptions {
            requests_per_minute: 10,
            burst: 10,
            failure_threshold: 3,
            failure_window: Duration::from_secs(60),
            cooldown: Duration::from_secs(60),
            connect_timeout: Duration::from_secs(5),
        }
    }
}

/// Circuit-breaker state (interior-mutable behind a mutex).
#[derive(Debug, Default)]
struct BreakerState {
    failures: VecDeque<Instant>,
    open_until: Option<Instant>,
}

type DirectLimiter = RateLimiter<NotKeyed, InMemoryState, QuantaClock>;

/// The chat-completions client. Constructed only when AI is enabled.
pub struct AiClient {
    http: reqwest::Client,
    endpoint: String,
    model: Mutex<String>,
    api_key: Option<SecretString>,
    timeout: Duration,
    provider: ProviderKind,
    limiter: DirectLimiter,
    clock: QuantaClock,
    breaker: Mutex<BreakerState>,
    options: AiClientOptions,
}

impl std::fmt::Debug for AiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiClient")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "«redacted»"))
            .field("timeout", &self.timeout)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl AiClient {
    /// Build a client from an enabled config with default [`AiClientOptions`].
    ///
    /// Errors with [`AiError::Disabled`] when `config.enabled` is false — no HTTP client
    /// exists while AI is off (research 07 §2).
    pub fn new(config: &AiConfig) -> Result<Self, AiError> {
        Self::with_options(config, AiClientOptions::default())
    }

    /// Build a client with explicit options (tests tighten timeouts/limits).
    pub fn with_options(config: &AiConfig, options: AiClientOptions) -> Result<Self, AiError> {
        if !config.enabled {
            return Err(AiError::Disabled);
        }
        let rpm = NonZeroU32::new(options.requests_per_minute)
            .ok_or_else(|| AiError::Config("requests_per_minute must be > 0".into()))?;
        let burst = NonZeroU32::new(options.burst)
            .ok_or_else(|| AiError::Config("burst must be > 0".into()))?;
        let http = reqwest::Client::builder()
            .connect_timeout(options.connect_timeout)
            .build()
            .map_err(|e| AiError::Config(format!("http client: {}", e.without_url())))?;
        let clock = QuantaClock::default();
        let limiter =
            RateLimiter::direct_with_clock(Quota::per_minute(rpm).allow_burst(burst), clock.clone());
        let base = config.base_url.trim_end_matches('/');
        let provider = config.provider();
        let endpoint = match provider {
            ProviderKind::OpenAiCompatible => format!("{base}/chat/completions"),
            ProviderKind::Anthropic => format!("{base}/messages"),
        };
        Ok(AiClient {
            http,
            endpoint,
            model: Mutex::new(config.model.clone()),
            api_key: config.api_key.clone(),
            timeout: config.timeout,
            provider,
            limiter,
            clock,
            breaker: Mutex::new(BreakerState::default()),
            options,
        })
    }

    /// The resolved chat-completions endpoint URL.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The provider protocol this client speaks.
    #[must_use]
    pub fn provider(&self) -> ProviderKind {
        self.provider
    }

    /// The model currently sent in requests.
    #[must_use]
    pub fn model(&self) -> String {
        self.model.lock().map(|m| m.clone()).unwrap_or_default()
    }

    /// Switch the model used for subsequent requests (the TUI model picker).
    ///
    /// Cheap: only the request-body `model` field changes; no reconnect is needed.
    pub fn set_model(&self, model: impl Into<String>) {
        let model = model.into();
        tracing::info!(model = %model, "ai model changed");
        if let Ok(mut m) = self.model.lock() {
            *m = model;
        }
    }

    /// List the models the provider exposes (`GET {base}/models`).
    ///
    /// Both OpenAI-compatible providers and Anthropic implement this endpoint; the response
    /// is normalized to plain id strings. Returns an empty list on a provider that errors.
    pub async fn list_models(&self) -> Result<Vec<String>, AiError> {
        let base = self
            .endpoint
            .rsplit_once('/')
            .map(|(b, _)| b)
            .unwrap_or(&self.endpoint);
        // endpoint is {base}/chat/completions or {base}/messages; go up one segment.
        let base = base.rsplit_once('/').map(|(b, _)| b).unwrap_or(base);
        let url = format!("{base}/models");
        self.check_breaker()?;
        self.check_limiter()?;
        let request = self.apply_auth(self.http.get(&url).timeout(self.timeout));
        let response = match request.send().await {
            Ok(r) => r,
            Err(e) => {
                if e.is_timeout() {
                    return Err(AiError::Timeout(self.timeout));
                }
                return Err(AiError::Transport(e.without_url().to_string()));
            }
        };
        if !response.status().is_success() {
            return Err(AiError::Http {
                status: response.status().as_u16(),
                message: body_snippet(response.text().await.unwrap_or_default()),
            });
        }
        let body: Value = response
            .json()
            .await
            .map_err(|e| AiError::MalformedResponse(e.without_url().to_string()))?;
        Ok(parse_model_list(&body))
    }

    /// Build the provider-shaped request body.
    fn build_body(&self, messages: &[ChatMessage], tool_values: &[Value]) -> Value {
        match self.provider {
            ProviderKind::OpenAiCompatible => json!({
                "model": self.model(),
                "messages": messages,
                "tools": tool_values,
                "tool_choice": "required",
                "stream": false,
            }),
            ProviderKind::Anthropic => build_anthropic_body(&self.model(), messages, tool_values),
        }
    }

    /// Attach provider-appropriate auth headers. Key material is exposed only here, and the
    /// header is marked sensitive so it is never logged (research 07 §2).
    fn apply_auth(&self, mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(key) = &self.api_key {
            match self.provider {
                ProviderKind::OpenAiCompatible => {
                    if let Ok(mut value) = reqwest::header::HeaderValue::from_str(&format!(
                        "Bearer {}",
                        key.expose_secret()
                    )) {
                        value.set_sensitive(true);
                        request = request.header(reqwest::header::AUTHORIZATION, value);
                    }
                }
                ProviderKind::Anthropic => {
                    if let Ok(mut value) =
                        reqwest::header::HeaderValue::from_str(key.expose_secret())
                    {
                        value.set_sensitive(true);
                        request = request
                            .header("x-api-key", value)
                            .header("anthropic-version", "2023-06-01");
                    }
                }
            }
        }
        request
    }

    /// `true` while the circuit breaker refuses requests.
    #[must_use]
    pub fn is_circuit_open(&self) -> bool {
        let breaker = lock_breaker(&self.breaker);
        breaker
            .open_until
            .is_some_and(|until| Instant::now() < until)
    }

    /// One chat turn: send `messages` with `tools` + the always-attached
    /// `submit_visualization_plan` tool, `tool_choice: "required"`, streaming off.
    ///
    /// Returns the parsed completion; the caller decides whether it is a plan submission
    /// or a batch of read-only tool calls. Fails with [`AiError::NoToolCall`] when the
    /// provider ignored `tool_choice`.
    #[tracing::instrument(level = "debug", skip_all, fields(messages = messages.len(), tools = tools.len()))]
    pub async fn chat_with_plan(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDef],
    ) -> Result<RawPlanResponse, AiError> {
        self.check_breaker()?;
        self.check_limiter()?;

        let mut tool_values: Vec<Value> = tools.iter().map(ToolDef::to_openai).collect();
        if !tools.iter().any(|t| t.name == PLAN_TOOL_NAME) {
            tool_values.push(plan_tool().to_openai());
        }
        let body = self.build_body(messages, &tool_values);

        let mut request = self
            .http
            .post(&self.endpoint)
            .timeout(self.timeout)
            .json(&body);
        request = self.apply_auth(request);

        let response = match request.send().await {
            Ok(r) => r,
            Err(e) => {
                self.record_failure();
                if e.is_timeout() {
                    tracing::warn!(timeout = ?self.timeout, "ai request timed out");
                    return Err(AiError::Timeout(self.timeout));
                }
                let sanitized = e.without_url();
                tracing::warn!(error = %sanitized, "ai transport error");
                return Err(AiError::Transport(sanitized.to_string()));
            }
        };

        let status = response.status();
        if status.as_u16() == 429 {
            let retry_after = parse_retry_after(response.headers());
            tracing::warn!(?retry_after, "ai provider rate limited the request");
            return Err(AiError::RateLimited { retry_after });
        }
        if !status.is_success() {
            let message = body_snippet(response.text().await.unwrap_or_default());
            if status.is_server_error() {
                self.record_failure();
            }
            tracing::warn!(status = status.as_u16(), "ai provider returned an error");
            return Err(AiError::Http {
                status: status.as_u16(),
                message,
            });
        }

        let completion: Value = match response.json().await {
            Ok(v) => v,
            Err(e) => {
                // Transport died mid-body → breaker strike; a complete-but-invalid JSON
                // body is a protocol problem, not availability.
                if e.is_timeout() {
                    self.record_failure();
                    return Err(AiError::Timeout(self.timeout));
                }
                return Err(AiError::MalformedResponse(e.without_url().to_string()));
            }
        };
        self.record_success();
        match self.provider {
            ProviderKind::OpenAiCompatible => parse_completion(completion),
            ProviderKind::Anthropic => parse_anthropic_response(completion),
        }
    }

    fn check_breaker(&self) -> Result<(), AiError> {
        let mut breaker = lock_breaker(&self.breaker);
        if let Some(until) = breaker.open_until {
            let now = Instant::now();
            if now < until {
                return Err(AiError::CircuitOpen {
                    retry_in: until - now,
                });
            }
            // Cooldown elapsed: close for a single probe.
            tracing::info!("ai circuit breaker half-open; probing provider");
            breaker.open_until = None;
        }
        Ok(())
    }

    fn check_limiter(&self) -> Result<(), AiError> {
        self.limiter.check().map_err(|not_until| {
            let retry_after = not_until.wait_time_from(self.clock.now());
            tracing::debug!(?retry_after, "ai request locally throttled");
            AiError::Throttled { retry_after }
        })?;
        Ok(())
    }

    /// Record a transport failure (connect error, timeout, 5xx). Opens the circuit when
    /// [`AiClientOptions::failure_threshold`] failures land inside the sliding window.
    fn record_failure(&self) {
        let mut breaker = lock_breaker(&self.breaker);
        let now = Instant::now();
        breaker.failures.push_back(now);
        let window = self.options.failure_window;
        while breaker
            .failures
            .front()
            .is_some_and(|t| now.duration_since(*t) > window)
        {
            breaker.failures.pop_front();
        }
        if breaker.failures.len() >= self.options.failure_threshold as usize {
            breaker.open_until = Some(now + self.options.cooldown);
            breaker.failures.clear();
            tracing::warn!(
                cooldown = ?self.options.cooldown,
                "ai circuit breaker opened after repeated transport failures"
            );
        }
    }

    fn record_success(&self) {
        let mut breaker = lock_breaker(&self.breaker);
        breaker.failures.clear();
        breaker.open_until = None;
    }
}

fn lock_breaker(breaker: &Mutex<BreakerState>) -> std::sync::MutexGuard<'_, BreakerState> {
    breaker.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Parse a `Retry-After` header (integer seconds form), capped at [`RETRY_AFTER_CAP`].
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?;
    let secs: u64 = value.to_str().ok()?.trim().parse().ok()?;
    Some(Duration::from_secs(secs).min(RETRY_AFTER_CAP))
}

/// First ~200 chars of an error body, newlines collapsed (safe for the status line).
fn body_snippet(text: String) -> String {
    let mut snippet: String = text.chars().take(200).collect();
    snippet.retain(|c| c != '\n' && c != '\r');
    snippet
}

/// Extract the assistant message and its tool calls from a completion object.
/// Build an Anthropic Messages-API body from the OpenAI-shaped conversation.
///
/// The tool loop inside `AiService` builds OpenAI envelopes (`system` / `user` / `assistant`
/// with `tool_calls` / `tool` results). Anthropic expects: `system` hoisted to a top-level
/// field, `messages` alternating user/assistant, tool calls as assistant `tool_use` content
/// blocks, and tool results as user `tool_result` blocks.
fn build_anthropic_body(model: &str, messages: &[ChatMessage], tool_values: &[Value]) -> Value {
    let mut system_parts: Vec<String> = Vec::new();
    let mut out_messages: Vec<Value> = Vec::new();
    for msg in messages {
        let v = msg.as_value();
        match v.get("role").and_then(Value::as_str) {
            Some("system") => {
                if let Some(c) = v.get("content").and_then(Value::as_str) {
                    system_parts.push(c.to_string());
                }
            }
            Some("user") => {
                out_messages.push(json!({
                    "role": "user",
                    "content": v.get("content").cloned().unwrap_or(Value::String(String::new())),
                }));
            }
            Some("assistant") => {
                out_messages.push(anthropic_assistant_message(v));
            }
            Some("tool") => {
                // OpenAI tool result → Anthropic user message carrying tool_result blocks.
                out_messages.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": v.get("tool_call_id").cloned().unwrap_or(Value::Null),
                        "content": v.get("content").cloned().unwrap_or(Value::String(String::new())),
                    }],
                }));
            }
            _ => {}
        }
    }
    // Anthropic requires alternating roles; merge consecutive same-role messages.
    let merged = merge_same_role(out_messages);
    let mut body = json!({
        "model": model,
        "max_tokens": 4096,
        "messages": merged,
        "tools": anthropic_tools(tool_values),
        "tool_choice": { "type": "any" },
    });
    if !system_parts.is_empty() {
        body["system"] = Value::String(system_parts.join("\n\n"));
    }
    body
}

/// Convert an OpenAI assistant message (content + tool_calls) into Anthropic content blocks.
fn anthropic_assistant_message(v: &Value) -> Value {
    let mut content: Vec<Value> = Vec::new();
    if let Some(text) = v.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
    }
    if let Some(calls) = v.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let args = match call["function"]["arguments"].clone() {
                Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::Object(Default::default())),
                other => other,
            };
            content.push(json!({
                "type": "tool_use",
                "id": call["id"].clone(),
                "name": call["function"]["name"].clone(),
                "input": args,
            }));
        }
    }
    json!({"role": "assistant", "content": content})
}

/// Anthropic tool defs: `name`, `description`, `input_schema` (the OpenAI `parameters`).
fn anthropic_tools(tool_values: &[Value]) -> Vec<Value> {
    tool_values
        .iter()
        .map(|t| {
            let f = t.get("function").cloned().unwrap_or(Value::Null);
            json!({
                "name": f.get("name").cloned().unwrap_or(Value::Null),
                "description": f.get("description").cloned().unwrap_or(Value::Null),
                "input_schema": f.get("parameters").cloned().unwrap_or(json!({"type":"object"})),
            })
        })
        .collect()
}

/// Merge consecutive same-role messages (Anthropic requires strict alternation).
fn merge_same_role(messages: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user").to_string();
        if let Some(last) = out.last_mut() {
            if last.get("role").and_then(Value::as_str) == Some(role.as_str()) {
                let mut content = last.get("content").cloned().unwrap_or(Value::Array(vec![]));
                let incoming = m.get("content").cloned().unwrap_or(Value::Null);
                let mut arr = match content.as_array_mut() {
                    Some(a) => std::mem::take(a),
                    None => vec![json!({"type":"text","text":content})],
                };
                match incoming {
                    Value::Array(a) => arr.extend(a),
                    other => arr.push(json!({"type":"text","text":other})),
                }
                last["content"] = Value::Array(arr);
                continue;
            }
        }
        out.push(m);
    }
    out
}

/// Parse an Anthropic Messages response into the shared [`RawPlanResponse`] shape.
fn parse_anthropic_response(body: Value) -> Result<RawPlanResponse, AiError> {
    let model = body["model"].as_str().map(str::to_string);
    let mut tool_calls = Vec::new();
    if let Some(blocks) = body.get("content").and_then(Value::as_array) {
        for b in blocks {
            if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                let name = b["name"].as_str().unwrap_or_default();
                if name.is_empty() {
                    return Err(AiError::MalformedResponse("tool_use without a name".into()));
                }
                let input = b.get("input").cloned().unwrap_or(Value::Object(Default::default()));
                tool_calls.push(RawToolCall {
                    id: b["id"].as_str().unwrap_or_default().to_string(),
                    name: name.to_string(),
                    arguments: input.to_string(),
                });
            }
        }
    }
    if tool_calls.is_empty() {
        return Err(AiError::NoToolCall);
    }
    // Rebuild an OpenAI-shaped assistant message so the tool loop's echo path stays uniform.
    let message = json!({
        "role": "assistant",
        "tool_calls": tool_calls.iter().map(|c| json!({
            "id": c.id,
            "type": "function",
            "function": {"name": c.name, "arguments": c.arguments},
        })).collect::<Vec<_>>(),
    });
    Ok(RawPlanResponse {
        message,
        tool_calls,
        model,
    })
}

/// Normalize a `GET {base}/models` response (OpenAI `data[].id` or Anthropic `data[].id`)
/// into plain model id strings.
fn parse_model_list(body: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(items) = body.get("data").and_then(Value::as_array) {
        for item in items {
            if let Some(id) = item.get("id").and_then(Value::as_str) {
                out.push(id.to_string());
            } else if let Some(id) = item.get("name").and_then(Value::as_str) {
                out.push(id.to_string());
            }
        }
    }
    out
}

fn parse_completion(completion: Value) -> Result<RawPlanResponse, AiError> {
    let model = completion["model"].as_str().map(str::to_string);
    let message = completion
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .cloned()
        .ok_or_else(|| {
            AiError::MalformedResponse("completion has no choices[0].message".into())
        })?;

    let mut tool_calls = Vec::new();
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let id = call["id"].as_str().unwrap_or_default().to_string();
            let name = call["function"]["name"].as_str().unwrap_or_default();
            let arguments = match &call["function"]["arguments"] {
                Value::String(s) => s.clone(),
                // Some providers emit arguments as a JSON object instead of a string.
                other if !other.is_null() => other.to_string(),
                _ => String::new(),
            };
            if name.is_empty() {
                return Err(AiError::MalformedResponse(
                    "tool call without a function name".into(),
                ));
            }
            tool_calls.push(RawToolCall {
                id,
                name: name.to_string(),
                arguments,
            });
        }
    }
    if tool_calls.is_empty() {
        return Err(AiError::NoToolCall);
    }
    tracing::debug!(calls = tool_calls.len(), "completion parsed");
    Ok(RawPlanResponse {
        message,
        tool_calls,
        model,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_config() -> AiConfig {
        AiConfig {
            enabled: true,
            base_url: "http://127.0.0.1:1/v1/".into(),
            model: "test/model".into(),
            api_key: Some(SecretString::from("sk-test".to_string())),
            timeout: Duration::from_millis(50),
            max_tool_calls: 8,
        }
    }

    #[test]
    fn disabled_config_builds_no_client() {
        let cfg = AiConfig::disabled();
        assert!(matches!(AiClient::new(&cfg), Err(AiError::Disabled)));
    }

    #[test]
    fn endpoint_join_trims_trailing_slash() {
        let client = AiClient::new(&enabled_config()).unwrap();
        assert_eq!(client.endpoint(), "http://127.0.0.1:1/v1/chat/completions");
    }

    #[test]
    fn client_debug_redacts_key() {
        let client = AiClient::new(&enabled_config()).unwrap();
        let debug = format!("{client:?}");
        assert!(!debug.contains("sk-test"), "leaked: {debug}");
    }

    #[test]
    fn retry_after_parse_and_cap() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(parse_retry_after(&headers), None);
        headers.insert(reqwest::header::RETRY_AFTER, "7".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(7)));
        headers.insert(reqwest::header::RETRY_AFTER, "500".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(RETRY_AFTER_CAP));
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2026 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(parse_retry_after(&headers), None, "http-date form unsupported");
    }

    #[test]
    fn parse_completion_extracts_calls() {
        let completion = serde_json::json!({
            "model": "m",
            "choices": [{"message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "c1", "type": "function",
                     "function": {"name": "get_hunk", "arguments": "{\"file\":\"a.go\",\"hunk_index\":0}"}},
                    {"id": "c2", "type": "function",
                     "function": {"name": PLAN_TOOL_NAME, "arguments": "{\"plan_version\":1}"}}
                ]
            }}]
        });
        let parsed = parse_completion(completion).unwrap();
        assert_eq!(parsed.tool_calls.len(), 2);
        assert_eq!(parsed.plan_arguments(), Some("{\"plan_version\":1}"));
        assert_eq!(parsed.read_only_calls().count(), 1);
        assert_eq!(parsed.model.as_deref(), Some("m"));
    }

    #[test]
    fn parse_completion_object_arguments_are_stringified() {
        let completion = serde_json::json!({
            "choices": [{"message": {"tool_calls": [
                {"id": "c1", "type": "function",
                 "function": {"name": PLAN_TOOL_NAME, "arguments": {"plan_version": 1}}}
            ]}}]
        });
        let parsed = parse_completion(completion).unwrap();
        assert_eq!(parsed.plan_arguments(), Some("{\"plan_version\":1}"));
    }

    #[test]
    fn parse_completion_failures() {
        assert!(matches!(
            parse_completion(serde_json::json!({"nope": true})),
            Err(AiError::MalformedResponse(_))
        ));
        // Assistant text without tool calls → NoToolCall.
        assert!(matches!(
            parse_completion(serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "hello"}}]
            })),
            Err(AiError::NoToolCall)
        ));
    }

    #[test]
    fn breaker_opens_after_threshold_and_probes_after_cooldown() {
        let options = AiClientOptions {
            cooldown: Duration::from_millis(20),
            ..AiClientOptions::default()
        };
        let client = AiClient::with_options(&enabled_config(), options).unwrap();
        assert!(client.check_breaker().is_ok());
        client.record_failure();
        client.record_failure();
        assert!(!client.is_circuit_open());
        client.record_failure();
        assert!(client.is_circuit_open());
        assert!(matches!(
            client.check_breaker(),
            Err(AiError::CircuitOpen { .. })
        ));
        std::thread::sleep(Duration::from_millis(25));
        // Half-open: one probe allowed, then success closes it fully.
        assert!(client.check_breaker().is_ok());
        client.record_success();
        assert!(!client.is_circuit_open());
    }

    #[test]
    fn breaker_success_resets_failures() {
        let client = AiClient::new(&enabled_config()).unwrap();
        client.record_failure();
        client.record_failure();
        client.record_success();
        client.record_failure();
        assert!(!client.is_circuit_open(), "count must reset on success");
    }

    #[test]
    fn limiter_throttles_locally() {
        let options = AiClientOptions {
            requests_per_minute: 1,
            burst: 1,
            ..AiClientOptions::default()
        };
        let client = AiClient::with_options(&enabled_config(), options).unwrap();
        assert!(client.check_limiter().is_ok());
        let err = client.check_limiter().unwrap_err();
        assert!(matches!(err, AiError::Throttled { .. }));
    }

    #[test]
    fn body_snippet_truncates_and_flattens() {
        let long = format!("a\nb\r\n{}", "x".repeat(500));
        let s = body_snippet(long);
        assert!(s.len() <= 200);
        assert!(!s.contains('\n'));
    }

    #[test]
    fn anthropic_body_hoists_system_and_maps_tools() {
        let messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("u1"),
            ChatMessage::assistant_raw(json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": {"name": "get_hunk", "arguments": "{\"file\":\"a.go\"}"},
                }],
            })),
            ChatMessage::tool("c1", "result text"),
        ];
        let tools = vec![json!({
            "type": "function",
            "function": {"name": "get_hunk", "description": "d", "parameters": {"type":"object"}},
        })];
        let body = build_anthropic_body("claude-x", &messages, &tools);
        assert_eq!(body["model"], "claude-x");
        assert_eq!(body["system"], "sys");
        assert_eq!(body["tool_choice"]["type"], "any");
        // user, assistant(tool_use), user(tool_result)
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "c1");
        // tools mapped to input_schema
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn anthropic_response_parses_tool_use_blocks() {
        let body = json!({
            "model": "claude-x",
            "content": [
                {"type": "text", "text": "thinking"},
                {"type": "tool_use", "id": "t1", "name": "submit_visualization_plan",
                 "input": {"plan_version": 1}},
            ],
        });
        let res = parse_anthropic_response(body).unwrap();
        assert_eq!(res.tool_calls.len(), 1);
        assert_eq!(res.tool_calls[0].name, "submit_visualization_plan");
        assert!(res.tool_calls[0].arguments.contains("plan_version"));
    }

    #[test]
    fn anthropic_response_without_tool_use_is_no_tool_call() {
        let body = json!({"model": "claude-x", "content": [{"type": "text", "text": "hi"}]});
        assert!(matches!(
            parse_anthropic_response(body),
            Err(AiError::NoToolCall)
        ));
    }

    #[test]
    fn parse_model_list_openai_and_anthropic_shapes() {
        let openai = json!({"data": [{"id": "gpt-5"}, {"id": "gpt-5-mini"}]});
        assert_eq!(parse_model_list(&openai), vec!["gpt-5", "gpt-5-mini"]);
        let anthropic = json!({"data": [{"id": "claude-a"}, {"name": "claude-b"}]});
        assert_eq!(parse_model_list(&anthropic), vec!["claude-a", "claude-b"]);
        assert!(parse_model_list(&json!({})).is_empty());
    }

    #[test]
    fn merge_same_role_merges_consecutive() {
        let msgs = vec![
            json!({"role": "user", "content": [{"type":"text","text":"a"}]}),
            json!({"role": "user", "content": [{"type":"text","text":"b"}]}),
            json!({"role": "assistant", "content": [{"type":"text","text":"c"}]}),
        ];
        let merged = merge_same_role(msgs);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0]["content"].as_array().unwrap().len(), 2);
    }
}
