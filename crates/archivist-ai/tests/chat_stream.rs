//! Streamed Document Chat (#449): line parsing per provider framing and the
//! real clients against local mock servers that stream their bodies in small,
//! line-splitting chunks.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use archivist_ai::{
    AnthropicClient, ChatRequest, ChatStreamFormat, ChatStreamLine, OllamaClient,
    OpenAiCompatibleClient, parse_chat_stream_line,
};
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use secrecy::SecretString;
use serde_json::Value;
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
        system_prompt: "system".to_owned(),
        user_prompt: "question".to_owned(),
        temperature: 0.1,
        num_ctx: None,
        response_schema: None,
        reasoning_effort: None,
        max_output_tokens: None,
        structured_output: None,
    }
}

/// Serve `body` in 7-byte chunks so lines (and multi-byte characters) are
/// split across chunk boundaries.
fn chunked(body: &'static str, content_type: &'static str) -> Response {
    let chunks = body
        .as_bytes()
        .chunks(7)
        .map(|chunk| Ok::<_, Infallible>(chunk.to_vec()))
        .collect::<Vec<_>>();
    Response::builder()
        .header("content-type", content_type)
        .body(Body::from_stream(futures::stream::iter(chunks)))
        .unwrap()
}

type Captured = Arc<Mutex<Vec<Value>>>;

async fn capture(bodies: &Captured, body: Value) {
    bodies.lock().unwrap().push(body);
}

#[test]
fn parses_each_provider_framing() {
    assert_eq!(
        parse_chat_stream_line(
            ChatStreamFormat::OllamaNdjson,
            r#"{"message":{"role":"assistant","content":"Hal"},"done":false}"#
        ),
        ChatStreamLine {
            delta: Some("Hal".to_owned()),
            done: false,
            error: None
        }
    );
    assert!(
        parse_chat_stream_line(
            ChatStreamFormat::OllamaNdjson,
            r#"{"message":{"content":""},"done":true}"#
        )
        .done
    );
    assert_eq!(
        parse_chat_stream_line(
            ChatStreamFormat::OpenAiSse,
            r#"data: {"choices":[{"delta":{"content":"lo"}}]}"#
        )
        .delta
        .as_deref(),
        Some("lo")
    );
    // Reasoning deltas are not part of the visible answer.
    assert_eq!(
        parse_chat_stream_line(
            ChatStreamFormat::OpenAiSse,
            r#"data: {"choices":[{"delta":{"reasoning_content":"thinking"}}]}"#
        ),
        ChatStreamLine::default()
    );
    assert!(parse_chat_stream_line(ChatStreamFormat::OpenAiSse, "data: [DONE]").done);
    assert_eq!(
        parse_chat_stream_line(ChatStreamFormat::OpenAiSse, ": keep-alive"),
        ChatStreamLine::default()
    );
    assert_eq!(
        parse_chat_stream_line(
            ChatStreamFormat::AnthropicSse,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#
        )
        .delta
        .as_deref(),
        Some("Hi")
    );
    assert_eq!(
        parse_chat_stream_line(
            ChatStreamFormat::AnthropicSse,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"x"}}"#
        ),
        ChatStreamLine::default()
    );
    assert_eq!(
        parse_chat_stream_line(ChatStreamFormat::AnthropicSse, "event: message_stop"),
        ChatStreamLine::default()
    );
    assert!(
        parse_chat_stream_line(
            ChatStreamFormat::AnthropicSse,
            r#"data: {"type":"message_stop"}"#
        )
        .done
    );
    assert_eq!(
        parse_chat_stream_line(
            ChatStreamFormat::AnthropicSse,
            r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
        )
        .error
        .as_deref(),
        Some("Overloaded")
    );
}

#[tokio::test]
async fn openai_compatible_stream_forwards_deltas_and_sets_stream_flag() {
    async fn handler(State(bodies): State<Captured>, Json(body): Json<Value>) -> Response {
        capture(&bodies, body).await;
        chunked(
            concat!(
                "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"Grüße \"}}]}\n\n",
                ": keep-alive\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"[doc:12]\"}}]}\n\n",
                "data: [DONE]\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"ignored\"}}]}\n\n"
            ),
            "text/event-stream",
        )
    }
    let bodies = Captured::default();
    let addr = spawn(
        Router::new()
            .route("/chat/completions", post(handler))
            .with_state(bodies.clone()),
    )
    .await;
    let client = OpenAiCompatibleClient::new(
        "compat",
        &format!("http://{addr}"),
        Some(SecretString::from("sk-test")),
    )
    .unwrap();
    let mut deltas = Vec::new();
    let response = client
        .chat_stream(chat_request(), &mut |delta: &str| {
            deltas.push(delta.to_owned())
        })
        .await
        .expect("streamed chat");
    assert_eq!(deltas, vec!["Grüße ".to_owned(), "[doc:12]".to_owned()]);
    assert_eq!(response.text, "Grüße [doc:12]");
    assert_eq!(response.provider, "compat");
    let body = bodies.lock().unwrap()[0].clone();
    assert_eq!(body["stream"], true);
    assert_eq!(body["model"], "test-model");
}

#[tokio::test]
async fn ollama_stream_reads_ndjson_until_done() {
    async fn handler(State(bodies): State<Captured>, Json(body): Json<Value>) -> Response {
        capture(&bodies, body).await;
        chunked(
            concat!(
                "{\"message\":{\"content\":\"Eins \"},\"done\":false}\n",
                "{\"message\":{\"content\":\"zwei\"},\"done\":false}\n",
                "{\"message\":{\"content\":\"\"},\"done\":true}\n"
            ),
            "application/x-ndjson",
        )
    }
    let bodies = Captured::default();
    let addr = spawn(
        Router::new()
            .route("/api/chat", post(handler))
            .with_state(bodies.clone()),
    )
    .await;
    let client = OllamaClient::new("ollama-local", &format!("http://{addr}"), None).unwrap();
    let mut count = 0;
    let response = client
        .chat_stream(chat_request(), &mut |_delta: &str| count += 1)
        .await
        .expect("streamed chat");
    assert_eq!(count, 2);
    assert_eq!(response.text, "Eins zwei");
    assert_eq!(bodies.lock().unwrap()[0]["stream"], true);
}

#[tokio::test]
async fn anthropic_stream_surfaces_in_stream_errors() {
    async fn handler(Json(_body): Json<Value>) -> Response {
        chunked(
            concat!(
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Teil\"}}\n\n",
                "event: error\n",
                "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n"
            ),
            "text/event-stream",
        )
    }
    let addr = spawn(Router::new().route("/messages", post(handler))).await;
    let client = AnthropicClient::new(
        "anthropic",
        &format!("http://{addr}"),
        SecretString::from("key"),
    )
    .unwrap();
    let mut deltas = Vec::new();
    let error = client
        .chat_stream(chat_request(), &mut |delta: &str| {
            deltas.push(delta.to_owned())
        })
        .await
        .expect_err("in-stream error must fail the call");
    assert_eq!(deltas, vec!["Teil".to_owned()]);
    assert!(format!("{error:#}").contains("Overloaded"), "{error:#}");
}

#[tokio::test]
async fn stream_http_errors_keep_the_typed_status() {
    async fn handler() -> (StatusCode, &'static str) {
        (StatusCode::UNAUTHORIZED, "{\"error\":\"bad key\"}")
    }
    let addr = spawn(Router::new().route("/chat/completions", post(handler))).await;
    let client = OpenAiCompatibleClient::new("compat", &format!("http://{addr}"), None).unwrap();
    let error = client
        .chat_stream(chat_request(), &mut |_delta: &str| {})
        .await
        .expect_err("401 must fail");
    let typed = error
        .downcast_ref::<archivist_ai::AiProviderError>()
        .expect("typed provider error");
    assert!(matches!(
        typed,
        archivist_ai::AiProviderError::Client { status: 401, .. }
    ));
}
