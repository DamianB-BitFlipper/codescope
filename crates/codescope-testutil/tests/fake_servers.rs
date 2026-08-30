//! Integration tests for the fake LSP server and the scripted fake AI provider, driven
//! through their public APIs with hand-rolled test clients (no client frameworks).

use codescope_core::{Epoch, VisualizationPlan};
use codescope_testutil::fake_ai::{AiScriptStep, ScriptedProvider, FAKE_MODEL, PLAN_TOOL_NAME};
use codescope_testutil::fake_lsp::{
    read_frame, spawn_in_process, write_frame, FakeLspConfig, ScriptedResponse, METHOD_NOT_FOUND,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------
// fake LSP
// ---------------------------------------------------------------------------

type ClientReader = BufReader<ReadHalf<tokio::io::DuplexStream>>;
type ClientWriter = WriteHalf<tokio::io::DuplexStream>;

fn client_halves(io: tokio::io::DuplexStream) -> (ClientReader, ClientWriter) {
    let (read, write) = tokio::io::split(io);
    (BufReader::new(read), write)
}

async fn send(writer: &mut ClientWriter, message: &Value) {
    write_frame(writer, &serde_json::to_vec(message).unwrap())
        .await
        .unwrap();
}

async fn recv(reader: &mut ClientReader) -> Value {
    let bytes = read_frame(reader).await.unwrap().expect("frame");
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn lsp_handshake_diagnostics_and_method_not_found() {
    let config = FakeLspConfig::gopls_like()
        .with_diagnostics(json!({"uri": "file:///fx/internal/api/api.go", "diagnostics": []}));
    let session = spawn_in_process(config);
    let (mut reader, mut writer) = client_halves(session.client_io);

    // initialize → canned gopls-like result, matched by id.
    send(&mut writer, &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}))
        .await;
    let response = recv(&mut reader).await;
    assert_eq!(response["id"], 1);
    assert_eq!(
        response["result"]["capabilities"]["positionEncoding"],
        "utf-16"
    );

    // initialized notification triggers the diagnostics push.
    send(&mut writer, &json!({"jsonrpc": "2.0", "method": "initialized", "params": {}})).await;
    let push = recv(&mut reader).await;
    assert_eq!(push["method"], "textDocument/publishDiagnostics");
    assert_eq!(push["params"]["uri"], "file:///fx/internal/api/api.go");

    // Unknown method → -32601, like gopls.
    send(
        &mut writer,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "textDocument/monikers", "params": {}}),
    )
    .await;
    let response = recv(&mut reader).await;
    assert_eq!(response["id"], 2);
    assert_eq!(response["error"]["code"], METHOD_NOT_FOUND);

    // shutdown → null result; exit → session ends.
    send(&mut writer, &json!({"jsonrpc": "2.0", "id": 3, "method": "shutdown"})).await;
    let response = recv(&mut reader).await;
    assert_eq!(response["id"], 3);
    assert!(response["result"].is_null());
    send(&mut writer, &json!({"jsonrpc": "2.0", "method": "exit"})).await;

    let log = session.handle.await.unwrap().unwrap();
    let methods: Vec<&str> = log.iter().map(|m| m.method.as_str()).collect();
    assert_eq!(
        methods,
        ["initialize", "initialized", "textDocument/monikers", "shutdown", "exit"]
    );
    assert!(log[0].is_request());
    assert!(!log[1].is_request());
}

#[tokio::test]
async fn lsp_null_capabilities_and_canned_document_symbols() {
    let symbols = json!([{
        "name": "MemoryRepo",
        "kind": 23,
        "range": {"start": {"line": 2, "character": 0}, "end": {"line": 6, "character": 1}},
        "selectionRange": {"start": {"line": 2, "character": 5}, "end": {"line": 2, "character": 15}},
        "children": []
    }]);
    let config = FakeLspConfig::null_capabilities().with_document_symbols(symbols.clone());
    let session = spawn_in_process(config);
    let (mut reader, mut writer) = client_halves(session.client_io);

    send(&mut writer, &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}))
        .await;
    let response = recv(&mut reader).await;
    assert!(response["result"]["capabilities"].is_null());

    send(
        &mut writer,
        &json!({
            "jsonrpc": "2.0", "id": 2, "method": "textDocument/documentSymbol",
            "params": {"textDocument": {"uri": "file:///fx/internal/store/memstore.go"}}
        }),
    )
    .await;
    let response = recv(&mut reader).await;
    assert_eq!(response["result"], symbols);
}

#[tokio::test]
async fn lsp_malformed_json_body_reaches_client_verbatim() {
    let config = FakeLspConfig::gopls_like()
        .with_response("textDocument/hover", ScriptedResponse::malformed_json());
    let session = spawn_in_process(config);
    let (mut reader, mut writer) = client_halves(session.client_io);

    send(&mut writer, &json!({"jsonrpc": "2.0", "id": 1, "method": "textDocument/hover"})).await;
    let bytes = read_frame(&mut reader).await.unwrap().expect("frame");
    assert!(serde_json::from_slice::<Value>(&bytes).is_err(), "body must not parse");
}

#[tokio::test]
async fn lsp_wrong_content_length_starves_the_client() {
    let config = FakeLspConfig::gopls_like().with_response(
        "textDocument/hover",
        ScriptedResponse::WrongContentLength {
            body: "{}".to_string(),
            excess: 64,
        },
    );
    let session = spawn_in_process(config);
    let (mut reader, mut writer) = client_halves(session.client_io);

    send(&mut writer, &json!({"jsonrpc": "2.0", "id": 1, "method": "textDocument/hover"})).await;
    // The header promises 66 bytes but only 2 arrive: a spec-following reader must block.
    let starved = tokio::time::timeout(Duration::from_millis(200), read_frame(&mut reader)).await;
    assert!(starved.is_err(), "read must time out on a short frame");
}

#[tokio::test]
async fn lsp_truncate_and_close_ends_the_stream_mid_frame() {
    let config = FakeLspConfig::gopls_like().with_response(
        "initialize",
        ScriptedResponse::TruncateAndClose {
            body: r#"{"jsonrpc":"2.0","#.to_string(),
            declared_len: 4096,
        },
    );
    let session = spawn_in_process(config);
    let (mut reader, mut writer) = client_halves(session.client_io);

    send(&mut writer, &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"})).await;
    let result = read_frame(&mut reader).await;
    assert!(result.is_err(), "truncated stream must error, got {result:?}");
    session.handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn lsp_response_delay_is_applied() {
    let config = FakeLspConfig::gopls_like().with_response_delay(Duration::from_millis(120));
    let session = spawn_in_process(config);
    let (mut reader, mut writer) = client_halves(session.client_io);

    let start = Instant::now();
    send(&mut writer, &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"})).await;
    let response = recv(&mut reader).await;
    assert_eq!(response["id"], 1);
    assert!(
        start.elapsed() >= Duration::from_millis(100),
        "response arrived too fast: {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn lsp_ignored_shutdown_forces_kill_path() {
    let config = FakeLspConfig::gopls_like().with_shutdown_ignored();
    let session = spawn_in_process(config);
    let (mut reader, mut writer) = client_halves(session.client_io);

    send(&mut writer, &json!({"jsonrpc": "2.0", "id": 1, "method": "shutdown"})).await;
    let silence = tokio::time::timeout(Duration::from_millis(200), read_frame(&mut reader)).await;
    assert!(silence.is_err(), "shutdown must be ignored");
    // The client's escalation: drop the transport (≙ kill); the server ends cleanly.
    drop(writer);
    drop(reader);
    let log = session.handle.await.unwrap().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].method, "shutdown");
}

// ---------------------------------------------------------------------------
// fake AI provider
// ---------------------------------------------------------------------------

struct HttpResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: String,
}

/// Minimal HTTP/1.1 POST client (the provider closes the connection after responding).
async fn post_chat(provider: &ScriptedProvider, body: &Value) -> HttpResponse {
    let mut stream = TcpStream::connect(provider.addr()).await.unwrap();
    let payload = body.to_string();
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nhost: {}\r\ncontent-type: application/json\r\nauthorization: Bearer fake-key\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        provider.addr(),
        payload.len(),
        payload
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> HttpResponse {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text.split_once("\r\n\r\n").expect("http head/body split");
    let mut lines = head.split("\r\n");
    let status_line = lines.next().expect("status line");
    let status: u16 = status_line.split_whitespace().nth(1).expect("code").parse().expect("numeric status");
    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    HttpResponse { status, headers, body: body.to_string() }
}

fn chat_request() -> Value {
    json!({
        "model": "gpt-fake",
        "messages": [{"role": "user", "content": "visualize the change set"}],
        "tools": [{"type": "function", "function": {"name": PLAN_TOOL_NAME}}]
    })
}

#[tokio::test]
async fn ai_valid_plan_round_trips_through_core_types() {
    let provider = ScriptedProvider::start([AiScriptStep::valid_plan(Epoch(9)).unwrap()])
        .await
        .unwrap();

    let response = post_chat(&provider, &chat_request()).await;
    assert_eq!(response.status, 200);
    assert_eq!(response.headers["content-type"], "application/json");

    let completion: Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(completion["model"], FAKE_MODEL);
    let choice = &completion["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls");
    let call = &choice["message"]["tool_calls"][0];
    assert_eq!(call["type"], "function");
    assert_eq!(call["function"]["name"], PLAN_TOOL_NAME);

    let plan: VisualizationPlan =
        serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(plan.epoch, Epoch(9), "provider must echo the scripted epoch");
    assert!(!plan.forms.is_empty());

    // The client's request was recorded.
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/v1/chat/completions");
    assert_eq!(requests[0].headers["authorization"], "Bearer fake-key");
    assert_eq!(requests[0].body_json().unwrap()["model"], "gpt-fake");
    assert_eq!(provider.remaining_steps(), 0);
}

#[tokio::test]
async fn ai_scripted_failure_modes_in_order() {
    let provider = ScriptedProvider::start([
        AiScriptStep::malformed_json(),
        AiScriptStep::hallucinated_plan(Epoch(1)).unwrap(),
        AiScriptStep::AssistantText { content: "cannot help".to_string() },
        AiScriptStep::RateLimited { retry_after_secs: 7 },
        AiScriptStep::Raw {
            status: 503,
            content_type: "text/html".to_string(),
            body: "<h1>down</h1>".to_string(),
        },
    ])
    .await
    .unwrap();

    // 1. Malformed tool-call arguments: HTTP-valid, JSON arguments unparseable.
    let response = post_chat(&provider, &chat_request()).await;
    assert_eq!(response.status, 200);
    let completion: Value = serde_json::from_str(&response.body).unwrap();
    let arguments = completion["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .unwrap();
    assert!(serde_json::from_str::<Value>(arguments).is_err());

    // 2. Hallucinated entities parse fine — catching them is the validator's job.
    let response = post_chat(&provider, &chat_request()).await;
    let completion: Value = serde_json::from_str(&response.body).unwrap();
    let plan: VisualizationPlan = serde_json::from_str(
        completion["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        plan.forms[0].nodes[0].entity.as_ref().unwrap().file.to_string(),
        "internal/api/quantum_flux.go"
    );

    // 3. Plain text instead of a tool call.
    let response = post_chat(&provider, &chat_request()).await;
    let completion: Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(completion["choices"][0]["finish_reason"], "stop");
    assert_eq!(completion["choices"][0]["message"]["content"], "cannot help");
    assert!(completion["choices"][0]["message"]["tool_calls"].is_null());

    // 4. 429 + Retry-After.
    let response = post_chat(&provider, &chat_request()).await;
    assert_eq!(response.status, 429);
    assert_eq!(response.headers["retry-after"], "7");

    // 5. Raw scripted response.
    let response = post_chat(&provider, &chat_request()).await;
    assert_eq!(response.status, 503);
    assert_eq!(response.headers["content-type"], "text/html");
    assert_eq!(response.body, "<h1>down</h1>");

    // 6. Script exhausted → explanatory 500, never a panic.
    let response = post_chat(&provider, &chat_request()).await;
    assert_eq!(response.status, 500);
    let error: Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(error["error"]["type"], "script_exhausted");

    assert_eq!(provider.requests().len(), 6);
}

#[tokio::test]
async fn ai_hang_step_hangs_until_abort() {
    let provider = ScriptedProvider::start([AiScriptStep::Hang]).await.unwrap();

    let mut stream = TcpStream::connect(provider.addr()).await.unwrap();
    let request =
        "POST /v1/chat/completions HTTP/1.1\r\nhost: fake\r\ncontent-length: 2\r\n\r\n{}";
    stream.write_all(request.as_bytes()).await.unwrap();

    // No response while the script says Hang…
    let mut buf = [0u8; 64];
    let silent = tokio::time::timeout(Duration::from_millis(250), stream.read(&mut buf)).await;
    assert!(silent.is_err(), "hang step must not respond");
    // …but the request was still recorded.
    assert_eq!(provider.requests().len(), 1);

    // Abort ends the hung connection promptly.
    provider.abort();
    let ended = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf)).await;
    match ended {
        Ok(Ok(0)) => {}                 // clean EOF
        Ok(Ok(n)) => panic!("unexpected {n} bytes after abort"),
        Ok(Err(_)) => {}                // reset — also fine
        Err(_) => panic!("connection must end after abort"),
    }
}

#[tokio::test]
async fn ai_steps_can_be_pushed_while_serving() {
    let provider = ScriptedProvider::start([]).await.unwrap();
    provider.push_step(AiScriptStep::valid_plan(Epoch(2)).unwrap());
    assert_eq!(provider.remaining_steps(), 1);

    let response = post_chat(&provider, &chat_request()).await;
    assert_eq!(response.status, 200);
    assert_eq!(provider.remaining_steps(), 0);
    assert!(provider.chat_completions_url().ends_with("/v1/chat/completions"));
    assert!(provider.base_url().starts_with("http://127.0.0.1:"));
}
