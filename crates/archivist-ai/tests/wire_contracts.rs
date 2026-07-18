//! Wire-level contract tests: assert on the exact bytes/JSON the real
//! clients send and how the OpenAI-compatible schema-400 self-healing retry
//! decides whether to fire. Spawns a real axum `Router` and points the real
//! `reqwest`-backed clients at it, reusing `quota_provider_name.rs`'s harness
//! pattern (spawn/addr/client construction) instead of unit-testing the
//! payload builders in isolation.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use archivist_ai::{
    ChatRequest, ImageInput, MINIMAX_M3_MODEL, MineruClient, OpenAiCompatibleClient, TextProvider,
    VisionProvider, VisionRequest,
};
use archivist_core::{ReasoningEffort, StructuredOutputMode};
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header::AUTHORIZATION};
use axum::routing::post;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::net::TcpListener;

async fn spawn(router: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

fn chat_request() -> ChatRequest {
    ChatRequest {
        model: "test-model".to_owned(),
        system_prompt: String::new(),
        user_prompt: "hello".to_owned(),
        temperature: 0.0,
        num_ctx: None,
        response_schema: None,
        reasoning_effort: None,
        max_output_tokens: None,
        structured_output: None,
    }
}

// --- Test 1: schema-400 retry reposts without response_format ---------

async fn schema_retry_handler(
    State(bodies): State<Arc<Mutex<Vec<Value>>>>,
    Json(body): Json<Value>,
) -> (StatusCode, String) {
    let call_index = {
        let mut bodies = bodies.lock().unwrap();
        bodies.push(body);
        bodies.len()
    };
    if call_index == 1 {
        (
            StatusCode::BAD_REQUEST,
            r#"{"error":"json_schema is not supported by the grammar backend"}"#.to_owned(),
        )
    } else {
        (
            StatusCode::OK,
            r#"{"choices":[{"message":{"content":"ok"}}]}"#.to_owned(),
        )
    }
}

#[tokio::test]
async fn schema_400_retry_reposts_without_response_format() {
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let router = Router::new()
        .route("/chat/completions", post(schema_retry_handler))
        .with_state(bodies.clone());
    let addr = spawn(router).await;
    let client = OpenAiCompatibleClient::new("test-provider", &format!("http://{addr}"), None)
        .expect("client construction");

    let mut request = chat_request();
    request.response_schema = Some(json!({"type": "object"}));
    request.structured_output = None; // Auto

    let response = client
        .chat(request)
        .await
        .expect("Auto-mode schema-400 must self-heal via retry");
    assert_eq!(response.text, "ok");

    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2, "server must see exactly 2 requests");
    assert!(
        bodies[0].get("response_format").is_some(),
        "first request must carry response_format: {:?}",
        bodies[0]
    );
    assert!(
        bodies[1].get("response_format").is_none(),
        "retry must drop response_format: {:?}",
        bodies[1]
    );
}

// --- Test 2: retry must not arm outside Auto mode / unrelated errors ---

#[derive(Clone)]
struct CountingResponder {
    hits: Arc<Mutex<u32>>,
    status: StatusCode,
    body: &'static str,
}

async fn counting_handler(State(responder): State<CountingResponder>) -> (StatusCode, String) {
    *responder.hits.lock().unwrap() += 1;
    (responder.status, responder.body.to_owned())
}

/// Spins up a fresh server that always answers 400 with `error_body`, drives
/// one `chat()` call with the given request shape, asserts it errors, and
/// returns how many requests the server actually saw.
async fn run_no_retry_case(
    structured_output: Option<StructuredOutputMode>,
    response_schema: Option<Value>,
    error_body: &'static str,
) -> u32 {
    let hits = Arc::new(Mutex::new(0u32));
    let responder = CountingResponder {
        hits: hits.clone(),
        status: StatusCode::BAD_REQUEST,
        body: error_body,
    };
    let router = Router::new()
        .route("/chat/completions", post(counting_handler))
        .with_state(responder);
    let addr = spawn(router).await;
    let client = OpenAiCompatibleClient::new("test-provider", &format!("http://{addr}"), None)
        .expect("client construction");

    let mut request = chat_request();
    request.response_schema = response_schema;
    request.structured_output = structured_output;

    client
        .chat(request)
        .await
        .expect_err("non-retryable 400 must surface as an error");

    *hits.lock().unwrap()
}

#[tokio::test]
async fn schema_400_retry_not_armed_when_mode_not_auto_or_error_unrelated() {
    let schema_body = r#"{"error":"json_schema is not supported by the grammar backend"}"#;

    // a. Explicit JsonObject mode disarms the retry even though the error
    // body mentions json_schema — self-healing is Auto-only.
    let hits_a = run_no_retry_case(
        Some(StructuredOutputMode::JsonObject),
        Some(json!({"type": "object"})),
        schema_body,
    )
    .await;
    assert_eq!(hits_a, 1, "JsonObject mode must not retry");

    // b. Off mode never puts response_format on the wire in the first
    // place, so the retry gate can't arm regardless of the error body.
    let hits_b = run_no_retry_case(
        Some(StructuredOutputMode::Off),
        Some(json!({"type": "object"})),
        schema_body,
    )
    .await;
    assert_eq!(hits_b, 1, "Off mode must not retry");

    // c. Auto mode arms the gate, but an error body with no schema-related
    // keyword must not trigger the retry.
    let hits_c = run_no_retry_case(
        None,
        Some(json!({"type": "object"})),
        r#"{"error": "model not found"}"#,
    )
    .await;
    assert_eq!(hits_c, 1, "unrelated error body must not retry");
}

// --- Test 3: MiniMax reasoning stays auditable but never becomes result text

async fn minimax_reasoning_handler(
    State(body): State<Arc<Mutex<Option<Value>>>>,
    Json(request): Json<Value>,
) -> Json<Value> {
    *body.lock().unwrap() = Some(request);
    Json(json!({
        "choices": [{
            "message": {
                "content": "<mm:think>inline fallback</mm:think>  final answer  ",
                "reasoning_content": "parser-separated reasoning"
            }
        }]
    }))
}

#[tokio::test]
async fn minimax_reasoning_is_preserved_raw_but_removed_from_result_text() {
    let body = Arc::new(Mutex::new(None));
    let router = Router::new()
        .route("/chat/completions", post(minimax_reasoning_handler))
        .with_state(body.clone());
    let addr = spawn(router).await;
    let client = OpenAiCompatibleClient::new("sglang", &format!("http://{addr}"), None)
        .expect("client construction");

    let mut request = chat_request();
    request.model = MINIMAX_M3_MODEL.to_owned();
    request.reasoning_effort = Some(ReasoningEffort::Off);
    let response = client.chat(request).await.expect("valid M3 response");

    assert_eq!(response.text, "final answer");
    assert_eq!(
        response.raw_response["choices"][0]["message"]["reasoning_content"],
        "parser-separated reasoning"
    );
    assert_eq!(
        body.lock().unwrap().as_ref().unwrap()["chat_template_kwargs"]["thinking_mode"],
        "disabled"
    );
}

// --- Test 4: MinerU vision multipart roundtrip --------------------------

#[derive(Default)]
struct CapturedRequest {
    headers: HeaderMap,
    body: String,
}

async fn mineru_capture_handler(
    State(captured): State<Arc<Mutex<CapturedRequest>>>,
    headers: HeaderMap,
    body: String,
) -> Json<Value> {
    let mut captured = captured.lock().unwrap();
    captured.headers = headers;
    captured.body = body;
    Json(json!({ "md_content": "# Rechnung" }))
}

#[tokio::test]
async fn mineru_vision_multipart_roundtrip() {
    let captured = Arc::new(Mutex::new(CapturedRequest::default()));
    let router = Router::new()
        .route("/file_parse", post(mineru_capture_handler))
        .with_state(captured.clone());
    let addr = spawn(router).await;

    let client = MineruClient::new(
        "mineru",
        &format!("http://{addr}"),
        Some(SecretString::new("test-key".to_owned().into())),
    )
    .expect("client construction");

    let request = VisionRequest {
        model: "mineru".to_owned(),
        prompt: String::new(),
        images: vec![ImageInput {
            mime_type: "image/png".to_owned(),
            bytes: b"fake-png-bytes".to_vec(),
        }],
        temperature: 0.0,
        num_ctx: None,
        reasoning_effort: None,
        max_output_tokens: None,
    };

    let response = client
        .vision(request)
        .await
        .expect("MinerU multipart roundtrip must succeed");
    assert_eq!(response.text, "# Rechnung");
    assert_eq!(response.model, "mineru");

    let captured = captured.lock().unwrap();
    assert!(
        captured.body.contains(r#"name="files""#),
        "multipart must carry the files field: {}",
        captured.body
    );
    assert!(
        captured.body.contains(r#"filename="page.png""#),
        "multipart must set the page.png filename: {}",
        captured.body
    );
    assert!(
        captured
            .body
            .to_ascii_lowercase()
            .contains("content-type: image/png"),
        "multipart part must declare the image/png mime type: {}",
        captured.body
    );
    assert!(
        captured.body.contains(r#"name="return_md""#),
        "multipart must carry the return_md field: {}",
        captured.body
    );
    assert!(
        captured.body.contains("true"),
        "return_md value must be true: {}",
        captured.body
    );

    let auth = captured
        .headers
        .get(AUTHORIZATION)
        .expect("Authorization header must be present")
        .to_str()
        .expect("Authorization header must be ASCII");
    assert_eq!(auth, "Bearer test-key");
}

// --- Test 4: OpenAI-compatible vision sends max_tokens on the wire ------

async fn capture_vision_handler(
    State(bodies): State<Arc<Mutex<Vec<Value>>>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    bodies.lock().unwrap().push(body);
    Json(json!({"choices":[{"message":{"content":"ok"}}]}))
}

#[tokio::test]
async fn openai_vision_sends_max_tokens_on_wire() {
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let router = Router::new()
        .route("/chat/completions", post(capture_vision_handler))
        .with_state(bodies.clone());
    let addr = spawn(router).await;
    let client = OpenAiCompatibleClient::new("test-provider", &format!("http://{addr}"), None)
        .expect("client construction");

    let with_cap = VisionRequest {
        model: archivist_ai::MINIMAX_M3_MODEL.to_owned(),
        prompt: "describe".to_owned(),
        images: vec![ImageInput {
            mime_type: "image/png".to_owned(),
            bytes: b"fake-png-bytes".to_vec(),
        }],
        temperature: 0.0,
        num_ctx: None,
        reasoning_effort: Some(archivist_core::ReasoningEffort::Off),
        max_output_tokens: Some(1234),
    };
    client
        .vision(with_cap)
        .await
        .expect("vision call with max_output_tokens should succeed");

    let without_cap = VisionRequest {
        model: "vision-model".to_owned(),
        prompt: "describe".to_owned(),
        images: vec![ImageInput {
            mime_type: "image/png".to_owned(),
            bytes: b"fake-png-bytes".to_vec(),
        }],
        temperature: 0.0,
        num_ctx: None,
        reasoning_effort: None,
        max_output_tokens: None,
    };
    client
        .vision(without_cap)
        .await
        .expect("vision call without max_output_tokens should succeed");

    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2, "server must see exactly 2 requests");
    assert_eq!(bodies[0]["max_tokens"], json!(1234));
    assert_eq!(
        bodies[0]["chat_template_kwargs"]["thinking_mode"],
        json!("disabled")
    );
    assert!(
        bodies[0]["messages"][0]["content"][1]["image_url"]["url"]
            .as_str()
            .is_some_and(|url| url.starts_with("data:image/png;base64,"))
    );
    assert!(bodies[1].get("chat_template_kwargs").is_none());
    assert!(
        bodies[1].get("max_tokens").is_none(),
        "omitted max_output_tokens must not appear on the wire: {:?}",
        bodies[1]
    );
}
