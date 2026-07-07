//! Regression test for #279: a quota-exhausted Ollama response must carry the
//! CONFIGURED provider name (e.g. "ollama-cloud"), not a hardcoded "ollama".
//! The worker looks up the persisted cooldown by the configured name, so a
//! mismatch makes the cooldown short-circuit fail open and the worker hammers
//! the exhausted provider.

use std::net::SocketAddr;

use archivist_ai::{AiProviderError, ChatRequest, OllamaClient, TextProvider};
use axum::Router;
use axum::http::StatusCode;
use axum::routing::post;
use tokio::net::TcpListener;

async fn quota_429() -> (StatusCode, &'static str) {
    (
        StatusCode::TOO_MANY_REQUESTS,
        r#"{"error":"you have reached your weekly usage limit, upgrade for higher limits"}"#,
    )
}

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

#[tokio::test]
async fn ollama_quota_error_carries_configured_provider_name() {
    let addr = spawn(Router::new().route("/api/chat", post(quota_429))).await;
    let client = OllamaClient::new("ollama-cloud", &format!("http://{addr}"), None).unwrap();

    let err = client
        .chat(chat_request())
        .await
        .expect_err("429 quota body must error");

    let quota = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<AiProviderError>())
        .expect("error chain must contain AiProviderError");
    match quota {
        AiProviderError::QuotaExhausted { provider, .. } => {
            assert_eq!(
                provider, "ollama-cloud",
                "cooldown must be keyed by the configured provider name, not a hardcoded one"
            );
        }
        other => panic!("expected QuotaExhausted, got {other:?}"),
    }
}
