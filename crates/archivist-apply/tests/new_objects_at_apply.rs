//! #404: model-proposed Paperless objects (new tags / correspondent) travel
//! as names in the review patch and are only created at apply time, capped
//! and gated by the live settings. No database required.

use std::sync::Arc;

use archivist_apply::{
    MAX_NEW_TAGS_PER_DOCUMENT, NewObjectPolicy, PENDING_NEW_OBJECTS_KEY, PendingNewObjects,
    materialize_pending_new_objects,
};
use archivist_core::{DocumentPatch, WorkflowTags};
use archivist_paperless::PaperlessClient;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

#[derive(Default)]
struct Catalog {
    tags: Vec<(i32, String)>,
    correspondents: Vec<(i32, String)>,
    created_tags: Vec<String>,
    created_correspondents: Vec<String>,
}

fn page(items: &[(i32, String)]) -> Json<Value> {
    let results: Vec<Value> = items
        .iter()
        .map(|(id, name)| json!({"id": id, "name": name, "slug": null, "color": null}))
        .collect();
    Json(json!({"count": results.len(), "next": null, "previous": null, "results": results}))
}

async fn list_tags(State(state): State<Arc<Mutex<Catalog>>>) -> Json<Value> {
    page(&state.lock().await.tags)
}

async fn create_tag(
    State(state): State<Arc<Mutex<Catalog>>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let mut state = state.lock().await;
    let name = body["name"].as_str().expect("name").to_owned();
    let id = 100 + state.tags.len() as i32;
    state.tags.push((id, name.clone()));
    state.created_tags.push(name.clone());
    Json(json!({"id": id, "name": name, "slug": null, "color": null}))
}

async fn list_correspondents(State(state): State<Arc<Mutex<Catalog>>>) -> Json<Value> {
    page(&state.lock().await.correspondents)
}

async fn create_correspondent(
    State(state): State<Arc<Mutex<Catalog>>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let mut state = state.lock().await;
    let name = body["name"].as_str().expect("name").to_owned();
    let id = 500 + state.correspondents.len() as i32;
    state.correspondents.push((id, name.clone()));
    state.created_correspondents.push(name.clone());
    Json(json!({"id": id, "name": name}))
}

async fn mock_client() -> (PaperlessClient, Arc<Mutex<Catalog>>) {
    let state = Arc::new(Mutex::new(Catalog {
        tags: vec![(1, "Rechnung".to_owned()), (2, "ai-process".to_owned())],
        ..Catalog::default()
    }));
    let app = Router::new()
        .route("/api/tags/", get(list_tags).post(create_tag))
        .route(
            "/api/correspondents/",
            get(list_correspondents).post(create_correspondent),
        )
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let address = listener.local_addr().expect("mock address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });
    let client = PaperlessClient::new(
        &format!("http://{address}/"),
        SecretString::from("test-token".to_owned()),
        5,
    )
    .expect("client");
    (client, state)
}

fn empty_patch() -> DocumentPatch {
    DocumentPatch {
        content: None,
        title: None,
        tags: None,
        correspondent: None,
        document_type: None,
        created: None,
        custom_fields: None,
    }
}

#[test]
fn pending_objects_are_capped_sanitized_and_refuse_workflow_tags() {
    let workflow = WorkflowTags::default();
    let mut names = vec![
        "  Neu ".to_owned(),
        "neu".to_owned(),
        "AI-PROCESS".to_owned(),
        "x".repeat(129),
        "bad\nname".to_owned(),
        String::new(),
    ];
    names.extend((0..20).map(|index| format!("spam-{index}")));
    let pending = PendingNewObjects::new(names, Some("  Brack AG ".to_owned()), &workflow);
    assert_eq!(pending.tags.len(), MAX_NEW_TAGS_PER_DOCUMENT);
    assert_eq!(pending.tags[0], "Neu");
    assert!(
        !pending
            .tags
            .iter()
            .any(|tag| tag.eq_ignore_ascii_case("neu") && tag != "Neu")
    );
    assert!(!pending.tags.iter().any(|tag| workflow.is_workflow_tag(tag)));
    assert_eq!(pending.correspondent.as_deref(), Some("Brack AG"));

    let too_long = PendingNewObjects::new(Vec::new(), Some("y".repeat(200)), &workflow);
    assert!(too_long.is_empty());

    // Round-trip through a review patch value; DocumentPatch ignores the key.
    let mut patch = json!({"tags": [1], "standard_metadata": {"field": "tags"}});
    pending.attach_to(&mut patch);
    assert!(patch.get(PENDING_NEW_OBJECTS_KEY).is_some());
    assert_eq!(
        PendingNewObjects::from_patch_value(&patch, &workflow),
        pending
    );
    let parsed: DocumentPatch = serde_json::from_value(patch).expect("patch still parses");
    assert_eq!(parsed.tags, Some(vec![1]));

    // An edited patch that smuggles a workflow tag in is re-sanitized.
    let edited = json!({ PENDING_NEW_OBJECTS_KEY: {"tags": ["ai-process", "Neu"]} });
    assert_eq!(
        PendingNewObjects::from_patch_value(&edited, &workflow).tags,
        vec!["Neu".to_owned()]
    );
}

#[tokio::test]
async fn objects_are_created_only_at_apply_and_only_when_allowed() {
    let workflow = WorkflowTags::default();
    let (client, state) = mock_client().await;
    let pending = PendingNewObjects {
        tags: vec![
            "rechnung".to_owned(),
            "Neu".to_owned(),
            "ai-process".to_owned(),
        ],
        correspondent: Some("Brack AG".to_owned()),
    };

    // Policy off: nothing is created, the patch is untouched.
    let mut patch = empty_patch();
    let ids = materialize_pending_new_objects(
        &client,
        &pending,
        NewObjectPolicy {
            allow_new_tags: false,
            allow_new_correspondents: false,
            workflow_tags: &workflow,
        },
        &mut patch,
    )
    .await
    .expect("materialize with policy off");
    assert!(ids.is_empty());
    assert!(patch.correspondent.is_none());
    {
        let state = state.lock().await;
        assert!(state.created_tags.is_empty());
        assert!(state.created_correspondents.is_empty());
    }

    // Policy on: existing names are reused, only missing ones are created,
    // the workflow tag is never attached.
    let mut patch = empty_patch();
    let ids = materialize_pending_new_objects(
        &client,
        &pending,
        NewObjectPolicy {
            allow_new_tags: true,
            allow_new_correspondents: true,
            workflow_tags: &workflow,
        },
        &mut patch,
    )
    .await
    .expect("materialize with policy on");
    let state = state.lock().await;
    assert_eq!(state.created_tags, vec!["Neu".to_owned()]);
    assert_eq!(state.created_correspondents, vec!["Brack AG".to_owned()]);
    assert!(ids.contains(&1));
    assert!(!ids.contains(&2), "workflow tag must never be attached");
    assert_eq!(ids.len(), 2);
    assert_eq!(patch.correspondent, Some(Some(500)));
}
