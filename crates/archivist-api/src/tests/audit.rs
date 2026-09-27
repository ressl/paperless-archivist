//! Audit log cursor, CSV and router-level audit tests.

use crate::test_support::*;
use crate::*;

#[test]
fn csv_export_escapes_special_characters() {
    assert_eq!(csv_escape("plain"), "plain");
    assert_eq!(csv_escape("a,b"), "\"a,b\"");
    assert_eq!(csv_escape("a\"b"), "\"a\"\"b\"");
}

#[test]
fn audit_cursor_and_time_bounds_round_trip() {
    let created_at = Utc::now();
    let id = Uuid::now_v7();
    let cursor = encode_audit_cursor(created_at, id);
    let (decoded_at, decoded_id) = decode_audit_cursor(&cursor).expect("decode");
    assert_eq!(decoded_id, id);
    assert_eq!(decoded_at.timestamp_micros(), created_at.timestamp_micros());
    for bad in ["", "not base64!", "Zm9v"] {
        assert_eq!(
            decode_audit_cursor(bad).expect_err(bad).status,
            StatusCode::BAD_REQUEST
        );
    }
    let from = parse_audit_time_bound("from", Some("2026-09-27"), false)
        .unwrap()
        .unwrap();
    let to = parse_audit_time_bound("to", Some("2026-09-27"), true)
        .unwrap()
        .unwrap();
    assert_eq!(to - from, Duration::days(1));
    assert_eq!(
        parse_audit_time_bound("to", Some("2026-09-27T10:00:00+02:00"), true)
            .unwrap()
            .unwrap()
            .to_rfc3339(),
        "2026-09-27T08:00:00+00:00"
    );
    assert!(parse_audit_time_bound("from", Some("yesterday"), false).is_err());
    assert!(
        parse_audit_time_bound("from", Some("  "), false)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn audit_log_filters_pages_and_redacts_details_through_the_router() {
    // #448
    let Some(state) = security_db_state().await else {
        return;
    };
    let pool = state.pool.clone();
    let (auditor, auditor_session) = harness_user(&pool, "auditor", &[Role::Auditor]).await;
    let (_, viewer_session) = harness_user(&pool, "audit-viewer", &[Role::Viewer]).await;
    for index in 0..3 {
        append_audit(
            &pool,
            AuditEventInput {
                event_type: "document.patch_confirmed".to_owned(),
                actor_type: "user".to_owned(),
                actor_id: Some(auditor.to_string()),
                run_id: None,
                job_id: None,
                paperless_document_id: Some(448000 + index),
                before: Some(json!({ "title": "old", "password": "hunter2" })),
                after: Some(json!({ "title": format!("new {index}") })),
                metadata: None,
                outcome: "success".to_owned(),
                error_message: None,
                source_ip: None,
                user_agent: None,
            },
        )
        .await
        .expect("fixture");
    }
    let api = TestApi::new(state);

    let denied = api
        .send(
            Method::GET,
            "/api/audit?document_id=448001",
            &viewer_session,
            None,
        )
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);

    let one = api
        .send(
            Method::GET,
            "/api/audit?document_id=448001",
            &auditor_session,
            None,
        )
        .await;
    assert_eq!(one.status, StatusCode::OK, "{}", one.body);
    assert_eq!(one.body["items"].as_array().unwrap().len(), 1);
    assert_eq!(one.body["items"][0]["has_changes"], true);
    assert_eq!(one.body["next_cursor"], Value::Null);

    // Two pages of the actor's events.
    let first = api
        .send(
            Method::GET,
            &format!("/api/audit?actor={auditor}&event_type=document.patch_confirmed&limit=2"),
            &auditor_session,
            None,
        )
        .await;
    assert_eq!(first.body["items"].as_array().unwrap().len(), 2);
    let cursor = first.body["next_cursor"]
        .as_str()
        .expect("cursor")
        .to_owned();
    let second = api
        .send(
            Method::GET,
            &format!(
                "/api/audit?actor={auditor}&event_type=document.patch_confirmed&limit=2&cursor={cursor}"
            ),
            &auditor_session,
            None,
        )
        .await;
    let second_items = second.body["items"].as_array().unwrap();
    assert_eq!(second_items.len(), 1);
    assert_eq!(second_items[0]["paperless_document_id"], 448000);
    assert_eq!(second.body["next_cursor"], Value::Null);

    let bad_cursor = api
        .send(
            Method::GET,
            "/api/audit?cursor=nope",
            &auditor_session,
            None,
        )
        .await;
    assert_eq!(bad_cursor.status, StatusCode::BAD_REQUEST);
    let bad_from = api
        .send(Method::GET, "/api/audit?from=soon", &auditor_session, None)
        .await;
    assert_eq!(bad_from.status, StatusCode::BAD_REQUEST);

    let id = one.body["items"][0]["id"].as_str().unwrap().to_owned();
    let detail = api
        .send(
            Method::GET,
            &format!("/api/audit/{id}"),
            &auditor_session,
            None,
        )
        .await;
    assert_eq!(detail.status, StatusCode::OK, "{}", detail.body);
    assert_eq!(detail.body["before"]["title"], "old");
    assert_eq!(detail.body["before"]["password"], "[REDACTED]");
    assert_eq!(detail.body["after"]["title"], "new 1");
    let missing = api
        .send(
            Method::GET,
            &format!("/api/audit/{}", Uuid::now_v7()),
            &auditor_session,
            None,
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}
