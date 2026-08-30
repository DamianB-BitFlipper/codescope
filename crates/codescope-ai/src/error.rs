//! Error type for the AI layer.
//!
//! Reasons are always safe to surface in the UI/status bar: no API keys, no
//! `Authorization` headers, and no request bodies ever appear in an [`AiError`] message
//! (research 07 §2). Transport errors are sanitized with [`reqwest::Error::without_url`]
//! before formatting so credentials embedded in URLs can never leak.

use std::time::Duration;

/// Errors produced by the codescope AI layer.
#[derive(Debug, thiserror::Error)]
pub enum AiError {
    /// AI is disabled by configuration; no client may be constructed (research 07 §2).
    #[error("ai is disabled by configuration")]
    Disabled,

    /// Invalid configuration (bad env value, unusable base URL, …).
    #[error("invalid ai configuration: {0}")]
    Config(String),

    /// A literal `api_key` value appeared in a config file. Config files may only name an
    /// env var (`api_key_env`); refusing to enable AI prevents keys committed into config
    /// (research 07 §2).
    #[error("literal api_key in a config file is not allowed; set api_key_env to an env var name instead")]
    LiteralApiKeyInConfig,

    /// The local circuit breaker is open after repeated transport failures; the provider
    /// is not contacted (research 07 §4).
    #[error("ai provider unavailable (circuit open, retry in {retry_in:?})")]
    CircuitOpen {
        /// Time until the next probe is allowed.
        retry_in: Duration,
    },

    /// The local rate limiter (token bucket) rejected the request before any network I/O.
    #[error("ai request locally throttled, retry in {retry_after:?}")]
    Throttled {
        /// Time until the limiter would admit the request.
        retry_after: Duration,
    },

    /// HTTP 429 from the provider.
    #[error("ai provider rate limited the request{}", retry_after.map(|d| format!(" (retry after {d:?})")).unwrap_or_default())]
    RateLimited {
        /// Parsed `Retry-After` header, if present (capped by the caller).
        retry_after: Option<Duration>,
    },

    /// Non-success HTTP status other than 429.
    #[error("ai provider returned http {status}: {message}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Short, sanitized message extracted from the error body (truncated).
        message: String,
    },

    /// Connection/protocol-level failure (sanitized; never contains the URL query or
    /// headers).
    #[error("ai transport error: {0}")]
    Transport(String),

    /// The request exceeded the configured timeout.
    #[error("ai request timed out after {0:?}")]
    Timeout(Duration),

    /// The provider response was not a parseable chat completion.
    #[error("ai provider response malformed: {0}")]
    MalformedResponse(String),

    /// The completion carried no tool call at all (provider ignored `tool_choice`).
    #[error("ai provider returned no tool call (tool_choice: required was ignored)")]
    NoToolCall,

    /// The `submit_visualization_plan` arguments were not a valid plan document.
    #[error("visualization plan malformed: {0}")]
    MalformedPlan(String),

    /// The plan declared an unsupported `plan_version`.
    #[error("unsupported plan_version {got} (expected {expected})")]
    PlanVersion {
        /// Version the plan declared.
        got: u32,
        /// The version this build understands ([`codescope_core::PLAN_VERSION`]).
        expected: u32,
    },

    /// The model asked for more read-only tool calls than the per-plan budget allows.
    #[error("tool-call budget exceeded (max {max} per plan)")]
    ToolBudgetExceeded {
        /// The enforced budget.
        max: u32,
    },
}

impl AiError {
    /// `true` for errors worth retrying (429, 5xx, connect errors, timeouts — research 07
    /// §4). Everything else (4xx, malformed responses, local throttle, open circuit) is
    /// not retried.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            AiError::RateLimited { .. } | AiError::Timeout(_) | AiError::Transport(_) => true,
            AiError::Http { status, .. } => *status >= 500,
            _ => false,
        }
    }

    /// The provider-requested backoff for 429 responses, if any.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            AiError::RateLimited { retry_after } => *retry_after,
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryability_matrix() {
        assert!(AiError::RateLimited { retry_after: None }.is_retryable());
        assert!(AiError::Timeout(Duration::from_secs(20)).is_retryable());
        assert!(AiError::Transport("connect refused".into()).is_retryable());
        assert!(AiError::Http {
            status: 503,
            message: "overloaded".into()
        }
        .is_retryable());
        assert!(!AiError::Http {
            status: 401,
            message: "unauthorized".into()
        }
        .is_retryable());
        assert!(!AiError::NoToolCall.is_retryable());
        assert!(!AiError::Disabled.is_retryable());
        assert!(!AiError::CircuitOpen {
            retry_in: Duration::from_secs(60)
        }
        .is_retryable());
        assert!(!AiError::Throttled {
            retry_after: Duration::from_secs(6)
        }
        .is_retryable());
    }

    #[test]
    fn retry_after_extraction() {
        let e = AiError::RateLimited {
            retry_after: Some(Duration::from_secs(3)),
        };
        assert_eq!(e.retry_after(), Some(Duration::from_secs(3)));
        assert_eq!(AiError::NoToolCall.retry_after(), None);
    }
}
