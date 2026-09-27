use super::*;

fn audit_hash_fixture(source_ip: Option<&str>, user_agent: Option<&str>) -> AuditEventInput {
    AuditEventInput {
        event_type: "user.roles_changed".to_owned(),
        actor_type: "user".to_owned(),
        actor_id: Some("actor-17".to_owned()),
        run_id: Some(Uuid::parse_str("018f0000-0000-7000-8000-000000000001").unwrap()),
        job_id: Some(Uuid::parse_str("018f0000-0000-7000-8000-000000000002").unwrap()),
        paperless_document_id: Some(4904),
        before: Some(json!({ "roles": ["admin"] })),
        after: Some(json!({ "roles": ["viewer"] })),
        metadata: Some(json!({ "reason": "fixture" })),
        outcome: "success".to_owned(),
        error_message: None,
        source_ip: source_ip.map(str::to_owned),
        user_agent: user_agent.map(str::to_owned),
    }
}

#[test]
fn audit_hash_v1_canonical_fixture_is_stable() {
    let id = Uuid::parse_str("018f0000-0000-7000-8000-000000000003").unwrap();
    let created_at = "2026-07-17T08:00:00Z".parse::<DateTime<Utc>>().unwrap();
    let previous = Some("previous-event-hash".to_owned());
    let event = audit_hash_fixture(Some("203.0.113.17"), Some("Archivist-Test/2"));

    assert_eq!(
        audit_event_hash_v1(id, created_at, &previous, &event),
        "ffd758b87049d65f9446a44190021fe0f1886a6fbaecace90a28de3c3d9368ea"
    );
}

#[test]
fn audit_hash_v1_ignores_origin_but_v2_binds_values_and_nulls() {
    let id = Uuid::parse_str("018f0000-0000-7000-8000-000000000003").unwrap();
    let created_at = "2026-07-17T08:00:00Z".parse::<DateTime<Utc>>().unwrap();
    let previous = Some("previous-event-hash".to_owned());
    let with_origin = audit_hash_fixture(Some("203.0.113.17"), Some("Archivist-Test/2"));
    let other_origin = audit_hash_fixture(Some("203.0.113.18"), Some("Archivist-Test/3"));
    let null_origin = audit_hash_fixture(None, None);

    assert_eq!(
        audit_event_hash_v1(id, created_at, &previous, &with_origin),
        audit_event_hash_v1(id, created_at, &previous, &other_origin)
    );
    let v2 = audit_event_hash_v2(id, created_at, &previous, &with_origin);
    assert_eq!(
        v2,
        "55f41a0611b9a83a4e7abdf657f9ffc463ecf49ffd575699a29c6c113d4073a7"
    );
    assert_ne!(
        v2,
        audit_event_hash_v2(id, created_at, &previous, &other_origin)
    );
    assert!(audit_event_hash_for_version(99, id, created_at, &previous, &with_origin).is_none());
    assert_ne!(
        v2,
        audit_event_hash_v2(id, created_at, &previous, &null_origin)
    );
    assert_ne!(
        audit_event_hash_v2(id, created_at, &previous, &null_origin),
        audit_event_hash_v2(
            id,
            created_at,
            &previous,
            &audit_hash_fixture(Some("203.0.113.17"), None)
        )
    );
}

#[test]
fn audit_timestamp_is_canonicalized_to_postgres_precision_before_hashing() {
    let id = Uuid::parse_str("018f0000-0000-7000-8000-000000000003").unwrap();
    let source = "2026-07-17T08:00:00.123456789Z"
        .parse::<DateTime<Utc>>()
        .unwrap();
    let stored = "2026-07-17T08:00:00.123456Z"
        .parse::<DateTime<Utc>>()
        .unwrap();
    let previous = Some("previous-event-hash".to_owned());
    let event = audit_hash_fixture(None, None);

    let canonical = postgres_timestamp_precision(source);
    assert_eq!(canonical, stored);
    assert_ne!(
        audit_event_hash_v2(id, source, &previous, &event),
        audit_event_hash_v2(id, stored, &previous, &event),
        "the regression requires a source timestamp PostgreSQL would truncate"
    );
    assert_eq!(
        audit_event_hash_v2(id, canonical, &previous, &event),
        audit_event_hash_v2(id, stored, &previous, &event),
        "the hash input must exactly match the timestamp read back from PostgreSQL"
    );
}

#[test]
fn hashes_tokens_without_returning_raw_value() {
    assert_eq!(hash_token("secret"), hash_token("secret"));
    assert_ne!(hash_token("secret"), "secret");
}

#[test]
fn status_table_names_are_static_known_tables() {
    assert_eq!(StatusTable::Jobs.name(), "jobs");
    assert_eq!(StatusTable::PipelineRuns.name(), "pipeline_runs");
    assert_eq!(StatusTable::ReviewItems.name(), "review_items");
}

#[test]
fn status_column_for_stage_round_trips_every_business_stage() {
    // Every business stage must yield a static column name; orchestration-only stages
    // must surface a typed error so callers never silently fall through to format!.
    for stage in Stage::all_business_stages() {
        let column = status_column_for_stage(stage)
            .unwrap_or_else(|err| panic!("missing column for {stage}: {err}"));
        assert!(
            column.ends_with("_status"),
            "column for {stage} must end with _status, got {column}"
        );
    }
    assert!(status_column_for_stage(Stage::Apply).is_err());
}

fn empty_counts(total: i64, complete: i64) -> BacklogCounts {
    BacklogCounts {
        total_documents: total,
        complete,
        missing_ocr: 0,
        waiting_review: 0,
        failed: 0,
        running: 0,
        never_processed: 0,
    }
}

fn unrestricted_safety(dry_run: bool) -> WorkflowSafetyStatus {
    WorkflowSafetyStatus {
        paused: false,
        dry_run,
        hourly_document_limit: None,
        daily_document_limit: None,
        hourly_remaining: None,
        daily_remaining: None,
    }
}

fn live_failure(failure_kind: &str) -> DashboardLiveFailure {
    DashboardLiveFailure {
        id: Uuid::nil(),
        run_id: Uuid::nil(),
        paperless_document_id: 0,
        stage: Stage::Ocr,
        status: "failed".to_owned(),
        failure_kind: failure_kind.to_owned(),
        attempts: 1,
        error_message: String::new(),
        next_attempt_at: None,
        updated_at: Utc::now(),
    }
}

#[test]
fn dashboard_comparison_subtracts_previous_window_and_uses_snapshot_when_present() {
    let counts = empty_counts(120, 80);
    let current = ActivitySummary {
        jobs_created: 50,
        jobs_succeeded: 40,
        jobs_failed: 7,
    };
    let previous = Some(ActivitySummary {
        jobs_created: 30,
        jobs_succeeded: 25,
        jobs_failed: 4,
    });
    let comparison = compute_dashboard_comparison(&counts, current, previous, Some(50));
    assert_eq!(comparison.jobs_created_delta, 20);
    assert_eq!(comparison.jobs_succeeded_delta, 15);
    assert_eq!(comparison.jobs_failed_delta, 3);
    // open_backlog = 120 - 80 = 40; previous_open_backlog = 50; delta = -10.
    assert_eq!(comparison.open_backlog_delta, -10);
}

#[test]
fn dashboard_comparison_falls_back_to_zero_deltas_when_history_is_missing() {
    let counts = empty_counts(120, 80);
    let current = ActivitySummary {
        jobs_created: 5,
        jobs_succeeded: 3,
        jobs_failed: 1,
    };
    // No previous window and no snapshot -> deltas should all be zero
    // because the "previous" defaults to the current values and the
    // historical backlog defaults to the current open backlog.
    let comparison = compute_dashboard_comparison(&counts, current, None, None);
    assert_eq!(comparison.jobs_created_delta, 0);
    assert_eq!(comparison.jobs_succeeded_delta, 0);
    assert_eq!(comparison.jobs_failed_delta, 0);
    assert_eq!(comparison.open_backlog_delta, 0);
}

#[test]
fn backlog_series_empty_state_synthesises_a_single_now_point() {
    let mut points: Vec<DashboardBacklogPoint> = Vec::new();
    let now = Utc::now();
    let counts = BacklogCounts {
        total_documents: 250,
        complete: 200,
        missing_ocr: 0,
        waiting_review: 3,
        failed: 4,
        running: 2,
        never_processed: 0,
    };
    apply_backlog_series_empty_state_fallback(
        &mut points,
        now,
        archivist_core::DashboardGranularity::Hour,
        &counts,
    );
    assert_eq!(points.len(), 1);
    let point = &points[0];
    assert_eq!(point.total_documents, 250);
    assert_eq!(point.complete, 200);
    assert_eq!(point.open_backlog, 50);
    assert_eq!(point.failed, 4);
    assert_eq!(point.waiting_review, 3);
    assert_eq!(point.running, 2);
}

#[test]
fn backlog_series_empty_state_does_not_overwrite_existing_points() {
    let mut points: Vec<DashboardBacklogPoint> = vec![DashboardBacklogPoint {
        bucket: Utc::now(),
        label: "10:00".to_owned(),
        total_documents: 1,
        complete: 1,
        open_backlog: 0,
        failed: 0,
        waiting_review: 0,
        running: 0,
    }];
    apply_backlog_series_empty_state_fallback(
        &mut points,
        Utc::now(),
        archivist_core::DashboardGranularity::Hour,
        &empty_counts(99, 99),
    );
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].total_documents, 1);
}

#[test]
fn needs_attention_items_emit_one_entry_per_kind() {
    let safety = WorkflowSafetyStatus {
        paused: false,
        dry_run: true,
        hourly_document_limit: Some(100),
        daily_document_limit: Some(1000),
        hourly_remaining: Some(2),  // <= ceil(100 * 0.1) = 10
        daily_remaining: Some(900), // 900 > 100 -> not below threshold
    };
    let failures = vec![
        live_failure("failed"),
        live_failure("failed"),
        live_failure("failed"),
        live_failure("retry_scheduled"),
    ];
    let items = compose_needs_attention_items(
        2,
        1,
        &safety,
        &failures,
        &BlockedQueuedCounts::default(),
        &[],
    );
    let kinds: Vec<&str> = items.iter().map(|i| i.kind.as_str()).collect();
    assert!(kinds.contains(&"stuck_runs"));
    assert!(kinds.contains(&"stale_leases"));
    assert!(kinds.contains(&"quota_low"));
    assert!(kinds.contains(&"provider_error"));
    assert!(kinds.contains(&"dry_run_active"));
}

#[test]
fn needs_attention_items_sort_critical_before_warning_before_info() {
    let items = compose_needs_attention_items(
        5,
        5,
        &unrestricted_safety(true),
        &[
            live_failure("failed"),
            live_failure("failed"),
            live_failure("failed"),
        ],
        &BlockedQueuedCounts::default(),
        &[],
    );
    let severities: Vec<&str> = items.iter().map(|i| i.severity.as_str()).collect();
    // stuck_runs (critical) must come before stale_leases (warning),
    // dry_run_active (info) must come last.
    let critical_pos = severities
        .iter()
        .position(|s| *s == "critical")
        .expect("expected at least one critical item");
    let info_pos = severities
        .iter()
        .position(|s| *s == "info")
        .expect("expected at least one info item");
    assert!(
        critical_pos < info_pos,
        "critical severity ({critical_pos}) must sort before info ({info_pos}): {severities:?}"
    );
    for (index, severity) in severities.iter().enumerate().skip(1) {
        let prev = match severities[index - 1] {
            "critical" => 0,
            "warning" => 1,
            "info" => 2,
            _ => 3,
        };
        let curr = match *severity {
            "critical" => 0,
            "warning" => 1,
            "info" => 2,
            _ => 3,
        };
        assert!(prev <= curr, "ordering broken at {index}: {severities:?}");
    }
}

#[test]
fn needs_attention_items_skips_provider_error_when_failures_are_below_threshold() {
    let items = compose_needs_attention_items(
        0,
        0,
        &unrestricted_safety(false),
        &[live_failure("failed"), live_failure("failed")],
        &BlockedQueuedCounts::default(),
        &[],
    );
    let has_provider_error = items.iter().any(|i| i.kind == "provider_error");
    assert!(!has_provider_error);
}

#[test]
fn needs_attention_items_emit_blocked_jobs_when_present() {
    let items = compose_needs_attention_items(
        0,
        0,
        &unrestricted_safety(false),
        &[],
        &BlockedQueuedCounts {
            blocked_by_failed: 69,
            blocked_by_review: 24,
            total: 93,
        },
        &[],
    );
    let blocked = items
        .iter()
        .find(|i| i.kind == "blocked_jobs")
        .expect("expected a blocked_jobs alert when total > 0");
    assert_eq!(blocked.severity, "critical"); // any failed predecessor → critical
    assert_eq!(blocked.count, Some(93));
    assert_eq!(
        blocked.action_key.as_deref(),
        Some("dashboard.alerts.action.unblock_jobs"),
    );
}

#[test]
fn needs_attention_items_emit_provider_cooldown_when_active() {
    let cooldown = AiProviderCooldown {
        provider_name: "ollama".to_owned(),
        cooldown_until: Utc::now() + chrono::Duration::hours(6),
        reason: "weekly usage limit".to_owned(),
        set_at: Utc::now(),
    };
    let items = compose_needs_attention_items(
        0,
        0,
        &unrestricted_safety(false),
        &[],
        &BlockedQueuedCounts::default(),
        &[cooldown],
    );
    let item = items
        .iter()
        .find(|i| i.kind == "provider_cooldown")
        .expect("expected provider_cooldown alert");
    assert_eq!(item.severity, "critical");
    assert!(item.description.contains("ollama"));
}

#[test]
fn quota_below_threshold_uses_ten_percent_floor() {
    // Limit of 100 -> threshold = 10; remaining 10 must trip the alert,
    // remaining 11 must not.
    assert!(quota_below_threshold(Some(10), Some(100)));
    assert!(!quota_below_threshold(Some(11), Some(100)));
    // Limit of 3 -> threshold = max(ceil(0.3), 1) = 1; remaining 0 trips,
    // remaining 2 doesn't.
    assert!(quota_below_threshold(Some(0), Some(3)));
    assert!(!quota_below_threshold(Some(2), Some(3)));
    // Missing remaining or limit means no alert.
    assert!(!quota_below_threshold(None, Some(100)));
    assert!(!quota_below_threshold(Some(10), None));
    assert!(!quota_below_threshold(Some(10), Some(0)));
}

#[test]
fn encrypted_secret_round_trips() {
    let key = SecretString::from("a long local encryption key for tests".to_owned());
    let ciphertext = encrypt_secret(&key, "paperless-token").unwrap();
    assert_ne!(ciphertext, "paperless-token");
    let plaintext = decrypt_secret(&key, &ciphertext).unwrap();
    assert_eq!(plaintext, "paperless-token");
}

#[test]
fn ai_artifact_redaction_removes_prompts_images_and_response_text() {
    let value = json!({
        "model": "example",
        "system_prompt": "secret system prompt",
        "user_prompt": "full document text",
        "messages": [
            { "role": "user", "content": "private content", "images": ["base64-image"] }
        ],
        "usage": { "prompt_tokens": 10 }
    });
    let stored = prepare_ai_artifact_value(Some(value), AiArtifactStorageMode::Redacted).unwrap();
    let serialized = stored.to_string();

    assert!(!serialized.contains("secret system prompt"));
    assert!(!serialized.contains("full document text"));
    assert!(!serialized.contains("private content"));
    assert!(!serialized.contains("base64-image"));
    assert!(serialized.contains("redacted"));
    // Usage counters must survive redaction numerically, not as "[REDACTED]".
    assert_eq!(stored["usage"]["prompt_tokens"], 10);
}

#[test]
fn full_storage_mode_keeps_numeric_usage_but_redacts_credentials() {
    let value = json!({
        "api_key": "sk-very-secret",
        "options": { "token": "raw-secret", "num_ctx": 4096 },
        "usage": { "prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14 },
        "prompt_eval_count": 12,
        "eval_count": 7
    });
    let stored = prepare_ai_artifact_value(Some(value), AiArtifactStorageMode::Full).unwrap();

    assert_eq!(stored["usage"]["prompt_tokens"], 10);
    assert_eq!(stored["usage"]["completion_tokens"], 4);
    assert_eq!(stored["usage"]["total_tokens"], 14);
    assert_eq!(stored["prompt_eval_count"], 12);
    assert_eq!(stored["eval_count"], 7);
    assert_eq!(stored["api_key"], "[REDACTED]");
    assert_eq!(stored["options"]["token"], "[REDACTED]");
    assert_eq!(stored["options"]["num_ctx"], 4096);
}

#[test]
fn ai_artifact_metadata_only_keeps_usage_without_raw_content() {
    let value = json!({
        "model": "example",
        "response": "private model text",
        "usage": { "completion_tokens": 4 }
    });
    let stored =
        prepare_ai_artifact_value(Some(value), AiArtifactStorageMode::MetadataOnly).unwrap();
    let serialized = stored.to_string();

    assert!(!serialized.contains("private model text"));
    assert!(serialized.contains("metadata_only"));
    assert_eq!(stored["usage"]["completion_tokens"], 4);
}

#[test]
fn ai_response_token_usage_handles_all_wire_shapes() {
    // OpenAI/Anthropic usage block.
    assert_eq!(
        ai_response_token_usage(Some(
            &json!({ "usage": { "prompt_tokens": 100, "completion_tokens": 40 } })
        )),
        (100, 40)
    );
    // Anthropic-style input_tokens/output_tokens.
    assert_eq!(
        ai_response_token_usage(Some(
            &json!({ "usage": { "input_tokens": 5, "output_tokens": 2 } })
        )),
        (5, 2)
    );
    // Ollama top-level counters.
    assert_eq!(
        ai_response_token_usage(Some(&json!({ "prompt_eval_count": 7, "eval_count": 3 }))),
        (7, 3)
    );
    // OCR pages[] fallback fires only without a top-level usage block...
    assert_eq!(
        ai_response_token_usage(Some(&json!({
            "pages": [
                { "usage": { "prompt_tokens": 1000, "completion_tokens": 50 } },
                { "prompt_eval_count": 200, "eval_count": 30 },
            ]
        }))),
        (1200, 80)
    );
    // ...so a post-#259 flattened response is never double counted.
    assert_eq!(
        ai_response_token_usage(Some(&json!({
            "pages": [ { "usage": { "prompt_tokens": 10, "completion_tokens": 5 } } ],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5 },
        }))),
        (10, 5)
    );
    // Redacted strings / objects / negatives contribute 0, like the SQL
    // regexp guard in the 0040 backfill.
    assert_eq!(
        ai_response_token_usage(Some(&json!({
            "usage": { "prompt_tokens": "[REDACTED]", "completion_tokens": { "redacted": true } },
            "prompt_eval_count": -3
        }))),
        (0, 0)
    );
    assert_eq!(ai_response_token_usage(None), (0, 0));
}

#[test]
fn paperless_document_date_parses_dates_and_timestamps_leniently() {
    // Current Paperless reports a plain ISO date; older releases sent a
    // full RFC3339 timestamp. Both must yield the date; junk yields None
    // instead of failing the sync. #315
    let expected = NaiveDate::from_ymd_opt(2026, 6, 1);
    assert_eq!(parse_paperless_document_date(Some("2026-06-01")), expected);
    assert_eq!(
        parse_paperless_document_date(Some("2026-06-01T00:00:00+02:00")),
        expected
    );
    assert_eq!(
        parse_paperless_document_date(Some(" 2026-06-01 ")),
        expected
    );
    assert_eq!(parse_paperless_document_date(Some("01.06.2026")), None);
    assert_eq!(parse_paperless_document_date(Some("")), None);
    assert_eq!(parse_paperless_document_date(None), None);
}

#[test]
fn paperless_modified_timestamp_preserves_the_utc_instant() {
    let expected = DateTime::parse_from_rfc3339("2026-07-18T06:12:34.567890Z")
        .expect("valid expected timestamp")
        .with_timezone(&Utc);
    assert_eq!(
        parse_paperless_modified_at(Some("2026-07-18T08:12:34.567890+02:00")),
        Some(expected)
    );
    assert_eq!(parse_paperless_modified_at(Some("not-a-timestamp")), None);
    assert_eq!(parse_paperless_modified_at(None), None);
}

#[test]
fn live_llm_status_prefers_running_jobs() {
    let now = Utc::now();
    let job = DashboardLiveJob {
        id: Uuid::now_v7(),
        run_id: Uuid::now_v7(),
        trace_id: Uuid::now_v7(),
        paperless_document_id: 42,
        stage: Stage::Metadata,
        status: "running".to_owned(),
        attempts: 1,
        max_attempts: 3,
        lease_owner: Some("worker-1".to_owned()),
        lease_until: Some(now),
        updated_at: now,
        error_message: None,
    };

    let status = llm_processing_status(&[job], &[], &[]);

    assert_eq!(status.state, "running");
    assert!(status.description.contains("42"));
}

#[test]
fn live_paperless_status_reports_failed_audit_event() {
    let now = Utc::now();
    let event = PaperlessAuditEvent {
        event_type: "paperless.sync".to_owned(),
        outcome: "failed".to_owned(),
        created_at: now,
        error_message: Some("Paperless timeout".to_owned()),
    };

    let status = paperless_processing_status(&[], Some(&event), &[]);

    assert_eq!(status.state, "error");
    assert_eq!(status.description, "Paperless timeout");
}

#[test]
fn live_status_ignores_retry_scheduled_failures_as_hard_errors() {
    let now = Utc::now();
    let retry = DashboardLiveFailure {
        id: Uuid::now_v7(),
        run_id: Uuid::now_v7(),
        paperless_document_id: 135,
        stage: Stage::Ocr,
        status: "queued".to_owned(),
        failure_kind: "retry_scheduled".to_owned(),
        attempts: 1,
        error_message: "temporary model runner failure".to_owned(),
        next_attempt_at: Some(now),
        updated_at: now,
    };

    let status = llm_processing_status(&[], &[], &[retry]);

    assert_eq!(status.state, "idle");
    assert_eq!(status.title, "LLM idle");
}

#[test]
fn selector_document_budget_uses_tightest_remaining_limit() {
    let safety = WorkflowSafetyStatus {
        paused: false,
        dry_run: false,
        hourly_document_limit: Some(10),
        daily_document_limit: Some(100),
        hourly_remaining: Some(4),
        daily_remaining: Some(25),
    };

    assert_eq!(selector_document_budget(&safety), Some(4));

    let unlimited = WorkflowSafetyStatus {
        hourly_document_limit: None,
        daily_document_limit: None,
        hourly_remaining: None,
        daily_remaining: None,
        ..safety
    };

    assert_eq!(selector_document_budget(&unlimited), None);
}

#[test]
fn missing_pipeline_stages_skip_completed_documents_and_stage_tags() {
    // v1.4.0 default selector sequence is [Ocr, Metadata]; document with the OCR
    // completion tag but no metadata yet should yield Metadata only.
    let stages = missing_pipeline_stages_for_inventory(
        &Stage::all_business_stages(),
        InventoryStageState {
            ocr_status: "unknown".to_owned(),
            metadata_status: "unknown".to_owned(),
            has_ocr_completion_tag: true,
            // Documents with the tagging-completion tag are considered "metadata done"
            // because the legacy tag was applied after the per-field stages all ran.
            has_tagging_completion_tag: false,
            has_full_completion_tag: false,
        },
    );

    assert!(!stages.contains(&Stage::Ocr));
    assert!(stages.contains(&Stage::Metadata));

    let completed = missing_pipeline_stages_for_inventory(
        &Stage::all_business_stages(),
        InventoryStageState {
            ocr_status: "unknown".to_owned(),
            metadata_status: "unknown".to_owned(),
            has_ocr_completion_tag: false,
            has_tagging_completion_tag: false,
            has_full_completion_tag: true,
        },
    );

    assert!(completed.is_empty());
}

#[test]
fn missing_pipeline_stages_skip_documents_with_succeeded_metadata() {
    // Regression guard for the 0039 cleanup: before the fossil per-field
    // columns were dropped, the Metadata arm OR-ed six always-'unknown'
    // columns and therefore re-enqueued documents whose metadata_status
    // was already 'succeeded'. Only the consolidated column decides now.
    let stages = missing_pipeline_stages_for_inventory(
        &Stage::all_business_stages(),
        InventoryStageState {
            ocr_status: "succeeded".to_owned(),
            metadata_status: "succeeded".to_owned(),
            has_ocr_completion_tag: true,
            has_tagging_completion_tag: false,
            has_full_completion_tag: false,
        },
    );
    assert!(stages.is_empty());

    let needs_metadata = missing_pipeline_stages_for_inventory(
        &Stage::all_business_stages(),
        InventoryStageState {
            ocr_status: "succeeded".to_owned(),
            metadata_status: "failed".to_owned(),
            has_ocr_completion_tag: true,
            has_tagging_completion_tag: false,
            has_full_completion_tag: false,
        },
    );
    assert_eq!(needs_metadata, vec![Stage::Metadata]);
}

#[test]
fn missing_pipeline_stages_skip_rejected_terminal_stage() {
    let stages = missing_pipeline_stages_for_inventory(
        &[Stage::Metadata],
        InventoryStageState {
            ocr_status: "succeeded".to_owned(),
            metadata_status: "rejected".to_owned(),
            has_ocr_completion_tag: true,
            has_tagging_completion_tag: false,
            has_full_completion_tag: false,
        },
    );

    assert!(
        stages.is_empty(),
        "an explicit review rejection is resolved and must not be auto-requeued"
    );
}
