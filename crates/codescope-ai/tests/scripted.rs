//! Offline integration tests against `codescope-testutil`'s [`ScriptedProvider`]:
//! the full client/service behavior matrix (valid plan, malformed JSON, 429 + Retry-After,
//! hang→timeout, circuit breaker, tool loop + budget, stale epoch, hallucination policy,
//! local throttle) without any real network dependency.

use codescope_ai::{
    AiClient, AiClientOptions, AiConfig, AiError, AiOutcome, AiService, ChatMessage, FactView,
    Lookup, RetryPolicy, ToolExecError, ToolExecutor, PLAN_TOOL_NAME,
};
use codescope_core::{EntityRef, Epoch, FileId, LineRange, PlanEdgeKind, ValidationVerdict};
use codescope_testutil::fake_ai::{
    hallucinated_sample_plan, sample_plan, AiScriptStep, ScriptedProvider,
};
use codescope_testutil::go_fixture::{MIDDLEWARE_FILE, POSTGRES_FILE};
use futures::future::BoxFuture;
use secrecy::SecretString;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const REPO_ROOT: &str = "/abs/fixture/root";

fn config_for(provider: &ScriptedProvider, timeout: Duration) -> AiConfig {
    AiConfig {
        enabled: true,
        base_url: format!("{}/v1", provider.base_url()),
        model: "codescope-test/model".into(),
        api_key: Some(SecretString::from("sk-test".to_string())),
        timeout,
        max_tool_calls: 8,
        prime_team_id: None,
    }
}

fn fast_retry() -> RetryPolicy {
    RetryPolicy {
        min_delay: Duration::from_millis(10),
        max_times: 2,
    }
}

fn service_for(provider: &ScriptedProvider) -> AiService {
    AiService::with_options(
        config_for(provider, Duration::from_secs(5)),
        REPO_ROOT,
        AiClientOptions::default(),
        fast_retry(),
    )
    .unwrap()
}

/// FactView accepting exactly the fixture entities `sample_plan` references.
struct FixtureFacts;

impl FactView for FixtureFacts {
    fn file(&self, file: &FileId) -> Lookup<()> {
        if matches!(file.as_path().as_str(), MIDDLEWARE_FILE | POSTGRES_FILE) {
            Lookup::Present(())
        } else {
            Lookup::Absent
        }
    }
    fn symbol(&self, file: &FileId, name: &str) -> Lookup<LineRange> {
        match (file.as_path().as_str(), name) {
            (MIDDLEWARE_FILE, "LoggingMiddleware") => Lookup::Present(LineRange::new(10, 0, 30, 1)),
            (POSTGRES_FILE, "(PostgresRepo).Get") => Lookup::Present(LineRange::new(40, 0, 60, 1)),
            _ => Lookup::Absent,
        }
    }
    fn edge(&self, _from: &EntityRef, _to: &EntityRef, _kind: PlanEdgeKind) -> Lookup<()> {
        Lookup::Absent
    }
    fn hunk(&self, _file: &FileId, _index: u32) -> Lookup<()> {
        Lookup::Absent
    }
}

/// Recording executor whose results deliberately contain an absolute repo path, so tests
/// can assert outbound redaction of tool results.
#[derive(Default)]
struct RecordingExecutor {
    calls: Mutex<Vec<(String, Value)>>,
    count: AtomicU32,
}

impl ToolExecutor for RecordingExecutor {
    fn execute<'a>(
        &'a self,
        name: &'a str,
        arguments: &'a Value,
    ) -> BoxFuture<'a, Result<String, ToolExecError>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push((name.to_string(), arguments.clone()));
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(format!(
                "{{\"outline\":\"{REPO_ROOT}/{MIDDLEWARE_FILE} has symbol LoggingMiddleware\"}}"
            ))
        })
    }
}

/// A raw 200 chat completion whose message calls `calls` read-only tools.
fn tool_call_step(names: &[&str]) -> AiScriptStep {
    let tool_calls: Vec<Value> = names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            json!({
                "id": format!("call_{i}"),
                "type": "function",
                "function": {"name": name, "arguments": "{\"file\": \"internal/api/middleware.go\"}"}
            })
        })
        .collect();
    let body = json!({
        "id": "chatcmpl-x",
        "object": "chat.completion",
        "created": 1,
        "model": "codescope-test/model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": null, "tool_calls": tool_calls},
            "finish_reason": "tool_calls"
        }]
    });
    AiScriptStep::Raw {
        status: 200,
        content_type: "application/json".into(),
        body: body.to_string(),
    }
}

fn server_error_step() -> AiScriptStep {
    AiScriptStep::Raw {
        status: 500,
        content_type: "application/json".into(),
        body: r#"{"error":{"message":"scripted failure"}}"#.into(),
    }
}

#[tokio::test]
async fn valid_plan_end_to_end_with_redaction_and_wire_shape() {
    let provider = ScriptedProvider::start([AiScriptStep::valid_plan(Epoch(7)).unwrap()])
        .await
        .unwrap();
    let service = service_for(&provider);
    let digest = format!("changed file {REPO_ROOT}/{MIDDLEWARE_FILE}: added LoggingMiddleware");

    let outcome = service
        .request_plan(
            &digest,
            &RecordingExecutor::default(),
            &FixtureFacts,
            Epoch(7),
        )
        .await;

    let AiOutcome::Plan(plan, report) = outcome else {
        panic!("expected plan, got {outcome:?}");
    };
    assert_eq!(report.verdict, ValidationVerdict::Valid);
    assert_eq!(plan, sample_plan(Epoch(7)));

    // Wire assertions on what was actually sent.
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let req = &requests[0];
    assert_eq!(req.method, "POST");
    assert_eq!(req.path, "/v1/chat/completions");
    assert_eq!(req.headers.get("authorization").unwrap(), "Bearer sk-test");
    let body = req.body_json().unwrap();
    assert_eq!(body["model"], "codescope-test/model");
    assert_eq!(body["tool_choice"], "required");
    assert_eq!(body["stream"], false);
    let tool_names: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert!(tool_names.contains(&PLAN_TOOL_NAME));
    for expected in [
        "get_file_outline",
        "get_symbol",
        "get_hunk",
        "get_references",
        "get_callers",
        "get_callees",
        "get_implementations",
        "search_symbols",
        "get_diagnostics",
    ] {
        assert!(tool_names.contains(&expected), "missing tool {expected}");
    }
    // Digest redaction: repo-relative paths only.
    let user = body["messages"][1]["content"].as_str().unwrap();
    assert!(user.contains(&format!("changed file {MIDDLEWARE_FILE}")));
    assert!(!user.contains(REPO_ROOT), "absolute root leaked: {user}");
    // Epoch echo contract present in the system prompt.
    let system = body["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("\"epoch\": 7"));
}

#[tokio::test]
async fn malformed_plan_json_fails_without_retry() {
    let provider = ScriptedProvider::start([AiScriptStep::malformed_json()])
        .await
        .unwrap();
    let service = service_for(&provider);
    let outcome = service
        .request_plan(
            "digest",
            &RecordingExecutor::default(),
            &FixtureFacts,
            Epoch(1),
        )
        .await;
    let AiOutcome::Failed(reason) = outcome else {
        panic!("expected failure, got {outcome:?}");
    };
    assert!(reason.contains("plan malformed"), "{reason}");
    assert_eq!(provider.requests().len(), 1, "parse errors must not retry");
}

#[tokio::test]
async fn assistant_text_without_tool_call_fails() {
    let provider = ScriptedProvider::start([AiScriptStep::AssistantText {
        content: "I think the change is fine.".into(),
    }])
    .await
    .unwrap();
    let service = service_for(&provider);
    let outcome = service
        .request_plan(
            "digest",
            &RecordingExecutor::default(),
            &FixtureFacts,
            Epoch(1),
        )
        .await;
    let AiOutcome::Failed(reason) = outcome else {
        panic!("expected failure, got {outcome:?}");
    };
    assert!(reason.contains("no tool call"), "{reason}");
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn rate_limited_then_success_honors_retry_after() {
    let provider = ScriptedProvider::start([
        AiScriptStep::RateLimited {
            retry_after_secs: 1,
        },
        AiScriptStep::valid_plan(Epoch(3)).unwrap(),
    ])
    .await
    .unwrap();
    // Tiny exponential min_delay: if Retry-After were ignored the retry would land in
    // ~10 ms; honoring it must take ≥ 1 s.
    let service = service_for(&provider);
    let started = Instant::now();
    let outcome = service
        .request_plan(
            "digest",
            &RecordingExecutor::default(),
            &FixtureFacts,
            Epoch(3),
        )
        .await;
    let elapsed = started.elapsed();
    assert!(matches!(outcome, AiOutcome::Plan(..)), "got {outcome:?}");
    assert_eq!(provider.requests().len(), 2);
    assert!(
        elapsed >= Duration::from_millis(950),
        "Retry-After not honored: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "took too long: {elapsed:?}"
    );
}

#[tokio::test]
async fn hang_times_out() {
    let provider = ScriptedProvider::start([AiScriptStep::Hang]).await.unwrap();
    let service = AiService::with_options(
        config_for(&provider, Duration::from_millis(300)),
        REPO_ROOT,
        AiClientOptions::default(),
        RetryPolicy {
            min_delay: Duration::from_millis(1),
            max_times: 0, // isolate the timeout path from retries
        },
    )
    .unwrap();
    let started = Instant::now();
    let outcome = service
        .request_plan(
            "digest",
            &RecordingExecutor::default(),
            &FixtureFacts,
            Epoch(1),
        )
        .await;
    let elapsed = started.elapsed();
    let AiOutcome::Failed(reason) = outcome else {
        panic!("expected failure, got {outcome:?}");
    };
    assert!(reason.contains("timed out"), "{reason}");
    assert!(elapsed >= Duration::from_millis(280), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    provider.abort();
}

#[tokio::test]
async fn circuit_breaker_opens_after_three_failures() {
    let provider = ScriptedProvider::start([
        server_error_step(),
        server_error_step(),
        server_error_step(),
    ])
    .await
    .unwrap();
    let client = AiClient::new(&config_for(&provider, Duration::from_secs(2))).unwrap();
    let messages = [ChatMessage::user("hi")];

    for attempt in 0..3 {
        let err = client.chat_with_plan(&messages, &[]).await.unwrap_err();
        assert!(
            matches!(err, AiError::Http { status: 500, .. }),
            "attempt {attempt}: {err}"
        );
    }
    assert!(client.is_circuit_open());
    // Fourth call: rejected locally, no request reaches the provider.
    let err = client.chat_with_plan(&messages, &[]).await.unwrap_err();
    assert!(matches!(err, AiError::CircuitOpen { .. }), "{err}");
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test]
async fn circuit_probe_allowed_after_cooldown() {
    let provider = ScriptedProvider::start([
        server_error_step(),
        server_error_step(),
        server_error_step(),
        AiScriptStep::valid_plan(Epoch(1)).unwrap(),
    ])
    .await
    .unwrap();
    let options = AiClientOptions {
        cooldown: Duration::from_millis(100),
        ..AiClientOptions::default()
    };
    let client =
        AiClient::with_options(&config_for(&provider, Duration::from_secs(2)), options).unwrap();
    let messages = [ChatMessage::user("hi")];
    for _ in 0..3 {
        let _ = client.chat_with_plan(&messages, &[]).await.unwrap_err();
    }
    assert!(client.is_circuit_open());
    tokio::time::sleep(Duration::from_millis(150)).await;
    // Half-open probe goes through and the success closes the breaker.
    let response = client.chat_with_plan(&messages, &[]).await.unwrap();
    assert!(response.plan_arguments().is_some());
    assert!(!client.is_circuit_open());
    assert_eq!(provider.requests().len(), 4);
}

#[tokio::test]
async fn tool_loop_executes_reads_and_submits_plan() {
    let provider = ScriptedProvider::start([
        tool_call_step(&["get_file_outline", "get_symbol"]),
        AiScriptStep::valid_plan(Epoch(5)).unwrap(),
    ])
    .await
    .unwrap();
    let service = service_for(&provider);
    let executor = RecordingExecutor::default();
    let outcome = service
        .request_plan("digest", &executor, &FixtureFacts, Epoch(5))
        .await;
    assert!(matches!(outcome, AiOutcome::Plan(..)), "got {outcome:?}");
    assert_eq!(executor.count.load(Ordering::SeqCst), 2);
    {
        let calls = executor.calls.lock().unwrap();
        assert_eq!(calls[0].0, "get_file_outline");
        assert_eq!(calls[1].0, "get_symbol");
        assert_eq!(calls[0].1["file"], "internal/api/middleware.go");
    }

    // Second request must carry the assistant echo and the redacted tool results.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let body = requests[1].body_json().unwrap();
    let messages = body["messages"].as_array().unwrap();
    let roles: Vec<&str> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "tool", "tool"]);
    assert_eq!(messages[3]["tool_call_id"], "call_0");
    let tool_content = messages[3]["content"].as_str().unwrap();
    assert!(
        !tool_content.contains(REPO_ROOT),
        "tool result leaked absolute path: {tool_content}"
    );
    assert!(tool_content.contains(MIDDLEWARE_FILE));
}

#[tokio::test]
async fn tool_call_budget_enforced() {
    // One message requesting 9 tool calls: the 9th exceeds the budget of 8.
    let names: Vec<&str> = std::iter::repeat_n("get_file_outline", 9).collect();
    let provider = ScriptedProvider::start([tool_call_step(&names)])
        .await
        .unwrap();
    let service = service_for(&provider);
    let executor = RecordingExecutor::default();
    let outcome = service
        .request_plan("digest", &executor, &FixtureFacts, Epoch(1))
        .await;
    let AiOutcome::Failed(reason) = outcome else {
        panic!("expected failure, got {outcome:?}");
    };
    assert!(reason.contains("budget exceeded"), "{reason}");
    assert_eq!(
        executor.count.load(Ordering::SeqCst),
        8,
        "exactly the budget may execute"
    );
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn budget_spans_multiple_turns() {
    // 5 calls, then 4 more: the 9th call (4th of turn two) must trip the budget.
    let provider = ScriptedProvider::start([
        tool_call_step(&["get_file_outline"; 5]),
        tool_call_step(&["get_symbol"; 4]),
    ])
    .await
    .unwrap();
    let service = service_for(&provider);
    let executor = RecordingExecutor::default();
    let outcome = service
        .request_plan("digest", &executor, &FixtureFacts, Epoch(1))
        .await;
    assert!(matches!(outcome, AiOutcome::Failed(_)), "got {outcome:?}");
    assert_eq!(executor.count.load(Ordering::SeqCst), 8);
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn stale_epoch_yields_stale_outcome() {
    let provider = ScriptedProvider::start([AiScriptStep::valid_plan(Epoch(1)).unwrap()])
        .await
        .unwrap();
    let service = service_for(&provider);
    let outcome = service
        .request_plan(
            "digest",
            &RecordingExecutor::default(),
            &FixtureFacts,
            Epoch(2),
        )
        .await;
    assert_eq!(outcome, AiOutcome::Stale);
}

#[tokio::test]
async fn hallucinated_plan_is_rejected_by_validation() {
    let provider = ScriptedProvider::start([AiScriptStep::hallucinated_plan(Epoch(1)).unwrap()])
        .await
        .unwrap();
    let service = service_for(&provider);
    let outcome = service
        .request_plan(
            "digest",
            &RecordingExecutor::default(),
            &FixtureFacts,
            Epoch(1),
        )
        .await;
    let AiOutcome::Failed(reason) = outcome else {
        panic!("expected failure, got {outcome:?}");
    };
    assert!(reason.contains("plan rejected"), "{reason}");
    // Sanity: the scripted plan really was hallucinated.
    assert!(hallucinated_sample_plan(Epoch(1)).forms[0].nodes[0]
        .entity
        .as_ref()
        .is_some_and(|e| e.file.as_path().as_str().contains("quantum")));
}

#[tokio::test]
async fn local_throttle_maps_to_unavailable() {
    let provider = ScriptedProvider::start([
        AiScriptStep::valid_plan(Epoch(1)).unwrap(),
        AiScriptStep::valid_plan(Epoch(1)).unwrap(),
    ])
    .await
    .unwrap();
    let options = AiClientOptions {
        requests_per_minute: 1,
        burst: 1,
        ..AiClientOptions::default()
    };
    let service = AiService::with_options(
        config_for(&provider, Duration::from_secs(5)),
        REPO_ROOT,
        options,
        fast_retry(),
    )
    .unwrap();
    let first = service
        .request_plan(
            "digest",
            &RecordingExecutor::default(),
            &FixtureFacts,
            Epoch(1),
        )
        .await;
    assert!(matches!(first, AiOutcome::Plan(..)), "got {first:?}");
    let second = service
        .request_plan(
            "digest",
            &RecordingExecutor::default(),
            &FixtureFacts,
            Epoch(1),
        )
        .await;
    assert_eq!(second, AiOutcome::Unavailable);
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn keyless_local_provider_sends_no_authorization() {
    let provider = ScriptedProvider::start([AiScriptStep::valid_plan(Epoch(1)).unwrap()])
        .await
        .unwrap();
    let mut config = config_for(&provider, Duration::from_secs(5));
    config.api_key = None;
    let service =
        AiService::with_options(config, REPO_ROOT, AiClientOptions::default(), fast_retry())
            .unwrap();
    let outcome = service
        .request_plan(
            "digest",
            &RecordingExecutor::default(),
            &FixtureFacts,
            Epoch(1),
        )
        .await;
    assert!(matches!(outcome, AiOutcome::Plan(..)));
    assert!(!provider.requests()[0].headers.contains_key("authorization"));
}
