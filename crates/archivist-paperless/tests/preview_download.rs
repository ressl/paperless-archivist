//! #445: thumbnail/preview downloads for the review preview proxy only pass
//! allow-listed media types and respect their byte caps.

use std::net::SocketAddr;

use archivist_paperless::{
    MAX_THUMBNAIL_BYTES, PaperlessClient, PaperlessError, preview_content_type,
};
use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use secrecy::SecretString;
use tokio::net::TcpListener;

async fn spawn_server(router: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

fn client(addr: SocketAddr) -> PaperlessClient {
    PaperlessClient::new(
        &format!("http://{addr}"),
        SecretString::from("token".to_owned()),
        30,
    )
    .unwrap()
}

#[test]
fn content_type_allow_list_is_strict() {
    assert_eq!(
        preview_content_type(Some("image/webp"), false),
        Some("image/webp")
    );
    assert_eq!(
        preview_content_type(Some("IMAGE/PNG; charset=binary"), false),
        Some("image/png")
    );
    assert_eq!(preview_content_type(Some("application/pdf"), false), None);
    assert_eq!(
        preview_content_type(Some("application/pdf"), true),
        Some("application/pdf")
    );
    for hostile in ["text/html", "image/svg+xml", "application/javascript", ""] {
        assert_eq!(preview_content_type(Some(hostile), true), None, "{hostile}");
    }
    assert_eq!(preview_content_type(None, true), None);
}

#[tokio::test]
async fn thumbnail_and_preview_pass_vetted_types() {
    let router = Router::new()
        .route(
            "/api/documents/5/thumb/",
            get(|| async { ([(header::CONTENT_TYPE, "image/webp")], "thumb-bytes") }),
        )
        .route(
            "/api/documents/5/preview/",
            get(|| async { ([(header::CONTENT_TYPE, "application/pdf")], "%PDF-1.7") }),
        );
    let client = client(spawn_server(router).await);
    let thumb = client.download_thumbnail(5).await.expect("thumbnail");
    assert_eq!(thumb.content_type, "image/webp");
    assert_eq!(&thumb.bytes[..], b"thumb-bytes");
    let preview = client.download_preview(5).await.expect("preview");
    assert_eq!(preview.content_type, "application/pdf");
    assert_eq!(&preview.bytes[..], b"%PDF-1.7");
}

#[tokio::test]
async fn hostile_types_oversize_bodies_and_missing_documents_are_rejected() {
    let oversized = vec![0_u8; (MAX_THUMBNAIL_BYTES + 1) as usize];
    let router = Router::new()
        .route(
            "/api/documents/6/thumb/",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html")],
                    "<script>alert(1)</script>",
                )
            }),
        )
        .route(
            "/api/documents/6/preview/",
            get(|| async { ([(header::CONTENT_TYPE, "image/svg+xml")], "<svg/>") }),
        )
        .route(
            "/api/documents/7/thumb/",
            get(move || {
                let body = oversized.clone();
                async move { ([(header::CONTENT_TYPE, "image/png")], body).into_response() }
            }),
        )
        .route(
            "/api/documents/8/thumb/",
            get(|| async { (StatusCode::NOT_FOUND, r#"{"detail":"Not found."}"#) }),
        );
    let client = client(spawn_server(router).await);
    assert!(client.download_thumbnail(6).await.is_err());
    assert!(client.download_preview(6).await.is_err());
    let oversize = client
        .download_thumbnail(7)
        .await
        .expect_err("oversize thumbnail");
    assert!(oversize.to_string().contains("download cap"), "{oversize}");
    let missing = client.download_thumbnail(8).await.expect_err("missing");
    assert!(matches!(
        missing.downcast_ref::<PaperlessError>(),
        Some(PaperlessError::Client { status: 404, .. })
    ));
}
