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

use crate::config::AiConfig;
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
    model: String,
    api_key: Option<SecretString>,
    timeout: Duration,
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
        Ok(AiClient {
            http,
            endpoint: format!("{}/chat/completions", config.base_url.trim_end_matches('/')),
            model: config.model.clone(),
            api_key: config.api_key.clone(),
            timeout: config.timeout,
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
        let body = json!({
            "model": self.model,
            "messages": messages,
            "tools": tool_values,
            "tool_choice": "required",
            "stream": false,
        });

        let mut request = self
            .http
            .post(&self.endpoint)
            .timeout(self.timeout)
            .json(&body);
        if let Some(key) = &self.api_key {
            let mut value = reqwest::header::HeaderValue::from_str(&format!(
                "Bearer {}",
                key.expose_secret()
            ))
            .map_err(|_| AiError::Config("api key contains invalid header characters".into()))?;
            value.set_sensitive(true);
            request = request.header(reqwest::header::AUTHORIZATION, value);
        }

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
        parse_completion(completion)
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
}
