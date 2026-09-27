//! Paperless consistency, completion-tag reconciliation and login bridge tests.

use crate::test_support::*;
use crate::*;

#[test]
fn completion_tag_reconcile_audit_records_applied_documents_on_partial_failure() {
    // #410: documents already tagged in Paperless must be audited even
    // when a later document fails.
    let auth = auth_context_for_session_listing(true, Uuid::new_v4(), vec![], vec![]);
    let error = anyhow!("Paperless returned 502");
    let audit = completion_tags_reconciled_audit(&auth, false, 3, &[7, 9], Some(&error));
    assert_eq!(audit.outcome, "partial_failure");
    assert_eq!(
        audit.error_message.as_deref(),
        Some("Paperless returned 502")
    );
    let after = audit.after.expect("after");
    assert_eq!(after["applied"], 2);
    assert_eq!(after["applied_document_ids"], json!([7, 9]));
    let audit = completion_tags_reconciled_audit(&auth, false, 1, &[7], None);
    assert_eq!(audit.outcome, "success");
    assert!(audit.error_message.is_none());
}

#[test]
fn completion_tag_reconcile_requires_all_stage_tags_and_missing_full_tag() {
    let stage_tags = vec!["archivist-ocr".to_owned(), "archivist-tags".to_owned()];
    let document_tags = vec!["Archivist-OCR".to_owned(), "archivist-tags".to_owned()];
    assert!(completion_tag_reconcile_needed(
        &document_tags,
        &stage_tags,
        "ai-processed",
        false,
    ));

    let already_complete = vec![
        "archivist-ocr".to_owned(),
        "archivist-tags".to_owned(),
        "AI-PROCESSED".to_owned(),
    ];
    assert!(!completion_tag_reconcile_needed(
        &already_complete,
        &stage_tags,
        "ai-processed",
        true,
    ));

    let missing_stage = vec!["archivist-ocr".to_owned()];
    assert!(!completion_tag_reconcile_needed(
        &missing_stage,
        &stage_tags,
        "ai-processed",
        false,
    ));
    assert!(completion_tag_reconcile_needed(
        &missing_stage,
        &stage_tags,
        "ai-processed",
        true,
    ));

    assert!(!completion_tag_reconcile_needed(
        &document_tags,
        &[],
        "ai-processed",
        true,
    ));
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn consistency_inventory_decodes_typed_document_dates() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = connect(&database_url, 10)
        .await
        .expect("connect consistency test database");
    migrate(&pool).await.expect("apply migrations");
    sqlx::query("delete from document_inventory where paperless_document_id in (386001, 386002)")
        .execute(&pool)
        .await
        .expect("clear consistency fixtures");
    sqlx::query(
        r#"
            insert into document_inventory (
              paperless_document_id, title, current_tag_ids, correspondent_id,
              document_type_id, document_date
            ) values
              (386001, 'Rechnung', '{3,1}', 7, 9, date '2026-09-27'),
              (386002, null, '{}', null, null, null)
            "#,
    )
    .execute(&pool)
    .await
    .expect("insert consistency fixtures");

    let inventory = load_consistency_inventory(&pool)
        .await
        .expect("typed document_date must decode (#386)");
    sqlx::query("delete from document_inventory where paperless_document_id in (386001, 386002)")
        .execute(&pool)
        .await
        .expect("clean up consistency fixtures");

    let dated = inventory.get(&386001).expect("dated row");
    assert_eq!(
        dated.document_date,
        chrono::NaiveDate::from_ymd_opt(2026, 9, 27)
    );
    assert_eq!(dated.current_tag_ids, vec![3, 1]);
    assert_eq!(dated.correspondent, Some(7));
    assert_eq!(
        dated.document_date,
        archivist_db::parse_paperless_document_date(Some("2026-09-27T00:00:00+02:00"))
    );
    assert_eq!(
        inventory.get(&386002).expect("undated row").document_date,
        None
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn paperless_bridge_requires_origin_mapping_and_is_concurrency_safe() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = connect(&database_url, 10)
        .await
        .expect("connect identity test database");
    migrate(&pool).await.expect("apply identity migration");
    sqlx::query("delete from users")
        .execute(&pool)
        .await
        .expect("delete identity fixtures");
    sqlx::query("truncate audit_events restart identity")
        .execute(&pool)
        .await
        .expect("truncate audit fixtures");

    let local_id = create_user_with_roles(
        &pool,
        "paperless-alice",
        Some("local@example.com"),
        "local-hash",
        &[Role::Admin],
        None,
    )
    .await
    .expect("create prefixed local account");
    let paperless_instance = Url::parse("https://paperless-a.example/api/").unwrap();
    let alice_subject = paperless_user_subject(&paperless_instance, 101);
    let bridge = find_or_create_paperless_bridge_user(
        &pool,
        "paperless-alice",
        &alice_subject,
        "disabled-hash",
    )
    .await
    .expect("allocate a distinct bridge-owned account");
    assert_ne!(bridge.id, local_id);
    assert_ne!(bridge.username, "paperless-alice");
    assert_eq!(
        find_user_for_login(&pool, "paperless-alice")
            .await
            .unwrap()
            .unwrap()
            .id,
        local_id,
        "generic local login still resolves the unrelated local owner"
    );
    assert!(
        find_paperless_bridge_user(&pool, &alice_subject)
            .await
            .expect("lookup bridge mapping")
            .is_some_and(|user| user.id == bridge.id),
        "only the verified Paperless subject may resolve the bridge account"
    );
    sqlx::query("update users set enabled = false where id = $1")
        .bind(bridge.id)
        .execute(&pool)
        .await
        .expect("disable bridge account");
    let after_token_rotation = find_or_create_paperless_bridge_user(
        &pool,
        "paperless-renamed-alice",
        &alice_subject,
        "different-disabled-hash",
    )
    .await
    .expect("stable Paperless user ID resolves after token rotation");
    assert_eq!(after_token_rotation.id, bridge.id);
    assert!(!after_token_rotation.enabled);
    let alice_accounts: i64 = sqlx::query_scalar(
        "select count(*)::bigint from users where external_auth_provider = 'paperless_bridge' and external_subject = $1",
    )
    .bind(&alice_subject)
    .fetch_one(&pool)
    .await
    .expect("count stable bridge mapping");
    assert_eq!(alice_accounts, 1);

    sqlx::query("delete from users")
        .execute(&pool)
        .await
        .expect("reset identity fixtures");
    sqlx::query("truncate audit_events restart identity")
        .execute(&pool)
        .await
        .expect("reset audit fixtures");
    let pool_a = connect(&database_url, 2).await.expect("connect writer A");
    let pool_b = connect(&database_url, 2).await.expect("connect writer B");
    let concurrent_subject = paperless_user_subject(&paperless_instance, 202);
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let writer_a = {
        let barrier = Arc::clone(&barrier);
        let concurrent_subject = concurrent_subject.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            find_or_create_paperless_bridge_user(
                &pool_a,
                "paperless-concurrent",
                &concurrent_subject,
                "disabled-hash-a",
            )
            .await
        })
    };
    let writer_b = {
        let barrier = Arc::clone(&barrier);
        let concurrent_subject = concurrent_subject.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            find_or_create_paperless_bridge_user(
                &pool_b,
                " PAPERLESS-CONCURRENT ",
                &concurrent_subject,
                "disabled-hash-b",
            )
            .await
        })
    };
    barrier.wait().await;

    let user_a = writer_a.await.unwrap().expect("writer A resolves user");
    let user_b = writer_b.await.unwrap().expect("writer B resolves user");
    assert_eq!(user_a.id, user_b.id);
    let users: i64 = sqlx::query_scalar("select count(*)::bigint from users")
        .fetch_one(&pool)
        .await
        .expect("count bridge users");
    assert_eq!(users, 1);
    let mapping =
        sqlx::query("select external_auth_provider, external_subject from users where id = $1")
            .bind(user_a.id)
            .fetch_one(&pool)
            .await
            .expect("read bridge origin mapping");
    assert_eq!(
        mapping
            .try_get::<Option<String>, _>("external_auth_provider")
            .unwrap()
            .as_deref(),
        Some("paperless_bridge")
    );
    assert_eq!(
        mapping
            .try_get::<Option<String>, _>("external_subject")
            .unwrap()
            .as_deref(),
        Some(concurrent_subject.as_str())
    );

    let plus_subject = paperless_user_subject(&paperless_instance, 303);
    let first = find_or_create_paperless_bridge_user(
        &pool,
        "paperless-alice-ops",
        &plus_subject,
        "disabled-hash-c",
    )
    .await
    .expect("create first lossy-name bridge account");
    let dash_subject = paperless_user_subject(&paperless_instance, 304);
    let second = find_or_create_paperless_bridge_user(
        &pool,
        "paperless-alice-ops",
        &dash_subject,
        "disabled-hash-d",
    )
    .await
    .expect("create second lossy-name bridge account");
    assert_ne!(first.id, second.id);
    assert_ne!(first.username, second.username);
    assert_eq!(
        find_paperless_bridge_user(&pool, &plus_subject)
            .await
            .unwrap()
            .unwrap()
            .id,
        first.id
    );
    assert_eq!(
        find_paperless_bridge_user(&pool, &dash_subject)
            .await
            .unwrap()
            .unwrap()
            .id,
        second.id
    );
}
