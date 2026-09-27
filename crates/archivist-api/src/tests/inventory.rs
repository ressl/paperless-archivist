//! #447: inventory filters, saved views and export tests.

use crate::test_support::*;
use crate::*;

// ----- #447 inventory filters / views / export, #448 audit log --------

#[test]
fn inventory_id_filters_accept_ids_and_none_only() {
    let filter = parse_inventory_id_filter("correspondent", Some("7, none,7,12".to_owned()))
        .expect("valid filter");
    assert_eq!(filter.ids, vec![7, 12]);
    assert!(filter.include_none);
    assert!(
        parse_inventory_id_filter("correspondent", None)
            .unwrap()
            .is_empty()
    );
    for bad in ["abc", "0", "-3", "7;8"] {
        let error =
            parse_inventory_id_filter("document_type", Some(bad.to_owned())).expect_err(bad);
        assert_eq!(error.status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let too_many = (1..=101)
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");
    assert!(parse_inventory_id_filter("correspondent", Some(too_many)).is_err());
}

#[test]
fn inventory_filter_queries_are_validated_and_canonicalised() {
    let (canonical, query) =
        canonical_inventory_filter_query("?tag=inbox&q=&ocr_status=failed&correspondent=none")
            .expect("valid");
    assert_eq!(canonical, "ocr_status=failed&tag=inbox&correspondent=none");
    assert_eq!(query.tags_include, vec!["inbox".to_owned()]);
    assert!(query.correspondent.include_none);
    assert_eq!(canonical_inventory_filter_query("").unwrap().0, "");
    for bad in [
        "limit=10",
        "unknown=1",
        "tag=a&tag=b",
        "date_from=27.09.2026",
        "has_error=maybe",
        "document_type=x",
    ] {
        let error = canonical_inventory_filter_query(bad).expect_err(bad);
        assert_eq!(error.status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let long = format!("q={}", "x".repeat(INVENTORY_FILTER_QUERY_MAX_BYTES));
    assert!(canonical_inventory_filter_query(&long).is_err());

    let (format, canonical, _) =
        parse_inventory_export_query("format=json&document_type=3").expect("export query");
    assert_eq!(format, InventoryExportFormat::Json);
    assert_eq!(canonical, "document_type=3");
    assert_eq!(
        parse_inventory_export_query("").unwrap().0,
        InventoryExportFormat::Csv
    );
    assert!(parse_inventory_export_query("format=xml").is_err());
    assert!(parse_inventory_export_query("offset=5").is_err());

    let view = validate_inventory_view(&InventoryViewRequest {
        name: "  Inbox  ".to_owned(),
        query: "tag=inbox".to_owned(),
    })
    .expect("valid view");
    assert_eq!(view, ("Inbox".to_owned(), "tag=inbox".to_owned()));
    for name in ["", "   ", "tab\tname", &"n".repeat(81)] {
        assert!(
            validate_inventory_view(&InventoryViewRequest {
                name: name.to_owned(),
                query: String::new(),
            })
            .is_err(),
            "{name:?}"
        );
    }
}

#[test]
fn inventory_csv_rows_guard_formulas_and_escape() {
    let item = DocumentInventoryItem {
        paperless_document_id: 5,
        title: Some("=HYPERLINK(\"x\")".to_owned()),
        original_file_name: Some("a,b.pdf".to_owned()),
        current_tags: vec!["inbox".to_owned(), "tax".to_owned()],
        ocr_status: "succeeded".to_owned(),
        metadata_status: "queued".to_owned(),
        current_run_status: None,
        last_run_id: None,
        last_error: None,
        next_required_stage: None,
        needs_review: false,
        complete: false,
        document_date: chrono::NaiveDate::from_ymd_opt(2026, 9, 27),
        detected_language: None,
        detected_language_confidence: None,
        detected_language_source: None,
        last_seen_at: Utc::now(),
        correspondent_id: Some(7),
        correspondent_name: Some("ACME".to_owned()),
        document_type_id: None,
        document_type_name: None,
    };
    let row = inventory_csv_row(&item);
    assert!(
        row.starts_with("5,\"'=HYPERLINK(\"\"x\"\")\",\"a,b.pdf\",7,ACME,,,2026-09-27,inbox; tax,")
    );
    assert!(row.ends_with('\n'));
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn inventory_filters_views_and_export_through_the_router() {
    // #447
    let Some(state) = security_db_state().await else {
        return;
    };
    let pool = state.pool.clone();
    sqlx::query(
        "delete from document_inventory where paperless_document_id between 447000 and 447999",
    )
    .execute(&pool)
    .await
    .expect("clear fixtures");
    sqlx::query(
        r#"
            insert into paperless_correspondents (id, name) values (44701, 'ACME Bank')
              on conflict (id) do update set name = excluded.name;
            "#,
    )
    .execute(&pool)
    .await
    .expect("correspondent");
    sqlx::query(
        r#"
            insert into document_inventory (paperless_document_id, title, correspondent_id)
            values (447001, '=SUM(A1)', 44701), (447002, 'Letter', 44701), (447003, 'Other', null)
            "#,
    )
    .execute(&pool)
    .await
    .expect("inventory fixtures");
    let (viewer, viewer_session) = harness_user(&pool, "inv-viewer", &[Role::Viewer]).await;
    let (_, other_session) = harness_user(&pool, "inv-other", &[Role::Viewer]).await;
    let token = harness_token(&pool, viewer, &["inventory:read"]).await;
    let api = TestApi::new(state);

    let list = api
        .send(
            Method::GET,
            "/api/inventory?correspondent=44701",
            &viewer_session,
            None,
        )
        .await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.body);
    assert_eq!(list.body["total"], 2);
    assert_eq!(list.body["items"][0]["correspondent_name"], "ACME Bank");
    let rejected = api
        .send(
            Method::GET,
            "/api/inventory?correspondent=acme",
            &viewer_session,
            None,
        )
        .await;
    assert_eq!(rejected.status, StatusCode::BAD_REQUEST);

    // CSV: header + the two matching rows, formula neutralised.
    let csv = api
        .send(
            Method::GET,
            "/api/inventory/export?correspondent=44701",
            &viewer_session,
            None,
        )
        .await;
    assert_eq!(csv.status, StatusCode::OK);
    assert!(
        csv.headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/csv")
    );
    let text = csv.body.as_str().expect("csv text");
    assert_eq!(text.lines().count(), 3, "{text}");
    assert!(text.contains("447001,'=SUM(A1),"), "{text}");
    assert!(!text.contains("447003"));

    // JSON via an API token with inventory:read.
    let json_export = api
        .send(
            Method::GET,
            "/api/inventory/export?format=json&correspondent=none&q=Other",
            &token,
            None,
        )
        .await;
    assert_eq!(json_export.status, StatusCode::OK);
    let rows = json_export.body.as_array().expect("json array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["paperless_document_id"], 447003);
    let bad_format = api
        .send(
            Method::GET,
            "/api/inventory/export?format=xlsx",
            &token,
            None,
        )
        .await;
    assert_eq!(bad_format.status, StatusCode::BAD_REQUEST);

    let exported: Vec<Value> = sqlx::query_scalar(
        "select metadata from audit_events where event_type = 'inventory.exported' order by created_at",
    )
    .fetch_all(&pool)
    .await
    .expect("export audit");
    assert_eq!(
        exported,
        vec![
            json!({ "format": "csv", "filters": "correspondent=44701" }),
            json!({ "format": "json", "filters": "q=Other&correspondent=none" }),
        ]
    );

    // Saved views: canonicalised, private, unique per user.
    let created = api
        .send(
            Method::POST,
            "/api/inventory/views",
            &viewer_session,
            Some(json!({ "name": "ACME", "query": "?correspondent=44701&ocr_status=failed" })),
        )
        .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    assert_eq!(
        created.body["query"],
        "ocr_status=failed&correspondent=44701"
    );
    let view_id = created.body["id"].as_str().unwrap().to_owned();
    let duplicate = api
        .send(
            Method::POST,
            "/api/inventory/views",
            &viewer_session,
            Some(json!({ "name": "acme", "query": "" })),
        )
        .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT);
    let invalid = api
        .send(
            Method::POST,
            "/api/inventory/views",
            &viewer_session,
            Some(json!({ "name": "Bad", "query": "limit=5" })),
        )
        .await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    let others = api
        .send(Method::GET, "/api/inventory/views", &other_session, None)
        .await;
    assert_eq!(others.body["items"], json!([]));
    let foreign = api
        .send(
            Method::DELETE,
            &format!("/api/inventory/views/{view_id}"),
            &other_session,
            None,
        )
        .await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND);
    let renamed = api
        .send(
            Method::PUT,
            &format!("/api/inventory/views/{view_id}"),
            &viewer_session,
            Some(json!({ "name": "ACME failures", "query": "correspondent=44701" })),
        )
        .await;
    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.body);
    let listed = api
        .send(Method::GET, "/api/inventory/views", &viewer_session, None)
        .await;
    assert_eq!(listed.body["items"][0]["name"], "ACME failures");
    let deleted = api
        .send(
            Method::DELETE,
            &format!("/api/inventory/views/{view_id}"),
            &viewer_session,
            None,
        )
        .await;
    assert_eq!(deleted.status, StatusCode::OK);
    let token_views = api
        .send(Method::GET, "/api/inventory/views", &token, None)
        .await;
    assert_eq!(token_views.status, StatusCode::FORBIDDEN);

    sqlx::query(
        "delete from document_inventory where paperless_document_id between 447000 and 447999",
    )
    .execute(&pool)
    .await
    .expect("clean fixtures");
}
