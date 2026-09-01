//! AI opt-in configuration (research 05 §5, research 07 §2).
//!
//! Environment-over-file resolution: [`AiConfig::from_env`] reads environment only, while
//! [`AiConfig::from_env_with_file`] accepts the binary's global `[ai]` configuration and then
//! applies the same environment variables as higher-precedence overrides. The supported vars are
//! `CODESCOPE_AI`, `CODESCOPE_AI_BASE_URL`, `CODESCOPE_AI_TIMEOUT_MS`,
//! `CODESCOPE_AI_TOOL_CHOICE`, and the API key from the first of
//! `PRIME_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY` that is set. **AI is disabled by
//! default**: with no explicit `CODESCOPE_AI=on|off` the subsystem enables itself only when
//! an API key is found (auto mode). The default `base_url` follows the key's provider
//! (Prime Inference / OpenAI / Anthropic); `CODESCOPE_AI_BASE_URL` overrides it.
//!
//! Key handling follows research 07 §2 exactly:
//!
//! - config **files** may only *name* an env var ([`AiFileConfig::api_key_env`]); a literal
//!   `api_key` value in a file is a hard error ([`AiError::LiteralApiKeyInConfig`]);
//! - the resolved key lives in a [`SecretString`] and is exposed only when the
//!   `Authorization` header is built;
//! - [`AiConfig`] has a hand-written `Debug` that redacts the key and derives no
//!   `Serialize`/`Display`, so the key cannot leak through logging or serialization.

use crate::error::AiError;
use crate::tools::MAX_TOOL_CALLS;
use secrecy::SecretString;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

/// Default chat-completions base for OpenAI.
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// Default chat-completions base for Prime Inference (used when the key came from
/// `PRIME_API_KEY`).
pub const PRIME_BASE_URL: &str = "https://api.pinference.ai/api/v1";

/// Default base for Anthropic's native Messages API (used when the key came from
/// `ANTHROPIC_API_KEY`).
pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com/v1";

/// Default model for Prime Inference: plans are schema-constrained, so a small model suffices
/// (research 05 §5). The provider-specific default follows the resolved key — a Prime model id
/// is not valid on OpenAI/Anthropic (review 09 finding F2).
pub const DEFAULT_MODEL: &str = "openai/gpt-5-mini";

/// Default model when the key came from `OPENAI_API_KEY`.
pub const DEFAULT_OPENAI_MODEL: &str = "gpt-5-mini";

/// Default model when the key came from `ANTHROPIC_API_KEY` (a `-latest` alias stays valid).
pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-haiku-4-5-latest";

/// Default per-request timeout (research 07 §4: 20 s budget).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(20_000);

/// How the provider should decide whether to call one of Codescope's tools.
///
/// [`ToolChoice::Required`] preserves Codescope's strict default. Some OpenAI-compatible
/// providers only accept [`ToolChoice::Auto`], which can be selected with
/// `CODESCOPE_AI_TOOL_CHOICE=auto`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ToolChoice {
    /// Require the model to call a tool (`required` for OpenAI-compatible providers,
    /// `any` for Anthropic).
    #[default]
    Required,
    /// Let the model decide whether to call a tool.
    Auto,
}

impl ToolChoice {
    fn parse(value: &str) -> Result<Self, AiError> {
        match value.to_ascii_lowercase().as_str() {
            "required" => Ok(Self::Required),
            "auto" => Ok(Self::Auto),
            other => Err(AiError::Config(format!(
                "CODESCOPE_AI_TOOL_CHOICE must be required|auto, got {other:?}"
            ))),
        }
    }

    /// OpenAI-compatible wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Auto => "auto",
        }
    }

    pub(crate) const fn anthropic_type(self) -> &'static str {
        match self {
            Self::Required => "any",
            Self::Auto => "auto",
        }
    }
}

/// Reasoning budget requested from an OpenAI-compatible Chat Completions model.
///
/// [`ReasoningEffort::Default`] selects Codescope's automatic provider/model behavior,
/// which normally omits `reasoning_effort` (Prime-hosted GLM is the compatibility exception:
/// it uses `minimal`). The remaining variants are sent verbatim as the top-level
/// `reasoning_effort` field. Providers and models support different subsets, so an explicit
/// choice can still be rejected honestly by the upstream API.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffort {
    /// Use Codescope's automatic provider/model behavior.
    #[default]
    Default,
    /// Disable reasoning where the model supports it.
    None,
    /// Use the smallest non-zero reasoning budget supported by some model families.
    Minimal,
    /// Use a low reasoning budget.
    Low,
    /// Use a medium reasoning budget.
    Medium,
    /// Use a high reasoning budget.
    High,
    /// Use an extra-high reasoning budget.
    XHigh,
    /// Use the provider's maximum reasoning budget where supported.
    Max,
}

impl ReasoningEffort {
    /// All user-selectable values, in increasing-effort order.
    pub const ALL: [Self; 8] = [
        Self::Default,
        Self::None,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    /// Stable CLI/config/UI spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Explicit wire value, or `None` when compatibility/default behavior should be used.
    #[must_use]
    pub const fn wire_value(self) -> Option<&'static str> {
        match self {
            Self::Default => None,
            other => Some(other.as_str()),
        }
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ReasoningEffort {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "default" => Ok(Self::Default),
            "none" => Ok(Self::None),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" | "x-high" => Ok(Self::XHigh),
            "max" => Ok(Self::Max),
            other => Err(format!(
                "reasoning effort must be default|none|minimal|low|medium|high|xhigh|max, got {other:?}"
            )),
        }
    }
}

impl<'de> serde::Deserialize<'de> for ReasoningEffort {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// Resolved AI configuration.
///
/// Construct via [`AiConfig::from_env`] / [`AiConfig::resolve`]; the `api_key` is `None`
/// whenever the subsystem is disabled, so no key material is held while AI is off.
#[derive(Clone)]
pub struct AiConfig {
    /// Whether the AI subsystem may run at all. `false` ⇒ no HTTP client is constructed.
    pub enabled: bool,
    /// Chat-completions base URL (endpoint is `{base_url}/chat/completions`).
    pub base_url: String,
    /// Model identifier sent in the request body.
    pub model: String,
    /// Chat Completions reasoning budget, or [`ReasoningEffort::Default`] for automatic behavior.
    pub reasoning_effort: ReasoningEffort,
    /// Bearer token, if any (local providers like Ollama run keyless).
    pub api_key: Option<SecretString>,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Provider tool-selection policy. Defaults to [`ToolChoice::Required`].
    pub tool_choice: ToolChoice,
    /// Read-only tool-call budget per plan (≤ [`MAX_TOOL_CALLS`]).
    pub max_tool_calls: u32,
    /// Prime Inference team id, sent as `X-Prime-Team-ID` so requests bill the team balance
    /// instead of the key's personal balance (read from `PRIME_TEAM_ID`; only sent on the
    /// Prime Inference endpoint).
    pub prime_team_id: Option<String>,
}

impl AiConfig {
    /// A disabled configuration (the built-in default: AI off, no key material).
    #[must_use]
    pub fn disabled() -> Self {
        AiConfig {
            enabled: false,
            base_url: OPENAI_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            reasoning_effort: ReasoningEffort::Default,
            api_key: None,
            timeout: DEFAULT_TIMEOUT,
            tool_choice: ToolChoice::Required,
            max_tool_calls: MAX_TOOL_CALLS,
            prime_team_id: None,
        }
    }

    /// Resolve from process environment variables only (no config file). The Codescope binary
    /// uses [`AiConfig::from_env_with_file`] after loading its global user configuration.
    ///
    /// See [`AiConfig::resolve`] for the full resolution rules.
    pub fn from_env() -> Result<Self, AiError> {
        Self::resolve(None, env_lookup)
    }

    /// Resolve from a deserialized config-file section plus process environment.
    pub fn from_env_with_file(file: &AiFileConfig) -> Result<Self, AiError> {
        Self::resolve(Some(file), env_lookup)
    }

    /// Resolve configuration from an optional file layer and an env lookup function
    /// (injectable for tests; env always overrides file — research 07 §2 layering).
    ///
    /// Rules:
    ///
    /// - `CODESCOPE_AI` = `on`/`1`/`true` | `off`/`0`/`false` | `auto` (default). `auto`
    ///   enables AI iff a key resolves; explicit `on` enables even keyless (local
    ///   providers); `off` disables and drops any key material.
    /// - Key resolution order: [`AiFileConfig::api_key_env`]-named var, then
    ///   `PRIME_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`. A named `api_key_env` var
    ///   that is unset is a hard [`AiError::Config`] (silent misconfiguration is worse than
    ///   an error). The three built-in names retain their provider identity; an arbitrary
    ///   name requires an explicit base URL so its credential is never sent to a guessed
    ///   endpoint.
    /// - Default `base_url` follows the key's provider: [`PRIME_BASE_URL`] from
    ///   `PRIME_API_KEY`, [`ANTHROPIC_BASE_URL`] from `ANTHROPIC_API_KEY`, otherwise
    ///   [`OPENAI_BASE_URL`]. `CODESCOPE_AI_BASE_URL` overrides.
    /// - A literal [`AiFileConfig::api_key`] in the file layer is
    ///   [`AiError::LiteralApiKeyInConfig`], even when AI ends up disabled.
    /// - `CODESCOPE_AI_TOOL_CHOICE` accepts `required` (default) or `auto`.
    ///
    /// Empty / whitespace-only env values are treated as unset.
    pub fn resolve(
        file: Option<&AiFileConfig>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, AiError> {
        let env = |name: &str| {
            env(name)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        if let Some(file) = file {
            if file.api_key.is_some() {
                return Err(AiError::LiteralApiKeyInConfig);
            }
        }

        let mode = match env("CODESCOPE_AI") {
            None => file.and_then(|f| f.enabled).map_or(AiMode::Auto, |on| {
                if on {
                    AiMode::On
                } else {
                    AiMode::Off
                }
            }),
            Some(v) => AiMode::parse(&v)?,
        };

        if mode == AiMode::Off {
            tracing::debug!("ai disabled by configuration");
            return Ok(AiConfig::disabled());
        }

        let (key, key_source) = resolve_key(file, &env)?;
        let enabled = match mode {
            AiMode::On => true,
            AiMode::Auto => key.is_some(),
            AiMode::Off => unreachable!("handled above"),
        };
        if !enabled {
            tracing::debug!("ai disabled: auto mode and no api key found");
            return Ok(AiConfig::disabled());
        }

        let configured_base =
            env("CODESCOPE_AI_BASE_URL").or_else(|| file.and_then(|f| f.base_url.clone()));
        if matches!(key_source, Some(KeySource::FileNamedEnv)) && configured_base.is_none() {
            return Err(AiError::Config(
                "an arbitrary api_key_env requires an explicit base_url so its credential is not sent to an inferred provider"
                    .into(),
            ));
        }
        let default_base = match key_source {
            Some(KeySource::PrimeApiKey) => PRIME_BASE_URL,
            Some(KeySource::AnthropicApiKey) => ANTHROPIC_BASE_URL,
            _ => OPENAI_BASE_URL,
        };
        let base_url = configured_base.unwrap_or_else(|| default_base.to_string());
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            return Err(AiError::Config(format!(
                "base_url must start with http:// or https://, got {base_url:?}"
            )));
        }

        let default_model = match key_source {
            Some(KeySource::OpenaiApiKey) => DEFAULT_OPENAI_MODEL,
            Some(KeySource::AnthropicApiKey) => DEFAULT_ANTHROPIC_MODEL,
            Some(KeySource::FileNamedEnv)
                if base_url.trim_end_matches('/') == OPENAI_BASE_URL.trim_end_matches('/') =>
            {
                DEFAULT_OPENAI_MODEL
            }
            Some(KeySource::FileNamedEnv)
                if base_url.trim_end_matches('/') == ANTHROPIC_BASE_URL.trim_end_matches('/') =>
            {
                DEFAULT_ANTHROPIC_MODEL
            }
            _ => DEFAULT_MODEL, // Prime, file-named, or keyless local
        };
        let model = file
            .and_then(|f| f.model.clone())
            .unwrap_or_else(|| default_model.to_string());
        let reasoning_effort = file.and_then(|f| f.reasoning_effort).unwrap_or_default();

        let timeout_ms = match env("CODESCOPE_AI_TIMEOUT_MS") {
            Some(v) => v.parse::<u64>().map_err(|e| {
                AiError::Config(format!("CODESCOPE_AI_TIMEOUT_MS must be an integer: {e}"))
            })?,
            None => file.and_then(|f| f.timeout_ms).unwrap_or(20_000),
        };
        if timeout_ms == 0 {
            return Err(AiError::Config("timeout must be greater than zero".into()));
        }

        let tool_choice = env("CODESCOPE_AI_TOOL_CHOICE")
            .map(|value| ToolChoice::parse(&value))
            .transpose()?
            .unwrap_or_default();

        let max_tool_calls = file
            .and_then(|f| f.max_tool_calls)
            .unwrap_or(MAX_TOOL_CALLS);
        let prime_team_id = env("PRIME_TEAM_ID");

        let max_tool_calls = if max_tool_calls > MAX_TOOL_CALLS {
            tracing::warn!(
                requested = max_tool_calls,
                cap = MAX_TOOL_CALLS,
                "max_tool_calls exceeds the per-plan budget cap; clamping"
            );
            MAX_TOOL_CALLS
        } else {
            max_tool_calls
        };

        tracing::info!(
            base_url = %base_url,
            model = %model,
            reasoning_effort = %reasoning_effort,
            timeout_ms,
            tool_choice = tool_choice.as_str(),
            max_tool_calls,
            keyed = key.is_some(),
            "ai enabled"
        );
        Ok(AiConfig {
            enabled: true,
            base_url,
            model,
            reasoning_effort,
            api_key: key.map(SecretString::from),
            timeout: Duration::from_millis(timeout_ms),
            tool_choice,
            max_tool_calls,
            prime_team_id,
        })
    }
}

impl AiConfig {
    /// The provider protocol the base URL speaks.
    ///
    /// Anthropic's native Messages API is **not** OpenAI-compatible (different envelope and
    /// auth header), so the client must know which protocol to use. Inference: the default
    /// Anthropic base URL, or any URL whose host contains `anthropic.com`, selects the
    /// Anthropic protocol; everything else is OpenAI-compatible.
    #[must_use]
    pub fn provider(&self) -> ProviderKind {
        if self.base_url.trim_end_matches('/') == ANTHROPIC_BASE_URL
            || self.base_url.contains("anthropic.com")
        {
            ProviderKind::Anthropic
        } else {
            ProviderKind::OpenAiCompatible
        }
    }

    /// A short display label for which credential/provider is active (for the UI).
    ///
    /// Derived from the base URL: the provider whose default base matches, or "custom" for an
    /// overridden/unknown base. Only meaningful for an enabled config.
    #[must_use]
    pub fn provider_label(&self) -> &'static str {
        let base = self.base_url.trim_end_matches('/');
        if base == PRIME_BASE_URL.trim_end_matches('/') {
            "prime"
        } else if base == OPENAI_BASE_URL.trim_end_matches('/') {
            "openai"
        } else if base == ANTHROPIC_BASE_URL.trim_end_matches('/') {
            "anthropic"
        } else {
            "custom"
        }
    }
}

/// The wire protocol a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// OpenAI-compatible `POST {base}/chat/completions` (OpenAI, Prime Inference, Ollama, …).
    OpenAiCompatible,
    /// Anthropic's native `POST {base}/messages` with `x-api-key` auth.
    Anthropic,
}

/// Hand-written `Debug`: never prints key material (research 07 §2).
impl fmt::Debug for AiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AiConfig")
            .field("enabled", &self.enabled)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("api_key", &self.api_key.as_ref().map(|_| "«redacted»"))
            .field("timeout", &self.timeout)
            .field("tool_choice", &self.tool_choice)
            .field("max_tool_calls", &self.max_tool_calls)
            .finish()
    }
}

/// The `[ai]` section of a codescope config file, as the binary deserializes it.
///
/// Files may only *name* the env var holding the key (`api_key_env`); the presence of a
/// literal `api_key` value is rejected wholesale by [`AiConfig::resolve`] so keys cannot be
/// committed into config files (research 07 §2).
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct AiFileConfig {
    /// Explicit on/off; absent = auto (on iff a key is found).
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Base URL override.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Model override.
    #[serde(default)]
    pub model: Option<String>,
    /// Chat Completions reasoning budget (`default` uses automatic behavior).
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// *Name* of the env var to read the key from (e.g. `"OPENAI_API_KEY"`).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Literal key value — **always rejected**; present in the struct only so a config
    /// file containing it deserializes and can then be refused with a clear error.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Per-request timeout override, in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Read-only tool-call budget override (clamped to [`MAX_TOOL_CALLS`]).
    #[serde(default)]
    pub max_tool_calls: Option<u32>,
}

/// Tri-state enable mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AiMode {
    On,
    Off,
    Auto,
}

impl AiMode {
    fn parse(v: &str) -> Result<Self, AiError> {
        match v.to_ascii_lowercase().as_str() {
            "on" | "1" | "true" => Ok(AiMode::On),
            "off" | "0" | "false" => Ok(AiMode::Off),
            "auto" => Ok(AiMode::Auto),
            other => Err(AiError::Config(format!(
                "CODESCOPE_AI must be on|off|auto (or 1|0|true|false), got {other:?}"
            ))),
        }
    }
}

/// Where the resolved key came from (drives the default base URL).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeySource {
    FileNamedEnv,
    PrimeApiKey,
    OpenaiApiKey,
    AnthropicApiKey,
}

/// Resolve the API key: file-named env var first, then the built-in fallback chain.
fn resolve_key(
    file: Option<&AiFileConfig>,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<(Option<String>, Option<KeySource>), AiError> {
    if let Some(name) = file.and_then(|f| f.api_key_env.as_deref()) {
        let source = match name {
            "PRIME_API_KEY" => KeySource::PrimeApiKey,
            "OPENAI_API_KEY" => KeySource::OpenaiApiKey,
            "ANTHROPIC_API_KEY" => KeySource::AnthropicApiKey,
            _ => KeySource::FileNamedEnv,
        };
        return match env(name) {
            Some(v) => Ok((Some(v), Some(source))),
            None => Err(AiError::Config(format!(
                "api_key_env names env var {name:?}, which is unset or empty"
            ))),
        };
    }
    let chain = [
        ("PRIME_API_KEY", KeySource::PrimeApiKey),
        ("OPENAI_API_KEY", KeySource::OpenaiApiKey),
        ("ANTHROPIC_API_KEY", KeySource::AnthropicApiKey),
    ];
    for (name, source) in chain {
        if let Some(v) = env(name) {
            return Ok((Some(v), Some(source)));
        }
    }
    Ok((None, None))
}

/// `std::env::var` as an `Option`-returning lookup.
fn env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn prime_team_id_reads_from_env() {
        let cfg = AiConfig::resolve(
            None,
            env_of(&[("PRIME_API_KEY", "sk-p"), ("PRIME_TEAM_ID", "team-123")]),
        )
        .unwrap();
        assert_eq!(cfg.prime_team_id.as_deref(), Some("team-123"));
        // absent when unset
        let cfg2 = AiConfig::resolve(None, env_of(&[("PRIME_API_KEY", "sk-p")])).unwrap();
        assert!(cfg2.prime_team_id.is_none());
    }

    #[test]
    fn provider_label_follows_the_base_url() {
        let mut c = AiConfig::disabled();
        c.base_url = PRIME_BASE_URL.to_string();
        assert_eq!(c.provider_label(), "prime");
        c.base_url = OPENAI_BASE_URL.to_string();
        assert_eq!(c.provider_label(), "openai");
        c.base_url = ANTHROPIC_BASE_URL.to_string();
        assert_eq!(c.provider_label(), "anthropic");
        c.base_url = "http://127.0.0.1:11434/v1".to_string();
        assert_eq!(c.provider_label(), "custom");
    }

    #[test]
    fn disabled_by_default_without_key() {
        let cfg = AiConfig::resolve(None, env_of(&[])).unwrap();
        assert!(!cfg.enabled);
        assert!(cfg.api_key.is_none());
    }

    #[test]
    fn auto_enables_with_key_and_prefers_prime_key() {
        let cfg = AiConfig::resolve(
            None,
            env_of(&[
                ("PRIME_API_KEY", "sk-prime"),
                ("OPENAI_API_KEY", "sk-openai"),
                ("ANTHROPIC_API_KEY", "sk-anthropic"),
            ]),
        )
        .unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.api_key.as_ref().unwrap().expose_secret(), "sk-prime");
        // Key came from PRIME_API_KEY → Prime default base.
        assert_eq!(cfg.base_url, PRIME_BASE_URL);
        assert_eq!(cfg.model, DEFAULT_MODEL);
        assert_eq!(cfg.timeout, DEFAULT_TIMEOUT);
        assert_eq!(cfg.max_tool_calls, MAX_TOOL_CALLS);
        assert_eq!(cfg.provider(), ProviderKind::OpenAiCompatible);
    }

    #[test]
    fn prime_key_fallback_selects_prime_base_url() {
        let cfg = AiConfig::resolve(None, env_of(&[("PRIME_API_KEY", "sk-prime")])).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.base_url, PRIME_BASE_URL);
        assert_eq!(cfg.api_key.as_ref().unwrap().expose_secret(), "sk-prime");
    }

    #[test]
    fn openai_key_selects_openai_base_url() {
        let cfg = AiConfig::resolve(None, env_of(&[("OPENAI_API_KEY", "sk-openai")])).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.base_url, OPENAI_BASE_URL);
        assert_eq!(cfg.api_key.as_ref().unwrap().expose_secret(), "sk-openai");
        assert_eq!(cfg.provider(), ProviderKind::OpenAiCompatible);
    }

    #[test]
    fn anthropic_key_selects_anthropic_base_url_and_provider() {
        let cfg = AiConfig::resolve(None, env_of(&[("ANTHROPIC_API_KEY", "sk-ant")])).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.base_url, ANTHROPIC_BASE_URL);
        assert_eq!(cfg.api_key.as_ref().unwrap().expose_secret(), "sk-ant");
        assert_eq!(cfg.provider(), ProviderKind::Anthropic);
        // The default model is a valid Anthropic id, not a Prime-only one (review 09 F2).
        assert_eq!(cfg.model, DEFAULT_ANTHROPIC_MODEL);
    }

    #[test]
    fn openai_key_defaults_to_an_openai_model_id() {
        let cfg = AiConfig::resolve(None, env_of(&[("OPENAI_API_KEY", "sk-o")])).unwrap();
        assert_eq!(cfg.model, DEFAULT_OPENAI_MODEL);
    }

    #[test]
    fn prime_key_defaults_to_prime_model_id() {
        let cfg = AiConfig::resolve(None, env_of(&[("PRIME_API_KEY", "sk-p")])).unwrap();
        assert_eq!(cfg.model, DEFAULT_MODEL);
    }

    #[test]
    fn model_is_not_selected_from_the_environment() {
        let cfg = AiConfig::resolve(
            None,
            env_of(&[
                ("OPENAI_API_KEY", "sk-o"),
                ("CODESCOPE_AI_MODEL", "ignored/model"),
            ]),
        )
        .unwrap();
        assert_eq!(cfg.model, DEFAULT_OPENAI_MODEL);
    }

    #[test]
    fn openai_wins_over_anthropic() {
        let cfg = AiConfig::resolve(
            None,
            env_of(&[("OPENAI_API_KEY", "sk-o"), ("ANTHROPIC_API_KEY", "sk-a")]),
        )
        .unwrap();
        assert_eq!(cfg.api_key.as_ref().unwrap().expose_secret(), "sk-o");
        assert_eq!(cfg.base_url, OPENAI_BASE_URL);
    }

    #[test]
    fn explicit_off_wins_over_key() {
        let cfg = AiConfig::resolve(
            None,
            env_of(&[("CODESCOPE_AI", "off"), ("OPENAI_API_KEY", "sk-x")]),
        )
        .unwrap();
        assert!(!cfg.enabled);
        assert!(cfg.api_key.is_none(), "off must drop key material");
    }

    #[test]
    fn explicit_on_enables_keyless_local_providers() {
        let cfg = AiConfig::resolve(
            None,
            env_of(&[
                ("CODESCOPE_AI", "on"),
                ("CODESCOPE_AI_BASE_URL", "http://127.0.0.1:11434/v1"),
            ]),
        )
        .unwrap();
        assert!(cfg.enabled);
        assert!(cfg.api_key.is_none());
        assert_eq!(cfg.base_url, "http://127.0.0.1:11434/v1");
    }

    #[test]
    fn env_overrides_and_timeout_parse() {
        let cfg = AiConfig::resolve(
            None,
            env_of(&[
                ("CODESCOPE_AI", "on"),
                ("CODESCOPE_AI_BASE_URL", "https://example.test/v1"),
                ("CODESCOPE_AI_TIMEOUT_MS", "1500"),
                ("CODESCOPE_AI_TOOL_CHOICE", "auto"),
                ("OPENAI_API_KEY", "sk-x"),
            ]),
        )
        .unwrap();
        assert_eq!(cfg.base_url, "https://example.test/v1");
        assert_eq!(cfg.model, DEFAULT_OPENAI_MODEL);
        assert_eq!(cfg.timeout, Duration::from_millis(1500));
        assert_eq!(cfg.tool_choice, ToolChoice::Auto);
    }

    #[test]
    fn tool_choice_defaults_to_required() {
        let cfg = AiConfig::resolve(None, env_of(&[("OPENAI_API_KEY", "sk-x")])).unwrap();
        assert_eq!(cfg.tool_choice, ToolChoice::Required);
    }

    #[test]
    fn bad_values_are_config_errors() {
        assert!(matches!(
            AiConfig::resolve(None, env_of(&[("CODESCOPE_AI", "maybe")])),
            Err(AiError::Config(_))
        ));
        assert!(matches!(
            AiConfig::resolve(
                None,
                env_of(&[("CODESCOPE_AI", "on"), ("CODESCOPE_AI_TIMEOUT_MS", "soon")]),
            ),
            Err(AiError::Config(_))
        ));
        assert!(matches!(
            AiConfig::resolve(
                None,
                env_of(&[("CODESCOPE_AI", "on"), ("CODESCOPE_AI_TIMEOUT_MS", "0")]),
            ),
            Err(AiError::Config(_))
        ));
        assert!(matches!(
            AiConfig::resolve(
                None,
                env_of(&[
                    ("CODESCOPE_AI", "on"),
                    ("CODESCOPE_AI_TOOL_CHOICE", "sometimes"),
                ]),
            ),
            Err(AiError::Config(_))
        ));
        assert!(matches!(
            AiConfig::resolve(
                None,
                env_of(&[
                    ("CODESCOPE_AI", "on"),
                    ("CODESCOPE_AI_BASE_URL", "ftp://nope"),
                    ("OPENAI_API_KEY", "sk-x"),
                ]),
            ),
            Err(AiError::Config(_))
        ));
    }

    #[test]
    fn empty_env_values_are_unset() {
        let cfg = AiConfig::resolve(
            None,
            env_of(&[("PRIME_API_KEY", "   "), ("CODESCOPE_AI", "auto")]),
        )
        .unwrap();
        assert!(!cfg.enabled);
    }

    #[test]
    fn literal_api_key_in_file_is_rejected() {
        let file = AiFileConfig {
            api_key: Some("sk-oops".into()),
            ..AiFileConfig::default()
        };
        let err = AiConfig::resolve(Some(&file), env_of(&[("OPENAI_API_KEY", "sk-x")]));
        assert!(matches!(err, Err(AiError::LiteralApiKeyInConfig)));
        // Rejected even when AI would end up disabled.
        let err = AiConfig::resolve(Some(&file), env_of(&[]));
        assert!(matches!(err, Err(AiError::LiteralApiKeyInConfig)));
    }

    #[test]
    fn api_key_env_names_the_var() {
        let file = AiFileConfig {
            api_key_env: Some("MY_CUSTOM_KEY".into()),
            base_url: Some("http://127.0.0.1:11434/v1".into()),
            ..AiFileConfig::default()
        };
        let cfg =
            AiConfig::resolve(Some(&file), env_of(&[("MY_CUSTOM_KEY", "sk-custom")])).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.api_key.as_ref().unwrap().expose_secret(), "sk-custom");
        // Named but unset → hard error, not silent disable.
        assert!(matches!(
            AiConfig::resolve(Some(&file), env_of(&[])),
            Err(AiError::Config(_))
        ));

        let missing_endpoint = AiFileConfig {
            api_key_env: Some("MY_CUSTOM_KEY".into()),
            ..AiFileConfig::default()
        };
        assert!(matches!(
            AiConfig::resolve(
                Some(&missing_endpoint),
                env_of(&[("MY_CUSTOM_KEY", "sk-custom")])
            ),
            Err(AiError::Config(_))
        ));
    }

    #[test]
    fn built_in_api_key_env_names_keep_provider_identity() {
        for (name, base, model, provider) in [
            ("PRIME_API_KEY", PRIME_BASE_URL, DEFAULT_MODEL, "prime"),
            (
                "OPENAI_API_KEY",
                OPENAI_BASE_URL,
                DEFAULT_OPENAI_MODEL,
                "openai",
            ),
            (
                "ANTHROPIC_API_KEY",
                ANTHROPIC_BASE_URL,
                DEFAULT_ANTHROPIC_MODEL,
                "anthropic",
            ),
        ] {
            let file = AiFileConfig {
                api_key_env: Some(name.into()),
                ..AiFileConfig::default()
            };
            let cfg = AiConfig::resolve(Some(&file), env_of(&[(name, "secret")])).unwrap();
            assert_eq!(cfg.base_url, base, "{name}");
            assert_eq!(cfg.model, model, "{name}");
            assert_eq!(cfg.provider_label(), provider, "{name}");
        }
    }

    #[test]
    fn file_layer_is_overridden_by_env() {
        let file = AiFileConfig {
            enabled: Some(true),
            base_url: Some("https://file.example/v1".into()),
            model: Some("file/model".into()),
            timeout_ms: Some(9000),
            max_tool_calls: Some(4),
            ..AiFileConfig::default()
        };
        let cfg = AiConfig::resolve(Some(&file), env_of(&[("OPENAI_API_KEY", "sk-x")])).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.base_url, "https://file.example/v1"); // no env override
        assert_eq!(cfg.model, "file/model");
        assert_eq!(cfg.timeout, Duration::from_millis(9000));
        assert_eq!(cfg.max_tool_calls, 4);
    }

    #[test]
    fn max_tool_calls_clamped_to_budget() {
        let file = AiFileConfig {
            enabled: Some(true),
            max_tool_calls: Some(99),
            ..AiFileConfig::default()
        };
        let cfg = AiConfig::resolve(Some(&file), env_of(&[("OPENAI_API_KEY", "sk-x")])).unwrap();
        assert_eq!(cfg.max_tool_calls, MAX_TOOL_CALLS);
    }

    #[test]
    fn debug_never_leaks_the_key() {
        let cfg =
            AiConfig::resolve(None, env_of(&[("PRIME_API_KEY", "sk-supersecret-123")])).unwrap();
        let debug = format!("{cfg:?}");
        assert!(!debug.contains("supersecret"), "debug leaked key: {debug}");
        assert!(debug.contains("redacted"));
        // The SecretString itself also redacts.
        let inner = format!("{:?}", cfg.api_key.as_ref().unwrap());
        assert!(!inner.contains("supersecret"), "secrecy leaked: {inner}");
    }
}
