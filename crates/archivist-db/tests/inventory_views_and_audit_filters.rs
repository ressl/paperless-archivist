//! DB-required integration tests for the inventory filters / saved views /
//! export paging (#447) and the filtered, keyset-paginated audit log (#448).
//!
//! Run with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored --test-threads=1`.

use archivist_core::{AuditEventInput, Role};
use archivist_db::{
    AuditEventFilter, DbPool, INVENTORY_SAVED_VIEW_LIMIT, InventoryIdFilter, InventoryQuery,
    InventoryViewError, append_audit, audit_events_query_builder, connect, count_inventory,
    create_inventory_view, create_user_with_roles, delete_inventory_view, find_user_id_by_username,
    get_audit_event, list_audit_events, list_inventory, list_inventory_keyset,
    list_inventory_views, migrate, update_inventory_view,
};
use chrono::{Duration, TimeZone, Utc};
use serde_json::json;
use sqlx::{Executor, Row};
use tokio::sync::{Mutex, MutexGuard};
use uuid::Uuid;

static DB_TABLE_LOCK: Mutex<()> = Mutex::const_new(());

async fn fresh_pool() -> Option<(MutexGuard<'static, ()>, DbPool)> {
    let guard = DB_TABLE_LOCK.lock().await;
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        r#"truncate document_inventory, paperless_correspondents, paperless_document_types,
                    inventory_saved_views, audit_events restart identity cascade;"#,
    )
    .await
    .expect("truncate test tables");
    Some((guard, pool))
}

async fn seed_inventory(pool: &DbPool, id: i32, correspondent: Option<i32>, doc_type: Option<i32>) {
    sqlx::query(
        "insert into document_inventory (paperless_document_id, title, correspondent_id, document_type_id) \
         values ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(format!("doc {id}"))
    .bind(correspondent)
    .bind(doc_type)
    .execute(pool)
    .await
    .expect("seed inventory row");
}

fn ids(items: &[archivist_core::DocumentInventoryItem]) -> Vec<i32> {
    items
        .iter()
        .map(|item| item.paperless_document_id)
        .collect()
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn inventory_filters_by_correspondent_and_document_type_with_names() {
    // #447
    let Some((_guard, pool)) = fresh_pool().await else {
        return;
    };
    pool.execute(
        "insert into paperless_correspondents (id, name) values (7, 'ACME Bank'), (8, 'Zeta Insurance'); \
         insert into paperless_document_types (id, name) values (3, 'Invoice'), (4, 'Letter');",
    )
    .await
    .expect("seed vocabularies");
    seed_inventory(&pool, 1, Some(7), Some(3)).await;
    seed_inventory(&pool, 2, Some(8), Some(3)).await;
    seed_inventory(&pool, 3, None, Some(4)).await;
    seed_inventory(&pool, 4, Some(7), None).await;
    // Correspondent id that was never synced: filterable, name stays null.
    seed_inventory(&pool, 5, Some(99), None).await;

    let by_correspondent = InventoryQuery {
        correspondent: InventoryIdFilter {
            ids: vec![7],
            include_none: false,
        },
        ..InventoryQuery::default()
    };
    let rows = list_inventory(&pool, &by_correspondent, 100, 0)
        .await
        .expect("list");
    assert_eq!(ids(&rows), vec![4, 1]);
    assert_eq!(rows[1].correspondent_name.as_deref(), Some("ACME Bank"));
    assert_eq!(rows[1].document_type_name.as_deref(), Some("Invoice"));
    assert_eq!(count_inventory(&pool, &by_correspondent).await.unwrap(), 2);

    let without_or_zeta = InventoryQuery {
        correspondent: InventoryIdFilter {
            ids: vec![8],
            include_none: true,
        },
        ..InventoryQuery::default()
    };
    let rows = list_inventory(&pool, &without_or_zeta, 100, 0)
        .await
        .expect("list");
    assert_eq!(ids(&rows), vec![3, 2]);

    let invoices_without_type = InventoryQuery {
        document_type: InventoryIdFilter {
            ids: vec![],
            include_none: true,
        },
        ..InventoryQuery::default()
    };
    let rows = list_inventory(&pool, &invoices_without_type, 100, 0)
        .await
        .expect("list");
    assert_eq!(ids(&rows), vec![5, 4]);
    assert_eq!(rows[0].correspondent_id, Some(99));
    assert_eq!(rows[0].correspondent_name, None);

    let combined = InventoryQuery {
        correspondent: InventoryIdFilter {
            ids: vec![7, 8],
            include_none: false,
        },
        document_type: InventoryIdFilter {
            ids: vec![3],
            include_none: false,
        },
        ..InventoryQuery::default()
    };
    assert_eq!(count_inventory(&pool, &combined).await.unwrap(), 2);

    // Keyset paging (export) walks the same filtered set without overlap.
    let everything = InventoryQuery::default();
    let first = list_inventory_keyset(&pool, &everything, None, 2)
        .await
        .unwrap();
    let second = list_inventory_keyset(&pool, &everything, Some(4), 2)
        .await
        .unwrap();
    let third = list_inventory_keyset(&pool, &everything, Some(2), 2)
        .await
        .unwrap();
    assert_eq!(ids(&first), vec![5, 4]);
    assert_eq!(ids(&second), vec![3, 2]);
    assert_eq!(ids(&third), vec![1]);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn saved_inventory_views_are_private_unique_and_bounded() {
    // #447
    let Some((_guard, pool)) = fresh_pool().await else {
        return;
    };
    let suffix = Uuid::now_v7().simple().to_string();
    let alice = create_user_with_roles(
        &pool,
        &format!("views-a-{suffix}"),
        None,
        "hash",
        &[Role::Viewer],
        None,
    )
    .await
    .expect("alice");
    let bob = create_user_with_roles(
        &pool,
        &format!("views-b-{suffix}"),
        None,
        "hash",
        &[Role::Viewer],
        None,
    )
    .await
    .expect("bob");

    let view = create_inventory_view(&pool, alice, "Failed OCR", "ocr_status=failed")
        .await
        .expect("create");
    // Same name, other case: conflict for Alice, fine for Bob.
    let duplicate = create_inventory_view(&pool, alice, "failed ocr", "ocr_status=failed")
        .await
        .expect_err("duplicate name");
    assert_eq!(
        duplicate.downcast_ref::<InventoryViewError>(),
        Some(&InventoryViewError::DuplicateName)
    );
    create_inventory_view(&pool, bob, "Failed OCR", "has_error=true")
        .await
        .expect("bob's own view");

    assert_eq!(list_inventory_views(&pool, alice).await.unwrap().len(), 1);
    // Bob can neither update nor delete Alice's view.
    let foreign = update_inventory_view(&pool, bob, view.id, "Mine", "q=x")
        .await
        .expect_err("foreign update");
    assert_eq!(
        foreign.downcast_ref::<InventoryViewError>(),
        Some(&InventoryViewError::NotFound)
    );
    let foreign = delete_inventory_view(&pool, bob, view.id)
        .await
        .expect_err("foreign delete");
    assert_eq!(
        foreign.downcast_ref::<InventoryViewError>(),
        Some(&InventoryViewError::NotFound)
    );

    let updated = update_inventory_view(
        &pool,
        alice,
        view.id,
        "OCR failures",
        "ocr_status=failed&tag=inbox",
    )
    .await
    .expect("update");
    assert_eq!(updated.name, "OCR failures");
    assert_eq!(updated.query, "ocr_status=failed&tag=inbox");

    for index in 1..INVENTORY_SAVED_VIEW_LIMIT {
        create_inventory_view(&pool, alice, &format!("view {index}"), "")
            .await
            .expect("within limit");
    }
    let over = create_inventory_view(&pool, alice, "one too many", "")
        .await
        .expect_err("limit");
    assert_eq!(
        over.downcast_ref::<InventoryViewError>(),
        Some(&InventoryViewError::LimitReached)
    );
    delete_inventory_view(&pool, alice, view.id)
        .await
        .expect("delete");
    assert_eq!(
        list_inventory_views(&pool, alice).await.unwrap().len() as i64,
        INVENTORY_SAVED_VIEW_LIMIT - 1
    );
}

fn audit_input(event_type: &str, actor_id: Option<&str>, document: Option<i32>) -> AuditEventInput {
    AuditEventInput {
        event_type: event_type.to_owned(),
        actor_type: if actor_id.is_some() { "user" } else { "worker" }.to_owned(),
        actor_id: actor_id.map(str::to_owned),
        run_id: None,
        job_id: None,
        paperless_document_id: document,
        before: document.map(|_| json!({ "title": "old", "api_key": "sk-secret" })),
        after: document.map(|_| json!({ "title": "new" })),
        metadata: None,
        outcome: "success".to_owned(),
        error_message: None,
        source_ip: None,
        user_agent: None,
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn audit_events_filter_and_keyset_paginate_without_gaps() {
    // #448
    let Some((_guard, pool)) = fresh_pool().await else {
        return;
    };
    let suffix = Uuid::now_v7().simple().to_string();
    let username = format!("Audit-Actor-{suffix}");
    let user = create_user_with_roles(&pool, &username, None, "hash", &[Role::Admin], None)
        .await
        .expect("user");
    let user_id = user.to_string();
    for index in 0..5 {
        append_audit(
            &pool,
            audit_input(
                "document.patch_confirmed",
                Some(&user_id),
                Some(100 + index),
            ),
        )
        .await
        .expect("append");
        append_audit(&pool, audit_input("job.succeeded", None, None))
            .await
            .expect("append");
    }
    // Ten events sharing one timestamp: the id tie-breaker must page them
    // without skipping or repeating a row.
    let tie = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
    for _ in 0..10 {
        sqlx::query(
            "insert into audit_events (event_type, actor_type, created_at, hash_version) values ('test.tie', 'system', $1, null)",
        )
        .bind(tie)
        .execute(&pool)
        .await
        .expect("tie row");
    }

    let resolved = find_user_id_by_username(&pool, &username.to_lowercase())
        .await
        .unwrap();
    assert_eq!(resolved, Some(user));

    let by_actor = AuditEventFilter {
        actor_id: Some(user_id.clone()),
        ..AuditEventFilter::default()
    };
    let rows = list_audit_events(&pool, &by_actor, 100).await.unwrap();
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().all(|row| row.has_changes));
    assert!(
        rows.iter()
            .all(|row| row.actor_username.as_deref() == Some(username.as_str()))
    );

    let by_document = AuditEventFilter {
        paperless_document_id: Some(102),
        ..AuditEventFilter::default()
    };
    let rows = list_audit_events(&pool, &by_document, 100).await.unwrap();
    assert_eq!(rows.len(), 1);
    let detail = get_audit_event(&pool, rows[0].id)
        .await
        .unwrap()
        .expect("detail");
    // append_audit already redacts credentials on write; the API redacts
    // again on read so legacy rows written before that are covered too.
    assert_eq!(
        detail.before,
        Some(json!({ "title": "old", "api_key": "[REDACTED]" }))
    );
    assert_eq!(detail.after, Some(json!({ "title": "new" })));
    assert!(
        get_audit_event(&pool, Uuid::now_v7())
            .await
            .unwrap()
            .is_none()
    );

    let by_types = AuditEventFilter {
        event_types: vec!["job.succeeded".to_owned(), "test.tie".to_owned()],
        ..AuditEventFilter::default()
    };
    assert_eq!(
        list_audit_events(&pool, &by_types, 100)
            .await
            .unwrap()
            .len(),
        15
    );
    let only_jobs = AuditEventFilter {
        event_types: vec!["job.succeeded".to_owned()],
        actor_type: Some("worker".to_owned()),
        ..AuditEventFilter::default()
    };
    let jobs = list_audit_events(&pool, &only_jobs, 100).await.unwrap();
    assert_eq!(jobs.len(), 5);
    assert!(
        jobs.iter()
            .all(|row| !row.has_changes && row.actor_username.is_none())
    );

    let in_range = AuditEventFilter {
        from: Some(tie - Duration::hours(1)),
        to: Some(tie + Duration::seconds(1)),
        ..AuditEventFilter::default()
    };
    assert_eq!(
        list_audit_events(&pool, &in_range, 100)
            .await
            .unwrap()
            .len(),
        10
    );
    let excluded_end = AuditEventFilter {
        to: Some(tie),
        ..AuditEventFilter::default()
    };
    assert!(
        list_audit_events(&pool, &excluded_end, 100)
            .await
            .unwrap()
            .is_empty()
    );

    // Walk everything three rows at a time.
    let mut seen = Vec::new();
    let mut filter = AuditEventFilter::default();
    loop {
        let page = list_audit_events(&pool, &filter, 3).await.unwrap();
        seen.extend(page.iter().map(|row| row.id));
        match page.last() {
            Some(last) if page.len() == 3 => filter.before = Some((last.created_at, last.id)),
            _ => break,
        }
    }
    let all = list_audit_events(&pool, &AuditEventFilter::default(), 100)
        .await
        .unwrap();
    // 5 patch + 5 job + 10 tie fixtures, plus the user.created event of the fixture user.
    assert_eq!(seen.len(), 21);
    assert_eq!(seen, all.iter().map(|row| row.id).collect::<Vec<_>>());
}

/// The audit keyset query for each single-dimension filter is served by an
/// index that also yields the ORDER BY, i.e. the plan has no Sort node.
/// Sequential scans are disabled so the tiny fixture table cannot make the
/// planner prefer a seq scan + sort. #448
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn audit_keyset_queries_use_ordered_indexes() {
    let Some((_guard, pool)) = fresh_pool().await else {
        return;
    };
    for index in 0..50 {
        append_audit(
            &pool,
            audit_input("document.patch_confirmed", Some("fixture"), Some(index)),
        )
        .await
        .expect("append");
    }
    pool.execute("analyze audit_events").await.expect("analyze");
    let cursor = Some((Utc::now(), Uuid::now_v7()));
    let cases = [
        (
            "audit_events_document_keyset_idx",
            AuditEventFilter {
                paperless_document_id: Some(7),
                before: cursor,
                ..AuditEventFilter::default()
            },
        ),
        (
            "audit_events_actor_keyset_idx",
            AuditEventFilter {
                actor_id: Some("fixture".to_owned()),
                before: cursor,
                ..AuditEventFilter::default()
            },
        ),
        (
            "audit_events_type_keyset_idx",
            AuditEventFilter {
                event_types: vec!["document.patch_confirmed".to_owned()],
                before: cursor,
                ..AuditEventFilter::default()
            },
        ),
        (
            "audit_events_created_idx",
            AuditEventFilter {
                before: cursor,
                ..AuditEventFilter::default()
            },
        ),
    ];
    for (index_name, filter) in cases {
        let mut tx = pool.begin().await.expect("begin");
        tx.execute("set local enable_seqscan = off")
            .await
            .expect("disable seqscan");
        let plan: Vec<String> = audit_events_query_builder("explain (costs off) ", &filter, 50)
            .build()
            .fetch_all(&mut *tx)
            .await
            .expect("explain")
            .iter()
            .map(|row| row.get::<String, _>(0))
            .collect();
        let plan = plan.join("\n");
        assert!(plan.contains(index_name), "{index_name}:\n{plan}");
        assert!(
            !plan.contains("Sort"),
            "{index_name} must not sort:\n{plan}"
        );
        tx.rollback().await.expect("rollback");
    }
}
