//! DB-required integration tests for #348: concurrent run creation converges
//! on one active run per Paperless document without global serialization.

use archivist_core::{ProcessingMode, Stage};
use archivist_db::{
    DbPool, backfill_metadata_stage_for_ocr_only_runs, connect, create_run_with_jobs,
    create_runs_for_documents, migrate, requeue_vision_crashed_jobs,
};
use sqlx::{Executor, Row};
use tokio::sync::{Mutex, MutexGuard, oneshot};
use tokio::time::{Duration, sleep, timeout};
use uuid::Uuid;

static DB_TABLE_LOCK: Mutex<()> = Mutex::const_new(());

const INSERT_GATE_NAME: &str = "paperless_archivist_issue_348_insert_gate";
const ACTIVE_RUN_LOCK_NAME: &str = "paperless_archivist_active_run_document";

async fn fresh_pool() -> Option<(MutexGuard<'static, ()>, String, DbPool)> {
    let guard = DB_TABLE_LOCK.lock().await;
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        r#"
        drop trigger if exists issue_348_gate_pipeline_run_insert on pipeline_runs;
        drop function if exists issue_348_gate_pipeline_run_insert();
        truncate document_inventory, jobs, pipeline_runs, audit_events restart identity cascade;
        "#,
    )
    .await
    .expect("reset run tables and test gate");
    Some((guard, url, pool))
}

async fn one_connection_pool(url: &str, application_name: &str) -> DbPool {
    let pool = connect(url, 1).await.expect("connect contender pool");
    sqlx::query("select set_config('application_name', $1, false)")
        .bind(application_name)
        .execute(&pool)
        .await
        .expect("name contender connection");
    pool
}

async fn install_insert_gate(pool: &DbPool) {
    sqlx::query(
        r#"
        create function issue_348_gate_pipeline_run_insert()
        returns trigger
        language plpgsql
        as $gate$
        begin
          perform pg_advisory_xact_lock(
            hashtext('paperless_archivist_issue_348_insert_gate'),
            new.paperless_document_id
          );
          return new;
        end
        $gate$;

        "#,
    )
    .execute(pool)
    .await
    .expect("install deterministic insert-gate function");
    sqlx::query(
        r#"
        create trigger issue_348_gate_pipeline_run_insert
        before insert on pipeline_runs
        for each row execute function issue_348_gate_pipeline_run_insert()
        "#,
    )
    .execute(pool)
    .await
    .expect("install deterministic insert-gate trigger");
}

async fn acquire_document_lock<'a>(
    pool: &'a DbPool,
    lock_name: &str,
    document_id: i32,
) -> sqlx::Transaction<'a, sqlx::Postgres> {
    let mut tx = pool.begin().await.expect("begin lock holder");
    sqlx::query("select pg_advisory_xact_lock(hashtext($1), $2)")
        .bind(lock_name)
        .bind(document_id)
        .execute(&mut *tx)
        .await
        .expect("acquire document lock");
    tx
}

async fn wait_for_issue_348_lock_waiters(pool: &DbPool, minimum: i64) {
    timeout(Duration::from_secs(3), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                r#"
                select count(*)
                  from pg_stat_activity
                 where datname = current_database()
                   and application_name like 'paperless_archivist_issue348_%'
                   and state = 'active'
                   and wait_event_type = 'Lock'
                "#,
            )
            .fetch_one(pool)
            .await
            .expect("inspect PostgreSQL lock waiters");
            if waiting >= minimum {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("expected concurrent operations to reach their lock waits");
}

async fn assert_one_created_run(
    pool: &DbPool,
    document_id: i32,
    expected_stages: &[Stage],
) -> Uuid {
    let runs = sqlx::query("select id from pipeline_runs where paperless_document_id = $1")
        .bind(document_id)
        .fetch_all(pool)
        .await
        .expect("load created runs");
    assert_eq!(runs.len(), 1, "exactly one pipeline run");
    let run_id: Uuid = runs[0].try_get("id").expect("run id");

    let job_rows = sqlx::query(
        "select run_id, stage from jobs where paperless_document_id = $1 order by stage",
    )
    .bind(document_id)
    .fetch_all(pool)
    .await
    .expect("load created jobs");
    assert_eq!(
        job_rows.len(),
        expected_stages.len(),
        "one complete job set"
    );
    let mut actual_stages = Vec::with_capacity(job_rows.len());
    for row in job_rows {
        assert_eq!(row.try_get::<Uuid, _>("run_id").unwrap(), run_id);
        actual_stages.push(row.try_get::<String, _>("stage").unwrap());
    }
    let mut expected_stages = expected_stages
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    expected_stages.sort_unstable();
    assert_eq!(actual_stages, expected_stages);

    let audit_run_ids = sqlx::query_scalar::<_, Option<Uuid>>(
        r#"
        select run_id
          from audit_events
         where paperless_document_id = $1 and event_type = 'run.created'
        "#,
    )
    .bind(document_id)
    .fetch_all(pool)
    .await
    .expect("load run-created audits");
    assert_eq!(
        audit_run_ids,
        vec![Some(run_id)],
        "one matching audit event"
    );
    run_id
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn concurrent_single_creates_return_the_same_run_without_duplicate_side_effects() {
    let Some((_guard, url, pool)) = fresh_pool().await else {
        return;
    };
    install_insert_gate(&pool).await;
    let document_id = 5_801;
    let gate = acquire_document_lock(&pool, INSERT_GATE_NAME, document_id).await;
    let first_pool = one_connection_pool(&url, "paperless_archivist_issue348_single_a").await;
    let second_pool = one_connection_pool(&url, "paperless_archivist_issue348_single_b").await;

    let first = tokio::spawn(async move {
        create_run_with_jobs(
            &first_pool,
            document_id,
            &[Stage::Ocr, Stage::Metadata],
            ProcessingMode::ManualReview,
            "test-single-a",
            "test",
        )
        .await
    });
    let second = tokio::spawn(async move {
        create_run_with_jobs(
            &second_pool,
            document_id,
            &[Stage::Ocr, Stage::Metadata],
            ProcessingMode::ManualReview,
            "test-single-b",
            "test",
        )
        .await
    });

    wait_for_issue_348_lock_waiters(&pool, 2).await;
    gate.commit().await.expect("release insert gate");
    let first_id = timeout(Duration::from_secs(5), first)
        .await
        .expect("first create completes")
        .expect("first create task")
        .expect("first create succeeds");
    let second_id = timeout(Duration::from_secs(5), second)
        .await
        .expect("second create completes")
        .expect("second create task")
        .expect("second create succeeds");

    assert_eq!(first_id, second_id);
    assert_eq!(
        assert_one_created_run(&pool, document_id, &[Stage::Ocr, Stage::Metadata]).await,
        first_id
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn concurrent_single_and_bulk_create_share_get_or_create_semantics() {
    let Some((_guard, url, pool)) = fresh_pool().await else {
        return;
    };
    install_insert_gate(&pool).await;
    let document_id = 5_802;
    let gate = acquire_document_lock(&pool, INSERT_GATE_NAME, document_id).await;
    let single_pool = one_connection_pool(&url, "paperless_archivist_issue348_mixed_single").await;
    let bulk_pool = one_connection_pool(&url, "paperless_archivist_issue348_mixed_bulk").await;

    let single = tokio::spawn(async move {
        create_run_with_jobs(
            &single_pool,
            document_id,
            &[Stage::Ocr, Stage::Metadata],
            ProcessingMode::ManualReview,
            "test-single",
            "test",
        )
        .await
    });
    let bulk = tokio::spawn(async move {
        create_runs_for_documents(
            &bulk_pool,
            &[document_id],
            &[Stage::Ocr, Stage::Metadata],
            ProcessingMode::ManualReview,
            "test-bulk",
            "test",
            None,
        )
        .await
    });

    wait_for_issue_348_lock_waiters(&pool, 2).await;
    gate.commit().await.expect("release insert gate");
    let single_id = timeout(Duration::from_secs(5), single)
        .await
        .expect("single create completes")
        .expect("single create task")
        .expect("single create succeeds");
    let bulk_count = timeout(Duration::from_secs(5), bulk)
        .await
        .expect("bulk create completes")
        .expect("bulk create task")
        .expect("bulk create succeeds");

    assert_eq!(bulk_count, 1);
    assert_eq!(
        assert_one_created_run(&pool, document_id, &[Stage::Ocr, Stage::Metadata]).await,
        single_id
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn overlapping_bulk_inputs_use_one_deadlock_free_lock_order() {
    let Some((_guard, url, pool)) = fresh_pool().await else {
        return;
    };
    let lower_document = 5_805;
    let higher_document = 5_806;
    let blocker = acquire_document_lock(&pool, ACTIVE_RUN_LOCK_NAME, lower_document).await;
    let forward_pool = one_connection_pool(&url, "paperless_archivist_issue348_bulk_forward").await;
    let reverse_pool = one_connection_pool(&url, "paperless_archivist_issue348_bulk_reverse").await;

    let forward = tokio::spawn(async move {
        create_runs_for_documents(
            &forward_pool,
            &[lower_document, higher_document],
            &[Stage::Ocr],
            ProcessingMode::ManualReview,
            "test-forward",
            "test",
            None,
        )
        .await
    });
    let reverse = tokio::spawn(async move {
        create_runs_for_documents(
            &reverse_pool,
            &[higher_document, lower_document],
            &[Stage::Ocr],
            ProcessingMode::ManualReview,
            "test-reverse",
            "test",
            None,
        )
        .await
    });

    wait_for_issue_348_lock_waiters(&pool, 2).await;
    blocker.commit().await.expect("release first document");
    let forward_count = timeout(Duration::from_secs(5), forward)
        .await
        .expect("forward bulk must not deadlock")
        .expect("forward bulk task")
        .expect("forward bulk succeeds");
    let reverse_count = timeout(Duration::from_secs(5), reverse)
        .await
        .expect("reverse bulk must not deadlock")
        .expect("reverse bulk task")
        .expect("reverse bulk succeeds");

    assert_eq!(forward_count, 2);
    assert_eq!(reverse_count, 2);
    assert_one_created_run(&pool, lower_document, &[Stage::Ocr]).await;
    assert_one_created_run(&pool, higher_document, &[Stage::Ocr]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn create_rechecks_an_active_run_that_concurrently_becomes_terminal() {
    let Some((_guard, url, pool)) = fresh_pool().await else {
        return;
    };
    let document_id = 5_807;
    let old_run_id = create_run_with_jobs(
        &pool,
        document_id,
        &[Stage::Ocr],
        ProcessingMode::ManualReview,
        "test-old-run",
        "test",
    )
    .await
    .expect("create initial active run");

    let mut transition = pool.begin().await.expect("begin terminal transition");
    sqlx::query(
        r#"
        update pipeline_runs
           set status = 'succeeded', finished_at = now(), updated_at = now()
         where id = $1
        "#,
    )
    .bind(old_run_id)
    .execute(&mut *transition)
    .await
    .expect("hold uncommitted terminal transition");

    let contender_pool =
        one_connection_pool(&url, "paperless_archivist_issue348_status_transition").await;
    let create = tokio::spawn(async move {
        create_run_with_jobs(
            &contender_pool,
            document_id,
            &[Stage::Ocr],
            ProcessingMode::ManualReview,
            "test-after-terminal",
            "test",
        )
        .await
    });
    wait_for_issue_348_lock_waiters(&pool, 1).await;
    transition
        .commit()
        .await
        .expect("commit terminal transition");

    let new_run_id = timeout(Duration::from_secs(5), create)
        .await
        .expect("replacement create completes")
        .expect("replacement create task")
        .expect("replacement create succeeds");
    assert_ne!(new_run_id, old_run_id);

    let rows = sqlx::query(
        "select id, status from pipeline_runs where paperless_document_id = $1 order by id",
    )
    .bind(document_id)
    .fetch_all(&pool)
    .await
    .expect("load old and replacement runs");
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| {
        row.try_get::<Uuid, _>("id").unwrap() == old_run_id
            && row.try_get::<String, _>("status").unwrap() == "succeeded"
    }));
    assert!(rows.iter().any(|row| {
        row.try_get::<Uuid, _>("id").unwrap() == new_run_id
            && row.try_get::<String, _>("status").unwrap() == "queued"
    }));
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn terminal_run_reactivation_and_create_share_the_document_lock() {
    let Some((_guard, url, pool)) = fresh_pool().await else {
        return;
    };
    let document_id = 5_808;
    let old_run_id = create_run_with_jobs(
        &pool,
        document_id,
        &[Stage::Ocr],
        ProcessingMode::ManualReview,
        "test-old-ocr-only",
        "test",
    )
    .await
    .expect("create old OCR-only run");
    sqlx::query("update jobs set status = 'succeeded' where run_id = $1")
        .bind(old_run_id)
        .execute(&pool)
        .await
        .expect("settle old OCR job");
    sqlx::query(
        r#"
        update pipeline_runs
           set status = 'succeeded', finished_at = now(), updated_at = now()
         where id = $1
        "#,
    )
    .bind(old_run_id)
    .execute(&pool)
    .await
    .expect("settle old OCR-only run");

    let blocker = acquire_document_lock(&pool, ACTIVE_RUN_LOCK_NAME, document_id).await;
    let backfill_pool =
        one_connection_pool(&url, "paperless_archivist_issue348_reactivation").await;
    let create_pool =
        one_connection_pool(&url, "paperless_archivist_issue348_reactivation_create").await;
    let backfill =
        tokio::spawn(
            async move { backfill_metadata_stage_for_ocr_only_runs(&backfill_pool).await },
        );
    let create = tokio::spawn(async move {
        create_run_with_jobs(
            &create_pool,
            document_id,
            &[Stage::Ocr, Stage::Metadata],
            ProcessingMode::ManualReview,
            "test-reactivation-race",
            "test",
        )
        .await
    });

    wait_for_issue_348_lock_waiters(&pool, 2).await;
    blocker.commit().await.expect("release reactivation race");
    timeout(Duration::from_secs(5), backfill)
        .await
        .expect("reactivation must not deadlock")
        .expect("reactivation task")
        .expect("reactivation succeeds");
    let returned_run_id = timeout(Duration::from_secs(5), create)
        .await
        .expect("create must not deadlock")
        .expect("create task")
        .expect("create succeeds");

    let active_run_ids = sqlx::query_scalar::<_, Uuid>(
        r#"
        select id
          from pipeline_runs
         where paperless_document_id = $1
           and status in ('queued', 'running', 'waiting_review', 'applying')
        "#,
    )
    .bind(document_id)
    .fetch_all(&pool)
    .await
    .expect("load active runs after reactivation race");
    assert_eq!(active_run_ids, vec![returned_run_id]);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn bulk_resolves_every_active_run_conflict_before_taking_the_audit_lock() {
    let Some((_guard, url, pool)) = fresh_pool().await else {
        return;
    };
    let new_document = 5_809;
    let transitioning_document = 5_810;
    let old_run_id = create_run_with_jobs(
        &pool,
        transitioning_document,
        &[Stage::Ocr],
        ProcessingMode::ManualReview,
        "test-transitioning-run",
        "test",
    )
    .await
    .expect("create transitioning run");

    let transition_pool =
        one_connection_pool(&url, "paperless_archivist_issue348_row_transition").await;
    let (row_locked_tx, row_locked_rx) = oneshot::channel();
    let (take_audit_tx, take_audit_rx) = oneshot::channel();
    let transition = tokio::spawn(async move {
        let mut tx = transition_pool
            .begin()
            .await
            .expect("begin status transition");
        sqlx::query(
            r#"
            update pipeline_runs
               set status = 'succeeded', finished_at = now(), updated_at = now()
             where id = $1
            "#,
        )
        .bind(old_run_id)
        .execute(&mut *tx)
        .await
        .expect("lock and transition existing run");
        row_locked_tx.send(()).expect("announce row lock");
        take_audit_rx.await.expect("wait before taking audit lock");
        sqlx::query("select pg_advisory_xact_lock(hashtext('paperless_archivist_audit_events'))")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok::<(), anyhow::Error>(())
    });
    row_locked_rx.await.expect("transition holds run row");

    let bulk_pool = one_connection_pool(&url, "paperless_archivist_issue348_row_bulk").await;
    let bulk = tokio::spawn(async move {
        create_runs_for_documents(
            &bulk_pool,
            &[new_document, transitioning_document],
            &[Stage::Ocr],
            ProcessingMode::ManualReview,
            "test-row-lock-order",
            "test",
            None,
        )
        .await
    });
    wait_for_issue_348_lock_waiters(&pool, 1).await;
    take_audit_tx
        .send(())
        .expect("let transition request audit lock");

    timeout(Duration::from_secs(5), transition)
        .await
        .expect("status transition must not deadlock")
        .expect("status transition task")
        .expect("status transition succeeds");
    assert_eq!(
        timeout(Duration::from_secs(5), bulk)
            .await
            .expect("bulk must not deadlock")
            .expect("bulk task")
            .expect("bulk succeeds"),
        2
    );

    let active_run_id: Uuid = sqlx::query_scalar(
        r#"
        select id from pipeline_runs
         where paperless_document_id = $1
           and status in ('queued', 'running', 'waiting_review', 'applying')
        "#,
    )
    .bind(transitioning_document)
    .fetch_one(&pool)
    .await
    .expect("replacement active run");
    assert_ne!(active_run_id, old_run_id);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn vision_requeue_reactivates_only_the_newest_terminal_run_per_document() {
    let Some((_guard, _url, pool)) = fresh_pool().await else {
        return;
    };
    let document_id = 5_811;
    let mut run_ids = Vec::new();
    for trigger in ["test-old-crash", "test-new-crash"] {
        let run_id = create_run_with_jobs(
            &pool,
            document_id,
            &[Stage::Ocr],
            ProcessingMode::ManualReview,
            trigger,
            "test",
        )
        .await
        .expect("create vision-crashed run");
        sqlx::query(
            "update jobs set status = 'failed', error_message = 'GGML_ASSERT test' where run_id = $1",
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .expect("fail vision job");
        sqlx::query(
            "update pipeline_runs set status = 'failed', finished_at = now() where id = $1",
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .expect("fail vision run");
        run_ids.push(run_id);
    }

    let summary = requeue_vision_crashed_jobs(&pool)
        .await
        .expect("requeue one latest vision-crashed run");
    assert_eq!(summary.jobs_requeued, 1);
    let active_run_id: Uuid = sqlx::query_scalar(
        r#"
        select id from pipeline_runs
         where paperless_document_id = $1
           and status in ('queued', 'running', 'waiting_review', 'applying')
        "#,
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("one reactivated vision run");
    assert_eq!(active_run_id, run_ids[1]);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "select count(*) from pipeline_runs where paperless_document_id = $1 and status = 'failed'",
        )
        .bind(document_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn metadata_backfill_reactivates_only_the_newest_terminal_run_per_document() {
    let Some((_guard, _url, pool)) = fresh_pool().await else {
        return;
    };
    let document_id = 5_812;
    let mut run_ids = Vec::new();
    for trigger in ["test-old-ocr", "test-new-ocr"] {
        let run_id = create_run_with_jobs(
            &pool,
            document_id,
            &[Stage::Ocr],
            ProcessingMode::ManualReview,
            trigger,
            "test",
        )
        .await
        .expect("create completed OCR-only run");
        sqlx::query("update jobs set status = 'succeeded' where run_id = $1")
            .bind(run_id)
            .execute(&pool)
            .await
            .expect("complete OCR job");
        sqlx::query(
            "update pipeline_runs set status = 'succeeded', finished_at = now() where id = $1",
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .expect("complete OCR-only run");
        run_ids.push(run_id);
    }

    let summary = backfill_metadata_stage_for_ocr_only_runs(&pool)
        .await
        .expect("backfill one latest completed OCR-only run");
    assert_eq!(summary.runs_updated, 1);
    assert_eq!(summary.jobs_inserted, 1);
    let active_run_id: Uuid = sqlx::query_scalar(
        r#"
        select id from pipeline_runs
         where paperless_document_id = $1
           and status in ('queued', 'running', 'waiting_review', 'applying')
        "#,
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("one reactivated metadata run");
    assert_eq!(active_run_id, run_ids[1]);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "select count(*) from pipeline_runs where paperless_document_id = $1 and status = 'succeeded'",
        )
        .bind(document_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn a_locked_document_does_not_block_creation_for_another_document() {
    let Some((_guard, url, pool)) = fresh_pool().await else {
        return;
    };
    let blocked_document = 5_803;
    let independent_document = 5_804;
    let blocker = acquire_document_lock(&pool, ACTIVE_RUN_LOCK_NAME, blocked_document).await;
    let blocked_pool = one_connection_pool(&url, "paperless_archivist_issue348_blocked_doc").await;

    let blocked_create = tokio::spawn(async move {
        create_run_with_jobs(
            &blocked_pool,
            blocked_document,
            &[Stage::Ocr],
            ProcessingMode::ManualReview,
            "test-blocked",
            "test",
        )
        .await
    });
    wait_for_issue_348_lock_waiters(&pool, 1).await;

    let independent_id = timeout(
        Duration::from_secs(2),
        create_run_with_jobs(
            &pool,
            independent_document,
            &[Stage::Ocr],
            ProcessingMode::ManualReview,
            "test-independent",
            "test",
        ),
    )
    .await
    .expect("another document must not wait for the blocked document")
    .expect("independent create succeeds");
    assert_eq!(
        assert_one_created_run(&pool, independent_document, &[Stage::Ocr]).await,
        independent_id
    );

    blocker.commit().await.expect("release blocked document");
    timeout(Duration::from_secs(5), blocked_create)
        .await
        .expect("blocked create completes")
        .expect("blocked create task")
        .expect("blocked create succeeds");
}
