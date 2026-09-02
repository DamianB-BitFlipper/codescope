//! Offline integration tests against `codescope-testutil`'s [`ScriptedProvider`]:
//! the full client/service behavior matrix (valid plan, malformed JSON, 429 + Retry-After,
//! hang→timeout, circuit breaker, tool loop + budget, stale epoch, hallucination policy,
//! local throttle) without any real network dependency.

use codescope_ai::{
    diagram_tools, research_tools, AiClient, AiClientOptions, AiConfig, AiError, AiOutcome,
    AiService, ChatMessage, DiagramObserver, FactView, Lookup, NoToolExecutor, ReasoningEffort,
    RetryPolicy, ToolChoice, ToolDef, ToolExecError, ToolExecutor, DIAGRAM_EDIT_TOOL_NAME,
    DIAGRAM_FINISH_TOOL_NAME, PLAN_TOOL_NAME,
};
use codescope_core::{
    DiagramCommand, DiagramDraft, DiffSide, EntityRef, Epoch, FileId, FormKind, LineRange,
    PlanCodeRef, PlanEdge, PlanEdgeKind, PlanEvidence, PlanNode, PlanNodeChange, ValidationVerdict,
    VisualizationPlan, VizForm, MAX_NODE_CODE_REFS,
};
use codescope_testutil::fake_ai::{
    hallucinated_sample_plan, sample_plan, AiScriptStep, ScriptedProvider,
};
use codescope_testutil::go_fixture::{MIDDLEWARE_FILE, POSTGRES_FILE};
use futures::future::BoxFuture;
use secrecy::SecretString;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REPO_ROOT: &str = "/abs/fixture/root";

fn config_for(provider: &ScriptedProvider, timeout: Duration) -> AiConfig {
    AiConfig {
        enabled: true,
        base_url: format!("{}/v1", provider.base_url()),
        model: "codescope-test/model".into(),
        reasoning_effort: ReasoningEffort::Default,
        api_key: Some(SecretString::from("sk-test".to_string())),
        timeout,
        tool_choice: ToolChoice::Required,
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

/// FactView accepting exactly the fixture entities `sample_plan` references, with
/// complete knowledge: every miss is a proven absence. Hunks and diff lines mirror the
/// fixture's real branch-vs-`main` diff:
///
/// - `middleware.go` (new file): hunk 0 is `@@ -0,0 +1,15 @@` — new-side lines 1..=15.
/// - `postgres.go` (modified `Get`): hunk 0 is `@@ -15,6 +15,9 @@` — old-side 15..=20,
///   new-side 15..=23.
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
    fn hunk(&self, file: &FileId, index: u32) -> Lookup<()> {
        match (file.as_path().as_str(), index) {
            (MIDDLEWARE_FILE, 0) | (POSTGRES_FILE, 0) => Lookup::Present(()),
            _ => Lookup::Absent,
        }
    }
    fn diff_line(&self, file: &FileId, index: u32, side: DiffSide, line: u32) -> Lookup<()> {
        let present = match (file.as_path().as_str(), index, side) {
            // middleware.go: new file, hunk 0 carries only new-side lines 1..=15.
            (MIDDLEWARE_FILE, 0, DiffSide::New) => (1..=15).contains(&line),
            // postgres.go: @@ -15,6 +15,9 @@ — old 15..=20, new 15..=23.
            (POSTGRES_FILE, 0, DiffSide::Old) => (15..=20).contains(&line),
            (POSTGRES_FILE, 0, DiffSide::New) => (15..=23).contains(&line),
            _ => false,
        };
        if present {
            Lookup::Present(())
        } else {
            Lookup::Absent
        }
    }
}

/// The synthetic drain hunk LazyFacts enumerates for MIDDLEWARE_FILE hunk 0 (new side
/// only, 24 lines): the readiness handler lands at [`DRAIN_READY_LINES`], the drain
/// delay at [`DRAIN_DELAY_LINES`], and the shutdown drain at [`DRAIN_SHUTDOWN_LINES`].
/// `drain_plan` code refs copy these lines exactly as a model would copy them from the
/// annotated focused source packet.
const DRAIN_HUNK_NEW_SPAN: u32 = 24;
/// New-side lines of the readiness-handler step (`readinessHandler → 503`).
const DRAIN_READY_LINES: (u32, u32) = (3, 7);
/// New-side lines of the drain-delay step (`wait shutdownDrainDelay`).
const DRAIN_DELAY_LINES: (u32, u32) = (12, 15);
/// New-side lines of the shutdown-drain step (`server.Shutdown drains`).
const DRAIN_SHUTDOWN_LINES: (u32, u32) = (19, 22);

/// Lazy-store facts mirroring the dispatcher's `SnapshotFacts`: files and hunks (with
/// their exact diff lines) are enumerated from the changeset, but symbols and edges were
/// never queried, so those misses are `Unknown` ("not queried"), never `Absent` — exactly
/// the world the live GLM failure hit.
struct LazyFacts;

impl FactView for LazyFacts {
    fn file(&self, file: &FileId) -> Lookup<()> {
        if file.as_path().as_str() == MIDDLEWARE_FILE {
            Lookup::Present(())
        } else {
            Lookup::Unknown
        }
    }
    fn symbol(&self, _file: &FileId, _name: &str) -> Lookup<LineRange> {
        // No changed-symbol catalog entry exists for this file: nothing was queried.
        Lookup::Unknown
    }
    fn edge(&self, _from: &EntityRef, _to: &EntityRef, _kind: PlanEdgeKind) -> Lookup<()> {
        Lookup::Unknown
    }
    fn hunk(&self, file: &FileId, index: u32) -> Lookup<()> {
        if file.as_path().as_str() == MIDDLEWARE_FILE && index == 0 {
            Lookup::Present(())
        } else {
            Lookup::Unknown
        }
    }
    fn diff_line(&self, file: &FileId, index: u32, side: DiffSide, line: u32) -> Lookup<()> {
        if file.as_path().as_str() == MIDDLEWARE_FILE
            && index == 0
            && side == DiffSide::New
            && (1..=DRAIN_HUNK_NEW_SPAN).contains(&line)
        {
            Lookup::Present(())
        } else if file.as_path().as_str() == MIDDLEWARE_FILE {
            // The file's hunks are enumerated from the changeset: a missing line is a
            // proven absence, mirroring SnapshotFacts' semantics.
            Lookup::Absent
        } else {
            Lookup::Unknown
        }
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

#[derive(Default)]
struct RequiredResearchExecutor(RecordingExecutor);

impl ToolExecutor for RequiredResearchExecutor {
    fn requires_research(&self) -> bool {
        true
    }

    fn execute<'a>(
        &'a self,
        name: &'a str,
        arguments: &'a Value,
    ) -> BoxFuture<'a, Result<String, ToolExecError>> {
        self.0.execute(name, arguments)
    }
}

#[derive(Default)]
struct IncrementalExecutor(RecordingExecutor);

impl ToolExecutor for IncrementalExecutor {
    fn available_tools(&self) -> Vec<ToolDef> {
        research_tools()
            .into_iter()
            .chain(diagram_tools())
            .collect()
    }

    fn requires_research(&self) -> bool {
        true
    }

    fn execute<'a>(
        &'a self,
        name: &'a str,
        arguments: &'a Value,
    ) -> BoxFuture<'a, Result<String, ToolExecError>> {
        self.0.execute(name, arguments)
    }
}

fn diagram_step(command: &DiagramCommand) -> AiScriptStep {
    AiScriptStep::tool_call(
        DIAGRAM_EDIT_TOOL_NAME,
        serde_json::to_value(command).unwrap(),
    )
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

    // v4: every validated node kept its exact code_refs and expanded_detail through the
    // request/parse/validate loop (serialization → tool call → parse → sanitize).
    for form in &plan.forms {
        for node in &form.nodes {
            assert!(
                (1..=MAX_NODE_CODE_REFS).contains(&node.code_refs.len()),
                "node {} kept 1..={MAX_NODE_CODE_REFS} code_refs",
                node.id
            );
        }
    }
    assert_eq!(
        plan.forms[0].nodes[0].code_refs,
        vec![PlanCodeRef::new(
            FileId::new_unchecked(MIDDLEWARE_FILE),
            0,
            DiffSide::New,
            5,
            8,
        )]
    );
    assert_eq!(
        plan.forms[0].nodes[1].code_refs,
        vec![
            PlanCodeRef::new(
                FileId::new_unchecked(POSTGRES_FILE),
                0,
                DiffSide::New,
                17,
                21,
            ),
            PlanCodeRef::new(
                FileId::new_unchecked(POSTGRES_FILE),
                0,
                DiffSide::Old,
                17,
                18
            ),
        ]
    );
    assert!(plan.forms[0].nodes[0].expanded_detail.is_some());

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
async fn previous_validated_design_is_sent_as_a_non_evidentiary_revision_seed() {
    let provider = ScriptedProvider::start([AiScriptStep::valid_plan(Epoch(7)).unwrap()])
        .await
        .unwrap();
    let service = service_for(&provider);
    let mut previous = sample_plan(Epoch(6));
    previous.intent = format!("Earlier cached design for {REPO_ROOT}/{MIDDLEWARE_FILE}.");

    let outcome = service
        .request_plan_with_previous(
            "current revision changes LoggingMiddleware",
            Some(&previous),
            &NoToolExecutor,
            &FixtureFacts,
            Epoch(7),
        )
        .await;
    assert!(matches!(outcome, AiOutcome::Plan(_, _)));

    let body = provider.requests()[0].body_json().unwrap();
    let user = body["messages"][1]["content"].as_str().unwrap();
    assert!(user.contains("current epoch: 7"));
    assert!(user.contains("## current research brief"));
    assert!(user.contains("previous validated design"));
    assert!(user.contains("Earlier cached design"));
    assert!(user.contains("\"epoch\": 6"));
    assert!(user.contains(MIDDLEWARE_FILE));
    // v4 refs serialize into the seed (they flow out on the wire, unredacted repo-relative).
    assert!(user.contains("\"code_refs\""));
    assert!(user.contains("\"expanded_detail\""));
    assert!(
        !user.contains(REPO_ROOT),
        "cached plan leaked repo root: {user}"
    );

    let system = body["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("previous validated design"));
    assert!(system.contains("current research always wins"));
    assert!(system.contains("Preserve useful structure for an incremental revision"));
    assert!(system.contains("never copy its old epoch"));
}

#[tokio::test]
async fn auto_tool_choice_is_sent_to_openai_compatible_providers() {
    let provider = ScriptedProvider::start([AiScriptStep::valid_plan(Epoch(7)).unwrap()])
        .await
        .unwrap();
    let mut config = config_for(&provider, Duration::from_secs(5));
    config.tool_choice = ToolChoice::Auto;
    let service =
        AiService::with_options(config, REPO_ROOT, AiClientOptions::default(), fast_retry())
            .unwrap();

    let outcome = service
        .request_plan(
            "digest",
            &RecordingExecutor::default(),
            &FixtureFacts,
            Epoch(7),
        )
        .await;
    assert!(matches!(outcome, AiOutcome::Plan(_, _)), "{outcome:?}");

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body_json().unwrap()["tool_choice"], "auto");
}

#[tokio::test]
async fn no_tool_executor_advertises_only_plan_submission() {
    let provider = ScriptedProvider::start([AiScriptStep::valid_plan(Epoch(7)).unwrap()])
        .await
        .unwrap();
    let mut config = config_for(&provider, Duration::from_secs(5));
    config.tool_choice = ToolChoice::Auto;
    let service =
        AiService::with_options(config, REPO_ROOT, AiClientOptions::default(), fast_retry())
            .unwrap();

    let outcome = service
        .request_plan("digest", &NoToolExecutor, &FixtureFacts, Epoch(7))
        .await;
    assert!(matches!(outcome, AiOutcome::Plan(_, _)), "{outcome:?}");

    let body = provider.requests()[0].body_json().unwrap();
    let tool_names: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(tool_names, [PLAN_TOOL_NAME]);
    let system = body["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("No read-only tools are available"));
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
async fn syntactically_valid_incomplete_plan_gets_one_schema_repair() {
    let provider = ScriptedProvider::start([
        AiScriptStep::ToolCallRaw {
            arguments: r#"{"epoch":5}"#.into(),
        },
        AiScriptStep::valid_plan(Epoch(5)).unwrap(),
    ])
    .await
    .unwrap();
    let service = service_for(&provider);
    let outcome = service
        .request_plan(
            "digest",
            &RecordingExecutor::default(),
            &FixtureFacts,
            Epoch(5),
        )
        .await;
    assert!(matches!(outcome, AiOutcome::Plan(..)), "got {outcome:?}");

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let messages = requests[1].body_json().unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone();
    let feedback = messages.last().unwrap()["content"].as_str().unwrap();
    assert!(feedback.contains("missing field `plan_version`"));
    assert!(feedback.contains(&format!("plan_version: {}", codescope_core::PLAN_VERSION)));
    assert!(feedback.contains("epoch: 5"));
    // Type errors get explicit element-shape guidance (a live GLM run submitted a bare
    // string "detail" where a node object was expected).
    assert!(
        feedback.contains("Every array element must be an object"),
        "element-shape clause: {feedback}"
    );
    assert!(
        feedback.contains("never a bare string or field name"),
        "bare-string prohibition: {feedback}"
    );
}

/// Under automatic tool choice a provider may answer in plain text instead of calling the
/// plan tool. That turn now costs one bounded repair — the assistant text is preserved and
/// a user turn asks for the required `submit_visualization_plan` call — and the corrected
/// structured submission validates. Repeated plain-text answers exhaust the repair
/// allowance and terminate with the bounded no-tool-call failure instead of looping.
#[tokio::test]
async fn plain_text_response_gets_structured_repair_then_validates() {
    let provider = ScriptedProvider::start([
        AiScriptStep::AssistantText {
            content: "I think the change is fine.".into(),
        },
        AiScriptStep::valid_plan(Epoch(1)).unwrap(),
    ])
    .await
    .unwrap();
    let mut config = config_for(&provider, Duration::from_secs(5));
    config.tool_choice = ToolChoice::Auto;
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
    let AiOutcome::Plan(plan, report) = outcome else {
        panic!("expected corrected plan, got {outcome:?}");
    };
    assert_eq!(report.verdict, ValidationVerdict::Valid);
    assert_eq!(plan, sample_plan(Epoch(1)));

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "plain-text turn plus one structured repair"
    );
    let messages = requests[1].body_json().unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone();
    let roles: Vec<&str> = messages
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect();
    // The plain-text assistant turn is preserved, then a user turn demands the tool call.
    assert_eq!(roles, ["system", "user", "assistant", "user"]);
    assert_eq!(
        messages[2]["content"], "I think the change is fine.",
        "assistant text echoed back to the provider"
    );
    let repair = messages[3]["content"].as_str().unwrap();
    assert!(
        repair.contains("did not call the required tool"),
        "named failure: {repair}"
    );
    assert!(
        repair.contains("submit_visualization_plan"),
        "the required tool is named: {repair}"
    );
    assert!(
        repair.contains("Return no plain text"),
        "plain text prohibited: {repair}"
    );
    assert!(repair.contains("for epoch 1"), "epoch echoed: {repair}");
    assert!(repair.contains("code_refs"), "v4 refs contract: {repair}");

    // Boundedness: a provider that keeps answering in plain text exhausts the three
    // repairs and terminates with the no-tool-call failure (4 requests, never a loop).
    let text_step = AiScriptStep::AssistantText {
        content: "still just prose".into(),
    };
    let provider = ScriptedProvider::start([
        text_step.clone(),
        text_step.clone(),
        text_step.clone(),
        text_step.clone(),
    ])
    .await
    .unwrap();
    let mut config = config_for(&provider, Duration::from_secs(5));
    config.tool_choice = ToolChoice::Auto;
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
    let AiOutcome::Failed(reason) = outcome else {
        panic!("expected bounded failure, got {outcome:?}");
    };
    assert!(reason.contains("no tool call"), "{reason}");
    assert_eq!(
        provider.requests().len(),
        4,
        "initial turn plus exactly three bounded repairs"
    );
}

/// Reasoning providers may return `content: null` plus output-only reasoning metadata when
/// automatic tool choice skips the required call. The repair transcript must not echo that
/// invalid assistant message: Chat Completions rejects null-content assistant turns unless
/// they contain `tool_calls`.
#[tokio::test]
async fn null_content_reasoning_response_repairs_without_invalid_assistant_replay() {
    let null_completion = json!({
        "id": "chatcmpl-null",
        "object": "chat.completion",
        "created": 1,
        "model": "codescope-fake",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "reasoning": "I should have called the plan tool"
            },
            "finish_reason": "stop"
        }]
    });
    let provider = ScriptedProvider::start([
        AiScriptStep::Raw {
            status: 200,
            content_type: "application/json".into(),
            body: null_completion.to_string(),
        },
        AiScriptStep::valid_plan(Epoch(1)).unwrap(),
    ])
    .await
    .unwrap();
    let mut config = config_for(&provider, Duration::from_secs(5));
    config.tool_choice = ToolChoice::Auto;
    config.reasoning_effort = ReasoningEffort::High;
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
    assert!(matches!(outcome, AiOutcome::Plan(_, _)), "{outcome:?}");

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let repair_body = requests[1].body_json().unwrap();
    assert_eq!(repair_body["reasoning_effort"], "high");
    let messages = repair_body["messages"].as_array().unwrap();
    let roles: Vec<&str> = messages
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "user"]);
    assert!(messages.iter().all(|message| {
        message["role"] != "assistant"
            || !message
                .get("content")
                .is_none_or(serde_json::Value::is_null)
            || message.get("tool_calls").is_some()
    }));
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
async fn incremental_tools_build_and_publish_the_observed_live_draft() {
    let expected = sample_plan(Epoch(5));
    let commands = [
        DiagramCommand::SetIntent {
            intent: expected.intent.clone(),
        },
        DiagramCommand::CreateForm {
            form_id: "main".to_string(),
            kind: expected.forms[0].kind,
        },
        DiagramCommand::CreateNode {
            form_id: "main".to_string(),
            node: expected.forms[0].nodes[0].clone(),
        },
        DiagramCommand::CreateNode {
            form_id: "main".to_string(),
            node: expected.forms[0].nodes[1].clone(),
        },
        DiagramCommand::AddEvidence {
            evidence: expected.evidence[0].clone(),
        },
    ];
    let mut script = vec![AiScriptStep::tool_call(
        "git_diff_file",
        json!({"path": MIDDLEWARE_FILE, "hunk_index": 0}),
    )];
    script.extend(commands.iter().map(diagram_step));
    script.push(AiScriptStep::tool_call(DIAGRAM_FINISH_TOOL_NAME, json!({})));
    let provider = ScriptedProvider::start(script).await.unwrap();
    let service = service_for(&provider);
    let observed = Arc::new(Mutex::new(Vec::<DiagramDraft>::new()));
    let observed_for_callback = observed.clone();
    let observer: DiagramObserver = Arc::new(move |draft| {
        observed_for_callback.lock().unwrap().push(draft);
    });

    let outcome = service
        .request_plan_with_previous_observer(
            "small research brief",
            None,
            &IncrementalExecutor::default(),
            &FixtureFacts,
            Epoch(5),
            Some(observer),
        )
        .await;
    let AiOutcome::Plan(plan, report) = outcome else {
        panic!("expected incrementally built plan, got {outcome:?}");
    };
    assert_eq!(plan, expected);
    assert_eq!(report.verdict, ValidationVerdict::Valid);

    let drafts = observed.lock().unwrap();
    assert!(drafts.len() >= commands.len() + 2);
    assert!(drafts.iter().any(|draft| draft.forms.len() == 1));
    assert!(drafts.iter().any(|draft| {
        draft
            .forms
            .first()
            .is_some_and(|form| form.nodes.len() == 2)
    }));

    let first_request = provider.requests()[0].body_json().unwrap();
    let tool_names = first_request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect::<Vec<_>>();
    assert!(tool_names.contains(&DIAGRAM_EDIT_TOOL_NAME));
    assert!(tool_names.contains(&DIAGRAM_FINISH_TOOL_NAME));
    assert!(!tool_names.contains(&PLAN_TOOL_NAME));
}

#[tokio::test]
async fn research_executor_rejects_a_plan_until_one_tool_succeeds() {
    let provider = ScriptedProvider::start([
        AiScriptStep::valid_plan(Epoch(5)).unwrap(),
        tool_call_step(&["get_file_outline"]),
        AiScriptStep::valid_plan(Epoch(5)).unwrap(),
    ])
    .await
    .unwrap();
    let service = service_for(&provider);
    let executor = RequiredResearchExecutor::default();

    let outcome = service
        .request_plan("small research brief", &executor, &FixtureFacts, Epoch(5))
        .await;
    assert!(matches!(outcome, AiOutcome::Plan(..)), "got {outcome:?}");
    assert_eq!(executor.0.count.load(Ordering::SeqCst), 1);

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let second = requests[1].body_json().unwrap();
    let feedback = second["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(feedback["role"], "tool");
    assert!(feedback["content"]
        .as_str()
        .unwrap()
        .contains("submitted before inspecting"));
}

#[tokio::test]
async fn two_validation_repair_turns_can_cross_schema_and_fact_boundaries() {
    let mut incomplete = sample_plan(Epoch(5));
    incomplete.forms[0].nodes[0].detail = None;
    let mut invalid_edge = sample_plan(Epoch(5));
    invalid_edge.forms[0].kind = FormKind::RelationshipFlow;
    for node in &mut invalid_edge.forms[0].nodes {
        node.children.clear();
    }
    invalid_edge.forms[0].edges.push(PlanEdge {
        from: "n1".into(),
        to: "n2".into(),
        kind: PlanEdgeKind::Calls,
        label: Some("continues request handling".into()),
    });
    let provider = ScriptedProvider::start([
        AiScriptStep::from_plan(&incomplete).unwrap(),
        AiScriptStep::from_plan(&invalid_edge).unwrap(),
        AiScriptStep::valid_plan(Epoch(5)).unwrap(),
    ])
    .await
    .unwrap();
    let service = service_for(&provider);

    let outcome = service
        .request_plan(
            "digest",
            &RecordingExecutor::default(),
            &FixtureFacts,
            Epoch(5),
        )
        .await;
    assert!(matches!(outcome, AiOutcome::Plan(..)), "got {outcome:?}");

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let messages = requests[1].body_json().unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone();
    let roles: Vec<&str> = messages
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "tool"]);
    let feedback = messages[3]["content"].as_str().unwrap();
    assert!(feedback.contains("node has no reviewer-facing detail"));
    assert!(feedback.contains("corrected complete plan"));

    let third_messages = requests[2].body_json().unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone();
    let feedback = third_messages.last().unwrap()["content"].as_str().unwrap();
    assert!(feedback.contains("not in the impact graph"));
    assert!(feedback.contains("relationship graph is unavailable"));
    assert!(feedback.contains("changed_symbol_tree"));
}

/// A shutdown-drain sequence plan of the kind GLM produced against the focused hunks.
/// `attach_entity` controls the exact live failure: the first submission attached a
/// `readinessHandler` symbol entity whose file was never analyzed, so the lazy fact store
/// reported it unqueried and validation rejected the whole sequence form.
fn drain_plan(epoch: Epoch, attach_entity: bool) -> VisualizationPlan {
    let mut plan = VisualizationPlan::new(epoch);
    plan.intent = "Shutdown marks readiness unhealthy and waits before draining requests.".into();
    let mut n1 = PlanNode::new("n1", "readinessHandler → 503", PlanNodeChange::Unchanged)
        .with_detail("readiness endpoint answers 503 draining once shutdown begins")
        .with_code_ref(PlanCodeRef::new(
            FileId::new_unchecked(MIDDLEWARE_FILE),
            0,
            DiffSide::New,
            DRAIN_READY_LINES.0,
            DRAIN_READY_LINES.1,
        ));
    if attach_entity {
        n1.entity = Some(EntityRef::for_symbol(
            FileId::new_unchecked(MIDDLEWARE_FILE),
            "readinessHandler",
            None,
        ));
    }
    plan.forms.push(VizForm {
        kind: FormKind::Sequence,
        nodes: vec![
            n1,
            PlanNode::new("n2", "wait shutdownDrainDelay", PlanNodeChange::Unchanged)
                .with_detail("10s grace window expected to cover probe propagation")
                .with_code_ref(PlanCodeRef::new(
                    FileId::new_unchecked(MIDDLEWARE_FILE),
                    0,
                    DiffSide::New,
                    DRAIN_DELAY_LINES.0,
                    DRAIN_DELAY_LINES.1,
                )),
            PlanNode::new("n3", "server.Shutdown drains", PlanNodeChange::Unchanged)
                .with_detail("in-flight requests finish before the listener closes")
                .with_code_ref(PlanCodeRef::new(
                    FileId::new_unchecked(MIDDLEWARE_FILE),
                    0,
                    DiffSide::New,
                    DRAIN_SHUTDOWN_LINES.0,
                    DRAIN_SHUTDOWN_LINES.1,
                )),
        ],
        edges: vec![
            PlanEdge {
                from: "n1".into(),
                to: "n2".into(),
                kind: PlanEdgeKind::Writes,
                label: Some("probe expected to see unhealthy".into()),
            },
            PlanEdge {
                from: "n2".into(),
                to: "n3".into(),
                kind: PlanEdgeKind::Writes,
                label: Some("grace window elapses".into()),
            },
        ],
    });
    plan.evidence.push(PlanEvidence {
        file: FileId::new_unchecked(MIDDLEWARE_FILE),
        hunk: Some(0),
        symbol: None,
        range: None,
        reason: "readiness handler and drain delay are defined in this hunk".into(),
    });
    plan
}

/// Regression for the live GLM failure: the first response attaches a symbol entity the
/// lazy fact store never queried ("readinessHandler not queried"), validation rejects the
/// sequence form, and the repair feedback must teach entity omission (not the generic
/// detail advice). The corrected plan — same sequence, entityless conceptual nodes
/// grounded by hunk evidence — validates on the second turn.
#[tokio::test]
async fn unqueried_symbol_entity_gets_entity_repair_then_validates() {
    let provider = ScriptedProvider::start([
        AiScriptStep::from_plan(&drain_plan(Epoch(4), true)).unwrap(),
        AiScriptStep::from_plan(&drain_plan(Epoch(4), false)).unwrap(),
    ])
    .await
    .unwrap();
    let service = service_for(&provider);
    // The digest names the symbol in raw diff text but has no changed-symbol catalog for
    // the file — precisely the spelling-in-diff trap the first plan fell into.
    let digest = format!(
        "changed file {MIDDLEWARE_FILE}\n\
         ## focused source evidence (hunk ids are zero-based)\n\
         hunk_id: 0  file: {MIDDLEWARE_FILE}\n\
         +func readinessHandler(w http.ResponseWriter, r *http.Request) -- 503 when unhealthy\n\
         note: symbol entities for this file were not queried"
    );

    let outcome = service
        .request_plan(&digest, &NoToolExecutor, &LazyFacts, Epoch(4))
        .await;
    let AiOutcome::Plan(plan, report) = outcome else {
        panic!("expected corrected plan, got {outcome:?}");
    };
    assert_eq!(report.verdict, ValidationVerdict::Valid);
    assert!(
        plan.forms[0].nodes.iter().all(|node| node.entity.is_none()),
        "corrected plan keeps every node entityless: {:?}",
        plan.forms[0].nodes
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "initial submission plus one repair");
    // The no-tool session contract ships in the system prompt.
    let first_body = requests[0].body_json().unwrap();
    let system = first_body["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("No read-only tools are available"));
    assert!(system.contains("hunk-derived"));
    assert!(system.contains("current symbol catalog"));
    // The repair feedback is entity-specific.
    let messages = requests[1].body_json().unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone();
    let roles: Vec<&str> = messages
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "tool"]);
    let feedback = messages.last().unwrap()["content"].as_str().unwrap();
    assert!(
        feedback.contains("not queried"),
        "reason echoes the unqueried symbol: {feedback}"
    );
    assert!(
        feedback.contains("exact current fact or tool result"),
        "catalog rule in guidance: {feedback}"
    );
    assert!(
        feedback.contains("omit entity entirely"),
        "entity omission taught: {feedback}"
    );
    assert!(
        feedback
            .to_ascii_lowercase()
            .contains("never attach a symbol or range merely because"),
        "spelling-in-diff trap named: {feedback}"
    );
}

/// The live run-2 failure: GLM submitted a Sequence whose consecutive nodes n2 -> n3 had
/// no ordered edge. The word "edge" in that reason must NOT route to the conservative
/// changed_symbol_tree instruction (which destroyed a good diagram); the feedback must
/// teach the structural fix and keep the sequence form. The corrected plan — same
/// sequence with the missing edge added — validates.
#[tokio::test]
async fn structural_missing_edge_gets_structural_repair_and_keeps_sequence() {
    let mut broken = drain_plan(Epoch(6), false);
    // Drop the n2 -> n3 ordered edge: validation rejects with "sequence has no ordered
    // edge n2 -> n3".
    broken.forms[0].edges.truncate(1);
    let provider = ScriptedProvider::start([
        AiScriptStep::from_plan(&broken).unwrap(),
        AiScriptStep::from_plan(&drain_plan(Epoch(6), false)).unwrap(),
    ])
    .await
    .unwrap();
    let service = service_for(&provider);
    let digest = format!(
        "changed file {MIDDLEWARE_FILE}\n\
         ## focused source evidence (hunk ids are zero-based)\n\
         hunk_id: 0  file: {MIDDLEWARE_FILE}\n\
         note: symbol entities for this file were not queried"
    );

    let outcome = service
        .request_plan(&digest, &NoToolExecutor, &LazyFacts, Epoch(6))
        .await;
    let AiOutcome::Plan(plan, report) = outcome else {
        panic!("expected corrected plan, got {outcome:?}");
    };
    assert_eq!(report.verdict, ValidationVerdict::Valid);
    assert_eq!(plan.forms[0].kind, FormKind::Sequence);

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "one structural repair");
    let messages = requests[1].body_json().unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone();
    let feedback = messages.last().unwrap()["content"].as_str().unwrap();
    assert!(
        feedback.contains("sequence has no ordered edge"),
        "reason echoed (the missing pair is now named directly): {feedback}"
    );
    assert!(
        feedback.contains("Preserve the useful sequence"),
        "form preserved: {feedback}"
    );
    assert!(
        feedback.contains("consecutive sequence node in document order"),
        "ordered-edge rule taught: {feedback}"
    );
    assert!(
        !feedback.contains("changed_symbol_tree"),
        "must not force a tree swap: {feedback}"
    );
    assert!(
        !feedback.contains("relationship graph is unavailable"),
        "structural failure is not a fact failure: {feedback}"
    );
}

/// A typed edge between two resolvable entities that the fact store cannot verify is a
/// fact failure and keeps the conservative guidance even after the round-2 routing change.
/// FixtureFacts proves the edge absent ("not in the impact graph"), the exact live shape
/// the conservative instruction was written for.
#[tokio::test]
async fn unqueried_typed_edge_still_gets_conservative_repair() {
    // Entities resolvable by FixtureFacts with a Calls edge the graph does not contain.
    let mut asserted = sample_plan(Epoch(7));
    asserted.forms[0].kind = FormKind::RelationshipFlow;
    for node in &mut asserted.forms[0].nodes {
        node.children.clear();
    }
    asserted.forms[0].edges = vec![PlanEdge {
        from: "n1".into(),
        to: "n2".into(),
        kind: PlanEdgeKind::Calls,
        label: Some("continues request handling".into()),
    }];
    let provider = ScriptedProvider::start([
        AiScriptStep::from_plan(&asserted).unwrap(),
        AiScriptStep::valid_plan(Epoch(7)).unwrap(),
    ])
    .await
    .unwrap();
    let service = service_for(&provider);

    let outcome = service
        .request_plan("digest", &NoToolExecutor, &FixtureFacts, Epoch(7))
        .await;
    assert!(matches!(outcome, AiOutcome::Plan(..)), "got {outcome:?}");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "one fact repair");
    let messages = requests[1].body_json().unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone();
    let feedback = messages.last().unwrap()["content"].as_str().unwrap();
    assert!(
        feedback.contains("not in the impact graph"),
        "reason echoed: {feedback}"
    );
    assert!(
        feedback.contains("relationship graph is unavailable"),
        "fact failure keeps conservative guidance: {feedback}"
    );
    assert!(feedback.contains("changed_symbol_tree"));
}

/// MAX_PLAN_REPAIRS is 3: a model can burn two corrections (schema, then entity) and
/// still succeed on the third submission, and a fourth rejected submission terminates
/// with a bounded failure instead of looping.
#[tokio::test]
async fn third_repair_succeeds_and_fourth_rejection_terminates() {
    let with_entity = drain_plan(Epoch(8), true);
    let mut missing_detail = drain_plan(Epoch(8), false);
    missing_detail.forms[0].nodes[1].detail = None;
    let provider = ScriptedProvider::start([
        // Submission 1: unqueried symbol entity → repair 1 (entity guidance).
        AiScriptStep::from_plan(&with_entity).unwrap(),
        // Submission 2: entity fixed but a node lost its detail → repair 2 (generic).
        AiScriptStep::from_plan(&missing_detail).unwrap(),
        // Submission 3: complete and valid.
        AiScriptStep::from_plan(&drain_plan(Epoch(8), false)).unwrap(),
    ])
    .await
    .unwrap();
    let service = service_for(&provider);
    let digest = format!("changed file {MIDDLEWARE_FILE}: hunk 0 adds the readiness handler");
    let outcome = service
        .request_plan(&digest, &NoToolExecutor, &LazyFacts, Epoch(8))
        .await;
    assert!(
        matches!(outcome, AiOutcome::Plan(..)),
        "third submission validates: {outcome:?}"
    );
    assert_eq!(provider.requests().len(), 3);

    // The exhausted allowance: three rejected repairs, then a fourth rejection fails.
    let hallucinated = AiScriptStep::hallucinated_plan(Epoch(1)).unwrap();
    let provider = ScriptedProvider::start([
        hallucinated.clone(),
        hallucinated.clone(),
        hallucinated.clone(),
        hallucinated.clone(),
        // A fifth step would never be served; a dry script answers 500.
    ])
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
    assert_eq!(
        provider.requests().len(),
        4,
        "initial plan plus exactly three bounded repairs"
    );
}

/// Round-3 live failure 1: a 7-node sequence is valid JSON and passes serde, but exceeds
/// the schema's advertised 6-node cap. The parse boundary must reject it as a repairable
/// error with observed/allowed counts (never silent truncation — the final lifecycle node
/// may be the point of the diagram), and the repaired <=5 plan validates.
#[tokio::test]
async fn seven_node_plan_gets_cap_repair_then_five_node_plan_validates() {
    let mut oversized = drain_plan(Epoch(9), false);
    // Four more conceptual steps: 3 fixture nodes + 4 = 7, well past the five-node ceiling.
    for i in 3..7 {
        let node = PlanNode::new(
            format!("n{i}"),
            format!("step {i}"),
            PlanNodeChange::Unchanged,
        )
        .with_detail("an intermediate mechanics step that should be merged");
        oversized.forms[0].nodes.push(node);
        let last = i;
        oversized.forms[0].edges.push(PlanEdge {
            from: format!("n{}", last - 1),
            to: format!("n{i}"),
            kind: PlanEdgeKind::Writes,
            label: Some("continues the shutdown path".into()),
        });
    }
    let provider = ScriptedProvider::start([
        AiScriptStep::from_plan(&oversized).unwrap(),
        AiScriptStep::from_plan(&drain_plan(Epoch(9), false)).unwrap(),
    ])
    .await
    .unwrap();
    let service = service_for(&provider);
    let digest = format!("changed file {MIDDLEWARE_FILE}: hunk 0 adds the readiness handler");

    let outcome = service
        .request_plan(&digest, &NoToolExecutor, &LazyFacts, Epoch(9))
        .await;
    let AiOutcome::Plan(plan, report) = outcome else {
        panic!("expected corrected plan, got {outcome:?}");
    };
    assert_eq!(report.verdict, ValidationVerdict::Valid);
    assert_eq!(plan.forms[0].nodes.len(), 3, "fixture plan has 3 nodes");

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "initial submission plus one cap repair");
    let messages = requests[1].body_json().unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone();
    let feedback = messages.last().unwrap()["content"].as_str().unwrap();
    assert!(
        feedback.contains("has 7 nodes"),
        "observed count in feedback: {feedback}"
    );
    assert!(
        feedback.contains("at most 5"),
        "allowed count in feedback: {feedback}"
    );
    assert!(
        feedback.contains("merge intermediate mechanics"),
        "merge guidance: {feedback}"
    );
}

/// The final2-attempt1 live failure: GLM's first submission put a bare string ("detail")
/// where a node object was expected inside `nodes`. Serde rejects it as a repairable
/// MalformedPlan; the repair feedback must teach the element shape, and the corrected
/// plan validates.
#[tokio::test]
async fn bare_string_node_element_gets_shape_repair_then_validates() {
    // A valid plan, then corrupt it the way the live run did: one nodes element becomes
    // the bare string "detail".
    let plan = drain_plan(Epoch(12), false);
    let raw = serde_json::to_value(&plan).unwrap();
    let mut corrupted = raw;
    corrupted["forms"][0]["nodes"][1] = serde_json::json!("detail");
    let provider = ScriptedProvider::start([
        AiScriptStep::ToolCallRaw {
            arguments: serde_json::to_string(&corrupted).unwrap(),
        },
        AiScriptStep::from_plan(&plan).unwrap(),
    ])
    .await
    .unwrap();
    let service = service_for(&provider);
    let digest = format!("changed file {MIDDLEWARE_FILE}: hunk 0 adds the readiness handler");

    let outcome = service
        .request_plan(&digest, &NoToolExecutor, &LazyFacts, Epoch(12))
        .await;
    let AiOutcome::Plan(repaired, report) = outcome else {
        panic!("expected corrected plan, got {outcome:?}");
    };
    assert_eq!(report.verdict, ValidationVerdict::Valid);
    assert_eq!(
        repaired.forms[0].nodes.len(),
        3,
        "corrected plan keeps the fixture nodes"
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "one schema repair");
    let messages = requests[1].body_json().unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone();
    let feedback = messages.last().unwrap()["content"].as_str().unwrap();
    assert!(
        feedback.contains("expected struct PlanNode"),
        "serde reason echoed: {feedback}"
    );
    assert!(
        feedback.contains("Every array element must be an object"),
        "element-shape clause: {feedback}"
    );
    assert!(
        feedback.contains("never a bare string or field name"),
        "bare-string prohibition: {feedback}"
    );
}

/// The boundedness caps are enforced at the parse boundary with count feedback: a
/// 9-edge form triggers one repair, and a plan with empty evidence triggers one repair;
/// each corrected submission validates.
#[tokio::test]
async fn edges_and_evidence_cap_violations_get_count_repair_then_validate() {
    // 9 edges: fixture 2 + 7 extra = 9 > MAX_AI_FORM_EDGES (8).
    let mut dense = drain_plan(Epoch(13), false);
    for i in 0..7 {
        dense.forms[0].edges.push(PlanEdge {
            from: "n3".into(),
            to: "n1".into(),
            kind: PlanEdgeKind::Writes,
            label: Some(format!("extra causal edge {i}")),
        });
    }
    let provider = ScriptedProvider::start([
        AiScriptStep::from_plan(&dense).unwrap(),
        AiScriptStep::from_plan(&drain_plan(Epoch(13), false)).unwrap(),
    ])
    .await
    .unwrap();
    let service = service_for(&provider);
    let outcome = service
        .request_plan("digest", &NoToolExecutor, &LazyFacts, Epoch(13))
        .await;
    assert!(matches!(outcome, AiOutcome::Plan(..)), "got {outcome:?}");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "one edges-cap repair");
    let messages = requests[1].body_json().unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone();
    let feedback = messages.last().unwrap()["content"].as_str().unwrap();
    assert!(
        feedback.contains("has 9 edges"),
        "observed count: {feedback}"
    );
    assert!(feedback.contains("at most 8"), "allowed: {feedback}");

    // Empty evidence: the parse boundary rejects with the no-evidence reason.
    let mut no_evidence = drain_plan(Epoch(14), false);
    no_evidence.evidence.clear();
    let provider = ScriptedProvider::start([
        AiScriptStep::from_plan(&no_evidence).unwrap(),
        AiScriptStep::from_plan(&drain_plan(Epoch(14), false)).unwrap(),
    ])
    .await
    .unwrap();
    let service = service_for(&provider);
    let outcome = service
        .request_plan("digest", &NoToolExecutor, &LazyFacts, Epoch(14))
        .await;
    assert!(matches!(outcome, AiOutcome::Plan(..)), "got {outcome:?}");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "one evidence-floor repair");
    let messages = requests[1].body_json().unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone();
    let feedback = messages.last().unwrap()["content"].as_str().unwrap();
    assert!(
        feedback.contains("no evidence entries"),
        "named reason: {feedback}"
    );
    assert!(
        feedback.contains("at least one"),
        "floor taught: {feedback}"
    );
}

/// Sole invalid evidence (a hunk that does not exist) rejects; the repair feedback names
/// the citation fix; the corrected response with a valid hunk citation succeeds.
#[tokio::test]
async fn invalid_evidence_gets_citation_repair_then_validates() {
    let mut bad = drain_plan(Epoch(15), false);
    bad.evidence[0].hunk = Some(9); // only hunk 0 exists in LazyFacts
    let provider = ScriptedProvider::start([
        AiScriptStep::from_plan(&bad).unwrap(),
        AiScriptStep::from_plan(&drain_plan(Epoch(15), false)).unwrap(),
    ])
    .await
    .unwrap();
    let service = service_for(&provider);
    let digest = format!("changed file {MIDDLEWARE_FILE}: hunk 0 adds the readiness handler");

    let outcome = service
        .request_plan(&digest, &NoToolExecutor, &LazyFacts, Epoch(15))
        .await;
    let AiOutcome::Plan(plan, report) = outcome else {
        panic!("expected corrected plan, got {outcome:?}");
    };
    assert_eq!(report.verdict, ValidationVerdict::Valid);
    assert_eq!(plan.evidence.len(), 1);
    assert!(plan.evidence[0].hunk == Some(0));

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "one evidence repair");
    let messages = requests[1].body_json().unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone();
    let feedback = messages.last().unwrap()["content"].as_str().unwrap();
    assert!(
        feedback.contains("#h9") || feedback.contains("does not exist"),
        "concrete dropped reason preserved: {feedback}"
    );
    assert!(
        feedback.contains("no valid evidence remains") || feedback.contains("hunk"),
        "evidence failure named: {feedback}"
    );
    assert!(
        feedback.contains("exact repo_path"),
        "citation guidance: {feedback}"
    );
}

/// v4 strict rule at the parse boundary: a node without code_refs is a repairable
/// schema error that names the 1-2 exact-range contract; the corrected plan — every node
/// grounded in exact refs — validates, and the refs survive the request/parse/validate
/// loop verbatim.
#[tokio::test]
async fn missing_code_refs_gets_named_repair_and_refs_survive_the_loop() {
    let mut bare = drain_plan(Epoch(17), false);
    bare.forms[0].nodes[1].code_refs.clear();
    let provider = ScriptedProvider::start([
        AiScriptStep::from_plan(&bare).unwrap(),
        AiScriptStep::from_plan(&drain_plan(Epoch(17), false)).unwrap(),
    ])
    .await
    .unwrap();
    let service = service_for(&provider);
    let digest = format!("changed file {MIDDLEWARE_FILE}: hunk 0 adds the readiness handler");

    let outcome = service
        .request_plan(&digest, &NoToolExecutor, &LazyFacts, Epoch(17))
        .await;
    let AiOutcome::Plan(plan, report) = outcome else {
        panic!("expected corrected plan, got {outcome:?}");
    };
    assert_eq!(report.verdict, ValidationVerdict::Valid);
    // Every validated node kept its exact refs through parse + validation.
    for node in &plan.forms[0].nodes {
        assert!(
            (1..=MAX_NODE_CODE_REFS).contains(&node.code_refs.len()),
            "node {} kept exact refs: {:?}",
            node.id,
            node.code_refs
        );
    }
    assert_eq!(
        plan.forms[0].nodes[1].code_refs,
        vec![PlanCodeRef::new(
            FileId::new_unchecked(MIDDLEWARE_FILE),
            0,
            DiffSide::New,
            DRAIN_DELAY_LINES.0,
            DRAIN_DELAY_LINES.1,
        )],
        "the repaired node's exact range is preserved verbatim"
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "one code_refs repair");
    let messages = requests[1].body_json().unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone();
    let feedback = messages.last().unwrap()["content"].as_str().unwrap();
    assert!(
        feedback.contains("code_refs"),
        "named field in feedback: {feedback}"
    );
    assert!(
        feedback.contains("1-2 exact ranges"),
        "count contract taught: {feedback}"
    );
    assert!(
        feedback.contains("git_diff_file"),
        "source of truth taught: {feedback}"
    );
}

/// v4 strict rule at the validation boundary: a code_ref citing lines outside the
/// enumerated hunk is a fact failure — the diff-line lookup proves the line absent — so
/// the form is rejected with the concrete reason, and the corrected plan with in-hunk
/// refs validates.
#[tokio::test]
async fn code_ref_outside_the_hunk_is_rejected_then_exact_refs_validate() {
    let mut stray = drain_plan(Epoch(18), false);
    stray.forms[0].nodes[0].code_refs[0] = PlanCodeRef::new(
        FileId::new_unchecked(MIDDLEWARE_FILE),
        0,
        DiffSide::New,
        DRAIN_HUNK_NEW_SPAN + 2,
        DRAIN_HUNK_NEW_SPAN + 6,
    );
    let provider = ScriptedProvider::start([
        AiScriptStep::from_plan(&stray).unwrap(),
        AiScriptStep::from_plan(&drain_plan(Epoch(18), false)).unwrap(),
    ])
    .await
    .unwrap();
    let service = service_for(&provider);
    let digest = format!("changed file {MIDDLEWARE_FILE}: hunk 0 adds the readiness handler");

    let outcome = service
        .request_plan(&digest, &NoToolExecutor, &LazyFacts, Epoch(18))
        .await;
    let AiOutcome::Plan(plan, report) = outcome else {
        panic!("expected corrected plan, got {outcome:?}");
    };
    assert_eq!(report.verdict, ValidationVerdict::Valid);
    assert_eq!(
        plan.forms[0].nodes[0].code_refs,
        vec![PlanCodeRef::new(
            FileId::new_unchecked(MIDDLEWARE_FILE),
            0,
            DiffSide::New,
            DRAIN_READY_LINES.0,
            DRAIN_READY_LINES.1,
        )],
        "the corrected node cites the in-hunk readiness lines"
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "one code_ref repair");
    let messages = requests[1].body_json().unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone();
    let feedback = messages.last().unwrap()["content"].as_str().unwrap();
    assert!(
        feedback.contains("is not in that hunk"),
        "concrete diff-line reason echoed: {feedback}"
    );
    assert!(
        feedback.contains("code_ref"),
        "the failing ref is named: {feedback}"
    );
}

/// Repeated invalid evidence stays bounded: every submission cites the nonexistent hunk,
/// all repairs are consumed, and the request fails with the evidence reason.
#[tokio::test]
async fn repeated_invalid_evidence_stays_bounded() {
    let mut bad = drain_plan(Epoch(16), false);
    bad.evidence[0].hunk = Some(9);
    let step = AiScriptStep::from_plan(&bad).unwrap();
    let provider =
        ScriptedProvider::start([step.clone(), step.clone(), step.clone(), step.clone()])
            .await
            .unwrap();
    let service = service_for(&provider);

    let outcome = service
        .request_plan("digest", &NoToolExecutor, &LazyFacts, Epoch(16))
        .await;
    let AiOutcome::Failed(reason) = outcome else {
        panic!("expected bounded failure, got {outcome:?}");
    };
    assert!(
        reason.contains("no valid evidence remains") || reason.contains("#h9"),
        "bounded evidence failure: {reason}"
    );
    assert_eq!(
        provider.requests().len(),
        4,
        "initial submission plus exactly three bounded repairs"
    );
}

/// Repeated cap violations terminate within the existing repair bound:
/// every submission is oversized, so all three repairs are consumed and the request
/// fails with the parse error instead of looping.
#[tokio::test]
async fn repeated_contract_violations_terminate_within_repair_bound() {
    let mut oversized = drain_plan(Epoch(11), false);
    for i in 3..7 {
        let node = PlanNode::new(
            format!("n{i}"),
            format!("step {i}"),
            PlanNodeChange::Unchanged,
        )
        .with_detail("an intermediate mechanics step that should be merged");
        oversized.forms[0].nodes.push(node);
    }
    let step = AiScriptStep::from_plan(&oversized).unwrap();
    let provider =
        ScriptedProvider::start([step.clone(), step.clone(), step.clone(), step.clone()])
            .await
            .unwrap();
    let service = service_for(&provider);

    let outcome = service
        .request_plan("digest", &NoToolExecutor, &LazyFacts, Epoch(11))
        .await;
    let AiOutcome::Failed(reason) = outcome else {
        panic!("expected bounded failure, got {outcome:?}");
    };
    assert!(reason.contains("has 7 nodes"), "{reason}");
    assert_eq!(
        provider.requests().len(),
        4,
        "initial submission plus exactly three bounded repairs"
    );
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
    let hallucinated = AiScriptStep::hallucinated_plan(Epoch(1)).unwrap();
    let provider = ScriptedProvider::start([
        hallucinated.clone(),
        hallucinated.clone(),
        hallucinated.clone(),
        hallucinated.clone(),
    ])
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
    assert_eq!(
        provider.requests().len(),
        4,
        "initial plan plus exactly three bounded repairs"
    );
    // Sanity: the scripted plan really was hallucinated.
    assert!(hallucinated_sample_plan(Epoch(1)).forms[0].nodes[0]
        .entity
        .as_ref()
        .is_some_and(|e| e.file.as_path().as_str().contains("quantum")));
}

#[tokio::test]
async fn scheduler_path_waits_for_local_rate_capacity() {
    let provider = ScriptedProvider::start([
        AiScriptStep::valid_plan(Epoch(1)).unwrap(),
        AiScriptStep::valid_plan(Epoch(1)).unwrap(),
    ])
    .await
    .unwrap();
    let options = AiClientOptions {
        // One permit every 10 ms keeps the regression fast while still proving that the
        // second logical plan waits behind the exhausted one-token burst.
        requests_per_minute: 6_000,
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
    assert!(matches!(second, AiOutcome::Plan(..)), "got {second:?}");
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn model_discovery_bypasses_exhausted_inference_limiter() {
    let provider = ScriptedProvider::start([
        AiScriptStep::valid_plan(Epoch(1)).unwrap(),
        AiScriptStep::Raw {
            status: 200,
            content_type: "application/json".to_string(),
            body: json!({"data": [{"id": "recovery/model"}]}).to_string(),
        },
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
    let outcome = service
        .request_plan(
            "digest",
            &RecordingExecutor::default(),
            &FixtureFacts,
            Epoch(1),
        )
        .await;
    assert!(matches!(outcome, AiOutcome::Plan(..)), "got {outcome:?}");

    let models = service.client().list_models().await.unwrap();
    assert_eq!(models, ["recovery/model"]);
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].method, "GET");
    assert_eq!(requests[1].path, "/v1/models");
}

#[tokio::test]
async fn model_discovery_remains_a_recovery_path_while_inference_circuit_is_open() {
    let provider = ScriptedProvider::start([
        AiScriptStep::Raw {
            status: 503,
            content_type: "application/json".to_string(),
            body: json!({"error": {"message": "inference unavailable"}}).to_string(),
        },
        AiScriptStep::Raw {
            status: 200,
            content_type: "application/json".to_string(),
            body: json!({"data": [{"id": "alternate/model"}]}).to_string(),
        },
    ])
    .await
    .unwrap();
    let options = AiClientOptions {
        failure_threshold: 1,
        ..AiClientOptions::default()
    };
    let service = AiService::with_options(
        config_for(&provider, Duration::from_secs(5)),
        REPO_ROOT,
        options,
        fast_retry(),
    )
    .unwrap();
    let _ = service
        .request_plan(
            "digest",
            &RecordingExecutor::default(),
            &FixtureFacts,
            Epoch(1),
        )
        .await;
    assert!(service.client().is_circuit_open());

    let models = service.client().list_models().await.unwrap();
    assert_eq!(models, ["alternate/model"]);
    assert_eq!(provider.requests().len(), 2);
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
