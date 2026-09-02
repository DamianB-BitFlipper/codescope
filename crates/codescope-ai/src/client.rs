//! Provider-neutral OpenAI-compatible chat-completions client (research 05 §5, 07 §4).
//!
//! One endpoint: `POST {base_url}/chat/completions` with the configured tool choice
//! (`required` by default). Callers provide the incremental `finish_visualization` and
//! draft-editor tools. Streaming is off (`"stream": false`); draft mutations are ordinary
//! tool turns and final publication is atomic.
//!
//! Local protections (all before/around the network call):
//!
//! - **in-flight semaphore** (default 8 concurrent requests) bounds actual provider work;
//! - **token bucket** (`governor`, default 600 requests/minute) remains a high safety
//!   ceiling rather than the primary scheduler;
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

use crate::config::{AiConfig, ProviderKind, ReasoningEffort, ToolChoice};
use crate::error::AiError;
use crate::tools::ToolDef;
use governor::clock::{Clock, QuantaClock};
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Cap applied to provider `Retry-After` values (research 07 §4).
pub const RETRY_AFTER_CAP: Duration = Duration::from_secs(30);

/// Provider-reported token usage accumulated for this running process.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TokenUsage {
    /// Input/prompt tokens consumed by completed provider responses.
    pub input: u64,
    /// Output/completion tokens consumed by completed provider responses.
    pub output: u64,
}

/// GLM enables long-form reasoning by default. Codescope needs one bounded structured plan,
/// not an open-ended agent trajectory, so keep enough output room for the complete schema.
const GLM_PLAN_MAX_TOKENS: u64 = 4096;

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

    /// Build a replay-safe assistant text turn for a structured-call repair.
    ///
    /// Some reasoning providers return `content: null` plus output-only reasoning metadata
    /// when automatic tool choice produces no tool call. Echoing that object into
    /// Chat Completions is invalid: an assistant message without `tool_calls` must carry
    /// content. Keep only valid text/content blocks and omit the turn entirely otherwise.
    #[must_use]
    pub fn assistant_text_for_repair(message: &Value) -> Option<Self> {
        let content = message.get("content")?;
        let present = match content {
            Value::String(text) => !text.trim().is_empty(),
            Value::Array(parts) => !parts.is_empty(),
            Value::Null => false,
            _ => true,
        };
        present.then(|| ChatMessage(json!({"role": "assistant", "content": content})))
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
/// The service interprets these as research or incremental diagram operations.
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

/// Client knobs beyond [`AiConfig`], with production defaults; tests tighten/loosen them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiClientOptions {
    /// High token-bucket safety ceiling. Normal pacing is controlled by
    /// [`Self::max_in_flight_requests`].
    pub requests_per_minute: u32,
    /// Token-bucket burst. Large enough for several complete agentic tool loops.
    pub burst: u32,
    /// Maximum provider HTTP requests executing concurrently. Requests beyond this limit
    /// wait asynchronously instead of failing or consuming a rate-limit token.
    pub max_in_flight_requests: usize,
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
            requests_per_minute: 600,
            burst: 100,
            max_in_flight_requests: 8,
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
    reasoning_effort: Mutex<ReasoningEffort>,
    api_key: Option<SecretString>,
    prime_team_id: Option<String>,
    timeout: Duration,
    provider: ProviderKind,
    tool_choice: ToolChoice,
    limiter: DirectLimiter,
    in_flight: tokio::sync::Semaphore,
    clock: QuantaClock,
    breaker: Mutex<BreakerState>,
    input_tokens: AtomicU64,
    output_tokens: AtomicU64,
    options: AiClientOptions,
}

impl std::fmt::Debug for AiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiClient")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("api_key", &self.api_key.as_ref().map(|_| "«redacted»"))
            .field("timeout", &self.timeout)
            .field("tool_choice", &self.tool_choice)
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
        if options.max_in_flight_requests == 0 {
            return Err(AiError::Config("max_in_flight_requests must be > 0".into()));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(options.connect_timeout)
            .build()
            .map_err(|e| AiError::Config(format!("http client: {}", e.without_url())))?;
        let clock = QuantaClock::default();
        let limiter = RateLimiter::direct_with_clock(
            Quota::per_minute(rpm).allow_burst(burst),
            clock.clone(),
        );
        let base = config.base_url.trim_end_matches('/');
        let provider = config.provider();
        if provider == ProviderKind::Anthropic
            && config.reasoning_effort != ReasoningEffort::Default
        {
            return Err(AiError::Config(
                "reasoning_effort is only supported by OpenAI-compatible Chat Completions providers; use default with Anthropic's native API"
                    .into(),
            ));
        }
        let endpoint = match provider {
            ProviderKind::OpenAiCompatible => format!("{base}/chat/completions"),
            ProviderKind::Anthropic => format!("{base}/messages"),
        };
        Ok(AiClient {
            http,
            endpoint,
            model: Mutex::new(config.model.clone()),
            reasoning_effort: Mutex::new(config.reasoning_effort),
            api_key: config.api_key.clone(),
            prime_team_id: config.prime_team_id.clone(),
            timeout: config.timeout,
            provider,
            tool_choice: config.tool_choice,
            limiter,
            in_flight: tokio::sync::Semaphore::new(options.max_in_flight_requests),
            clock,
            breaker: Mutex::new(BreakerState::default()),
            input_tokens: AtomicU64::new(0),
            output_tokens: AtomicU64::new(0),
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

    /// Provider-reported usage accumulated across all requests, retries, and repair turns
    /// made by this client since the process started.
    #[must_use]
    pub fn token_usage(&self) -> TokenUsage {
        TokenUsage {
            input: self.input_tokens.load(Ordering::Relaxed),
            output: self.output_tokens.load(Ordering::Relaxed),
        }
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

    /// The reasoning budget currently used for subsequent requests.
    #[must_use]
    pub fn reasoning_effort(&self) -> ReasoningEffort {
        self.reasoning_effort
            .lock()
            .map(|effort| *effort)
            .unwrap_or_default()
    }

    /// Switch the reasoning budget used for subsequent requests.
    ///
    /// Cheap: only the request-body `reasoning_effort` field changes; no reconnect is needed.
    pub fn set_reasoning_effort(&self, effort: ReasoningEffort) {
        tracing::info!(reasoning_effort = %effort, "ai reasoning effort changed");
        if let Ok(mut selected) = self.reasoning_effort.lock() {
            *selected = effort;
        }
    }

    /// The `GET {base}/models` URL, derived per provider (one path segment stripped for
    /// Anthropic, two for OpenAI-compatible). Split out for testing.
    fn models_url(&self) -> String {
        let strip = match self.provider {
            ProviderKind::OpenAiCompatible => 2,
            ProviderKind::Anthropic => 1,
        };
        let mut base = self.endpoint.as_str();
        for _ in 0..strip {
            base = base.rsplit_once('/').map(|(b, _)| b).unwrap_or(base);
        }
        format!("{base}/models")
    }

    /// List the models the provider exposes (`GET {base}/models`).
    ///
    /// Both OpenAI-compatible providers and Anthropic may implement this endpoint; the
    /// response is normalized to plain id strings. This user-triggered control-plane request
    /// intentionally does not share inference's token bucket or circuit breaker: the model
    /// picker is a recovery path after an inference failure. Provider/transport errors are
    /// still returned to the caller and shown honestly.
    pub async fn list_models(&self) -> Result<Vec<String>, AiError> {
        let url = self.models_url();
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
            ProviderKind::OpenAiCompatible => {
                let model = self.model();
                let mut body = json!({
                    "model": model,
                    "messages": messages,
                    "tools": tool_values,
                    "tool_choice": self.tool_choice.as_str(),
                    "stream": false,
                });
                let reasoning_effort = self.reasoning_effort();
                if let Some(effort) = reasoning_effort.wire_value() {
                    // Chat Completions uses a top-level `reasoning_effort` field. The
                    // Responses API's nested `reasoning: { effort }` shape is invalid here.
                    body["reasoning_effort"] = json!(effort);
                }
                if is_glm_model(&model) {
                    body["max_tokens"] = json!(GLM_PLAN_MAX_TOKENS);
                    if reasoning_effort == ReasoningEffort::Default
                        && self.endpoint.contains("pinference.ai")
                    {
                        // Prime's GLM route requires reasoning to remain enabled, but accepts
                        // Chat Completions' minimal effort that leaves room for the tool call.
                        body["reasoning_effort"] = json!("minimal");
                    } else if reasoning_effort == ReasoningEffort::Default {
                        // Native Z.AI-compatible GLM endpoints use the family-specific knob.
                        body["thinking"] = json!({"type": "disabled"});
                    }
                }
                body
            }
            ProviderKind::Anthropic => {
                build_anthropic_body(&self.model(), messages, tool_values, self.tool_choice)
            }
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
                    // Prime Inference bills the team balance only when X-Prime-Team-ID is sent;
                    // otherwise it bills the key's personal balance. Send it on the Prime endpoint.
                    if self.endpoint.contains("pinference.ai") {
                        if let Some(team) = &self.prime_team_id {
                            if let Ok(mut v) = reqwest::header::HeaderValue::from_str(team) {
                                v.set_sensitive(true);
                                request = request.header("X-Prime-Team-ID", v);
                            }
                        }
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

    /// One chat turn with configured tool choice and streaming off.
    ///
    /// Returns the parsed completion; the service interprets its research and diagram calls.
    /// Fails with [`AiError::NoToolCall`] when the
    /// provider returned an answer without selecting a tool.
    #[tracing::instrument(level = "debug", skip_all, fields(messages = messages.len(), tools = tools.len()))]
    pub async fn chat_with_plan(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDef],
    ) -> Result<RawPlanResponse, AiError> {
        self.check_breaker()?;
        let _in_flight = self
            .in_flight
            .acquire()
            .await
            .expect("the private AI request semaphore is never closed");
        self.check_limiter()?;
        self.check_breaker()?;
        let response = self.chat_with_plan_admitted(messages, tools).await?;
        if response.tool_calls.is_empty() {
            Err(AiError::NoToolCall)
        } else {
            Ok(response)
        }
    }

    /// Scheduler path: wait asynchronously for token-bucket capacity instead of turning
    /// normal background pacing into a user-visible throttle failure. Unlike the public
    /// public method, this keeps a tool-less assistant message so the service can spend one
    /// bounded repair turn asking an auto-choice model for the required structured call.
    pub(crate) async fn chat_with_plan_waiting(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDef],
    ) -> Result<RawPlanResponse, AiError> {
        self.check_breaker()?;
        let _in_flight = self
            .in_flight
            .acquire()
            .await
            .expect("the private AI request semaphore is never closed");
        self.limiter.until_ready().await;
        // The circuit may have opened while this turn waited behind another request.
        self.check_breaker()?;
        self.chat_with_plan_admitted(messages, tools).await
    }

    async fn chat_with_plan_admitted(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDef],
    ) -> Result<RawPlanResponse, AiError> {
        let tool_values: Vec<Value> = tools.iter().map(ToolDef::to_openai).collect();
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
        let usage = parse_token_usage(&completion);
        self.input_tokens.fetch_add(usage.input, Ordering::Relaxed);
        self.output_tokens
            .fetch_add(usage.output, Ordering::Relaxed);
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

/// Read the two common provider usage shapes. OpenAI-compatible responses report
/// `prompt_tokens` / `completion_tokens`; Anthropic reports `input_tokens` /
/// `output_tokens` and may split cached input into two additional counters.
fn parse_token_usage(body: &Value) -> TokenUsage {
    let Some(usage) = body.get("usage") else {
        return TokenUsage::default();
    };
    let ordinary_input = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached_input = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .saturating_add(
            usage
                .get("cache_read_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
    let output = usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    TokenUsage {
        input: ordinary_input.saturating_add(cached_input),
        output,
    }
}

fn is_glm_model(model: &str) -> bool {
    model
        .rsplit('/')
        .next()
        .is_some_and(|name| name.to_ascii_lowercase().starts_with("glm-"))
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
fn build_anthropic_body(
    model: &str,
    messages: &[ChatMessage],
    tool_values: &[Value],
    tool_choice: ToolChoice,
) -> Value {
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
        "tool_choice": { "type": tool_choice.anthropic_type() },
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
                Value::String(s) => {
                    serde_json::from_str(&s).unwrap_or(Value::Object(Default::default()))
                }
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
        let role = m
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user")
            .to_string();
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
                let input = b
                    .get("input")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));
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
        .ok_or_else(|| AiError::MalformedResponse("completion has no choices[0].message".into()))?;

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
            reasoning_effort: ReasoningEffort::Default,
            api_key: Some(SecretString::from("sk-test".to_string())),
            timeout: Duration::from_millis(50),
            tool_choice: ToolChoice::Required,
            max_tool_calls: 8,
            prime_team_id: None,
        }
    }

    #[test]
    fn disabled_config_builds_no_client() {
        let cfg = AiConfig::disabled();
        assert!(matches!(AiClient::new(&cfg), Err(AiError::Disabled)));
    }

    #[test]
    fn native_anthropic_rejects_chat_completions_reasoning_effort() {
        let mut cfg = enabled_config();
        cfg.base_url = crate::ANTHROPIC_BASE_URL.to_string();
        cfg.reasoning_effort = ReasoningEffort::High;
        let error = AiClient::new(&cfg).expect_err("native Anthropic must not silently ignore it");
        assert!(error.to_string().contains("OpenAI-compatible"), "{error}");
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
    fn parses_openai_and_anthropic_token_usage() {
        assert_eq!(
            parse_token_usage(&json!({
                "usage": {"prompt_tokens": 1_234, "completion_tokens": 56}
            })),
            TokenUsage {
                input: 1_234,
                output: 56,
            }
        );
        assert_eq!(
            parse_token_usage(&json!({
                "usage": {
                    "input_tokens": 900,
                    "cache_creation_input_tokens": 100,
                    "cache_read_input_tokens": 200,
                    "output_tokens": 75
                }
            })),
            TokenUsage {
                input: 1_200,
                output: 75,
            }
        );
        assert_eq!(parse_token_usage(&json!({})), TokenUsage::default());
    }

    #[test]
    fn glm_requests_bound_reasoning_and_plan_output() {
        let mut cfg = enabled_config();
        cfg.model = "z-ai/glm-5.3".into();
        cfg.base_url = "https://api.pinference.ai/api/v1".into();
        let client = AiClient::new(&cfg).unwrap();
        let body = client.build_body(&[ChatMessage::user("digest")], &[]);
        assert_eq!(body["reasoning_effort"], "minimal");
        assert!(body.get("reasoning").is_none());
        assert!(body.get("thinking").is_none());
        assert_eq!(body["max_tokens"], GLM_PLAN_MAX_TOKENS);

        cfg.base_url = "https://api.z.ai/api/paas/v4".into();
        let client = AiClient::new(&cfg).unwrap();
        let body = client.build_body(&[ChatMessage::user("digest")], &[]);
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("reasoning").is_none());
        assert!(body.get("reasoning_effort").is_none());

        cfg.reasoning_effort = ReasoningEffort::High;
        let client = AiClient::new(&cfg).unwrap();
        let body = client.build_body(&[ChatMessage::user("digest")], &[]);
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("thinking").is_none());

        cfg.model = "openai/gpt-5-mini".into();
        let client = AiClient::new(&cfg).unwrap();
        let body = client.build_body(&[ChatMessage::user("digest")], &[]);
        assert!(body.get("thinking").is_none());
        assert!(body.get("max_tokens").is_none());
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
        assert_eq!(
            parse_retry_after(&headers),
            None,
            "http-date form unsupported"
        );
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
                     "function": {"name": "finish_visualization", "arguments": "{}"}}
                ]
            }}]
        });
        let parsed = parse_completion(completion).unwrap();
        assert_eq!(parsed.tool_calls.len(), 2);
        assert_eq!(parsed.tool_calls[1].name, "finish_visualization");
        assert_eq!(parsed.tool_calls[1].arguments, "{}");
        assert_eq!(parsed.model.as_deref(), Some("m"));
    }

    #[test]
    fn parse_completion_object_arguments_are_stringified() {
        let completion = serde_json::json!({
            "choices": [{"message": {"tool_calls": [
                {"id": "c1", "type": "function",
                 "function": {"name": "edit_visualization", "arguments": {"op": "reset"}}}
            ]}}]
        });
        let parsed = parse_completion(completion).unwrap();
        assert_eq!(parsed.tool_calls[0].arguments, "{\"op\":\"reset\"}");
    }

    #[test]
    fn parse_completion_failures() {
        assert!(matches!(
            parse_completion(serde_json::json!({"nope": true})),
            Err(AiError::MalformedResponse(_))
        ));
        // Assistant text is preserved for the service's one bounded tool-call repair.
        let plain = parse_completion(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "hello"}}]
        }))
        .unwrap();
        assert!(plain.tool_calls.is_empty());
        assert_eq!(plain.message["content"], "hello");
    }

    #[test]
    fn repair_assistant_omits_null_content_and_strips_output_only_fields() {
        assert!(ChatMessage::assistant_text_for_repair(&json!({
            "role": "assistant",
            "content": null,
            "reasoning": "internal"
        }))
        .is_none());

        let replay = ChatMessage::assistant_text_for_repair(&json!({
            "role": "assistant",
            "content": "I should call the tool.",
            "reasoning": "internal"
        }))
        .expect("text response is replayable");
        assert_eq!(
            replay.as_value(),
            &json!({"role": "assistant", "content": "I should call the tool."})
        );
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
    fn defaults_use_concurrency_as_the_primary_guard_and_a_high_rate_ceiling() {
        let options = AiClientOptions::default();
        assert_eq!(options.max_in_flight_requests, 8);
        assert_eq!(options.requests_per_minute, 600);
        assert!(options.requests_per_minute > 48);

        let client = AiClient::with_options(&enabled_config(), options.clone()).unwrap();
        let permits = (0..options.max_in_flight_requests)
            .map(|_| client.in_flight.try_acquire().unwrap())
            .collect::<Vec<_>>();
        assert!(client.in_flight.try_acquire().is_err());
        drop(permits);
        assert!(client.in_flight.try_acquire().is_ok());
    }

    #[test]
    fn zero_in_flight_limit_is_rejected() {
        let options = AiClientOptions {
            max_in_flight_requests: 0,
            ..AiClientOptions::default()
        };
        let error = AiClient::with_options(&enabled_config(), options).unwrap_err();
        assert!(error.to_string().contains("max_in_flight_requests"));
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
        let body = build_anthropic_body("claude-x", &messages, &tools, ToolChoice::Required);
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

        let auto = build_anthropic_body("claude-x", &messages, &tools, ToolChoice::Auto);
        assert_eq!(auto["tool_choice"]["type"], "auto");
    }

    #[test]
    fn anthropic_response_parses_tool_use_blocks() {
        let body = json!({
            "model": "claude-x",
            "content": [
                {"type": "text", "text": "thinking"},
                {"type": "tool_use", "id": "t1", "name": "finish_visualization",
                 "input": {"plan_version": 1}},
            ],
        });
        let res = parse_anthropic_response(body).unwrap();
        assert_eq!(res.tool_calls.len(), 1);
        assert_eq!(res.tool_calls[0].name, "finish_visualization");
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

    #[test]
    fn list_models_url_respects_provider() {
        // OpenAI-compatible: {base}/chat/completions -> {base}/models
        let mut cfg = enabled_config();
        cfg.base_url = "https://api.openai.com/v1".into();
        let client = AiClient::new(&cfg).unwrap();
        let url = client.models_url();
        assert_eq!(url, "https://api.openai.com/v1/models");

        // Anthropic: {base}/messages -> {base}/models (only one segment stripped)
        let mut cfg = enabled_config();
        cfg.base_url = "https://api.anthropic.com/v1".into();
        let client = AiClient::new(&cfg).unwrap();
        assert_eq!(client.models_url(), "https://api.anthropic.com/v1/models");
    }
    #[test]
    fn prime_team_id_header_sent_on_prime_endpoint() {
        let mut cfg = enabled_config();
        cfg.base_url = "https://api.pinference.ai/api/v1".into();
        cfg.prime_team_id = Some("team-abc".into());
        let client = AiClient::new(&cfg).unwrap();
        // Build any request through apply_auth and inspect the headers.
        let req = client
            .apply_auth(client.http.get("https://api.pinference.ai/api/v1/models"))
            .build()
            .unwrap();
        assert_eq!(
            req.headers()
                .get("X-Prime-Team-ID")
                .and_then(|v| v.to_str().ok()),
            Some("team-abc")
        );
        assert!(req.headers().get("authorization").is_some());
    }

    #[test]
    fn prime_team_id_not_sent_off_prime_endpoint() {
        let mut cfg = enabled_config();
        cfg.prime_team_id = Some("team-abc".into());
        let client = AiClient::new(&cfg).unwrap();
        let req = client
            .apply_auth(client.http.get("http://127.0.0.1:1/v1/models"))
            .build()
            .unwrap();
        assert!(req.headers().get("X-Prime-Team-ID").is_none());
    }
}
