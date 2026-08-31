//! The AI plan service: digest in → validated plan (or honest failure) out.
//!
//! [`AiService::request_plan`] drives the full loop (research 05 §4–5):
//!
//! 1. redact absolute repo paths from the digest (strip the repo-root prefix — research
//!    07 §2: only repo-relative paths leave the machine);
//! 2. one chat turn with all read-only tools + the required `submit_visualization_plan`
//!    tool; transient failures (429/5xx/timeout/connect) retried twice with exponential
//!    backoff + jitter, honoring `Retry-After` (backon);
//! 3. read-only tool calls are executed through the caller's [`ToolExecutor`] under the
//!    ≤ [`MAX_TOOL_CALLS`](crate::MAX_TOOL_CALLS) budget, their results redacted and fed
//!    back;
//! 4. the submitted plan is parsed ([`parse_plan`]) and validated ([`validate`]) against
//!    the caller's [`FactView`] and the current epoch.
//!
//! Every path ends in an [`AiOutcome`] — the service never panics on provider behavior
//! and never blocks the UI: callers `tokio::spawn` the future and apply the outcome at
//! their epoch gate.

use crate::client::{AiClient, AiClientOptions, ChatMessage, RawPlanResponse};
use crate::config::AiConfig;
use crate::error::AiError;
use crate::plan::parse_plan;
use crate::tools::{is_read_only_tool, read_only_tools, ToolDef, ToolExecutor};
use crate::validator::{validate, FactView};
use backon::{ExponentialBuilder, Retryable};
use camino::{Utf8Path, Utf8PathBuf};
use codescope_core::{
    Epoch, ValidationReport, ValidationVerdict, VisualizationPlan, MAX_FORMS_PER_PLAN,
    MAX_FORM_DEPTH, MAX_FORM_NODES, MAX_SUMMARY_LINES, PLAN_VERSION,
};
use std::time::Duration;

/// Terminal result of one plan request, ready for the dispatcher/TUI.
#[derive(Debug, Clone, PartialEq)]
pub enum AiOutcome {
    /// A renderable plan (already sanitized) plus its validation report.
    Plan(VisualizationPlan, ValidationReport),
    /// The plan's epoch no longer matches; keep the last render, re-request.
    Stale,
    /// The request failed; `reason` is safe for the status line (never contains secrets).
    Failed(String),
    /// The provider is unreachable/cooling down (circuit open, local throttle, disabled):
    /// deterministic-only mode.
    Unavailable,
}

/// Retry policy for transient transport errors (research 07 §4: 2 retries, jitter,
/// honor `Retry-After`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// First backoff delay.
    pub min_delay: Duration,
    /// Maximum retry attempts after the initial one.
    pub max_times: usize,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            min_delay: Duration::from_millis(500),
            max_times: 2,
        }
    }
}

/// The AI plan service. Construct once per session (only when AI is enabled) and share.
#[derive(Debug)]
pub struct AiService {
    client: AiClient,
    config: AiConfig,
    repo_root: Utf8PathBuf,
    retry: RetryPolicy,
}

impl AiService {
    /// Build a service from an enabled config; `repo_root` is the absolute repository
    /// toplevel used for outbound redaction.
    ///
    /// Errors with [`AiError::Disabled`] when AI is off — a disabled config constructs
    /// nothing (research 07 §2).
    pub fn new(config: AiConfig, repo_root: impl Into<Utf8PathBuf>) -> Result<Self, AiError> {
        Self::with_options(
            config,
            repo_root,
            AiClientOptions::default(),
            RetryPolicy::default(),
        )
    }

    /// Build with explicit client options and retry policy (tests tighten both).
    pub fn with_options(
        config: AiConfig,
        repo_root: impl Into<Utf8PathBuf>,
        client_options: AiClientOptions,
        retry: RetryPolicy,
    ) -> Result<Self, AiError> {
        let client = AiClient::with_options(&config, client_options)?;
        Ok(AiService {
            client,
            config,
            repo_root: repo_root.into(),
            retry,
        })
    }

    /// The underlying client (status probes: circuit state, endpoint).
    #[must_use]
    pub fn client(&self) -> &AiClient {
        &self.client
    }

    /// The model currently in use.
    #[must_use]
    pub fn model(&self) -> String {
        self.client.model()
    }

    /// Which provider/credential is active ("prime"/"openai"/"anthropic"/"custom").
    #[must_use]
    pub fn provider_label(&self) -> &'static str {
        self.config.provider_label()
    }

    /// Switch the model for subsequent plan requests (the TUI model picker).
    pub fn set_model(&self, model: impl Into<String>) {
        self.client.set_model(model);
    }

    /// Request, execute tools for, parse, and validate one visualization plan.
    ///
    /// `digest` is the rendered change digest (tier 1–5 text; absolute repo paths are
    /// stripped before sending). `tools` executes read-only tool calls against the fact
    /// store; `facts` is the validation boundary; `epoch` is the repo-state generation the
    /// digest was built from.
    ///
    /// This future performs network I/O and tool execution — callers spawn it and must
    /// re-check the epoch when applying the outcome (research 06).
    #[tracing::instrument(level = "info", skip_all, fields(%epoch, digest_bytes = digest.len()))]
    pub async fn request_plan(
        &self,
        digest: &str,
        tools: &dyn ToolExecutor,
        facts: &dyn FactView,
        epoch: Epoch,
    ) -> AiOutcome {
        let digest = crate::scrub::scrub_secrets(&redact_repo_root(digest, &self.repo_root));
        let mut messages = vec![
            ChatMessage::system(build_system_prompt(epoch, self.config.max_tool_calls)),
            ChatMessage::user(format!("current epoch: {}\n\n{digest}", epoch.get())),
        ];
        let tool_defs = read_only_tools();

        let mut remaining = self.config.max_tool_calls;
        // Every productive turn either submits the plan or consumes ≥1 budget unit, so
        // budget + 2 turns bounds the loop even against a pathological provider.
        let max_turns = self.config.max_tool_calls as usize + 2;

        for turn in 0..max_turns {
            let response = match self.chat_turn(&messages, &tool_defs).await {
                Ok(r) => r,
                Err(e) => return outcome_from_error(&e),
            };

            if let Some(arguments) = response.plan_arguments() {
                let mut plan = match parse_plan(arguments) {
                    Ok(p) => p,
                    Err(e) => return outcome_from_error(&e),
                };
                let report = validate(&mut plan, facts, epoch);
                return match report.verdict {
                    ValidationVerdict::Stale => AiOutcome::Stale,
                    ValidationVerdict::Rejected => {
                        AiOutcome::Failed(format!("plan rejected: {}", report.notes.join("; ")))
                    }
                    ValidationVerdict::Valid | ValidationVerdict::ValidWithDrops => {
                        AiOutcome::Plan(plan, report)
                    }
                };
            }

            // No plan yet: execute the read-only tool calls under the budget.
            tracing::debug!(
                turn,
                calls = response.tool_calls.len(),
                remaining,
                "tool turn"
            );
            let assistant = ChatMessage::assistant_raw(response.message.clone());
            let mut tool_messages = Vec::new();
            for call in response.read_only_calls() {
                if remaining == 0 {
                    let err = AiError::ToolBudgetExceeded {
                        max: self.config.max_tool_calls,
                    };
                    tracing::warn!(%err, "aborting plan request");
                    return AiOutcome::Failed(err.to_string());
                }
                remaining -= 1;
                let result = self.execute_tool(tools, &call.name, &call.arguments).await;
                tool_messages.push(ChatMessage::tool(call.id.clone(), result));
            }
            if tool_messages.is_empty() {
                // Defensive: parse_completion guarantees ≥1 call, so this is a plan-less,
                // tool-less message — treat as protocol failure.
                return AiOutcome::Failed(AiError::NoToolCall.to_string());
            }
            messages.push(assistant);
            messages.extend(tool_messages);
        }
        AiOutcome::Failed(format!(
            "model did not submit a plan within {max_turns} turns"
        ))
    }

    /// One chat turn with retry (429/5xx/timeout/connect only), honoring `Retry-After`.
    async fn chat_turn(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDef],
    ) -> Result<RawPlanResponse, AiError> {
        let call = || self.client.chat_with_plan(messages, tools);
        call.retry(
            ExponentialBuilder::default()
                .with_min_delay(self.retry.min_delay)
                .with_max_times(self.retry.max_times)
                .with_jitter(),
        )
        .when(AiError::is_retryable)
        .adjust(|err, dur| match (err.retry_after(), dur) {
            // Honor the provider's Retry-After (already capped) but never extend the
            // retry count: a `None` budget stays exhausted.
            (Some(after), Some(_)) => Some(after),
            _ => dur,
        })
        .notify(|err, dur| tracing::info!(error = %err, backoff = ?dur, "retrying ai request"))
        .await
    }

    /// Execute one read-only tool call; failures become error results the model can see.
    async fn execute_tool(&self, tools: &dyn ToolExecutor, name: &str, arguments: &str) -> String {
        if !is_read_only_tool(name) {
            tracing::warn!(tool = name, "model requested an unknown tool");
            return error_result(format!(
                "unknown tool {name:?}: only the read-only tools offered may be called"
            ));
        }
        let args: serde_json::Value = match serde_json::from_str(arguments) {
            Ok(v) => v,
            Err(e) => return error_result(format!("arguments are not valid JSON: {e}")),
        };
        match tools.execute(name, &args).await {
            Ok(result) => redact_repo_root(&result, &self.repo_root),
            Err(e) => {
                tracing::debug!(tool = name, error = %e, "tool execution failed");
                error_result(redact_repo_root(&e.0, &self.repo_root))
            }
        }
    }
}

/// JSON error payload for a failed tool call.
fn error_result(message: String) -> String {
    serde_json::json!({ "error": message }).to_string()
}

/// Map a terminal error onto the UI-facing outcome.
fn outcome_from_error(error: &AiError) -> AiOutcome {
    match error {
        AiError::Disabled | AiError::CircuitOpen { .. } | AiError::Throttled { .. } => {
            tracing::info!(%error, "ai unavailable");
            AiOutcome::Unavailable
        }
        other => {
            tracing::warn!(error = %other, "ai request failed");
            AiOutcome::Failed(other.to_string())
        }
    }
}

/// Strip the absolute repository root prefix from `text`, making every path repo-relative
/// (research 07 §2: absolute paths leak username/home and never leave the machine).
#[must_use]
pub fn redact_repo_root(text: &str, root: &Utf8Path) -> String {
    let root = root.as_str().trim_end_matches('/');
    if root.is_empty() {
        return text.to_string();
    }
    text.replace(&format!("{root}/"), "").replace(root, "")
}

/// The system prompt: Show Me rules as hard constraints plus the epoch echo contract.
fn build_system_prompt(epoch: Epoch, max_tool_calls: u32) -> String {
    format!(
        "You are codescope's visualization planner. Answer exactly ONE question about the \
         current code change with a small, precise visualization plan.\n\
         Rules:\n\
         - Pick the smallest view that makes the key point clear: at most {MAX_FORMS_PER_PLAN} \
           forms, {MAX_FORM_NODES} nodes per form, tree depth {MAX_FORM_DEPTH}, summary at most \
           {MAX_SUMMARY_LINES} lines.\n\
         - Choose the form kind that matches the question: changed_symbol_tree, call_tree, \
           type_impl_tree, relationship_flow, impact_summary, focused_diff, before_after, \
           sequence.\n\
         - Every node entity (file, symbol, range) MUST be copied verbatim from the digest or \
           a tool result. Never invent files, symbols, ranges, or edges; edges may only select \
           relationships already stated. focused_diff bullets reference hunks as \
           {{\"file\": <file>, \"symbol\": \"hunk:<index>\"}}.\n\
         - You may call at most {max_tool_calls} read-only tools before submitting; then call \
           submit_visualization_plan exactly once with plan_version {PLAN_VERSION} and \
           \"epoch\": {epoch} copied as an integer.\n\
         - Labels use the project's real names. No prose outside the plan.",
        epoch = epoch.get(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_strips_root_prefix() {
        let root = Utf8Path::new("/Users/dev/repo");
        assert_eq!(
            redact_repo_root("edited /Users/dev/repo/src/a.go today", root),
            "edited src/a.go today"
        );
        assert_eq!(
            redact_repo_root("root is /Users/dev/repo, ok", root),
            "root is , ok"
        );
        // Trailing slash on the root behaves identically.
        assert_eq!(
            redact_repo_root("/Users/dev/repo/x", Utf8Path::new("/Users/dev/repo/")),
            "x"
        );
        // Empty root: no-op.
        assert_eq!(
            redact_repo_root("keep /a/b", Utf8Path::new("")),
            "keep /a/b"
        );
        assert_eq!(
            redact_repo_root("keep /a/b", Utf8Path::new("/")),
            "keep /a/b"
        );
    }

    #[test]
    fn system_prompt_carries_epoch_budget_and_caps() {
        let prompt = build_system_prompt(Epoch(42), 8);
        assert!(prompt.contains("\"epoch\": 42"));
        assert!(prompt.contains("at most 8 read-only tools"));
        assert!(prompt.contains("focused_diff"));
        assert!(prompt.contains("hunk:<index>"));
    }

    #[test]
    fn error_outcome_mapping() {
        assert_eq!(
            outcome_from_error(&AiError::Disabled),
            AiOutcome::Unavailable
        );
        assert_eq!(
            outcome_from_error(&AiError::CircuitOpen {
                retry_in: Duration::from_secs(1)
            }),
            AiOutcome::Unavailable
        );
        assert_eq!(
            outcome_from_error(&AiError::Throttled {
                retry_after: Duration::from_secs(1)
            }),
            AiOutcome::Unavailable
        );
        assert!(matches!(
            outcome_from_error(&AiError::NoToolCall),
            AiOutcome::Failed(_)
        ));
        assert!(matches!(
            outcome_from_error(&AiError::Timeout(Duration::from_secs(20))),
            AiOutcome::Failed(_)
        ));
    }

    #[test]
    fn disabled_config_builds_no_service() {
        let err = AiService::new(AiConfig::disabled(), "/tmp/repo").unwrap_err();
        assert!(matches!(err, AiError::Disabled));
    }

    #[test]
    fn tool_error_result_is_json() {
        let v: serde_json::Value = serde_json::from_str(&error_result("boom".into())).unwrap();
        assert_eq!(v["error"], "boom");
    }
}
