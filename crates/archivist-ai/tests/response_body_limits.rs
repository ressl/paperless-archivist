use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use archivist_ai::{
    AI_PROVIDER_RESPONSE_BODY_LIMIT_BYTES, AiProviderError, ChatRequest, ImageInput,
    OpenAiCompatibleClient, TextProvider, VisionProvider, VisionRequest,
};
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{Response, StatusCode, header::CONTENT_LENGTH};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{StreamExt, stream};
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::net::TcpListener;

#[derive(Clone, Copy)]
enum OversizedBody {
    Declared,
    Chunked,
}

async fn spawn(router: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

fn oversized_response(mode: OversizedBody, status: StatusCode) -> Response<Body> {
    match mode {
        OversizedBody::Declared => Response::builder()
            .status(status)
            .header(CONTENT_LENGTH, AI_PROVIDER_RESPONSE_BODY_LIMIT_BYTES + 1)
            .body(Body::from(vec![
                b'x';
                AI_PROVIDER_RESPONSE_BODY_LIMIT_BYTES + 1
            ]))
            .unwrap(),
        OversizedBody::Chunked => {
            const CHUNK_BYTES: usize = 256 * 1024;
            let chunk = Bytes::from(vec![b'x'; CHUNK_BYTES]);
            let chunk_count = AI_PROVIDER_RESPONSE_BODY_LIMIT_BYTES / CHUNK_BYTES + 1;
            let chunks = (0..chunk_count)
                .map(|_| Ok::<_, Infallible>(chunk.clone()))
                .collect::<Vec<_>>();
            Response::builder()
                .status(status)
                .body(Body::from_stream(stream::iter(chunks)))
                .unwrap()
        }
    }
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

fn vision_request() -> VisionRequest {
    VisionRequest {
        model: "vision-model".to_owned(),
        prompt: "describe".to_owned(),
        images: vec![ImageInput {
            mime_type: "image/png".to_owned(),
            bytes: b"fake-png-bytes".to_vec(),
        }],
        temperature: 0.0,
        num_ctx: None,
        reasoning_effort: None,
        max_output_tokens: Some(256),
    }
}

fn assert_body_limit_error(error: &anyhow::Error) {
    let message = format!("{error:#}");
    assert!(
        message.contains("response body")
            && message.contains("exceeded")
            && message.contains(&AI_PROVIDER_RESPONSE_BODY_LIMIT_BYTES.to_string()),
        "expected a deterministic body-limit error, got: {message}"
    );
}

#[tokio::test]
async fn models_accept_normal_json_and_reject_declared_and_chunked_oversize() {
    let normal = Router::new().route(
        "/models",
        get(|| async { Json(json!({"data": [{"id": "minimax-m3"}]})) }),
    );
    let addr = spawn(normal).await;
    let client = OpenAiCompatibleClient::new("test-provider", &format!("http://{addr}"), None)
        .expect("client construction");
    assert_eq!(client.list_models().await.unwrap(), vec!["minimax-m3"]);

    for mode in [OversizedBody::Declared, OversizedBody::Chunked] {
        let router = Router::new().route(
            "/models",
            get(move || async move { oversized_response(mode, StatusCode::OK) }),
        );
        let addr = spawn(router).await;
        let client = OpenAiCompatibleClient::new("test-provider", &format!("http://{addr}"), None)
            .expect("client construction");
        let error = client
            .list_models()
            .await
            .expect_err("oversized models body");
        assert_body_limit_error(&error);
    }
}

#[tokio::test]
async fn chat_and_vision_reject_declared_and_chunked_oversize_success_bodies() {
    for mode in [OversizedBody::Declared, OversizedBody::Chunked] {
        let router = Router::new().route(
            "/chat/completions",
            post(move || async move { oversized_response(mode, StatusCode::OK) }),
        );
        let addr = spawn(router).await;
        let client = OpenAiCompatibleClient::new("test-provider", &format!("http://{addr}"), None)
            .expect("client construction");
        let chat_error = client
            .chat(chat_request())
            .await
            .expect_err("oversized chat body");
        assert_body_limit_error(&chat_error);

        let vision_error = client
            .vision(vision_request())
            .await
            .expect_err("oversized vision body");
        assert_body_limit_error(&vision_error);
    }
}

#[derive(Clone)]
struct RetryResponder {
    hits: Arc<AtomicUsize>,
    mode: OversizedBody,
}

async fn retry_then_oversize(State(state): State<RetryResponder>) -> Response<Body> {
    if state.hits.fetch_add(1, Ordering::SeqCst) == 0 {
        Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from(
                r#"{"error":"json_schema is not supported by the grammar backend"}"#,
            ))
            .unwrap()
    } else {
        oversized_response(state.mode, StatusCode::OK)
    }
}

#[tokio::test]
async fn structured_output_retry_is_bounded_for_declared_and_chunked_bodies() {
    for mode in [OversizedBody::Declared, OversizedBody::Chunked] {
        let state = RetryResponder {
            hits: Arc::new(AtomicUsize::new(0)),
            mode,
        };
        let router = Router::new()
            .route("/chat/completions", post(retry_then_oversize))
            .with_state(state.clone());
        let addr = spawn(router).await;
        let client = OpenAiCompatibleClient::new("test-provider", &format!("http://{addr}"), None)
            .expect("client construction");
        let mut request = chat_request();
        request.response_schema = Some(json!({"type": "object"}));
        let error = client
            .chat(request)
            .await
            .expect_err("oversized schema retry body");
        assert_body_limit_error(&error);
        assert_eq!(state.hits.load(Ordering::SeqCst), 2);
    }
}

async fn redacted_error() -> (StatusCode, Json<Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": "provider unavailable",
            "api_key": "super-secret-provider-key",
            "details": "x".repeat(10_000)
        })),
    )
}

async fn quota_error() -> (StatusCode, Json<Value>) {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({
            "error": "quota exceeded",
            "authorization": "Bearer super-secret-token"
        })),
    )
}

async fn primitive_secret_error() -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_GATEWAY,
        Json(json!("Bearer super-secret-provider-key")),
    )
}

async fn nested_secret_error() -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({
            "error": {"message": "received Bearer super-secret-provider-key"}
        })),
    )
}

#[tokio::test]
async fn error_diagnostics_are_redacted_and_bounded_quota_detection_survives() {
    let addr = spawn(Router::new().route("/chat/completions", post(redacted_error))).await;
    let client = OpenAiCompatibleClient::new("test-provider", &format!("http://{addr}"), None)
        .expect("client construction");
    let error = client
        .chat(chat_request())
        .await
        .expect_err("503 must error");
    let provider_error = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<AiProviderError>())
        .unwrap_or_else(|| panic!("typed provider error, got: {error:#}"));
    let AiProviderError::Server { body, .. } = provider_error else {
        panic!("expected server error, got {provider_error:?}");
    };
    assert!(body.contains("redacted"), "redacted body: {body}");
    assert!(!body.contains("super-secret"), "secret leaked: {body}");
    assert!(
        body.len() < 2_000,
        "diagnostic body is not bounded: {}",
        body.len()
    );

    let addr = spawn(Router::new().route("/chat/completions", post(quota_error))).await;
    let client = OpenAiCompatibleClient::new("test-provider", &format!("http://{addr}"), None)
        .expect("client construction");
    let error = client
        .chat(chat_request())
        .await
        .expect_err("429 must error");
    let provider_error = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<AiProviderError>())
        .expect("typed provider error");
    let AiProviderError::QuotaExhausted {
        provider, message, ..
    } = provider_error
    else {
        panic!("expected quota exhaustion, got {provider_error:?}");
    };
    assert_eq!(provider, "test-provider");
    assert!(message.contains("redacted"));
    assert!(!message.contains("super-secret"));
}

async fn assert_secret_diagnostic_redacted(router: Router) {
    let addr = spawn(router).await;
    let client = OpenAiCompatibleClient::new(
        "test-provider",
        &format!("http://{addr}"),
        Some(SecretString::new(
            "super-secret-provider-key".to_owned().into(),
        )),
    )
    .expect("client construction");
    let error = client
        .chat(chat_request())
        .await
        .expect_err("502 must error");
    let rendered = format!("{error:#}");
    assert!(
        !rendered.contains("super-secret"),
        "secret leaked: {rendered}"
    );
    assert!(
        rendered.contains("redacted"),
        "missing redaction marker: {rendered}"
    );
}

#[tokio::test]
async fn diagnostics_never_echo_secrets_from_json_primitives_or_generic_fields() {
    assert_secret_diagnostic_redacted(
        Router::new().route("/chat/completions", post(primitive_secret_error)),
    )
    .await;
    assert_secret_diagnostic_redacted(
        Router::new().route("/chat/completions", post(nested_secret_error)),
    )
    .await;
}

async fn interrupted_unauthorized_body() -> Response<Body> {
    let chunks = stream::once(async {
        Ok::<_, std::io::Error>(Bytes::from_static(br#"{"error":"unauthorized"#))
    })
    .chain(stream::once(async {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        Err(std::io::Error::other("provider stream interrupted"))
    }));
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .body(Body::from_stream(chunks))
        .unwrap()
}

#[tokio::test]
async fn interrupted_error_body_preserves_known_http_status_classification() {
    let addr =
        spawn(Router::new().route("/chat/completions", post(interrupted_unauthorized_body))).await;
    let client = OpenAiCompatibleClient::new("test-provider", &format!("http://{addr}"), None)
        .expect("client construction");
    let error = client
        .chat(chat_request())
        .await
        .expect_err("interrupted 401 must error");
    let provider_error = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<AiProviderError>())
        .unwrap_or_else(|| panic!("typed provider error, got: {error:#}"));
    let AiProviderError::Client { status, body } = provider_error else {
        panic!("expected client error, got {provider_error:?}");
    };
    assert_eq!(*status, 401);
    assert!(!provider_error.is_transient());
    assert!(body.contains("read failed"));
    assert!(!body.contains("unauthorized"));
}

#[derive(Clone)]
struct TruncatedSchemaResponder {
    hits: Arc<AtomicUsize>,
}

async fn truncated_schema_error(State(state): State<TruncatedSchemaResponder>) -> Response<Body> {
    state.hits.fetch_add(1, Ordering::SeqCst);
    let prefix = Bytes::from_static(
        br#"{"error":"json_schema is not supported by this backend","padding":"#,
    );
    let padding = Bytes::from(vec![b'x'; 256 * 1024]);
    let padding_chunks = AI_PROVIDER_RESPONSE_BODY_LIMIT_BYTES / padding.len() + 1;
    let mut chunks = Vec::with_capacity(padding_chunks + 1);
    chunks.push(Ok::<_, Infallible>(prefix));
    chunks.extend((0..padding_chunks).map(|_| Ok::<_, Infallible>(padding.clone())));
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .body(Body::from_stream(stream::iter(chunks)))
        .unwrap()
}

#[tokio::test]
async fn truncated_schema_error_never_arms_the_compatibility_retry() {
    let state = TruncatedSchemaResponder {
        hits: Arc::new(AtomicUsize::new(0)),
    };
    let router = Router::new()
        .route("/chat/completions", post(truncated_schema_error))
        .with_state(state.clone());
    let addr = spawn(router).await;
    let client = OpenAiCompatibleClient::new("test-provider", &format!("http://{addr}"), None)
        .expect("client construction");
    let mut request = chat_request();
    request.response_schema = Some(json!({"type": "object"}));
    let error = client
        .chat(request)
        .await
        .expect_err("truncated schema error must fail without retrying");
    assert_eq!(state.hits.load(Ordering::SeqCst), 1);
    let provider_error = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<AiProviderError>())
        .expect("typed provider error");
    let AiProviderError::Client { status, body } = provider_error else {
        panic!("expected client error, got {provider_error:?}");
    };
    assert_eq!(*status, 400);
    assert!(body.contains("truncated"));
    assert!(!body.contains("json_schema"));
}

#[tokio::test]
async fn oversized_error_bodies_preserve_status_classification_with_truncation_notice() {
    for mode in [OversizedBody::Declared, OversizedBody::Chunked] {
        let router = Router::new().route(
            "/chat/completions",
            post(move || async move { oversized_response(mode, StatusCode::SERVICE_UNAVAILABLE) }),
        );
        let addr = spawn(router).await;
        let client = OpenAiCompatibleClient::new("test-provider", &format!("http://{addr}"), None)
            .expect("client construction");
        let error = client
            .chat(chat_request())
            .await
            .expect_err("503 must error");
        let provider_error = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<AiProviderError>())
            .expect("typed provider error");
        let AiProviderError::Server { status, body } = provider_error else {
            panic!("expected server error, got {provider_error:?}");
        };
        assert_eq!(*status, 503);
        assert!(
            body.contains("truncated"),
            "missing truncation notice: {body}"
        );
        assert!(
            body.len() < 2_000,
            "diagnostic body is not bounded: {}",
            body.len()
        );
    }
}
