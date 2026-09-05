//! Provider-neutral OpenAI Responses / compatible Chat Completions / Anthropic Messages client.
//!
//! OpenAI's official API uses `POST {base_url}/responses`; other OpenAI-compatible providers use
//! `POST {base_url}/chat/completions`; Anthropic uses its native Messages shape. Callers mark turns
//! Auto or Required and provide incremental draft-editor tools. Anthropic keeps provider tool
//! choice on Auto for thinking-model compatibility while Codescope enforces Required turns in its
//! controller. Streaming is off; draft mutations are ordinary tool turns and final publication is
//! atomic.
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
//! time and marked sensitive. Provider request/response bodies are retained in Codescope's
//! owner-only local telemetry after recognizable secret values are scrubbed; explicit debug
//! mode also emits scrubbed `trace` events. Reqwest errors are sanitized with
//! [`reqwest::Error::without_url`].

use crate::config::{
    AiConfig, OPENAI_BASE_URL, ProviderKind, ReasoningEffort, is_official_base_url,
};
use crate::error::AiError;
use crate::tools::ToolDef;
use governor::clock::{Clock, QuantaClock};
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
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

/// GLM enables long-form reasoning by default. Keep enough bounded output room for the model to
/// finish reasoning and emit the structured tool call instead of stopping at the old 4k ceiling.
const GLM_PLAN_MAX_TOKENS: u64 = 8_192;

/// One provider-neutral conversation turn, stored as a JSON object.
///
/// A thin newtype over [`Value`] keeps Chat Completions assistant messages wire-exact and lets a
/// Responses turn retain the provider's complete output-item array without modeling either full
/// API surface.
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

    /// An assistant turn retained for the next tool-loop request.
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
        let has_tool_calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| !calls.is_empty());
        if !has_tool_calls
            && (message.get(RESPONSES_OUTPUT_FIELD).is_some()
                || message.get(ANTHROPIC_CONTENT_FIELD).is_some())
        {
            // Native reasoning transports can replay their complete, provider-authenticated
            // assistant output before the repair prompt. A rejected response containing tool calls
            // cannot be replayed without matching results, so it follows the text-only path below.
            return Some(ChatMessage(message.clone()));
        }
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

/// A parsed provider response: its replayable assistant turn plus extracted tool calls.
///
/// The service interprets these as research or incremental diagram operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPlanResponse {
    /// A replayable assistant turn. Chat Completions retains the assistant message exactly;
    /// Responses retains its complete output-item list in a private transport field.
    pub message: Value,
    /// All tool calls in the message, in order.
    pub tool_calls: Vec<RawToolCall>,
    /// Model the provider reports having used.
    pub model: Option<String>,
    /// Provider stop reason (`tool_calls`, `stop`, `length`, …), when reported.
    pub finish_reason: Option<String>,
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

/// Concrete HTTP envelope selected independently of the provider's authentication family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WireProtocol {
    OpenAiResponses,
    ChatCompletions,
    AnthropicMessages,
}

impl WireProtocol {
    const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiResponses => "openai_responses",
            Self::ChatCompletions => "chat_completions",
            Self::AnthropicMessages => "anthropic_messages",
        }
    }
}

/// Provider request client for OpenAI Responses, compatible Chat Completions, or Anthropic.
pub struct AiClient {
    http: reqwest::Client,
    endpoint: String,
    models_endpoint: String,
    model: Mutex<String>,
    reasoning_effort: Mutex<ReasoningEffort>,
    api_key: Option<SecretString>,
    prime_team_id: Option<String>,
    timeout: Duration,
    provider: ProviderKind,
    protocol: WireProtocol,
    limiter: DirectLimiter,
    in_flight: tokio::sync::Semaphore,
    clock: QuantaClock,
    breaker: Mutex<BreakerState>,
    input_tokens: AtomicU64,
    output_tokens: AtomicU64,
    telemetry_request_seq: AtomicU64,
    options: AiClientOptions,
}

impl std::fmt::Debug for AiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiClient")
            .field("endpoint", &self.endpoint)
            .field("protocol", &self.protocol)
            .field("model", &self.model)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("api_key", &self.api_key.as_ref().map(|_| "«redacted»"))
            .field("timeout", &self.timeout)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl AiClient {
    /// Build a client from a resolved config with default [`AiClientOptions`].
    pub fn new(config: &AiConfig) -> Result<Self, AiError> {
        Self::with_options(config, AiClientOptions::default())
    }

    /// Build a client with explicit options (tests tighten timeouts/limits).
    pub fn with_options(config: &AiConfig, options: AiClientOptions) -> Result<Self, AiError> {
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
            && matches!(
                config.reasoning_effort,
                ReasoningEffort::None | ReasoningEffort::Minimal
            )
        {
            return Err(AiError::Config(
                "Anthropic output_config.effort supports default, low, medium, high, xhigh, or max; none and minimal are unavailable"
                    .into(),
            ));
        }
        let official_openai = reqwest::Url::parse(base)
            .ok()
            .is_some_and(|url| is_official_base_url(&url, OPENAI_BASE_URL));
        let protocol = match (provider, official_openai) {
            (ProviderKind::OpenAiCompatible, true) => WireProtocol::OpenAiResponses,
            (ProviderKind::OpenAiCompatible, false) => WireProtocol::ChatCompletions,
            (ProviderKind::Anthropic, _) => WireProtocol::AnthropicMessages,
        };
        let endpoint = match protocol {
            WireProtocol::OpenAiResponses => format!("{base}/responses"),
            WireProtocol::ChatCompletions => format!("{base}/chat/completions"),
            WireProtocol::AnthropicMessages => format!("{base}/messages"),
        };
        Ok(AiClient {
            http,
            endpoint,
            models_endpoint: format!("{base}/models"),
            model: Mutex::new(config.model.clone()),
            reasoning_effort: Mutex::new(config.reasoning_effort),
            api_key: config.api_key.clone(),
            prime_team_id: config.prime_team_id.clone(),
            timeout: config.timeout,
            provider,
            protocol,
            limiter,
            in_flight: tokio::sync::Semaphore::new(options.max_in_flight_requests),
            clock,
            breaker: Mutex::new(BreakerState::default()),
            input_tokens: AtomicU64::new(0),
            output_tokens: AtomicU64::new(0),
            telemetry_request_seq: AtomicU64::new(1),
            options,
        })
    }

    /// The resolved provider inference endpoint URL.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The configured provider family.
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
    /// Cheap: only the protocol-appropriate request-body reasoning field changes; no reconnect is
    /// needed.
    pub fn set_reasoning_effort(&self, effort: ReasoningEffort) {
        if self.protocol == WireProtocol::AnthropicMessages
            && matches!(effort, ReasoningEffort::None | ReasoningEffort::Minimal)
        {
            tracing::warn!(
                reasoning_effort = %effort,
                "unsupported Anthropic reasoning effort ignored"
            );
            return;
        }
        tracing::info!(reasoning_effort = %effort, "ai reasoning effort changed");
        if let Ok(mut selected) = self.reasoning_effort.lock() {
            *selected = effort;
        }
    }

    /// The `GET {base}/models` URL retained alongside the inference endpoint.
    fn models_url(&self) -> String {
        self.models_endpoint.clone()
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
    fn build_body(
        &self,
        messages: &[ChatMessage],
        tool_values: &[Value],
        require_tool: bool,
        max_tokens_override: Option<u64>,
    ) -> Value {
        match self.protocol {
            WireProtocol::OpenAiResponses => build_openai_responses_body(
                &self.model(),
                self.reasoning_effort(),
                messages,
                tool_values,
                require_tool,
                max_tokens_override,
            ),
            WireProtocol::ChatCompletions => {
                let model = self.model();
                let mut body = json!({
                    "model": model,
                    "messages": messages,
                    "tools": tool_values,
                    "tool_choice": if require_tool { "required" } else { "auto" },
                    "stream": false,
                });
                let reasoning_effort = self.reasoning_effort();
                if let Some(effort) = reasoning_effort.wire_value() {
                    // Chat Completions uses a top-level `reasoning_effort` field. The
                    // Responses API's nested `reasoning: { effort }` shape is invalid here.
                    body["reasoning_effort"] = json!(effort);
                }
                if is_glm_model(&model) {
                    body["max_tokens"] = json!(max_tokens_override.unwrap_or(GLM_PLAN_MAX_TOKENS));
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
                } else if let Some(max_tokens) = max_tokens_override {
                    body["max_tokens"] = json!(max_tokens);
                }
                body
            }
            WireProtocol::AnthropicMessages => build_anthropic_body_with_tool_choice(
                &self.model(),
                self.reasoning_effort(),
                messages,
                tool_values,
                require_tool,
                max_tokens_override,
            ),
        }
    }

    /// Attach provider-appropriate auth headers. Key material is exposed only here, and the
    /// header is marked sensitive so it is never logged (research 07 §2).
    fn apply_auth(&self, mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.provider == ProviderKind::Anthropic {
            // This version header is required for every native Anthropic API request, including
            // model listing. Keep it independent of the key so even a misconfigured request has
            // the correct protocol shape and receives Anthropic's honest authentication error.
            request = request.header("anthropic-version", "2023-06-01");
        }
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
                        request = request.header("x-api-key", value);
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
        let response = self
            .chat_with_plan_admitted(messages, tools, false, None)
            .await?;
        if response.tool_calls.is_empty() {
            Err(AiError::NoToolCall)
        } else {
            Ok(response)
        }
    }

    /// Scheduler path: wait asynchronously for token-bucket capacity instead of turning
    /// normal background pacing into a user-visible throttle failure. Unlike the public
    /// method, this keeps a tool-less assistant message so the service can spend one
    /// bounded repair turn asking an auto-choice model for the required structured call.
    pub(crate) async fn chat_with_plan_waiting(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDef],
        require_tool: bool,
        max_tokens_override: Option<u64>,
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
        self.chat_with_plan_admitted(messages, tools, require_tool, max_tokens_override)
            .await
    }

    async fn chat_with_plan_admitted(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDef],
        require_tool: bool,
        max_tokens_override: Option<u64>,
    ) -> Result<RawPlanResponse, AiError> {
        let telemetry_request_id = self.telemetry_request_seq.fetch_add(1, Ordering::Relaxed);
        let telemetry_started = Instant::now();
        let tool_values: Vec<Value> = tools.iter().map(ToolDef::to_openai).collect();
        let body = self.build_body(messages, &tool_values, require_tool, max_tokens_override);
        codescope_telemetry::record_with_origin(
            codescope_telemetry::TelemetryOrigin::InternalAgent,
            "llm.request",
            json!({
                "request_id": telemetry_request_id,
                "provider": format!("{:?}", self.provider),
                "protocol": self.protocol.as_str(),
                "endpoint": sanitized_endpoint(&self.endpoint),
                "model": self.model(),
                "reasoning_effort": self.reasoning_effort().to_string(),
                "require_tool": require_tool,
                "max_tokens_override": max_tokens_override,
                "body": crate::scrub::scrub_json(&body),
            }),
        );
        Self::trace_wire_json("request", &body);

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
                    codescope_telemetry::record_with_origin(
                        codescope_telemetry::TelemetryOrigin::InternalAgent,
                        "llm.error",
                        json!({
                            "request_id": telemetry_request_id,
                            "kind": "timeout",
                            "elapsed_ms": telemetry_started.elapsed().as_millis(),
                            "timeout_ms": self.timeout.as_millis(),
                        }),
                    );
                    tracing::warn!(timeout = ?self.timeout, "ai request timed out");
                    return Err(AiError::Timeout(self.timeout));
                }
                let sanitized = e.without_url();
                codescope_telemetry::record_with_origin(
                    codescope_telemetry::TelemetryOrigin::InternalAgent,
                    "llm.error",
                    json!({
                        "request_id": telemetry_request_id,
                        "kind": "transport",
                        "elapsed_ms": telemetry_started.elapsed().as_millis(),
                        "error": crate::scrub::scrub_secrets(&sanitized.to_string()),
                    }),
                );
                tracing::warn!(error = %sanitized, "ai transport error");
                return Err(AiError::Transport(sanitized.to_string()));
            }
        };

        let status = response.status();
        if status.as_u16() == 429 {
            let retry_after = parse_retry_after(response.headers());
            codescope_telemetry::record_with_origin(
                codescope_telemetry::TelemetryOrigin::InternalAgent,
                "llm.error",
                json!({
                    "request_id": telemetry_request_id,
                    "kind": "rate_limited",
                    "status": status.as_u16(),
                    "elapsed_ms": telemetry_started.elapsed().as_millis(),
                    "retry_after_ms": retry_after.map(|duration| duration.as_millis()),
                }),
            );
            tracing::warn!(?retry_after, "ai provider rate limited the request");
            return Err(AiError::RateLimited { retry_after });
        }
        if !status.is_success() {
            let response_body = response.text().await.unwrap_or_default();
            Self::trace_wire_text("error_response", &response_body);
            codescope_telemetry::record_with_origin(
                codescope_telemetry::TelemetryOrigin::InternalAgent,
                "llm.error",
                json!({
                    "request_id": telemetry_request_id,
                    "kind": "http",
                    "status": status.as_u16(),
                    "elapsed_ms": telemetry_started.elapsed().as_millis(),
                    "body": crate::scrub::scrub_secrets(&response_body),
                }),
            );
            let message = body_snippet(response_body);
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
                    codescope_telemetry::record_with_origin(
                        codescope_telemetry::TelemetryOrigin::InternalAgent,
                        "llm.error",
                        json!({
                            "request_id": telemetry_request_id,
                            "kind": "response_timeout",
                            "elapsed_ms": telemetry_started.elapsed().as_millis(),
                        }),
                    );
                    return Err(AiError::Timeout(self.timeout));
                }
                let sanitized = e.without_url();
                codescope_telemetry::record_with_origin(
                    codescope_telemetry::TelemetryOrigin::InternalAgent,
                    "llm.error",
                    json!({
                        "request_id": telemetry_request_id,
                        "kind": "malformed_response",
                        "elapsed_ms": telemetry_started.elapsed().as_millis(),
                        "error": crate::scrub::scrub_secrets(&sanitized.to_string()),
                    }),
                );
                return Err(AiError::MalformedResponse(sanitized.to_string()));
            }
        };
        Self::trace_wire_json("response", &completion);
        self.record_success();
        let usage = parse_token_usage(&completion);
        codescope_telemetry::record_with_origin(
            codescope_telemetry::TelemetryOrigin::InternalAgent,
            "llm.response",
            json!({
                "request_id": telemetry_request_id,
                "status": status.as_u16(),
                "elapsed_ms": telemetry_started.elapsed().as_millis(),
                "usage": { "input_tokens": usage.input, "output_tokens": usage.output },
                "body": crate::scrub::scrub_json(&completion),
            }),
        );
        self.input_tokens.fetch_add(usage.input, Ordering::Relaxed);
        self.output_tokens
            .fetch_add(usage.output, Ordering::Relaxed);
        tracing::debug!(
            input_tokens = usage.input,
            output_tokens = usage.output,
            "provider completion token usage"
        );
        let parsed = match self.protocol {
            WireProtocol::OpenAiResponses => parse_openai_response(completion),
            WireProtocol::ChatCompletions => parse_completion(completion),
            WireProtocol::AnthropicMessages => parse_anthropic_response(completion),
        };
        if let Err(error) = &parsed {
            codescope_telemetry::record_with_origin(
                codescope_telemetry::TelemetryOrigin::InternalAgent,
                "llm.error",
                json!({
                    "request_id": telemetry_request_id,
                    "kind": "response_protocol",
                    "elapsed_ms": telemetry_started.elapsed().as_millis(),
                    "error": crate::scrub::scrub_secrets(&error.to_string()),
                }),
            );
        }
        parsed
    }

    fn trace_wire_json(direction: &'static str, value: &Value) {
        if tracing::enabled!(target: "codescope_ai::wire", tracing::Level::TRACE) {
            Self::trace_wire_text(direction, &value.to_string());
        }
    }

    fn trace_wire_text(direction: &'static str, value: &str) {
        if tracing::enabled!(target: "codescope_ai::wire", tracing::Level::TRACE) {
            let payload = crate::scrub::scrub_secrets(value);
            tracing::trace!(
                target: "codescope_ai::wire",
                direction,
                payload = %payload,
                "ai provider wire payload"
            );
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

fn sanitized_endpoint(endpoint: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(endpoint) else {
        return crate::scrub::scrub_secrets(endpoint);
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

/// Read the common provider usage shapes. Chat Completions reports `prompt_tokens` /
/// `completion_tokens`; Responses and Anthropic report `input_tokens` / `output_tokens`, while
/// Anthropic may split cached input into two additional counters.
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

const RESPONSES_OUTPUT_FIELD: &str = "_codescope_responses_output";
const ANTHROPIC_CONTENT_FIELD: &str = "_codescope_anthropic_content";

/// Build an OpenAI Responses API request from Codescope's provider-neutral conversation.
///
/// Responses function tools are flat objects, reasoning effort is nested, and tool results are
/// input items keyed by `call_id`. We disable provider-side storage and request encrypted reasoning
/// content so every output item needed for a stateless tool continuation can be replayed.
fn build_openai_responses_body(
    model: &str,
    reasoning_effort: ReasoningEffort,
    messages: &[ChatMessage],
    tool_values: &[Value],
    require_tool: bool,
    max_tokens_override: Option<u64>,
) -> Value {
    let (instructions, input) = openai_responses_input(messages);
    let mut body = json!({
        "model": model,
        "input": input,
        "tools": openai_responses_tools(tool_values),
        "tool_choice": if require_tool { "required" } else { "auto" },
        "parallel_tool_calls": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
    });
    if !instructions.is_empty() {
        body["instructions"] = Value::String(instructions.join("\n\n"));
    }
    if let Some(effort) = reasoning_effort.wire_value() {
        body["reasoning"] = json!({"effort": effort});
    }
    if let Some(max_tokens) = max_tokens_override {
        body["max_output_tokens"] = json!(max_tokens);
    }
    body
}

/// Convert the shared Chat-style transcript into Responses input items.
fn openai_responses_input(messages: &[ChatMessage]) -> (Vec<String>, Vec<Value>) {
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for message in messages {
        let value = message.as_value();
        match value.get("role").and_then(Value::as_str) {
            Some("system") => {
                if let Some(content) = value.get("content").and_then(Value::as_str) {
                    instructions.push(content.to_string());
                }
            }
            Some("user") => input.push(json!({
                "role": "user",
                "content": value
                    .get("content")
                    .cloned()
                    .unwrap_or_else(|| Value::String(String::new())),
            })),
            Some("assistant") => {
                // A prior Responses turn carries its exact provider output. Replaying every item
                // preserves encrypted reasoning, function-call identity, and any phase metadata.
                if let Some(items) = value.get(RESPONSES_OUTPUT_FIELD).and_then(Value::as_array) {
                    input.extend(items.iter().cloned());
                    continue;
                }
                if value
                    .get("content")
                    .is_some_and(|content| !content.is_null())
                {
                    input.push(json!({
                        "role": "assistant",
                        "content": value["content"].clone(),
                    }));
                }
                if let Some(calls) = value.get("tool_calls").and_then(Value::as_array) {
                    input.extend(calls.iter().map(|call| {
                        json!({
                            "type": "function_call",
                            "call_id": call["id"].clone(),
                            "name": call["function"]["name"].clone(),
                            "arguments": call["function"]["arguments"].clone(),
                        })
                    }));
                }
            }
            Some("tool") => input.push(json!({
                "type": "function_call_output",
                "call_id": value
                    .get("tool_call_id")
                    .cloned()
                    .unwrap_or(Value::Null),
                "output": value
                    .get("content")
                    .cloned()
                    .unwrap_or_else(|| Value::String(String::new())),
            })),
            _ => {}
        }
    }
    (instructions, input)
}

/// Flatten Chat Completions function definitions into Responses function tools.
fn openai_responses_tools(tool_values: &[Value]) -> Vec<Value> {
    tool_values
        .iter()
        .map(|tool| {
            let function = &tool["function"];
            json!({
                "type": "function",
                "name": function["name"].clone(),
                "description": function["description"].clone(),
                "parameters": function["parameters"].clone(),
            })
        })
        .collect()
}

/// Parse a Responses result into the shared assistant/tool-call representation.
fn parse_openai_response(body: Value) -> Result<RawPlanResponse, AiError> {
    if body.get("status").and_then(Value::as_str) == Some("failed") {
        let message = body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .map(crate::scrub::scrub_secrets)
            .unwrap_or_else(|| "OpenAI response failed".to_string());
        return Err(AiError::MalformedResponse(message));
    }

    let output = body
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| AiError::MalformedResponse("response has no output items".into()))?;
    let mut tool_calls = Vec::new();
    let mut text_parts = Vec::new();
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
                if call_id.is_empty() || name.is_empty() {
                    return Err(AiError::MalformedResponse(
                        "function call without a call_id or name".into(),
                    ));
                }
                let arguments = match item.get("arguments") {
                    Some(Value::String(arguments)) => arguments.clone(),
                    Some(arguments) if !arguments.is_null() => arguments.to_string(),
                    _ => String::new(),
                };
                tool_calls.push(RawToolCall {
                    id: call_id.to_string(),
                    name: name.to_string(),
                    arguments,
                });
            }
            Some("message") => {
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for part in content {
                        if part.get("type").and_then(Value::as_str) == Some("output_text") {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                text_parts.push(text);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let status = body.get("status").and_then(Value::as_str);
    let finish_reason = if status == Some("incomplete") {
        body.pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
            .map(|reason| {
                if reason.eq_ignore_ascii_case("max_output_tokens") {
                    "length".to_string()
                } else {
                    reason.to_string()
                }
            })
            .or_else(|| Some("incomplete".to_string()))
    } else if tool_calls.is_empty() {
        Some("stop".to_string())
    } else {
        Some("tool_calls".to_string())
    };

    let mut message = json!({
        "role": "assistant",
        "content": if text_parts.is_empty() {
            Value::Null
        } else {
            Value::String(text_parts.join("\n"))
        },
    });
    message[RESPONSES_OUTPUT_FIELD] = Value::Array(output.clone());
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(
            tool_calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "type": "function",
                        "function": {"name": call.name, "arguments": call.arguments},
                    })
                })
                .collect(),
        );
    }

    tracing::debug!(
        calls = tool_calls.len(),
        finish_reason = finish_reason.as_deref().unwrap_or(""),
        "OpenAI response parsed"
    );
    Ok(RawPlanResponse {
        message,
        tool_calls,
        model: body
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string),
        finish_reason,
    })
}

/// Build an Anthropic Messages-API body from the OpenAI-shaped conversation.
///
/// The tool loop inside `AiService` builds OpenAI envelopes (`system` / `user` / `assistant`
/// with `tool_calls` / `tool` results). Anthropic expects: `system` hoisted to a top-level
/// field, `messages` alternating user/assistant, tool calls as assistant `tool_use` content
/// blocks, and tool results as user `tool_result` blocks.
///
/// Anthropic tool selection remains `auto`, including for focused singleton controller turns.
/// Newer thinking models reject forced `any`/`tool` choices, while Codescope's controller already
/// validates that focused turns return exactly the required call.
fn build_anthropic_body_with_tool_choice(
    model: &str,
    reasoning_effort: ReasoningEffort,
    messages: &[ChatMessage],
    tool_values: &[Value],
    _require_tool: bool,
    max_tokens_override: Option<u64>,
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
                let content = v
                    .get("content")
                    .cloned()
                    .unwrap_or_else(|| Value::String(String::new()));
                let mut result = json!({
                    "type": "tool_result",
                    "tool_use_id": v.get("tool_call_id").cloned().unwrap_or(Value::Null),
                    "content": content,
                });
                if anthropic_tool_result_is_error(&content) {
                    result["is_error"] = Value::Bool(true);
                }
                out_messages.push(json!({"role": "user", "content": [result]}));
            }
            _ => {}
        }
    }
    // Anthropic combines consecutive same-role turns. Merge them explicitly so tool_result blocks
    // remain first in the immediate user turn that follows an assistant tool_use.
    let merged = merge_same_role(out_messages);
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens_override.unwrap_or(4096),
        "messages": merged,
    });
    if !tool_values.is_empty() {
        body["tools"] = Value::Array(anthropic_tools(tool_values));
        // `auto` works across classic, extended-thinking, and always-adaptive models. Codescope's
        // controller still rejects a response that misses a locally required singleton operation.
        body["tool_choice"] = json!({"type": "auto"});
    }
    if !system_parts.is_empty() {
        body["system"] = Value::String(system_parts.join("\n\n"));
    }
    if let Some(effort) = anthropic_effort_wire_value(reasoning_effort) {
        body["output_config"] = json!({"effort": effort});
    }
    body
}

fn anthropic_effort_wire_value(effort: ReasoningEffort) -> Option<&'static str> {
    match effort {
        ReasoningEffort::Default | ReasoningEffort::None | ReasoningEffort::Minimal => None,
        ReasoningEffort::Low => Some("low"),
        ReasoningEffort::Medium => Some("medium"),
        ReasoningEffort::High => Some("high"),
        ReasoningEffort::XHigh => Some("xhigh"),
        ReasoningEffort::Max => Some("max"),
    }
}

fn anthropic_tool_result_is_error(content: &Value) -> bool {
    let parsed = match content {
        Value::String(text) => serde_json::from_str::<Value>(text).ok(),
        value => Some(value.clone()),
    };
    parsed.is_some_and(|value| {
        value.get("error").is_some() || value.get("ok").and_then(Value::as_bool) == Some(false)
    })
}

/// Convert an OpenAI assistant message (content + tool_calls) into Anthropic content blocks.
fn anthropic_assistant_message(v: &Value) -> Value {
    if let Some(content) = v.get(ANTHROPIC_CONTENT_FIELD).and_then(Value::as_array) {
        return json!({"role": "assistant", "content": content});
    }
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

/// Merge consecutive same-role messages using Anthropic's documented role-combination semantics.
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
    let finish_reason = body["stop_reason"].as_str().map(str::to_string);
    let content = body
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            AiError::MalformedResponse("Anthropic response has no content blocks".into())
        })?;
    let mut tool_calls = Vec::new();
    let mut text_parts = Vec::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if id.is_empty() || name.is_empty() {
                    return Err(AiError::MalformedResponse(
                        "Anthropic tool_use without an id or name".into(),
                    ));
                }
                let input = block
                    .get("input")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));
                tool_calls.push(RawToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments: input.to_string(),
                });
            }
            Some("text") => text_parts.push(block["text"].as_str().unwrap_or_default()),
            // Thinking and redacted-thinking blocks are not interpreted, but the exact signed
            // blocks are retained below and replayed unchanged on the next tool turn.
            _ => {}
        }
    }

    let mut message = json!({
        "role": "assistant",
        "content": if text_parts.is_empty() {
            Value::Null
        } else {
            Value::String(text_parts.join("\n"))
        },
    });
    message[ANTHROPIC_CONTENT_FIELD] = Value::Array(content.clone());
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(
            tool_calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "type": "function",
                        "function": {"name": call.name, "arguments": call.arguments},
                    })
                })
                .collect(),
        );
    }
    Ok(RawPlanResponse {
        message,
        tool_calls,
        model,
        finish_reason,
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
    let choice = completion
        .get("choices")
        .and_then(|choices| choices.get(0))
        .ok_or_else(|| AiError::MalformedResponse("completion has no choices[0]".into()))?;
    let message = choice
        .get("message")
        .cloned()
        .ok_or_else(|| AiError::MalformedResponse("completion has no choices[0].message".into()))?;
    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .map(str::to_string);

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
    tracing::debug!(
        calls = tool_calls.len(),
        finish_reason = finish_reason.as_deref().unwrap_or(""),
        "completion parsed"
    );
    Ok(RawPlanResponse {
        message,
        tool_calls,
        model,
        finish_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_config() -> AiConfig {
        AiConfig {
            base_url: "http://127.0.0.1:1/v1/".into(),
            model: "test/model".into(),
            reasoning_effort: ReasoningEffort::Default,
            api_key: Some(SecretString::from("sk-test".to_string())),
            timeout: Duration::from_millis(50),
            max_tool_calls: 8,
            prime_team_id: None,
        }
    }

    #[test]
    fn native_anthropic_maps_documented_effort_and_rejects_unsupported_levels() {
        let mut cfg = enabled_config();
        cfg.base_url = crate::ANTHROPIC_BASE_URL.to_string();
        for (effort, wire) in [
            (ReasoningEffort::Default, None),
            (ReasoningEffort::Low, Some("low")),
            (ReasoningEffort::Medium, Some("medium")),
            (ReasoningEffort::High, Some("high")),
            (ReasoningEffort::XHigh, Some("xhigh")),
            (ReasoningEffort::Max, Some("max")),
        ] {
            cfg.reasoning_effort = effort;
            let client = AiClient::new(&cfg).expect("documented Anthropic effort");
            let body = client.build_body(&[ChatMessage::user("digest")], &[], false, None);
            match wire {
                Some(wire) => assert_eq!(body["output_config"]["effort"], wire),
                None => assert!(body.get("output_config").is_none()),
            }
        }

        for effort in [ReasoningEffort::None, ReasoningEffort::Minimal] {
            cfg.reasoning_effort = effort;
            let error = AiClient::new(&cfg).expect_err("unsupported effort must fail locally");
            assert!(error.to_string().contains("none and minimal"), "{error}");
        }
    }

    #[test]
    fn native_anthropic_always_sends_version_and_uses_x_api_key() {
        let mut cfg = enabled_config();
        cfg.base_url = crate::ANTHROPIC_BASE_URL.to_string();
        let client = AiClient::new(&cfg).unwrap();
        let request = client
            .apply_auth(client.http.post(client.endpoint()))
            .build()
            .unwrap();
        assert_eq!(request.headers()["anthropic-version"], "2023-06-01");
        assert_eq!(request.headers()["x-api-key"], "sk-test");
        assert!(
            request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .is_none()
        );

        cfg.api_key = None;
        let keyless = AiClient::new(&cfg).unwrap();
        let request = keyless
            .apply_auth(keyless.http.post(keyless.endpoint()))
            .build()
            .unwrap();
        assert_eq!(request.headers()["anthropic-version"], "2023-06-01");
        assert!(request.headers().get("x-api-key").is_none());
    }

    #[test]
    fn endpoint_join_trims_trailing_slash() {
        let client = AiClient::new(&enabled_config()).unwrap();
        assert_eq!(client.endpoint(), "http://127.0.0.1:1/v1/chat/completions");
    }

    #[test]
    fn official_openai_uses_responses_without_changing_compatible_endpoints() {
        let mut cfg = enabled_config();
        cfg.base_url = format!("{}/", crate::OPENAI_BASE_URL);
        let client = AiClient::new(&cfg).unwrap();
        assert_eq!(client.endpoint(), "https://api.openai.com/v1/responses");
        assert_eq!(client.protocol, WireProtocol::OpenAiResponses);

        cfg.base_url = "https://openai-proxy.example.test/v1".into();
        let client = AiClient::new(&cfg).unwrap();
        assert_eq!(
            client.endpoint(),
            "https://openai-proxy.example.test/v1/chat/completions"
        );
        assert_eq!(client.protocol, WireProtocol::ChatCompletions);
    }

    #[test]
    fn telemetry_endpoint_omits_credentials_query_and_fragment() {
        assert_eq!(
            sanitized_endpoint("https://user:password@example.test/v1/chat?token=secret#debug"),
            "https://example.test/v1/chat"
        );
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
        let body = client.build_body(&[ChatMessage::user("digest")], &[], false, None);
        assert_eq!(body["reasoning_effort"], "minimal");
        assert!(body.get("reasoning").is_none());
        assert!(body.get("thinking").is_none());
        assert_eq!(body["max_tokens"], GLM_PLAN_MAX_TOKENS);

        cfg.base_url = "https://api.z.ai/api/paas/v4".into();
        let client = AiClient::new(&cfg).unwrap();
        let body = client.build_body(&[ChatMessage::user("digest")], &[], false, None);
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("reasoning").is_none());
        assert!(body.get("reasoning_effort").is_none());

        cfg.reasoning_effort = ReasoningEffort::High;
        let client = AiClient::new(&cfg).unwrap();
        let body = client.build_body(&[ChatMessage::user("digest")], &[], false, None);
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("thinking").is_none());

        cfg.model = "openai/gpt-5-mini".into();
        let client = AiClient::new(&cfg).unwrap();
        let body = client.build_body(&[ChatMessage::user("digest")], &[], false, None);
        assert!(body.get("thinking").is_none());
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn official_openai_body_uses_responses_tool_and_reasoning_shapes() {
        let mut cfg = enabled_config();
        cfg.base_url = crate::OPENAI_BASE_URL.into();
        cfg.model = crate::DEFAULT_OPENAI_MODEL.into();
        cfg.reasoning_effort = ReasoningEffort::High;
        let client = AiClient::new(&cfg).unwrap();

        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "git_diff_file",
                "description": "Read one diff",
                "parameters": {"type": "object", "properties": {}},
            },
        })];
        let body = client.build_body(
            &[
                ChatMessage::system("controller"),
                ChatMessage::user("digest"),
            ],
            &tools,
            true,
            Some(4_096),
        );
        assert_eq!(body["max_output_tokens"], 4_096);
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("max_completion_tokens").is_none());
        assert!(body.get("messages").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["instructions"], "controller");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"], "digest");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "git_diff_file");
        assert!(body["tools"][0].get("function").is_none());
        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["store"], false);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn custom_openai_compatible_output_override_uses_max_tokens() {
        let client = AiClient::new(&enabled_config()).unwrap();
        let body = client.build_body(&[ChatMessage::user("digest")], &[], true, Some(4_096));

        assert_eq!(body["max_tokens"], 4_096);
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn glm_output_override_uses_max_tokens() {
        let mut cfg = enabled_config();
        cfg.base_url = crate::PRIME_BASE_URL.into();
        cfg.model = "z-ai/glm-5.3".into();
        let client = AiClient::new(&cfg).unwrap();
        let body = client.build_body(&[ChatMessage::user("digest")], &[], false, Some(4_096));

        assert_eq!(body["max_tokens"], 4_096);
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn native_anthropic_output_override_uses_max_tokens() {
        let mut cfg = enabled_config();
        cfg.base_url = crate::ANTHROPIC_BASE_URL.into();
        cfg.model = crate::DEFAULT_ANTHROPIC_MODEL.into();
        let client = AiClient::new(&cfg).unwrap();
        let body = client.build_body(&[ChatMessage::user("digest")], &[], true, Some(4_096));

        assert_eq!(body["max_tokens"], 4_096);
        assert!(body.get("max_completion_tokens").is_none());
        assert!(body.get("tool_choice").is_none());
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
            "choices": [{"finish_reason": "tool_calls", "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "c1", "type": "function",
                     "function": {"name": "git_diff_file", "arguments": "{\"file\":\"a.go\",\"hunk_index\":0}"}},
                    {"id": "c2", "type": "function",
                     "function": {"name": "inspect_visualization", "arguments": "{}"}}
                ]
            }}]
        });
        let parsed = parse_completion(completion).unwrap();
        assert_eq!(parsed.tool_calls.len(), 2);
        assert_eq!(parsed.tool_calls[1].name, "inspect_visualization");
        assert_eq!(parsed.tool_calls[1].arguments, "{}");
        assert_eq!(parsed.model.as_deref(), Some("m"));
        assert_eq!(parsed.finish_reason.as_deref(), Some("tool_calls"));
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
    fn responses_parser_extracts_calls_text_and_preserves_replay_items() {
        let response = json!({
            "id": "resp_1",
            "model": "gpt-5.6-luna",
            "status": "completed",
            "output": [
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "encrypted_content": "opaque-reasoning",
                    "summary": [],
                    "phase": "analysis"
                },
                {
                    "type": "message",
                    "id": "msg_1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "I will inspect it."}]
                },
                {
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": "git_diff_file",
                    "arguments": "{\"file\":\"src/lib.rs\",\"hunk_index\":0}",
                    "status": "completed"
                }
            ]
        });

        let parsed = parse_openai_response(response.clone()).unwrap();
        assert_eq!(parsed.model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(parsed.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].id, "call_1");
        assert_eq!(parsed.tool_calls[0].name, "git_diff_file");
        assert_eq!(parsed.message["content"], "I will inspect it.");
        assert_eq!(parsed.message[RESPONSES_OUTPUT_FIELD], response["output"]);
    }

    #[test]
    fn responses_replay_preserves_reasoning_and_correlates_tool_output() {
        let prior_output = json!([
            {
                "type": "reasoning",
                "id": "rs_1",
                "encrypted_content": "opaque-reasoning",
                "summary": [],
                "phase": "analysis"
            },
            {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "git_diff_file",
                "arguments": "{\"file\":\"src/lib.rs\"}",
                "status": "completed"
            }
        ]);
        let mut prior_message = json!({"role": "assistant", "content": null});
        prior_message[RESPONSES_OUTPUT_FIELD] = prior_output.clone();
        let messages = vec![
            ChatMessage::system("controller"),
            ChatMessage::user("inspect"),
            ChatMessage::assistant_raw(prior_message),
            ChatMessage::tool("call_1", "diff text 🦀\nsecond line"),
        ];

        let (instructions, input) = openai_responses_input(&messages);
        assert_eq!(instructions, vec!["controller"]);
        assert_eq!(input[0], json!({"role": "user", "content": "inspect"}));
        assert_eq!(input[1], prior_output[0]);
        assert_eq!(input[2], prior_output[1]);
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_1");
        assert_eq!(input[3]["output"], "diff text 🦀\nsecond line");
    }

    #[test]
    fn responses_incomplete_output_limit_maps_to_length() {
        let parsed = parse_openai_response(json!({
            "model": "gpt-5.6-luna",
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": []
        }))
        .unwrap();
        assert_eq!(parsed.finish_reason.as_deref(), Some("length"));
    }

    #[test]
    fn repair_assistant_omits_null_content_and_strips_output_only_fields() {
        assert!(
            ChatMessage::assistant_text_for_repair(&json!({
                "role": "assistant",
                "content": null,
                "reasoning": "internal"
            }))
            .is_none()
        );

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
    fn repair_replays_native_anthropic_blocks_only_without_dangling_tool_calls() {
        let raw_content = json!([{
            "type": "thinking",
            "thinking": "I need a different tool.",
            "signature": "signed-repair-thinking"
        }, {
            "type": "text",
            "text": "I can revise the approach."
        }]);
        let mut natural = json!({
            "role": "assistant",
            "content": "I can revise the approach."
        });
        natural[ANTHROPIC_CONTENT_FIELD] = raw_content;
        let replay = ChatMessage::assistant_text_for_repair(&natural).unwrap();
        assert_eq!(replay.as_value(), &natural);

        let mut rejected_tool_turn = natural;
        rejected_tool_turn["tool_calls"] = json!([{
            "id": "toolu_rejected",
            "type": "function",
            "function": {"name": "wrong_tool", "arguments": "{}"}
        }]);
        let replay = ChatMessage::assistant_text_for_repair(&rejected_tool_turn).unwrap();
        assert_eq!(
            replay.as_value(),
            &json!({"role": "assistant", "content": "I can revise the approach."}),
            "a rejected tool_use cannot be replayed without its matching tool_result"
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
                    "function": {"name": "git_diff_file", "arguments": "{\"file\":\"a.go\"}"},
                }],
            })),
            ChatMessage::tool("c1", "result text"),
        ];
        let tools = vec![json!({
            "type": "function",
            "function": {"name": "git_diff_file", "description": "d", "parameters": {"type":"object"}},
        })];
        let body = build_anthropic_body_with_tool_choice(
            "claude-x",
            ReasoningEffort::Default,
            &messages,
            &tools,
            false,
            None,
        );
        assert_eq!(body["model"], "claude-x");
        assert_eq!(body["system"], "sys");
        assert_eq!(body["tool_choice"]["type"], "auto");
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
    fn anthropic_focused_tool_stays_auto_for_thinking_model_compatibility() {
        let messages = vec![ChatMessage::user("focused handoff")];
        let tools = vec![json!({
            "type": "function",
            "function": {"name": "edit_diagram", "parameters": {"type": "object"}},
        })];
        let body = build_anthropic_body_with_tool_choice(
            "claude-x",
            ReasoningEffort::Default,
            &messages,
            &tools,
            true,
            None,
        );
        assert_eq!(body["tool_choice"]["type"], "auto");
    }

    #[test]
    fn anthropic_hoists_combined_compact_protocol_context() {
        let protocol = "CONSTRUCTION PROTOCOL (mandatory, current step). Apply only `set_intent`.";
        let messages = vec![
            ChatMessage::system(format!("compact base\n\n{protocol}")),
            ChatMessage::user("compact handoff"),
        ];
        let body = build_anthropic_body_with_tool_choice(
            "claude-x",
            ReasoningEffort::Default,
            &messages,
            &[],
            false,
            None,
        );

        assert!(body.get("tool_choice").is_none());
        assert!(body.get("tools").is_none());
        assert_eq!(body["system"], format!("compact base\n\n{protocol}"));
        assert_eq!(
            body["system"]
                .as_str()
                .unwrap()
                .matches("CONSTRUCTION PROTOCOL")
                .count(),
            1
        );
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "compact handoff");
    }

    #[test]
    fn anthropic_response_parses_tool_use_blocks() {
        let body = json!({
            "model": "claude-x",
            "stop_reason": "tool_use",
            "content": [
                {"type": "text", "text": "thinking"},
                {"type": "tool_use", "id": "t1", "name": "inspect_visualization",
                 "input": {"plan_version": 1}},
            ],
        });
        let res = parse_anthropic_response(body).unwrap();
        assert_eq!(res.tool_calls.len(), 1);
        assert_eq!(res.tool_calls[0].name, "inspect_visualization");
        assert!(res.tool_calls[0].arguments.contains("plan_version"));
        assert_eq!(res.finish_reason.as_deref(), Some("tool_use"));
    }

    #[test]
    fn anthropic_tool_continuation_replays_signed_blocks_and_marks_errors() {
        let original_content = json!([
            {
                "type": "thinking",
                "thinking": "I should inspect the selected hunk.",
                "signature": "signed-thinking"
            },
            {"type": "text", "text": "I will inspect it."},
            {
                "type": "tool_use",
                "id": "toolu_1",
                "name": "git_diff_file",
                "input": {"file": "src/lib.rs", "hunk_index": 0}
            }
        ]);
        let parsed = parse_anthropic_response(json!({
            "model": "claude-sonnet-5",
            "stop_reason": "tool_use",
            "content": original_content,
        }))
        .unwrap();
        assert_eq!(
            parsed.message[ANTHROPIC_CONTENT_FIELD], original_content,
            "signed thinking and tool blocks must remain structurally unchanged"
        );

        let messages = vec![
            ChatMessage::user("inspect"),
            ChatMessage::assistant_raw(parsed.message),
            ChatMessage::tool("toolu_1", r#"{"error":"hunk became stale"}"#),
        ];
        let body = build_anthropic_body_with_tool_choice(
            "claude-sonnet-5",
            ReasoningEffort::Medium,
            &messages,
            &[json!({
                "type": "function",
                "function": {
                    "name": "git_diff_file",
                    "description": "Read a diff",
                    "parameters": {"type": "object"}
                }
            })],
            false,
            None,
        );
        assert_eq!(body["messages"][1]["content"], original_content);
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(body["messages"][2]["content"][0]["is_error"], true);
        assert_eq!(body["output_config"]["effort"], "medium");
    }

    #[test]
    fn anthropic_successful_tool_results_do_not_claim_an_error() {
        let messages = vec![ChatMessage::tool("toolu_1", r#"{"ok":true}"#)];
        let body = build_anthropic_body_with_tool_choice(
            "claude-x",
            ReasoningEffort::Default,
            &messages,
            &[json!({
                "type": "function",
                "function": {
                    "name": "inspect_visualization",
                    "description": "inspect",
                    "parameters": {"type": "object"}
                }
            })],
            false,
            None,
        );
        assert!(body["messages"][0]["content"][0].get("is_error").is_none());
    }

    #[test]
    fn anthropic_response_without_tool_use_is_a_natural_completion() {
        let body = json!({"model": "claude-x", "content": [{"type": "text", "text": "hi"}]});
        let response = parse_anthropic_response(body).unwrap();
        assert!(response.tool_calls.is_empty());
        assert_eq!(response.message["content"], "hi");
        assert_eq!(
            response.message[ANTHROPIC_CONTENT_FIELD],
            json!([{"type": "text", "text": "hi"}])
        );
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
        // OpenAI Responses: the models endpoint remains directly under the configured base.
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
