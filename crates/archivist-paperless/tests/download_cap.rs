//! Integration test for download_original: the streaming rewrite (#256) still
//! returns a normal body intact. The byte-cap arithmetic itself is unit-tested
//! in the crate (accumulate_download_size); exercising the 250 MB limit
//! end-to-end would require transferring 250 MB, so it is not done here.

use std::net::SocketAddr;

use archivist_paperless::PaperlessClient;
use axum::Router;
use axum::response::IntoResponse;
use axum::routing::get;
use secrecy::SecretString;
use tokio::net::TcpListener;

async fn small_download() -> impl IntoResponse {
    (axum::http::StatusCode::OK, "tiny pdf bytes")
}

async fn spawn_server(router: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

#[tokio::test]
async fn download_streams_normal_body_intact() {
    let addr =
        spawn_server(Router::new().route("/api/documents/2/download/", get(small_download))).await;
    let client = PaperlessClient::new(
        &format!("http://{addr}"),
        SecretString::from("token".to_owned()),
        30,
    )
    .unwrap();

    let bytes = client.download_original(2).await.expect("normal download");
    assert_eq!(&bytes[..], b"tiny pdf bytes");
}
